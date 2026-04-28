// SPDX-License-Identifier: GPL-3.0-or-later
//! Host audio output.
//!
//! Two producer paths share a single stereo ring buffer + cpal output:
//!
//! - **NES path** (`on_cpu_cycle`): the APU produces a single
//!   analog-style sample in 0.0..=1.0 on every CPU cycle
//!   (~1.789 MHz NTSC / ~1.662 MHz PAL). We feed deltas into Blargg's
//!   `blip_buf` band-limited resampler, which reads out properly
//!   anti-aliased samples at the host rate. Each mono output sample
//!   is duplicated to both stereo channels.
//! - **SNES path** (`push_stereo_sample`): the S-DSP produces
//!   pre-band-limited stereo at 32 kHz natively (every 32 SMP
//!   cycles). Resampling 32 kHz → host rate is straight linear
//!   interpolation - no need for a brick-wall filter, since the
//!   input is already mixed at sub-Nyquist. Both channels survive
//!   to the host device.
//!
//! Thread layout:
//!
//!   emulator thread                          audio thread (cpal callback)
//!   ───────────────                          ───────────────────────────
//!   NES Bus::tick_post_access                HeapCons::try_pop ×2
//!     → AudioSink::on_cpu_cycle               → writes (L, R) into device frame
//!         → BlipBuf::add_delta
//!   every ~5 ms:
//!     → flush(): BlipBuf::read_samples
//!     → HeapProd::try_push (s, s) interleaved
//!
//!   SNES Snes::end_audio_frame
//!     → drain mixer.samples
//!     → AudioSink::push_stereo_sample(l, r)
//!         → linear-interp 32k → host rate
//!         → HeapProd::try_push (l_f, r_f) interleaved
//!
//! The ring is interleaved L,R: every two consecutive `f32` slots form
//! one stereo frame. Capacity (`ring_cap` below) is doubled so the
//! ~300 ms latency budget is preserved in stereo. cpal's output
//! callback chunks `data` into device frames and pops one (L, R)
//! pair per frame. For a mono device (channels==1) we average L+R;
//! for >2 channels the extras are zeroed.
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
/// resampler (NES path) plus a linear-interp resampler (SNES path)
/// and the ring-buffer producer side. The ring is interleaved
/// stereo (`f32` pairs).
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
    /// Linear-interp resampler state for the SNES 32 kHz stereo path.
    /// `prev_l`/`prev_r` hold the most recent input pair (in
    /// `[-1.0, 1.0]`); `pos` is the current output position relative
    /// to that pair, expressed in input-sample units. `step` is
    /// `input_rate / output_rate` - the amount `pos` advances per
    /// output sample emitted.
    snes_prev_l: f32,
    snes_prev_r: f32,
    snes_pos: f64,
    snes_step: f64,
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
                // Mono → stereo: same value for L and R, written as an
                // interleaved (L, R) pair so the cpal callback sees a
                // proper stereo frame.
                let _ = self.producer.try_push(f);
                let _ = self.producer.try_push(f);
            }
        }
    }

    /// Reset all internal resampler state. Called on core swap so
    /// the new core doesn't inherit the old one's interpolation
    /// position / BlipBuf delta history. The ring's contents (still
    /// in flight to the cpal callback) are left alone - the queued
    /// samples drain naturally into silence as the new core ramps up.
    pub fn reset(&mut self) {
        self.cycles = 0;
        self.last_scaled = 0;
        // BlipBuf has no public clear; cycling one zero-length frame
        // discards any pending deltas.
        self.blip.end_frame(0);
        loop {
            let n = self.blip.read_samples(&mut self.scratch, false);
            if n == 0 {
                break;
            }
        }
        self.snes_prev_l = 0.0;
        self.snes_prev_r = 0.0;
        self.snes_pos = 0.0;
    }

    /// Push one 32 kHz stereo sample from the SNES S-DSP. `l` / `r`
    /// are the post-master-volume signed 16-bit voice mix produced
    /// by [`crate::snes::smp::dsp::mixer::Mixer::step_sample`].
    /// Resamples linearly to the host rate and pushes interleaved
    /// (L, R) pairs into the same ring the NES path uses.
    pub fn push_stereo_sample(&mut self, l: i16, r: i16) {
        let l = l as f32 / 32_768.0;
        let r = r as f32 / 32_768.0;
        // Emit any output samples that fall between the previous
        // input pair (snes_prev) and the new one (l, r). Each
        // emitted sample's interpolation weight is `pos`, in
        // input-sample units (so `0.0` aligns with the previous
        // input, `1.0` with the new one).
        while self.snes_pos < 1.0 {
            let t = self.snes_pos as f32;
            let out_l = self.snes_prev_l + (l - self.snes_prev_l) * t;
            let out_r = self.snes_prev_r + (r - self.snes_prev_r) * t;
            let _ = self.producer.try_push(out_l);
            let _ = self.producer.try_push(out_r);
            self.snes_pos += self.snes_step;
        }
        self.snes_pos -= 1.0;
        self.snes_prev_l = l;
        self.snes_prev_r = r;
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

    // Ring is interleaved stereo, so capacity is doubled compared
    // with the old mono path: every two consecutive `f32` slots are
    // one (L, R) frame. The latency budget (RING_MILLIS) stays the
    // same in *frames*.
    let ring_frames = ((sample_rate as u64 * RING_MILLIS as u64) / 1000) as usize;
    let ring_cap = ring_frames.max(4096) * 2;
    let rb: HeapRb<f32> = HeapRb::new(ring_cap);
    let (mut producer, consumer) = rb.split();

    // Pre-fill the ring with silence so the cpal callback has
    // something to drain during the emulator's first frame of
    // startup work. Push (0.0, 0.0) pairs.
    let prefill_frames = ((sample_rate as u64 * PREFILL_MILLIS as u64) / 1000) as usize;
    for _ in 0..prefill_frames {
        if producer.try_push(0.0).is_err() {
            break;
        }
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

    // SNES path: input rate is fixed at the S-DSP's 32 kHz output
    // (`Mixer::step_sample`'s output cadence). `step` is
    // `input_rate / output_rate` - the per-output-sample advance in
    // input-sample units that drives the linear-interp loop.
    const SNES_INPUT_RATE: f64 = 32_000.0;
    let snes_step = SNES_INPUT_RATE / sample_rate as f64;
    let sink = AudioSink {
        blip,
        cycles: 0,
        last_scaled: 0,
        producer,
        scratch,
        cycles_per_flush,
        sample_rate,
        snes_prev_l: 0.0,
        snes_prev_r: 0.0,
        snes_pos: 0.0,
        snes_step,
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
    // Pop one (L, R) frame from the interleaved ring. For mono
    // devices, average the two channels (NES path pushes the same
    // value twice so this is a no-op; SNES path collapses real
    // stereo to mono cleanly). For >2 channels, the extras are
    // zeroed - we don't synthesise surround.
    let stream = match fmt {
        SampleFormat::F32 => device
            .build_output_stream(
                config,
                move |data: &mut [f32], _| {
                    for frame in data.chunks_mut(ch) {
                        let l = consumer.try_pop().unwrap_or(0.0);
                        let r = consumer.try_pop().unwrap_or(0.0);
                        write_stereo_frame(frame, l, r, ch, |s| s);
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
                        let l = consumer.try_pop().unwrap_or(0.0);
                        let r = consumer.try_pop().unwrap_or(0.0);
                        write_stereo_frame(frame, l, r, ch, |s| {
                            (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
                        });
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
                        let l = consumer.try_pop().unwrap_or(0.0);
                        let r = consumer.try_pop().unwrap_or(0.0);
                        write_stereo_frame(frame, l, r, ch, |s| {
                            ((s.clamp(-1.0, 1.0) * 0.5 + 0.5) * u16::MAX as f32) as u16
                        });
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

#[cfg(test)]
impl AudioSink {
    /// Build an AudioSink without a live cpal stream, returning the
    /// consumer side of the ring so tests can observe what the
    /// producer pushed. Sample rate is parameterised so we can cover
    /// the common 32 → 48 / 32 → 44.1 / equal-rate ratios.
    pub fn for_test(sample_rate: u32) -> (Self, HeapCons<f32>) {
        const SNES_INPUT_RATE: f64 = 32_000.0;
        let ring_cap = 16384;
        let rb: HeapRb<f32> = HeapRb::new(ring_cap);
        let (producer, consumer) = rb.split();
        let cpu_clock_hz = 1_789_773.0;
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
            snes_prev_l: 0.0,
            snes_prev_r: 0.0,
            snes_pos: 0.0,
            snes_step: SNES_INPUT_RATE / sample_rate as f64,
        };
        (sink, consumer)
    }
}

/// Place a stereo (L, R) f32 pair into the device frame, converting
/// each sample with `cvt`. Mono device: average L+R; >2 channels:
/// L → 0, R → 1, rest zeroed.
#[inline]
fn write_stereo_frame<S: Default + Copy>(
    frame: &mut [S],
    l: f32,
    r: f32,
    ch: usize,
    cvt: impl Fn(f32) -> S,
) {
    if ch == 1 {
        frame[0] = cvt((l + r) * 0.5);
    } else {
        frame[0] = cvt(l);
        frame[1] = cvt(r);
        for c in &mut frame[2..] {
            *c = S::default();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ringbuf::traits::Consumer;

    /// Pop `n` interleaved samples (L, R, L, R, ...) into a Vec.
    fn drain(consumer: &mut HeapCons<f32>, n: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(consumer.try_pop().unwrap_or(0.0));
        }
        out
    }

    #[test]
    fn nes_path_pushes_each_mono_sample_to_both_channels() {
        // Simulate enough cycles for BlipBuf to emit at least a few
        // output samples, with a varying input so we can see deltas.
        // Then assert that every output frame on the ring has L == R
        // (mono content broadcast to stereo).
        let (mut sink, mut consumer) = AudioSink::for_test(48_000);
        // Drive the input as a slow ramp at CPU rate. Even a few thousand
        // cycles is enough at 48 kHz output to land a handful of samples.
        for i in 0..40_000u32 {
            sink.on_cpu_cycle((i as f32 * 1e-5).sin() * 0.5);
        }
        sink.end_frame();
        let drained = drain(&mut consumer, 256);
        // Skip the leading zeros (BlipBuf has a small startup transient
        // before deltas show up). Find the first non-zero pair.
        let first_nonzero = drained
            .chunks_exact(2)
            .position(|f| f[0] != 0.0 || f[1] != 0.0)
            .expect("ring should contain non-zero NES audio");
        let mut compared = 0;
        for frame in drained.chunks_exact(2).skip(first_nonzero) {
            assert_eq!(
                frame[0], frame[1],
                "NES mono frame must be duplicated (L=={}, R=={})",
                frame[0], frame[1]
            );
            compared += 1;
        }
        assert!(compared > 0, "should have compared at least one stereo frame");
    }

    #[test]
    fn snes_path_preserves_stereo_separation() {
        // Equal-rate (32 kHz device) so linear interp degenerates to
        // pass-through (with a 1-sample delay): every input pair lands
        // verbatim on the ring once interpolation has primed.
        let (mut sink, mut consumer) = AudioSink::for_test(32_000);
        // Push a sequence with distinct L vs R values so we can verify
        // they don't get crossed or averaged.
        for i in 0..16i16 {
            sink.push_stereo_sample(i * 100, i * -100);
        }
        let drained = drain(&mut consumer, 32);
        // After the 1-sample priming delay the resampler should emit
        // `prev_l, prev_r` pairs in order. Just check that L and R
        // have opposite signs (matching the input pattern).
        let mut found_distinct = false;
        for frame in drained.chunks_exact(2) {
            if frame[0] != 0.0 && frame[1] != 0.0 {
                assert!(
                    (frame[0] > 0.0 && frame[1] < 0.0) || (frame[0] < 0.0 && frame[1] > 0.0),
                    "stereo pair must keep L/R distinct: ({}, {})",
                    frame[0],
                    frame[1]
                );
                found_distinct = true;
            }
        }
        assert!(found_distinct, "should have emitted a non-silent stereo pair");
    }

    #[test]
    fn snes_resampler_emits_correct_number_of_samples_for_32_to_48k() {
        // 32 kHz input -> 48 kHz output: ratio 1.5, so N inputs should
        // produce ~1.5*N outputs (within ±1 to account for fractional
        // accumulator state).
        let (mut sink, mut consumer) = AudioSink::for_test(48_000);
        let n_inputs = 1_000;
        for i in 0..n_inputs {
            // Slow ramp so interpolation produces meaningful values.
            let v = (i as i16).wrapping_mul(10);
            sink.push_stereo_sample(v, v);
        }
        // Count drained interleaved samples (each pair = 1 frame).
        let mut frames = 0usize;
        while consumer.try_pop().is_some() {
            // Pop the matching R; if missing the ring was inconsistent.
            assert!(consumer.try_pop().is_some(), "ring must be a multiple of 2");
            frames += 1;
        }
        let expected = (n_inputs as f64 * 1.5) as usize;
        let diff = (frames as i64 - expected as i64).unsigned_abs() as usize;
        assert!(
            diff <= 2,
            "expected ~{expected} frames at 48k from {n_inputs} 32k inputs, got {frames}"
        );
    }

    #[test]
    fn write_stereo_frame_mono_device_averages_channels() {
        let mut frame = [0.0f32];
        write_stereo_frame(&mut frame, 0.4, -0.2, 1, |s| s);
        assert!((frame[0] - 0.1).abs() < 1e-6, "mono should be (L+R)/2: {}", frame[0]);
    }

    #[test]
    fn write_stereo_frame_stereo_device_keeps_channels_separate() {
        let mut frame = [0.0f32; 2];
        write_stereo_frame(&mut frame, 0.4, -0.2, 2, |s| s);
        assert_eq!(frame[0], 0.4);
        assert_eq!(frame[1], -0.2);
    }

    #[test]
    fn write_stereo_frame_surround_device_zeros_extra_channels() {
        let mut frame = [9.0f32; 6];
        write_stereo_frame(&mut frame, 0.4, -0.2, 6, |s| s);
        assert_eq!(frame[0], 0.4);
        assert_eq!(frame[1], -0.2);
        for c in &frame[2..] {
            assert_eq!(*c, 0.0);
        }
    }
}
