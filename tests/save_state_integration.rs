// SPDX-License-Identifier: GPL-3.0-or-later
//! Save-state integration tests.
//!
//! End-to-end correctness check: a ROM is run for N cycles, the
//! state is snapshotted, the original is run for M more cycles,
//! and a fresh `Nes` (or the original after apply) restored from
//! the snapshot must produce **byte-identical** state at the same
//! cycle target.
//!
//! These tests use an in-memory NROM cart with a known program -
//! no external test-rom dependency, so they run in CI without any
//! `~/Git/nes-test-roms` clone.
//!
//! What's covered:
//! - `Snapshot::capture` → `Snapshot::apply` round-trips the live
//!   Nes through the snapshot tree and produces byte-equal state.
//! - bincode encode → decode → apply to a fresh Nes from the same
//!   cart reproduces the saved state exactly. This is the actual
//!   "save to disk, restart, load" workflow minus the disk.
//! - Run-after-restore matches a continuous-run reference at the
//!   same cycle target. This is the strongest correctness signal:
//!   if any subsystem state is missing from the snapshot, the
//!   diverging run will desynchronize within a few thousand
//!   cycles and the assertion fails.

use vibenes::nes::Nes;
use vibenes::nes::rom::{Cartridge, Mirroring, TvSystem};
use vibenes::save_state::Snapshot;

/// Build a 32 KiB NROM cart whose reset vector points to a busy
/// loop that increments `$0000` (zero-page RAM) and `$0001`. The
/// CPU advances deterministically; the APU's frame counter still
/// fires periodic IRQs (we ignore them - I flag is set), and the
/// PPU runs whatever its idle pipeline does. No bus access in the
/// program touches an interesting register; that's deliberate -
/// we want the cycle/PPU-state machine running, not a bug-prone
/// program path to cover.
fn build_test_cart() -> Cartridge {
    let mut prg = vec![0u8; 0x8000]; // 32 KiB - mirrored at $8000 and $C000

    // Program at the start of PRG (mapped to $8000):
    //   $8000: SEI            ; mask IRQs (78)
    //   $8001: CLD            ; clear decimal (D8)
    //   $8002: LDX #$FF       ; A2 FF
    //   $8004: TXS            ; 9A - stack pointer = $FF
    //   $8005: LDA #$00       ; A9 00
    //   $8007: STA $00        ; 85 00
    //   $8009: STA $01        ; 85 01
    //   $800B: INC $00        ; E6 00
    //   $800D: INC $01        ; E6 01
    //   $800F: JMP $800B      ; 4C 0B 80
    let prog: &[u8] = &[
        0x78, 0xD8, 0xA2, 0xFF, 0x9A, 0xA9, 0x00, 0x85, 0x00, 0x85, 0x01, 0xE6, 0x00, 0xE6, 0x01,
        0x4C, 0x0B, 0x80,
    ];
    prg[..prog.len()].copy_from_slice(prog);

    // NROM-128 maps the 16 KiB PRG to $8000 AND $C000 - so the
    // reset vector at $FFFC needs to point to $8000 OR $C000 (same
    // contents). We use $8000.
    // For the 32 KiB NROM-256 case the entire 32 KiB is PRG[0]; we
    // duplicate the program at offset 0x4000 so the high half mirrors
    // it (irrelevant for the reset vector, but defensive).
    prg[0x4000..0x4000 + prog.len()].copy_from_slice(prog);

    // Reset vector at $FFFC/$FFFD = $8000.
    prg[0x7FFC] = 0x00;
    prg[0x7FFD] = 0x80;
    // NMI / IRQ vectors at $FFFA/$FFFE point back to $8000 too.
    prg[0x7FFA] = 0x00;
    prg[0x7FFB] = 0x80;
    prg[0x7FFE] = 0x00;
    prg[0x7FFF] = 0x80;

    Cartridge {
        prg_rom: prg,
        chr_rom: vec![0u8; 0x2000],
        chr_ram: false,
        mapper_id: 0,
        submapper: 0,
        mirroring: Mirroring::Vertical,
        battery_backed: false,
        prg_ram_size: 0x2000,
        prg_nvram_size: 0,
        tv_system: TvSystem::Ntsc,
        is_nes2: false,
        prg_chr_crc32: 0xCAFE_F00D,
        db_matched: false,
        fds_data: None,
    }
}

fn build_test_nes() -> Nes {
    Nes::from_cartridge(build_test_cart()).expect("construct Nes")
}

/// Capture two `Nes` states and assert they're byte-identical.
/// Going through the snapshot tree (and bincode-encoding it) is
/// the strongest "are these two Nes objects in the same state?"
/// check we have - it covers every field that's part of the
/// save-state schema, which is exactly what we want to round-trip.
fn assert_nes_byte_equal(label: &str, a: &Nes, b: &Nes) {
    let snap_a = Snapshot::capture(a).expect("capture A");
    let snap_b = Snapshot::capture(b).expect("capture B");
    let bytes_a = bincode::serde::encode_to_vec(&snap_a, bincode::config::standard())
        .expect("encode A");
    let bytes_b = bincode::serde::encode_to_vec(&snap_b, bincode::config::standard())
        .expect("encode B");
    assert_eq!(
        bytes_a.len(),
        bytes_b.len(),
        "{label}: snapshots disagree on length ({} vs {})",
        bytes_a.len(),
        bytes_b.len(),
    );
    if bytes_a != bytes_b {
        // Find the first divergence so a regression is debuggable
        // without manually diffing megabyte-scale snapshots.
        let first_diff = bytes_a
            .iter()
            .zip(bytes_b.iter())
            .position(|(x, y)| x != y)
            .unwrap_or(bytes_a.len());
        panic!(
            "{label}: snapshots diverge at byte {first_diff} (A={:#04X}, B={:#04X})",
            bytes_a.get(first_diff).copied().unwrap_or(0),
            bytes_b.get(first_diff).copied().unwrap_or(0),
        );
    }
}

