// SPDX-License-Identifier: GPL-3.0-or-later
//! IPS patch codec.
//!
//! IPS (International Patching System) is the simplest possible
//! sparse-delta format: a header, a sequence of `(offset, length,
//! data)` records, and a terminator. We use it as the storage format
//! for FDS disk saves - writes the game makes during play are
//! diffed against the original disk image and the diff saved
//! alongside the ROM as `<stem>.ips`. This matches Mesen2's
//! convention, giving cross-emulator interop.
//!
//! ## Format
//!
//! ```text
//! [5 bytes]   ASCII "PATCH"
//!
//! record (repeats):
//!   [3 bytes]   offset (big-endian) - stop if this is "EOF"
//!   [2 bytes]   length (big-endian)
//!     if length > 0:
//!       [length bytes]  data
//!     if length == 0:      (RLE record - we decode, don't emit)
//!       [2 bytes]   run length (big-endian)
//!       [1 byte]    fill byte
//!
//! [3 bytes]   ASCII "EOF" terminator
//! ```
//!
//! Offsets are 24-bit, so IPS can't represent files larger than
//! 16 MiB. FDS images are at most ~260 KiB so we're comfortably
//! within range.
//!
//! An offset whose three bytes happen to spell "EOF" (0x45 0x4F 0x46,
//! decimal 4542790) would be ambiguous with the terminator. No FDS
//! file is that large.
//!
//! ## What we implement
//!
//! - **Encode**: plain (non-RLE) records. Any byte in the new buffer
//!   that differs from the old is emitted, collapsing adjacent
//!   differences into a single record up to [`MAX_RECORD_LEN`].
//! - **Decode**: both plain and RLE records (patches produced by
//!   other tools may use RLE).
//! - **Round-trip**: `apply(base, encode(base, new)) == new` is the
//!   load-bearing property. Tested exhaustively on crafted inputs
//!   plus a random-mutation fuzz test.
//!
//! ## Why not a crate
//!
//! IPS encode + decode is ~100 LOC each and doing it ourselves keeps
//! the save path trustworthy - we know exactly how bytes become
//! files and back.

use std::fmt;

const MAGIC: &[u8; 5] = b"PATCH";
const EOF_MARKER: &[u8; 3] = b"EOF";

/// IPS 24-bit offset limit: 0..=0x00FFFFFF.
pub const MAX_OFFSET: u32 = 0x00FF_FFFF;

/// IPS 16-bit record length limit: 0..=0xFFFF. Adjacent differences
/// are split across records when the run exceeds this.
pub const MAX_RECORD_LEN: usize = 0xFFFF;

#[derive(Debug)]
pub enum IpsError {
    /// Missing or malformed "PATCH" magic.
    BadMagic,
    /// Offset would overflow 24 bits.
    OffsetTooLarge(u32),
    /// Input ended before a record completed.
    Truncated,
    /// Input is so large we can't represent any record-offset within
    /// it in 24 bits (files > 16 MiB).
    FileTooLarge(usize),
}

impl fmt::Display for IpsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IpsError::BadMagic => write!(f, "IPS patch missing 'PATCH' magic"),
            IpsError::OffsetTooLarge(o) => {
                write!(f, "IPS patch offset {} exceeds 24-bit maximum", o)
            }
            IpsError::Truncated => write!(f, "IPS patch truncated mid-record"),
            IpsError::FileTooLarge(n) => write!(
                f,
                "target file is {} bytes; IPS supports at most {} bytes",
                n,
                MAX_OFFSET as usize + 1
            ),
        }
    }
}

impl std::error::Error for IpsError {}

