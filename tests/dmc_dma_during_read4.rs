//! Integration tests for `~/Git/nes-test-roms/dmc_dma_during_read4/*.nes`.
//!
//! This suite exercises the DMC DMA interaction with CPU reads of
//! `$2007` and `$4016`. Two of the five ROMs are "CPU/PPU-sync-
//! dependent" and accept any of a set of CRC-hashed output patterns
//! rather than a single "golden" hash — which is why the standard
//! `test_runner` treats them as failing when the emulator's output
//! is nevertheless in an accepted bucket.
//!
//! The three `test_runner`-green ROMs (`dma_2007_read.nes`,
//! `dma_2007_write.nes`, `read_write_2007.nes`) are covered here too
//! so a future regression in the blargg $6000 protocol doesn't slip
//! past the gating sweep.

use std::path::PathBuf;

use vibenes::blargg_2005_scan::{nametable_has_text, read_nametable_ascii, StuckPcDetector};
use vibenes::nes::Nes;
use vibenes::rom::Cartridge;

const POLL_INTERVAL_CYCLES: u64 = 10_000;
const CYCLE_LIMIT: u64 = 200_000_000;

fn rom_path(name: &str) -> PathBuf {
    let home = std::env::var("HOME").expect("HOME must be set to locate test ROMs");
    PathBuf::from(home).join("Git/nes-test-roms/dmc_dma_during_read4").join(name)
}

/// Run the ROM until the CPU traps in a `forever:` loop and
/// nametable 0 contains printable text, then return the scanned
/// text. Every ROM in this suite reports via on-screen text (no
/// `$6000` handshake).
fn run_rom(name: &str) -> String {
    let path = rom_path(name);
    assert!(
        path.exists(),
        "missing test ROM {} — clone https://github.com/christopherpow/nes-test-roms",
        path.display()
    );
    let cart = Cartridge::load(&path).expect("load cartridge");
    let mut nes = Nes::from_cartridge(cart).expect("construct Nes");
    let mut detector = StuckPcDetector::new();
    let start = nes.bus.clock.cpu_cycles();
    loop {
        let elapsed = nes.bus.clock.cpu_cycles() - start;
        assert!(
            elapsed < CYCLE_LIMIT,
            "{name}: cycle limit exceeded before nametable settled"
        );
        nes.run_cycles(POLL_INTERVAL_CYCLES)
            .unwrap_or_else(|e| panic!("{name}: CPU error: {e}"));
        assert!(!nes.cpu.halted, "{name}: CPU halted");
        if detector.observe(nes.cpu.pc) && nametable_has_text(&nes) {
            return read_nametable_ascii(&nes);
        }
    }
}

/// Run the ROM until its nametable contains the given keyword
/// (case-insensitive). Used for the ROMs that finish by printing
/// `Passed` / `Failed` and never trap in a tight `forever:` loop
/// (so the stuck-PC detector isn't the right signal). Panics with
/// the nametable if the keyword hasn't appeared before the cycle
/// limit.
fn wait_for_keyword(name: &str, keyword: &str) -> String {
    let path = rom_path(name);
    assert!(
        path.exists(),
        "missing test ROM {} — clone https://github.com/christopherpow/nes-test-roms",
        path.display()
    );
    let cart = Cartridge::load(&path).expect("load cartridge");
    let mut nes = Nes::from_cartridge(cart).expect("construct Nes");
    let start = nes.bus.clock.cpu_cycles();
    let lower_needle = keyword.to_ascii_lowercase();
    loop {
        let elapsed = nes.bus.clock.cpu_cycles() - start;
        assert!(
            elapsed < CYCLE_LIMIT,
            "{name}: cycle limit exceeded without seeing {keyword:?}\n\
             --- nametable ---\n{}",
            read_nametable_ascii(&nes)
        );
        nes.run_cycles(POLL_INTERVAL_CYCLES)
            .unwrap_or_else(|e| panic!("{name}: CPU error: {e}"));
        assert!(!nes.cpu.halted, "{name}: CPU halted");
        if nametable_has_text(&nes) {
            let text = read_nametable_ascii(&nes);
            if text.to_ascii_lowercase().contains(&lower_needle) {
                return text;
            }
        }
    }
}

fn assert_printed_passed(name: &str) {
    wait_for_keyword(name, "Passed");
}

fn run_rom_nametable(name: &str) -> String {
    run_rom(name)
}

/// Extract the `$XXXXXXXX` CRC that blargg's shell.inc prints near the
/// end of every test. Returns the 8-hex-digit CRC as-is (uppercase).
fn extract_crc(text: &str) -> Option<String> {
    // Blargg's `print_crc` prints the CRC as exactly 8 hex digits on
    // its own line; look for an 8-uppercase-hex-digit token.
    for line in text.lines() {
        let token = line.trim();
        if token.len() == 8 && token.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(token.to_ascii_uppercase());
        }
    }
    None
}

// -------------------- test_runner-green ROMs (sanity) --------------------

