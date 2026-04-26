// SPDX-License-Identifier: GPL-3.0-or-later
//! Host audio output.
//!
//! The APU produces a single analog-style sample in 0.0..=1.0 on every
//! CPU cycle (~1.789 MHz NTSC / ~1.662 MHz PAL). We need to feed that
//! into the host's audio device (typically 48 kHz) without aliasing.
//!
//! Resampling is done with Blargg's `blip_buf` - a band-limited step
//! synthesizer designed for exactly this problem: you hand it signal
//! *deltas* at the high clock rate and it reads out properly
//! bandlimited samples at the target rate. It handles anti-aliasing,
//! rational-ratio resampling, and DC blocking in one pass, and is what
//! every serious NES/Game Boy/SNES emulator uses.
//!
//! Thread layout:
//!
//!   emulator thread                          audio thread (cpal callback)
//!   ───────────────                          ───────────────────────────
//!   Bus::tick_post_access                    HeapCons::try_pop
//!     → AudioSink::on_cpu_cycle               → writes f32 into device buffer
//!         → BlipBuf::add_delta
//!   every ~20 ms:
//!     → flush(): BlipBuf::read_samples
//!     → HeapProd::try_push
//!
//! The ring buffer between threads is a lock-free SPSC queue (`ringbuf`
//! 0.4). Sizing it generously (~300 ms) is deliberate: the emulator
//! produces ~734 samples in a burst at end-of-frame, while cpal drains
//! continuously, so a slow frame (wgpu present blocking on compositor,
//! OS scheduler hiccup, DMA-heavy scene) can starve the ring for
//! 30–100 ms at a time. Pre-filling ~100 ms of silence at startup
//! prevents the very first frame's setup cost from causing an
//! immediate underrun click.
//!
//! Back-pressure policy: if the ring ever *does* fill, `try_push`
//! silently drops the oldest-not-yet-written sample; if it drains,
//! the cpal callback writes zeros. A proper audio-driven pacing pass
//! (where the emulator sleeps when the ring is full rather than when
//! a wall-clock deadline says so) is a later phase.

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

/// Scale factor from the bus-combined analog level (APU + cart-side
/// expansion audio) into signed 16-bit range.
///
/// Peak 2A03 output is ~1.0 with all five channels at max. Expansion
/// chips (FDS ≈ 0.26, VRC6 ≈ 0.91, MMC5/N163/Sunsoft 5B/VRC7 TBD)
/// add on top linearly, so the bus sum can comfortably exceed 1.0
/// when an FDS or VRC6 cart hits its loud spots. `20000` gives us
/// 32768/20000 ≈ 1.64× headroom - enough to keep APU + VRC6 +
/// FDS peaks below the i16 ceiling without clipping into the
/// harmonic-distortion regime that was making our output sound
/// louder than Mesen2.
///
/// When a settings UI lands, this becomes the "100% master volume"
/// reference and users get a proper slider.
const AMP_SCALE: f32 = 20_000.0;

/// Ring-buffer depth in milliseconds - upper bound on audio latency
/// and the size of the stall this path can absorb without silencing.
/// 300 ms handles a Wayland compositor's occasional 60-Hz hitch or a
/// wgpu swapchain stall without a pop.
const RING_MILLIS: u32 = 300;

/// Initial silence pre-fill, in milliseconds. Given to the cpal
/// callback before the emulator has produced any samples so we don't
/// hear a click at startup.
const PREFILL_MILLIS: u32 = 100;

/// BlipBuf→ring flush cadence in milliseconds (converted to CPU
/// cycles at stream-open). 5 ms smooths out per-frame burstiness -
/// samples trickle into the ring rather than arriving in one
/// 16.6 ms lump per emulator frame - which keeps the consumer from
/// oscillating near the empty end of the ring.
const FLUSH_MILLIS: u32 = 5;

/// Produces samples on the emulator thread. Holds the `BlipBuf`
/// resampler and the ring-buffer producer side. `on_cpu_cycle` is
/// called from [`crate::bus::Bus::tick_post_access`] every CPU cycle.
pub struct AudioSink {
    blip: blip_buf::BlipBuf,
    /// Cycles accumulated since the last `blip.end_frame()` call. Reset
    /// by `flush`. Must stay below the BlipBuf's internal capacity -
    /// see `cycles_per_flush`.
    cycles: u32,
    /// Previous sample value in scaled integer space. BlipBuf consumes
    /// *deltas*, not levels, so we diff against this.
    last_scaled: i32,
    producer: HeapProd<f32>,
    scratch: Vec<i16>,
    cycles_per_flush: u32,
    /// Output sample rate of the cpal stream. Constant for the lifetime
    /// of the stream; cached so we can re-tune BlipBuf after a region
    /// change.
    sample_rate: u32,
}

impl AudioSink {
    /// Called by the bus once per CPU cycle. `analog` is the APU's
    /// nonlinear-mixer output in `0.0..=1.0`.
    #[inline]
    pub fn on_cpu_cycle(&mut self, analog: f32) {
        let scaled = (analog * AMP_SCALE) as i32;
        let delta = scaled - self.last_scaled;
        if delta != 0 {
            self.blip.add_delta(self.cycles, delta);
            self.last_scaled = scaled;
        }
        self.cycles += 1;
        if self.cycles >= self.cycles_per_flush {
            self.flush();
        }
    }

    /// Flush any pending samples at end of an emulator frame. The main
    /// loop calls this after `step_until_frame` to bound latency; the
    /// cycle-count trigger inside `on_cpu_cycle` is a safety net for
    /// emulation runs that don't go through the frame loop.
    pub fn end_frame(&mut self) {
        self.flush();
    }