/// Build an IPS patch transforming `base` into `new`. Both slices must
/// have the same length; callers pad/truncate to the canonical size
/// before calling. Returns an empty-ish patch (magic + EOF only) when
/// `base == new`.
pub fn encode(base: &[u8], new: &[u8]) -> Result<Vec<u8>, IpsError> {
    assert_eq!(
        base.len(),
        new.len(),
        "ips::encode requires equal-length slices"
    );
    if base.len() > MAX_OFFSET as usize + 1 {
        return Err(IpsError::FileTooLarge(base.len()));
    }

    let mut out = Vec::with_capacity(MAGIC.len() + EOF_MARKER.len());
    out.extend_from_slice(MAGIC);

    let mut i = 0;
    while i < base.len() {
        if base[i] == new[i] {
            i += 1;
            continue;
        }
        // Scan a run of differing bytes. Cap at MAX_RECORD_LEN so the
        // length fits in the 16-bit field.
        let start = i;
        let max_end = (start + MAX_RECORD_LEN).min(base.len());
        let mut end = start + 1;
        while end < max_end && base[end] != new[end] {
            end += 1;
        }

        let offset = start as u32;
        if offset > MAX_OFFSET {
            return Err(IpsError::OffsetTooLarge(offset));
        }
        let len = end - start;

        // Refuse to emit an offset whose three bytes spell "EOF" -
        // reading that patch would stop at the terminator prematurely.
        // 0x454F46 = 4_542_790. FDS files are way smaller so this
        // path is unreachable for our use but keeps the encoder
        // generally correct.
        let eof_ambiguous = offset == 0x00_45_4F_46;

        // 3-byte big-endian offset.
        out.push(((offset >> 16) & 0xFF) as u8);
        out.push(((offset >> 8) & 0xFF) as u8);
        out.push((offset & 0xFF) as u8);

        // 2-byte big-endian length.
        out.push(((len >> 8) & 0xFF) as u8);
        out.push((len & 0xFF) as u8);

        out.extend_from_slice(&new[start..end]);

        if eof_ambiguous {
            // Shouldn't happen for FDS but be explicit if it ever does.
            return Err(IpsError::OffsetTooLarge(offset));
        }

        i = end;
    }

    out.extend_from_slice(EOF_MARKER);
    Ok(out)
}