/// `dma_2007_read.nes` — DMC DMA hits the CPU's read of `$2007`.
/// Reports via the `$6000` status protocol. Gated here as a
/// regression guard on the halt-cycle-replay code path in
/// `Bus::service_pending_dmc_dma`.
/// `dma_2007_read.nes` — DMC DMA hits an `lda $2007` and causes
/// 2–3 extra buffer-advancing reads before the real read completes.
/// The test source lists two accepted output patterns (`33 44` or
/// `44 55` at the DMA-aligned iteration, everywhere else `11 22`)
/// with distinct CRCs (`159A7A8F` / `5E3DF9C4`).
///
/// Strict pattern check: exactly the 3rd of 5 rows (index 2)
/// is `33 44` or `44 55`; the other four are `11 22`. After the
/// parity-aware DMC stall fix (commit b413b09) we land on the
/// `44 55` pattern (CRC `5E3DF9C4`).
#[test]
fn rom_dma_2007_read_matches_sanctioned_pattern() {
    let text = run_rom_nametable("dma_2007_read.nes");
    let rows: Vec<&str> = text
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.len() == 5
                && trimmed.chars().nth(2) == Some(' ')
                && trimmed
                    .chars()
                    .filter(|c| *c != ' ')
                    .all(|c| c.is_ascii_hexdigit())
            {
                Some(trimmed)
            } else {
                None
            }
        })
        .take(5)
        .collect();
    assert_eq!(
        rows.len(),
        5,
        "dma_2007_read: expected 5 result rows, got {rows:?}\n\
         --- nametable ---\n{text}"
    );
    for (i, row) in rows.iter().enumerate() {
        if i == 2 {
            assert!(
                *row == "33 44" || *row == "44 55",
                "dma_2007_read: row 2 must be 33 44 or 44 55, got {row:?}\n\
                 --- nametable ---\n{text}"
            );
        } else {
            assert_eq!(
                *row, "11 22",
                "dma_2007_read: row {i} must be 11 22, got {row:?}\n\
                 --- nametable ---\n{text}"
            );
        }
    }
}

#[test]
fn rom_dma_2007_write() {
    assert_printed_passed("dma_2007_write.nes");
}

#[test]
fn rom_read_write_2007() {
    assert_printed_passed("read_write_2007.nes");
}

// -------------------- sync-dependent ROMs (CRC-bucket) --------------------

/// `double_2007_read.nes` — `lda abs,X` with page-cross to `$2007`.
/// The test's source comment (`dmc_dma_during_read4/source/
/// double_2007_read.s:5-11`) lists four accepted output patterns,
/// each with its own CRC — real hardware's outcome "depends on CPU-
/// PPU synchronization". We aim for a deterministic result in any
/// one of those buckets.
///
/// Current result: `$F018C287` (bucket 2: "first read returns
/// buffered value; second read advances v but returns same
/// buffer"). See `src/ppu.rs::cpu_read_dummy`.
#[test]
fn rom_double_2007_read_lands_in_accepted_bucket() {
    const ACCEPTED: &[&str] = &[
        "85CFD627", // 22 33 44 55 66 / 22 44 55 66 77
        "F018C287", // 22 33 44 55 66 / 22 33 44 55 66  <-- ours
        "440EF923", // 22 33 44 55 66 / 02 44 55 66 77
        "E52F41A5", // 22 33 44 55 66 / 32 44 55 66 77
    ];
    let text = run_rom_nametable("double_2007_read.nes");
    let crc = extract_crc(&text)
        .unwrap_or_else(|| panic!("no CRC found\n--- nametable ---\n{text}"));
    assert!(
        ACCEPTED.contains(&crc.as_str()),
        "double_2007_read produced CRC {crc}; not in accepted set {ACCEPTED:?}\n\
         --- nametable ---\n{text}"
    );
}

/// `dma_4016_read.nes` — DMC DMA's halt cycle re-reads `$4016`
/// during an `lda $4016`, consuming one extra controller bit. The
/// expected output per the source comment is `08 08 07 08 08`:
/// exactly one of the five iterations drops from 8 bits to 7
/// because the DMC halt aligned with the CPU's read cycle.
///
/// Hardware-exact pattern is `08 08 07 08 08` (CRC `F0AB808C`) —
/// after the parity-aware DMC stall + reset-tick alignment fixes
/// in commit b413b09 we land on it.
#[test]
fn rom_dma_4016_read_matches_golden_pattern() {
    let text = run_rom_nametable("dma_4016_read.nes");
    let counts: Vec<u8> = text
        .lines()
        .find_map(|line| {
            let tokens: Vec<&str> = line.split_whitespace().collect();
            if tokens.len() == 5
                && tokens.iter().all(|t| t.len() == 2 && t.chars().all(|c| c.is_ascii_hexdigit()))
            {
                Some(
                    tokens
                        .iter()
                        .map(|t| u8::from_str_radix(t, 16).unwrap())
                        .collect(),
                )
            } else {
                None
            }
        })
        .unwrap_or_else(|| panic!("dma_4016_read: no 5-count line found\n--- nametable ---\n{text}"));

    assert_eq!(
        counts,
        vec![8, 8, 7, 8, 8],
        "dma_4016_read: expected golden 08 08 07 08 08 (CRC F0AB808C), got {counts:?}\n\
         --- nametable ---\n{text}"
    );
}