const WARMUP_CYCLES: u64 = 50_000;
const POST_RESTORE_CYCLES: u64 = 50_000;

/// `Snapshot::capture(nes)` followed by `apply` back into the
/// same `Nes` is a no-op: capture is read-only, apply restores
/// every field we just read.
#[test]
fn capture_then_apply_self_is_a_noop() {
    let mut nes = build_test_nes();
    nes.run_cycles(WARMUP_CYCLES).expect("warmup");
    let before = Snapshot::capture(&nes).expect("capture");
    let bytes_before =
        bincode::serde::encode_to_vec(&before, bincode::config::standard()).unwrap();

    // Apply the snapshot we just took back into `nes`. State
    // should be unchanged at the byte level.
    before.apply(&mut nes).expect("apply self");

    let after = Snapshot::capture(&nes).expect("capture again");
    let bytes_after =
        bincode::serde::encode_to_vec(&after, bincode::config::standard()).unwrap();
    assert_eq!(bytes_before, bytes_after);
}

/// Capture → encode → decode → apply to a fresh `Nes` from the
/// same cart produces a Nes byte-identical to the source. This
/// is the actual save-to-disk + load-on-restart workflow, minus
/// the I/O.
#[test]
fn encode_decode_apply_to_fresh_nes_byte_equal() {
    let mut source = build_test_nes();
    source.run_cycles(WARMUP_CYCLES).expect("warmup source");

    let snap = Snapshot::capture(&source).expect("capture");
    let bytes = bincode::serde::encode_to_vec(&snap, bincode::config::standard()).unwrap();
    let (decoded, _consumed): (Snapshot, usize) =
        bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();

    let mut fresh = build_test_nes(); // pristine - just powered on
    decoded.apply(&mut fresh).expect("apply onto fresh Nes");

    assert_nes_byte_equal("post-apply onto fresh Nes", &source, &fresh);
}

/// Run-after-restore parity: a fresh `Nes` restored from a
/// snapshot, then advanced by N cycles, must produce the same
/// state as the source `Nes` advanced by the same N cycles. If
/// any subsystem state (DMA, frame counter, PPU pipeline, mapper
/// IRQ counter, etc.) is missing from the snapshot, the runs
/// will desync within a few thousand cycles and this test fails.
#[test]
fn run_after_restore_matches_continuous_run() {
    let mut source = build_test_nes();
    source.run_cycles(WARMUP_CYCLES).expect("warmup source");

    let snap = Snapshot::capture(&source).expect("capture");
    let bytes = bincode::serde::encode_to_vec(&snap, bincode::config::standard()).unwrap();
    let (decoded, _consumed): (Snapshot, usize) =
        bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();

    // Restore into a fresh Nes from the same cart.
    let mut restored = build_test_nes();
    decoded.apply(&mut restored).expect("apply");

    // Now run BOTH the source and the restored Nes by the same
    // additional cycles. They must remain in lock-step.
    source.run_cycles(POST_RESTORE_CYCLES).expect("source forward");
    restored
        .run_cycles(POST_RESTORE_CYCLES)
        .expect("restored forward");

    assert_nes_byte_equal(
        "after running POST_RESTORE_CYCLES on both",
        &source,
        &restored,
    );
}

/// Frame-level golden test: drive the source Nes 120 PPU frames
/// past the restore point and confirm the framebuffer matches a
/// freshly-restored Nes run for the same number of frames. The
/// PPU's framebuffer is cleared after every frame, so this
/// exercises the entire render pipeline post-restore - including
/// any subtle latch or shifter state.
#[test]
fn frame_buffer_byte_equal_after_round_trip() {
    let mut source = build_test_nes();
    // Warm up enough that the PPU has cycled through several
    // frames and is in a non-zero state when we snapshot.
    for _ in 0..30 {
        source.step_until_frame().expect("source warmup frame");
    }

    let snap = Snapshot::capture(&source).expect("capture");
    let bytes = bincode::serde::encode_to_vec(&snap, bincode::config::standard()).unwrap();
    let (decoded, _consumed): (Snapshot, usize) =
        bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();

    let mut restored = build_test_nes();
    decoded.apply(&mut restored).expect("apply");

    // Advance both Nes objects by the same number of frames and
    // compare the framebuffer at the end.
    const FRAMES_FORWARD: usize = 30;
    for i in 0..FRAMES_FORWARD {
        source
            .step_until_frame()
            .unwrap_or_else(|e| panic!("source frame {i}: {e}"));
        restored
            .step_until_frame()
            .unwrap_or_else(|e| panic!("restored frame {i}: {e}"));
    }

    assert_eq!(
        source.bus.ppu.frame_buffer.len(),
        restored.bus.ppu.frame_buffer.len(),
        "framebuffer length mismatch"
    );
    if source.bus.ppu.frame_buffer != restored.bus.ppu.frame_buffer {
        let first_diff = source
            .bus
            .ppu
            .frame_buffer
            .iter()
            .zip(restored.bus.ppu.frame_buffer.iter())
            .position(|(a, b)| a != b)
            .unwrap_or(source.bus.ppu.frame_buffer.len());
        panic!(
            "framebuffers diverge at byte {first_diff} \
             (source={:#04X}, restored={:#04X}) - some PPU / mapper \
             state is missing from the snapshot tree",
            source.bus.ppu.frame_buffer[first_diff],
            restored.bus.ppu.frame_buffer[first_diff],
        );
    }
}