/// Apply an IPS patch to `base`, returning a new buffer with the
/// patch applied. `base`'s length is preserved - we don't grow the
/// file (FDS disks are fixed-size; out-of-bounds writes would be a
/// bug we want to see).
pub fn apply(base: &[u8], patch: &[u8]) -> Result<Vec<u8>, IpsError> {
    if patch.len() < MAGIC.len() + EOF_MARKER.len() || &patch[0..5] != MAGIC {
        return Err(IpsError::BadMagic);
    }

    let mut out = base.to_vec();
    let mut p = &patch[5..];

    loop {
        if p.len() < 3 {
            return Err(IpsError::Truncated);
        }
        if &p[0..3] == EOF_MARKER {
            // Convention: if bytes remain after EOF, they're trailing
            // garbage we ignore - the reference IPS spec is silent
            // but real-world tools add stuff (e.g. size-extension
            // trailer). FDS needs none of that.
            return Ok(out);
        }
        if p.len() < 5 {
            return Err(IpsError::Truncated);
        }

        let offset = ((p[0] as usize) << 16) | ((p[1] as usize) << 8) | (p[2] as usize);
        let length = ((p[3] as usize) << 8) | (p[4] as usize);
        p = &p[5..];

        if length == 0 {
            // RLE record: 2-byte run + 1 fill byte.
            if p.len() < 3 {
                return Err(IpsError::Truncated);
            }
            let run = ((p[0] as usize) << 8) | (p[1] as usize);
            let fill = p[2];
            p = &p[3..];

            let end = offset.saturating_add(run);
            if end > out.len() {
                return Err(IpsError::OffsetTooLarge(end as u32));
            }
            for b in &mut out[offset..end] {
                *b = fill;
            }
        } else {
            if p.len() < length {
                return Err(IpsError::Truncated);
            }
            let end = offset.saturating_add(length);
            if end > out.len() {
                return Err(IpsError::OffsetTooLarge(end as u32));
            }
            out[offset..end].copy_from_slice(&p[..length]);
            p = &p[length..];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_diff_produces_magic_plus_eof_only() {
        let base = vec![1u8, 2, 3, 4, 5];
        let patch = encode(&base, &base).unwrap();
        assert_eq!(&patch[0..5], MAGIC);
        assert_eq!(&patch[5..8], EOF_MARKER);
        assert_eq!(patch.len(), 8);
    }

    #[test]
    fn single_byte_diff_roundtrips() {
        let base = vec![0u8; 32];
        let mut new = base.clone();
        new[7] = 0xAB;
        let patch = encode(&base, &new).unwrap();
        let applied = apply(&base, &patch).unwrap();
        assert_eq!(applied, new);
    }

    #[test]
    fn multiple_scattered_diffs_roundtrip() {
        let base = vec![0u8; 1024];
        let mut new = base.clone();
        new[0] = 1;
        new[500] = 2;
        new[1023] = 3;
        let patch = encode(&base, &new).unwrap();
        let applied = apply(&base, &patch).unwrap();
        assert_eq!(applied, new);
    }

    #[test]
    fn contiguous_diffs_merge_into_one_record() {
        let base = vec![0u8; 16];
        let mut new = base.clone();
        for i in 4..10 {
            new[i] = i as u8;
        }
        let patch = encode(&base, &new).unwrap();
        // Magic(5) + offset(3) + length(2) + data(6) + EOF(3) = 19.
        assert_eq!(patch.len(), 19);
        let applied = apply(&base, &patch).unwrap();
        assert_eq!(applied, new);
    }

    #[test]
    fn run_longer_than_max_record_splits() {
        // A contiguous diff longer than 0xFFFF bytes must split into
        // multiple records. Use 0x1_0001 = 65537.
        let len = MAX_RECORD_LEN + 2;
        let base = vec![0u8; len];
        let new = vec![0xFFu8; len];
        let patch = encode(&base, &new).unwrap();
        let applied = apply(&base, &patch).unwrap();
        assert_eq!(applied, new);
    }

    #[test]
    fn apply_rejects_missing_magic() {
        let err = apply(&[0u8; 4], b"NOPE\x00EOF").unwrap_err();
        assert!(matches!(err, IpsError::BadMagic));
    }

    #[test]
    fn apply_handles_rle_record() {
        // Hand-craft: fill bytes 8..16 of a 32-byte base with 0xAA.
        let mut patch = Vec::new();
        patch.extend_from_slice(MAGIC);
        // Offset 8 (3 bytes BE).
        patch.extend_from_slice(&[0x00, 0x00, 0x08]);
        // Length 0 → RLE.
        patch.extend_from_slice(&[0x00, 0x00]);
        // Run 8, fill 0xAA.
        patch.extend_from_slice(&[0x00, 0x08, 0xAA]);
        patch.extend_from_slice(EOF_MARKER);

        let base = vec![0u8; 32];
        let out = apply(&base, &patch).unwrap();
        for i in 0..32 {
            let expected = if (8..16).contains(&i) { 0xAA } else { 0 };
            assert_eq!(out[i], expected, "byte {i}");
        }
    }

    #[test]
    fn apply_rejects_truncated_record() {
        // Magic + offset (3) + length (2, non-zero) but no data.
        let mut patch = Vec::new();
        patch.extend_from_slice(MAGIC);
        patch.extend_from_slice(&[0x00, 0x00, 0x00]); // offset 0
        patch.extend_from_slice(&[0x00, 0x05]); // length 5
        // Missing 5 bytes of data and EOF.
        let base = vec![0u8; 16];
        assert!(matches!(apply(&base, &patch), Err(IpsError::Truncated)));
    }

    #[test]
    fn apply_rejects_oob_record() {
        // Record spans past the base buffer.
        let mut patch = Vec::new();
        patch.extend_from_slice(MAGIC);
        patch.extend_from_slice(&[0x00, 0x00, 0x0C]); // offset 12
        patch.extend_from_slice(&[0x00, 0x10]); // length 16 → reaches 28
        patch.extend_from_slice(&[0u8; 16]); // data
        patch.extend_from_slice(EOF_MARKER);
        let base = vec![0u8; 16];
        assert!(matches!(apply(&base, &patch), Err(IpsError::OffsetTooLarge(_))));
    }

    #[test]
    fn random_mutation_fuzz_roundtrip() {
        // Deterministic pseudo-randomness - seed it from a linear
        // congruential generator so the test is reproducible.
        let mut seed: u32 = 0xDEAD_BEEF;
        let mut rand = || -> u8 {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 24) as u8
        };

        for _ in 0..50 {
            let size = 1024 + (rand() as usize) * 8;
            let base: Vec<u8> = (0..size).map(|_| rand()).collect();
            let mut new = base.clone();
            // Mutate a random subset.
            for _ in 0..20 {
                let i = (rand() as usize) % size;
                new[i] = rand();
            }
            let patch = encode(&base, &new).unwrap();
            let applied = apply(&base, &patch).unwrap();
            assert_eq!(applied, new);
        }
    }

    #[test]
    fn trailing_bytes_after_eof_are_ignored() {
        let base = vec![0u8; 4];
        let mut patch = encode(&base, &[1u8, 2, 3, 4]).unwrap();
        patch.extend_from_slice(b"trailing garbage here");
        let out = apply(&base, &patch).unwrap();
        assert_eq!(out, vec![1, 2, 3, 4]);
    }
}
