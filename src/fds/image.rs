//! FDS disk-image file parsing (`.fds` format, optional fwNES header).
//!
//! ## The `.fds` format
//!
//! Shipping Famicom Disk System games were distributed on proprietary
//! 56 KiB double-sided magnetic disks. Dumps circulate in a file
//! format called "fwNES" after the first emulator that supported them
//! (FamicomWorldNES):
//!
//! ```text
//! 16-byte fwNES header (optional — some dumps skip it):
//!   [0..4]   'F' 'D' 'S' 0x1A  — magic
//!   [4]      side_count (1..=4 typical; 2 is almost universal)
//!   [5..16]  zero padding
//!
//! followed by side_count × 65500 bytes of disk data.
//! ```
//!
//! A side contains a sequence of "blocks" on real hardware — header
//! block, file-amount block, file-header blocks, file-data blocks —
//! each tagged with a block-type byte. We don't parse the block
//! structure here; that's the disk-transport state machine's job in
//! [`crate::mapper::fds`]. Phase 0 treats each side as an opaque 65500
//! byte buffer.
//!
//! ## What we validate
//!
//! - Total file size matches (`side_count × 65500` + optional 16 B
//!   header). Dumps with trailing garbage — rare but we've seen them —
//!   get truncated to the declared side count with a warning hook.
//! - Every side's first block tag is `0x01` (disk header marker). A
//!   mismatch raises a warning but doesn't reject the file — some
//!   homebrew dumps are slightly off-spec and still work.
//! - The disk-header string `*NINTENDO-HVC*` at side-offset 1 is
//!   checked; mismatch → warning.
//!
//! Malformed files return [`ImageError`]. Byte-level interpretation
//! is deferred to the mapper.
//!
//! ## Scope
//!
//! This phase supports `.fds`. `.qd` (Quick Disk) format has the same
//! game data in 65536-byte sides (vs. 65500 for `.fds`) and is
//! deferred — none of the user's ROMs are `.qd`; adding support is a
//! drop-in extension to `from_bytes`.

use std::fmt;

/// Bytes per side in the `.fds` format. Real FDS disks held slightly
/// more, but the 36 bytes per side of gap + CRC + sync patterns are
/// stripped by the "fwNES" format we work with.
pub const SIDE_SIZE: usize = 65500;

/// Leading-gap length in bytes, prepended to each side before block
/// data starts. 28300 bits / 8 ≈ 3537 bytes, matching Mesen2's
/// `FdsLoader::AddGaps`. This is what the disk-transport scans
/// through before the first `0x80` sync marker on each side.
const LEADING_GAP_BYTES: usize = 28300 / 8;

/// Inter-block gap length — 976 bits / 8 = 122 bytes, matching Mesen2.
const BLOCK_GAP_BYTES: usize = 976 / 8;

/// Sync byte written before each block to signal the transport that
/// the gap has ended and real data follows.
const BLOCK_SYNC_BYTE: u8 = 0x80;

/// Fake 16-bit CRC appended after every block on real `.fds` dumps
/// (the format strips real CRCs). Mesen2 uses `{0x4D, 0x62}`.
const FAKE_CRC: [u8; 2] = [0x4D, 0x62];

/// Disk-header block size on real FDS disks: 56 bytes after the
/// block-type tag.
const DISK_HEADER_BLOCK_LEN: usize = 56;

/// File-count block size (after the tag).
const FILE_COUNT_BLOCK_LEN: usize = 2;

/// File-header block size (after the tag). The last 2 bytes (`offset+0x0D`,
/// `offset+0x0E`) encode the following file-data-block size.
const FILE_HEADER_BLOCK_LEN: usize = 16;

/// Optional fwNES header prefix magic. When present, the header is 16
/// bytes and the file is `16 + N*SIDE_SIZE` bytes; when absent, the
/// file is `N*SIDE_SIZE` bytes and we derive N from file size.
const FWNES_HEADER: [u8; 4] = *b"FDS\x1A";
const FWNES_HEADER_SIZE: usize = 16;