    /// Re-tune the resampler for a new CPU clock rate. Called when a
    /// ROM swap changes the TV system (NTSC ↔ PAL). Flushes any in-flight
    /// samples at the old rate first so they drain at their original
    /// pitch, then reconfigures BlipBuf + the cycle→flush threshold so
    /// subsequent samples are resampled at the new rate.
    pub fn set_cpu_clock(&mut self, cpu_clock_hz: f64) {
        self.flush();
        self.blip.set_rates(cpu_clock_hz, self.sample_rate as f64);
        self.cycles_per_flush =
            ((cpu_clock_hz * FLUSH_MILLIS as f64) / 1000.0).max(1.0) as u32;
    }

    fn flush(&mut self) {
        if self.cycles == 0 {
            return;
        }
        self.blip.end_frame(self.cycles);
        self.cycles = 0;
        // Always drain BlipBuf to empty, even if the ring is full.
        // Unread samples accumulate in `blip.avail`; across several
        // full-ring flushes `avail` would eventually exceed the
        // internal `samples` vector and `end_frame` would panic. We
        // silently drop samples the consumer can't keep up with
        // instead - back-pressure manifests as occasional clicks, not
        // a dead audio thread.
        loop {
            let n = self.blip.read_samples(&mut self.scratch, false);
            if n == 0 {
                break;
            }
            for &s in &self.scratch[..n] {
                let f = s as f32 / 32_768.0;
                let _ = self.producer.try_push(f);
            }
        }
    }
}

/// Keeps the cpal output stream alive. Dropping this silences audio.
pub struct AudioStream {
    _stream: Stream,
    pub sample_rate: u32,
    pub channels: u16,
}

/// Open the default host output device, build a stream, and return the
/// (sink, stream) pair. The emulator pushes samples into `sink` on its
/// own thread; `stream` owns the cpal callback that drains samples on
/// the audio thread.
///
/// `cpu_clock_hz` is the APU sample rate: 1.789773 MHz NTSC,
/// 1.662607 MHz PAL. Getting this wrong only affects pitch.
pub fn start(cpu_clock_hz: f64) -> Result<(AudioSink, AudioStream)> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .context("no default audio output device")?;
    let supported = device
        .default_output_config()
        .context("probe default output config")?;
    let sample_format = supported.sample_format();
    let sample_rate = supported.sample_rate().0;
    let channels = supported.channels();
    let stream_config: StreamConfig = supported.into();

    let ring_cap = ((sample_rate as u64 * RING_MILLIS as u64) / 1000) as usize;
    let ring_cap = ring_cap.max(4096);
    let rb: HeapRb<f32> = HeapRb::new(ring_cap);
    let (mut producer, consumer) = rb.split();

    // Pre-fill the ring with silence so the cpal callback has
    // something to drain during the emulator's first frame of
    // startup work.
    let prefill = ((sample_rate as u64 * PREFILL_MILLIS as u64) / 1000) as usize;
    for _ in 0..prefill {
        if producer.try_push(0.0).is_err() {
            break;
        }
    }

    let stream = build_stream(&device, &stream_config, sample_format, channels, consumer)?;
    stream.play().context("start audio stream")?;

    // BlipBuf internal buffer must be >= samples produced between
    // flushes. We flush every FLUSH_MILLIS; sizing for 4× that is
    // comfortable headroom and lets a single missed flush tolerate.
    let blip_cap = (sample_rate / 1000 * FLUSH_MILLIS * 4).max(2048);
    let mut blip = blip_buf::BlipBuf::new(blip_cap);
    blip.set_rates(cpu_clock_hz, sample_rate as f64);

    let cycles_per_flush = ((cpu_clock_hz * FLUSH_MILLIS as f64) / 1000.0).max(1.0) as u32;
    let scratch = vec![0i16; blip_cap as usize];

    let sink = AudioSink {
        blip,
        cycles: 0,
        last_scaled: 0,
        producer,
        scratch,
        cycles_per_flush,
        sample_rate,
    };
    let stream = AudioStream {
        _stream: stream,
        sample_rate,
        channels,
    };
    Ok((sink, stream))
}

fn build_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    fmt: SampleFormat,
    channels: u16,
    mut consumer: HeapCons<f32>,
) -> Result<Stream> {
    let err_fn = |e: cpal::StreamError| log::warn!("vibenes audio: {e}");
    let ch = channels as usize;
    let stream = match fmt {
        SampleFormat::F32 => device
            .build_output_stream(
                config,
                move |data: &mut [f32], _| {
                    for frame in data.chunks_mut(ch) {
                        let s = consumer.try_pop().unwrap_or(0.0);
                        for c in frame {
                            *c = s;
                        }
                    }
                },
                err_fn,
                None,
            )
            .context("build f32 output stream")?,
        SampleFormat::I16 => device
            .build_output_stream(
                config,
                move |data: &mut [i16], _| {
                    for frame in data.chunks_mut(ch) {
                        let s = consumer.try_pop().unwrap_or(0.0).clamp(-1.0, 1.0);
                        let v = (s * i16::MAX as f32) as i16;
                        for c in frame {
                            *c = v;
                        }
                    }
                },
                err_fn,
                None,
            )
            .context("build i16 output stream")?,
        SampleFormat::U16 => device
            .build_output_stream(
                config,
                move |data: &mut [u16], _| {
                    for frame in data.chunks_mut(ch) {
                        let s = consumer.try_pop().unwrap_or(0.0).clamp(-1.0, 1.0);
                        let v = ((s * 0.5 + 0.5) * u16::MAX as f32) as u16;
                        for c in frame {
                            *c = v;
                        }
                    }
                },
                err_fn,
                None,
            )
            .context("build u16 output stream")?,
        other => return Err(anyhow!("unsupported cpal sample format: {other:?}")),
    };
    Ok(stream)
}
