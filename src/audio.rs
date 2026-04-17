//! Host audio output.
//!
//! The APU produces a single analog-style sample in 0.0..=1.0 on every
//! CPU cycle (~1.789 MHz NTSC / ~1.662 MHz PAL). We need to feed that
//! into the host's audio device (typically 48 kHz) without aliasing.
//!
//! Resampling is done with Blargg's `blip_buf` — a band-limited step
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
//! 0.4). If the emulator runs *faster* than realtime the ring fills and
//! `try_push` drops samples — manifest as a faint click every few
//! seconds when NTSC wall-clock pacing drifts relative to the host's
//! audio clock. If the emulator runs *slower* the ring drains and the
//! callback writes silence. Both are audible but not catastrophic; a
//! proper audio-driven pacing pass is a later phase.

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

/// Scale factor from APU's `0.0..=1.0` analog level into signed 16-bit
/// range. Keeps enough headroom that peak output (~0.5 on loud frames,
/// modulo DC offset) lands near half-scale rather than clipping. The
/// blip filter is DC-blocking, so the absolute offset doesn't matter —
/// only the magnitude of deltas.
const AMP_SCALE: f32 = 25_000.0;

/// How much audio to buffer between the emulator and the audio device,
/// expressed as a fraction of one second. 100 ms is enough slack to
/// absorb cpal callback jitter (~5–20 ms per wakeup on most OSes)
/// without perceptible latency.
const RING_SECONDS_NUM: u32 = 1;
const RING_SECONDS_DEN: u32 = 10;

/// How often the sink flushes `BlipBuf` into the ring, in CPU cycles.
/// Computed at startup as `cpu_clock_hz / FLUSH_DIVISOR` (≈ 20 ms).
/// Flushing more often lowers latency but adds per-flush overhead.
const FLUSH_DIVISOR: f64 = 50.0;

/// Produces samples on the emulator thread. Holds the `BlipBuf`
/// resampler and the ring-buffer producer side. `on_cpu_cycle` is
/// called from [`crate::bus::Bus::tick_post_access`] every CPU cycle.
pub struct AudioSink {
    blip: blip_buf::BlipBuf,
    /// Cycles accumulated since the last `blip.end_frame()` call. Reset
    /// by `flush`. Must stay below the BlipBuf's internal capacity —
    /// see `cycles_per_flush`.
    cycles: u32,
    /// Previous sample value in scaled integer space. BlipBuf consumes
    /// *deltas*, not levels, so we diff against this.
    last_scaled: i32,
    producer: HeapProd<f32>,
    scratch: Vec<i16>,
    cycles_per_flush: u32,
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
        // instead — back-pressure manifests as occasional clicks, not
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

    let ring_cap = ((sample_rate as u64 * RING_SECONDS_NUM as u64)
        / RING_SECONDS_DEN as u64) as usize;
    let ring_cap = ring_cap.max(4096);
    let rb: HeapRb<f32> = HeapRb::new(ring_cap);
    let (producer, consumer) = rb.split();

    let stream = build_stream(&device, &stream_config, sample_format, channels, consumer)?;
    stream.play().context("start audio stream")?;

    // BlipBuf internal buffer must be large enough to hold all samples
    // generated between flushes. We flush every ~20 ms, so sizing it
    // for ~100 ms is comfortable headroom.
    let blip_cap = (sample_rate / RING_SECONDS_DEN).max(2048);
    let mut blip = blip_buf::BlipBuf::new(blip_cap);
    blip.set_rates(cpu_clock_hz, sample_rate as f64);

    let cycles_per_flush = (cpu_clock_hz / FLUSH_DIVISOR).max(1.0) as u32;
    let scratch = vec![0i16; blip_cap as usize];

    let sink = AudioSink {
        blip,
        cycles: 0,
        last_scaled: 0,
        producer,
        scratch,
        cycles_per_flush,
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