/// Disk-header block tag — the first byte of every well-formed side.
const BLOCK_TAG_DISK_HEADER: u8 = 0x01;

/// "*NINTENDO-HVC*" (14 chars) — the magic string every legitimate
/// Famicom disk's header block starts with, sitting at side-offset 1.
const NINTENDO_HVC: &[u8] = b"*NINTENDO-HVC*";

/// A parsed FDS disk image. Contains one to N sides, each exactly
/// [`SIDE_SIZE`] bytes.
#[derive(Clone)]
pub struct FdsImage {
    /// Per-side raw bytes. `sides[i]` is side i (typically 0 = A, 1 = B).
    /// Exactly [`SIDE_SIZE`] bytes each.
    pub sides: Vec<Vec<u8>>,
    /// True when the source file carried the 16-byte fwNES header.
    /// Surfaced mainly for diagnostics; we re-emit the header when
    /// saving an IPS sidecar if the original had one.
    pub had_header: bool,
    /// Any non-fatal validation warnings encountered during parse.
    /// Empty on well-formed files.
    pub warnings: Vec<String>,
}

impl fmt::Debug for FdsImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FdsImage")
            .field("sides", &self.sides.len())
            .field("side_bytes", &self.sides.first().map(|s| s.len()).unwrap_or(0))
            .field("had_header", &self.had_header)
            .field("warnings", &self.warnings.len())
            .finish()
    }
}

/// Parse errors for `.fds` files.
#[derive(Debug)]
pub enum ImageError {
    /// File length doesn't align to [`SIDE_SIZE`] (± the optional
    /// 16-byte fwNES header), so we can't figure out the side count.
    BadSize {
        len: usize,
        had_header: bool,
    },
    /// fwNES header declared a side count the file bytes can't satisfy.
    HeaderSideMismatch {
        declared: u8,
        actual: usize,
    },
    /// File shorter than the minimum ( > 0 bytes but < 1 side).
    TooShort(usize),
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::BadSize { len, had_header } => write!(
                f,
                "FDS image size {} bytes{} is not a whole number of {}-byte sides",
                len,
                if *had_header {
                    " (excl. 16-byte header)"
                } else {
                    ""
                },
                SIDE_SIZE,
            ),
            ImageError::HeaderSideMismatch { declared, actual } => write!(
                f,
                "fwNES header declares {} side(s) but file contains {}",
                declared, actual
            ),
            ImageError::TooShort(len) => write!(
                f,
                "FDS image is only {} bytes (one side is {})",
                len, SIDE_SIZE
            ),
        }
    }
}

impl std::error::Error for ImageError {}

impl FdsImage {
    /// Parse a raw file byte slice into an [`FdsImage`]. Accepts both
    /// fwNES-headered and bare variants. Validation warnings land in
    /// [`FdsImage::warnings`] rather than failing the load — none of
    /// our tested dumps are perfectly spec-clean.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ImageError> {
        let (data, had_header) = strip_optional_header(bytes)?;

        if data.len() < SIDE_SIZE {
            return Err(ImageError::TooShort(data.len()));
        }
        if data.len() % SIDE_SIZE != 0 {
            return Err(ImageError::BadSize {
                len: bytes.len(),
                had_header,
            });
        }
        let actual_sides = data.len() / SIDE_SIZE;

        // If the fwNES header claimed a specific side count, enforce it.
        // Otherwise derive from file size. A header claiming more sides
        // than the file can supply is a genuine error; a header claiming
        // fewer is just trailing garbage we silently trim.
        let sides_count = if had_header {
            let declared = bytes[4] as usize;
            if declared == 0 {
                // Some well-known homebrew dumps have a zero side count
                // even though the file is clearly a valid 1-side image.
                // Trust the file, warn.
                actual_sides
            } else if declared > actual_sides {
                return Err(ImageError::HeaderSideMismatch {
                    declared: declared as u8,
                    actual: actual_sides,
                });
            } else {
                declared
            }
        } else {
            actual_sides
        };

        let mut sides = Vec::with_capacity(sides_count);
        let mut warnings = Vec::new();

        for i in 0..sides_count {
            let start = i * SIDE_SIZE;
            let end = start + SIDE_SIZE;
            let side = data[start..end].to_vec();
            validate_side(i, &side, &mut warnings);
            sides.push(side);
        }

        if actual_sides > sides_count {
            warnings.push(format!(
                "file contains {} more side(s) than declared; ignored",
                actual_sides - sides_count,
            ));
        }

        Ok(Self {
            sides,
            had_header,
            warnings,
        })
    }

    /// Produce the scan-ready form of each side: prepend the leading
    /// gap, insert `0x80` sync bytes before each block, and append
    /// fake CRCs + inter-block gap after each block. The disk
    /// transport runs over these "gapped" bytes during emulation —
    /// gap zeros keep `_gapEnded = false`, sync bytes flip it true so
    /// the BIOS sees a real byte on the read-data register.
    ///
    /// Call once at load time; the result is the buffer the mapper
    /// actually addresses via its `_diskPosition`.
    ///
    /// **Port note:** behavior mirrors Mesen2's `FdsLoader::AddGaps`
    /// (`~/Git/Mesen2/Core/NES/Loaders/FdsLoader.cpp:26-91`). Both
    /// projects are GPL-3.0-or-later and the byte counts are
    /// protocol-exact, so re-deriving them here would just be a
    /// transcription hazard.
    pub fn gapped_sides(&self) -> Vec<Vec<u8>> {
        self.sides.iter().map(|s| add_gaps(s)).collect()
    }

    /// Extract the 56-byte disk-header block content from each side
    /// (the bytes right after the block-type `0x01` tag). Used by
    /// the auto-insert matching heuristic in Phase 2+. Returns an
    /// all-zeros buffer for any side that's missing the header tag.
    pub fn headers(&self) -> Vec<Vec<u8>> {
        self.sides
            .iter()
            .map(|s| {
                if s.len() >= 1 + DISK_HEADER_BLOCK_LEN && s[0] == BLOCK_TAG_DISK_HEADER {
                    s[1..1 + DISK_HEADER_BLOCK_LEN].to_vec()
                } else {
                    vec![0u8; DISK_HEADER_BLOCK_LEN]
                }
            })
            .collect()
    }

    /// Short human-readable summary for the `loaded:` log line.
    pub fn describe(&self) -> String {
        format!(
            "FDS {} side{} ({} KiB){}",
            self.sides.len(),
            if self.sides.len() == 1 { "" } else { "s" },
            (self.sides.len() * SIDE_SIZE) / 1024,
            if self.warnings.is_empty() {
                String::new()
            } else {
                format!(" [{} warning(s)]", self.warnings.len())
            },
        )
    }
}

/// Walk the block structure of a raw 65500-byte side and emit the
/// scan-ready form the disk transport expects. See [`FdsImage::gapped_sides`].
///
/// Port of Mesen2's `FdsLoader::AddGaps`. Block walking works like
/// this:
///
/// - Leading 28300 bits of gap zeros.
/// - For each well-formed block:
///   - `0x80` sync byte (transport flips `_gapEnded` true here).
///   - Block bytes (type + payload).
///   - 2 fake CRC bytes.
///   - 976 bits of inter-block gap zeros.
///
/// The blocks come in a fixed sequence: disk header (1) → file count
/// (2) → (file header (3) → file data (4)) × N. Block 4's length
/// depends on the size field from the preceding block-3, so we carry
/// a `last_file_size` variable through the walk.
///
/// Any unexpected tag bytes stop the structured walk and copy the
/// remaining raw side data as-is (wrapped in a sync-byte prefix) —
/// homebrew dumps with non-standard trailer bytes still work.
fn add_gaps(raw_side: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw_side.len() + 4096);
    // Leading gap.
    out.extend(std::iter::repeat(0u8).take(LEADING_GAP_BYTES));

    let mut i: usize = 0;
    while i < raw_side.len() {
        let block_type = raw_side[i];
        let block_len: usize = match block_type {
            1 => DISK_HEADER_BLOCK_LEN,
            2 => FILE_COUNT_BLOCK_LEN,
            3 => FILE_HEADER_BLOCK_LEN,
            4 => {
                // File data. The preceding file-header block's bytes
                // at offsets +0x0D and +0x0E (zero-indexed: 13 and 14)
                // encode the file size. Mesen2 uses `i - 3` / `i - 2`
                // — that's the position of those bytes RELATIVE to
                // `i`, which points at the next block's type tag.
                // Walking the layout: the file-header block type is
                // at `i - 16`, the size bytes at `i - 16 + 13` =
                // `i - 3` and `i - 16 + 14` = `i - 2`. Mesen2 is
                // right; transcribe their offset.
                if i < 3 {
                    break;
                }
                let size_lo = raw_side[i - 3] as usize;
                let size_hi = raw_side[i - 2] as usize;
                1 + (size_lo | (size_hi << 8))
            }
            _ => {
                // Unexpected byte — copy the rest of the side raw,
                // prefixed with a sync marker. Matches Mesen2's
                // fallback and accommodates non-standard dumps.
                out.push(BLOCK_SYNC_BYTE);
                out.extend_from_slice(&raw_side[i..]);
                return out;
            }
        };

        if i + block_len > raw_side.len() {
            // Block would run past the end of the side — stop
            // emitting, leave remaining side as gap bytes. The
            // transport reads through them without asserting
            // transfer-complete.
            break;
        }

        out.push(BLOCK_SYNC_BYTE);
        out.extend_from_slice(&raw_side[i..i + block_len]);
        out.extend_from_slice(&FAKE_CRC);
        out.extend(std::iter::repeat(0u8).take(BLOCK_GAP_BYTES));

        i += block_len;
    }

    out
}

/// If the file starts with the fwNES magic, split off the 16-byte
/// header and return `(data, true)`. Otherwise pass through.
fn strip_optional_header(bytes: &[u8]) -> Result<(&[u8], bool), ImageError> {
    if bytes.len() >= FWNES_HEADER_SIZE && bytes[0..4] == FWNES_HEADER {
        Ok((&bytes[FWNES_HEADER_SIZE..], true))
    } else {
        Ok((bytes, false))
    }
}

fn validate_side(index: usize, side: &[u8], warnings: &mut Vec<String>) {
    if side[0] != BLOCK_TAG_DISK_HEADER {
        warnings.push(format!(
            "side {} does not start with disk-header block tag 0x01 (got 0x{:02X})",
            index, side[0]
        ));
    }
    if side.len() < 1 + NINTENDO_HVC.len() || &side[1..1 + NINTENDO_HVC.len()] != NINTENDO_HVC {
        warnings.push(format!(
            "side {} does not carry the expected *NINTENDO-HVC* magic at offset 1",
            index
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimally-valid side (header block tag + magic + zero
    /// padding to [`SIDE_SIZE`]). Tests use this to avoid pulling in
    /// full real FDS data.
    fn synthetic_side() -> Vec<u8> {
        let mut s = vec![0u8; SIDE_SIZE];
        s[0] = BLOCK_TAG_DISK_HEADER;
        s[1..1 + NINTENDO_HVC.len()].copy_from_slice(NINTENDO_HVC);
        s
    }

    fn synthetic_bare(sides: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(sides * SIDE_SIZE);
        for _ in 0..sides {
            out.extend_from_slice(&synthetic_side());
        }
        out
    }

    fn synthetic_headered(sides: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(FWNES_HEADER_SIZE + sides * SIDE_SIZE);
        out.extend_from_slice(&FWNES_HEADER);
        out.push(sides as u8);
        out.extend_from_slice(&[0u8; 11]);
        for _ in 0..sides {
            out.extend_from_slice(&synthetic_side());
        }
        out
    }

    #[test]
    fn parses_bare_single_side() {
        let bytes = synthetic_bare(1);
        let image = FdsImage::from_bytes(&bytes).unwrap();
        assert_eq!(image.sides.len(), 1);
        assert!(!image.had_header);
        assert!(image.warnings.is_empty());
    }

    #[test]
    fn parses_bare_two_sides() {
        let image = FdsImage::from_bytes(&synthetic_bare(2)).unwrap();
        assert_eq!(image.sides.len(), 2);
        assert_eq!(image.sides[0].len(), SIDE_SIZE);
        assert_eq!(image.sides[1].len(), SIDE_SIZE);
    }

    #[test]
    fn parses_headered_with_correct_side_count() {
        let image = FdsImage::from_bytes(&synthetic_headered(2)).unwrap();
        assert_eq!(image.sides.len(), 2);
        assert!(image.had_header);
    }

    #[test]
    fn bare_file_too_small_rejects() {
        let bytes = vec![0u8; SIDE_SIZE - 100];
        assert!(matches!(
            FdsImage::from_bytes(&bytes),
            Err(ImageError::TooShort(_))
        ));
    }

    #[test]
    fn non_aligned_size_rejects() {
        // 65500 + some junk trailing bytes — not a whole side count.
        let mut bytes = synthetic_bare(1);
        bytes.extend_from_slice(&[0u8; 100]);
        match FdsImage::from_bytes(&bytes) {
            Err(ImageError::BadSize { .. }) => {}
            other => panic!("expected BadSize, got {:?}", other),
        }
    }

    #[test]
    fn header_declaring_more_sides_than_file_has_rejects() {
        let mut bytes = synthetic_headered(1);
        // Overwrite the declared side count with a too-large value.
        bytes[4] = 4;
        match FdsImage::from_bytes(&bytes) {
            Err(ImageError::HeaderSideMismatch {
                declared: 4,
                actual: 1,
            }) => {}
            other => panic!("expected HeaderSideMismatch, got {:?}", other),
        }
    }

    #[test]
    fn header_declaring_fewer_sides_warns_and_trims() {
        // File has 2 sides on disk but header says 1. We trust the
        // header and warn.
        let mut bytes = synthetic_headered(2);
        bytes[4] = 1;
        let image = FdsImage::from_bytes(&bytes).unwrap();
        assert_eq!(image.sides.len(), 1);
        assert_eq!(image.warnings.len(), 1);
        assert!(image.warnings[0].contains("more side"));
    }

    #[test]
    fn malformed_side_produces_warning_not_error() {
        let mut bytes = synthetic_bare(1);
        bytes[0] = 0xFF; // bad block tag
        bytes[1] = 0xFF; // corrupts the magic
        let image = FdsImage::from_bytes(&bytes).unwrap();
        assert_eq!(image.sides.len(), 1);
        assert!(image.warnings.len() >= 2);
    }

    #[test]
    fn describe_formats_sensibly() {
        let image = FdsImage::from_bytes(&synthetic_bare(2)).unwrap();
        let desc = image.describe();
        assert!(desc.contains("2 sides"), "got: {desc}");
        assert!(desc.contains("KiB"), "got: {desc}");
    }

    #[test]
    fn gapped_sides_prepends_leading_gap() {
        let image = FdsImage::from_bytes(&synthetic_bare(1)).unwrap();
        let gapped = image.gapped_sides();
        assert_eq!(gapped.len(), 1);
        // First LEADING_GAP_BYTES bytes must be zeros.
        for b in &gapped[0][..LEADING_GAP_BYTES] {
            assert_eq!(*b, 0);
        }
        // The leading-gap ends in a sync byte.
        assert_eq!(gapped[0][LEADING_GAP_BYTES], BLOCK_SYNC_BYTE);
    }

    #[test]
    fn gapped_sides_structures_blocks_with_syncs_and_crcs() {
        // Build a side with disk header + file count + file header +
        // file data. add_gaps walks those four blocks, then hits the
        // trailing zero bytes — which are an "unexpected tag" per the
        // block-type switch, so add_gaps falls back to raw-copy for
        // the tail. That's intentional behavior ported from Mesen2.
        let mut s = vec![0u8; SIDE_SIZE];
        s[0] = BLOCK_TAG_DISK_HEADER;
        s[1..1 + NINTENDO_HVC.len()].copy_from_slice(NINTENDO_HVC);
        s[DISK_HEADER_BLOCK_LEN] = 2;
        s[DISK_HEADER_BLOCK_LEN + 1] = 1;
        let fh_off = DISK_HEADER_BLOCK_LEN + FILE_COUNT_BLOCK_LEN;
        s[fh_off] = 3;
        s[fh_off + 13] = 5; // file size low
        s[fh_off + 14] = 0; // file size high
        let fd_off = fh_off + FILE_HEADER_BLOCK_LEN;
        s[fd_off] = 4;

        let mut bytes = synthetic_bare(1);
        bytes[..s.len()].copy_from_slice(&s);
        let image = FdsImage::from_bytes(&bytes).unwrap();
        let g = &image.gapped_sides()[0];

        // First sync must land exactly after the leading gap.
        assert_eq!(g[LEADING_GAP_BYTES], BLOCK_SYNC_BYTE);
        // Byte right after first sync is the disk-header block tag.
        assert_eq!(g[LEADING_GAP_BYTES + 1], BLOCK_TAG_DISK_HEADER);
        // Fake CRC lands right after the 56-byte disk header.
        let disk_header_crc_off = LEADING_GAP_BYTES + 1 + DISK_HEADER_BLOCK_LEN;
        assert_eq!(g[disk_header_crc_off], FAKE_CRC[0]);
        assert_eq!(g[disk_header_crc_off + 1], FAKE_CRC[1]);
        // Inter-block gap bytes follow, then next sync.
        let next_sync_off = disk_header_crc_off + 2 + BLOCK_GAP_BYTES;
        assert_eq!(g[next_sync_off], BLOCK_SYNC_BYTE);
        // Next block is file-count (type 2).
        assert_eq!(g[next_sync_off + 1], 2);
    }

    #[test]
    fn headers_extracts_56_bytes_after_tag() {
        let image = FdsImage::from_bytes(&synthetic_bare(1)).unwrap();
        let headers = image.headers();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].len(), DISK_HEADER_BLOCK_LEN);
        // Starts with NINTENDO-HVC magic.
        assert_eq!(&headers[0][..NINTENDO_HVC.len()], NINTENDO_HVC);
    }

    #[test]
    fn headers_returns_zeros_when_side_is_malformed() {
        let mut bytes = synthetic_bare(1);
        bytes[0] = 0xFF; // missing disk-header tag
        let image = FdsImage::from_bytes(&bytes).unwrap();
        let headers = image.headers();
        assert_eq!(headers[0], vec![0u8; DISK_HEADER_BLOCK_LEN]);
    }

    #[test]
    fn add_gaps_unexpected_tag_falls_back_to_raw_copy() {
        // Raw side starts with a tag the state machine doesn't know
        // (5 isn't a valid block-type). add_gaps should emit sync +
        // the remaining raw side as-is.
        let mut raw = vec![0u8; 16];
        raw[0] = 5; // bogus tag
        raw[1] = 0xAA;
        raw[2] = 0xBB;
        let gapped = add_gaps(&raw);
        // After the leading gap we expect the sync byte, then 16
        // bytes of raw side (tag + 15 bytes), with NO additional
        // gap appended since we fell out of the structured walk.
        assert_eq!(gapped[..LEADING_GAP_BYTES], vec![0u8; LEADING_GAP_BYTES]);
        assert_eq!(gapped[LEADING_GAP_BYTES], BLOCK_SYNC_BYTE);
        assert_eq!(&gapped[LEADING_GAP_BYTES + 1..LEADING_GAP_BYTES + 17], &raw[..]);
    }

    #[test]
    fn zero_side_header_trusts_file() {
        // Some homebrew has `side_count = 0` in the header even though
        // the file clearly has sides. Trust the file, no error.
        let mut bytes = synthetic_headered(1);
        bytes[4] = 0;
        let image = FdsImage::from_bytes(&bytes).unwrap();
        assert_eq!(image.sides.len(), 1);
    }
}
