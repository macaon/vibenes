// SPDX-License-Identifier: GPL-3.0-or-later
//! Self-validating header for cartridge save sidecars.
//!
//! Battery-RAM (`.sav`) and PRG-flash (`.fsav`) sidecars get a
//! 48-byte fixed-layout header prepended to their payload. The
//! header carries enough fingerprint to refuse a load when the
//! save belongs to a different cart, format, or region than the
//! one currently mounted - the protection a CRC-in-the-filename
//! scheme tries (and fails) to provide, since users freely rename
//! save files but never edit them in a hex editor.
//!
//! FDS disk sidecars (`.ips`) are intentionally header-less to
//! preserve cross-emulator interop with Mesen2's `.ips` disk diffs;
//! the IPS format itself encodes "what bytes go where" against an
//! implicit base, and Mesen interop matters more than our extra
//! safety net for that channel.
//!
//! Save states have their own self-validating envelope (see
//! [`crate::save_state`]) and don't reuse this header.
//!
//! ## On-disk layout
//!
//! ```text
//! Offset  Field                Size  Notes
//! ------  -------------------  ----  -------------------------------------
//!   0     magic "VBSV"           4   ASCII; never changes across versions
//!   4     version                1   1 today; bump on incompatible change
//!   5     channel                1   0 = Battery, 1 = Flash
//!   6     region                 1   0 = NTSC,    1 = PAL
//!   7     submapper              1   NES 2.0 submapper nibble in low 4 bits
//!   8     mapper_id              2   little-endian, NES 2.0 mapper id (u16)
//!  10     reserved               2   zeroed; future v1-compat 2-byte slot
//!  12     cart_crc32             4   little-endian; PRG+CHR CRC, "which ROM"
//!  16     payload_crc32          4   little-endian; 0 = "not computed"
//!  20     payload_len            4   little-endian; bytes following header
//!  24     timestamp_unix_ms      8   little-endian; 0 = unset
//!  32     emulator_version      16   ASCII; NUL-padded
//!  48     -- end --
//! ```
//!
//! All multi-byte integers are little-endian for parity with the
//! save-state on-disk format and trivial hand-rolled encode/decode
//! on every host architecture we target.
//!
//! ## Validation policy on read
//!
//! - Magic mismatch:    treated as "no save" (caller behaves as
//!   if the file didn't exist; logs a warn).
//! - Version unsupported: same as magic mismatch. Future readers
//!   may grow a v2 path that delegates to v1 on `version == 1`.
//! - Channel mismatch:  rejected (refuse to load a battery save
//!   into the flash channel or vice-versa).
//! - Region mismatch:   warn-only; some users intentionally play
//!   carts under the "wrong" region for speedrun timing or to
//!   route a PAL-only cart through NTSC quirks. Cross-region
//!   battery RAM is byte-compatible on every cart we know of.
//! - Mapper / cart CRC mismatch: rejected. Wrong cart entirely.
//! - `payload_crc32 != 0` and CRC mismatch: rejected (torn write
//!   or bit-rot - safer to refuse than to apply garbage).
//! - `payload_len` doesn't match the actual file remainder:
//!   rejected (truncated write or surrounding-tool damage).

use crate::nes::clock::Region;

/// Magic bytes prepended to every header. ASCII "VBSV" - "vibenes
/// save".
pub const MAGIC: &[u8; 4] = b"VBSV";

/// Current header layout version. Bump on any incompatible
/// rearrangement; readers MUST reject unknown versions and let the
/// caller treat them as "no save".
pub const VERSION: u8 = 1;

/// Fixed serialized size of the header. Constant by design.
pub const HEADER_LEN: usize = 48;

/// Width of the [`SaveHeader::emulator_version`] string field.
pub const EMU_VERSION_LEN: usize = 16;

/// Channel discriminator. The on-disk byte at offset 5 is one of
/// these values; mismatched channel on read is a hard reject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Channel {
    Battery = 0,
    Flash = 1,
}

impl Channel {
    fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Battery),
            1 => Some(Self::Flash),
            _ => None,
        }
    }
}

/// Outcome of [`SaveHeader::decode_and_validate`]. `Accepted`
/// returns the verified payload slice (the bytes after the header)
/// and the parsed header for caller logging. Each `Rejected` arm
/// carries enough context to log a useful warning before the
/// caller treats the file as absent.
#[derive(Debug, Clone)]
pub enum DecodeOutcome<'a> {
    Accepted {
        header: SaveHeader,
        payload: &'a [u8],
    },
    Rejected(DecodeError),
}

/// Reasons a header load can be rejected. Used for warn-only logs;
/// production code does not branch on the variant beyond "treat as
/// no save."
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// File was shorter than the fixed-size header itself.
    Truncated { actual: usize },
    /// First four bytes weren't `"VBSV"`.
    BadMagic,
    /// Header `version` field is not a value this build understands.
    UnsupportedVersion(u8),
    /// `channel` byte was a value not in [`Channel`].
    UnknownChannel(u8),
    /// Caller asked for one channel; the file held a different one.
    WrongChannel { expected: Channel, found: Channel },
    /// Caller's live cart has a different mapper id / submapper.
    WrongMapper {
        expected: (u16, u8),
        found: (u16, u8),
    },
    /// Caller's live cart has a different PRG+CHR CRC32.
    WrongCart { expected: u32, found: u32 },
    /// Header's declared payload length didn't match the file's
    /// post-header remainder.
    LengthMismatch { declared: u32, actual: usize },
    /// Header carried a non-zero `payload_crc32` and the recomputed
    /// CRC over the payload disagreed.
    PayloadCrcMismatch { declared: u32, computed: u32 },
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated { actual } => write!(
                f,
                "save file is {actual} bytes; header alone is {HEADER_LEN}",
            ),
            Self::BadMagic => write!(f, "save file does not start with \"VBSV\" magic"),
            Self::UnsupportedVersion(v) => {
                write!(f, "save header version {v} is not supported by this build")
            }
            Self::UnknownChannel(b) => {
                write!(f, "save header carried unknown channel byte 0x{b:02X}")
            }
            Self::WrongChannel { expected, found } => write!(
                f,
                "save belongs to {found:?} channel; loader is the {expected:?} channel",
            ),
            Self::WrongMapper { expected, found } => write!(
                f,
                "save was written for mapper {}/{}; live cart is mapper {}/{}",
                found.0, found.1, expected.0, expected.1,
            ),
            Self::WrongCart { expected, found } => write!(
                f,
                "save was written for ROM CRC32 {found:08X}; live cart is {expected:08X}",
            ),
            Self::LengthMismatch { declared, actual } => write!(
                f,
                "save header declared payload_len={declared} but file holds {actual} bytes",
            ),
            Self::PayloadCrcMismatch { declared, computed } => write!(
                f,
                "save payload CRC32 mismatch: header {declared:08X} vs computed {computed:08X}",
            ),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Decoded header. Construction goes through
/// [`SaveHeader::with_payload`] (encoder side) so callers don't
/// have to remember to fill `payload_len` / `payload_crc32` /
/// `timestamp_unix_ms` correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SaveHeader {
    pub version: u8,
    pub channel: Channel,
    pub region: Region,
    pub mapper_id: u16,
    pub submapper: u8,
    pub cart_crc32: u32,
    pub payload_len: u32,
    pub payload_crc32: u32,
    pub timestamp_unix_ms: u64,
    pub emulator_version: [u8; EMU_VERSION_LEN],
}

impl SaveHeader {
    /// Build a header for `payload` using the supplied cart
    /// fingerprint and channel. Computes `payload_crc32`,
    /// stamps the current wall-clock time, and embeds the build's
    /// crate version.
    pub fn with_payload(
        channel: Channel,
        region: Region,
        mapper_id: u16,
        submapper: u8,
        cart_crc32: u32,
        payload: &[u8],
    ) -> Self {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self {
            version: VERSION,
            channel,
            region,
            mapper_id,
            submapper: submapper & 0x0F,
            cart_crc32,
            payload_len: payload.len() as u32,
            payload_crc32: crate::crc32::crc32(payload),
            timestamp_unix_ms: now_ms,
            emulator_version: emu_version_bytes(),
        }
    }

    /// Encode this header followed by `payload` as a single
    /// contiguous byte buffer ready for [`crate::save::write`].
    pub fn encode_with_payload(&self, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
        out.extend_from_slice(MAGIC);
        out.push(self.version);
        out.push(self.channel as u8);
        out.push(region_byte(self.region));
        out.push(self.submapper & 0x0F);
        out.extend_from_slice(&self.mapper_id.to_le_bytes());
        out.extend_from_slice(&[0u8, 0u8]); // reserved
        out.extend_from_slice(&self.cart_crc32.to_le_bytes());
        out.extend_from_slice(&self.payload_crc32.to_le_bytes());
        out.extend_from_slice(&self.payload_len.to_le_bytes());
        out.extend_from_slice(&self.timestamp_unix_ms.to_le_bytes());
        out.extend_from_slice(&self.emulator_version);
        debug_assert_eq!(out.len(), HEADER_LEN);
        out.extend_from_slice(payload);
        out
    }

    /// Parse a header out of `bytes` and validate it against the
    /// caller's expectations. Returns the verified payload slice
    /// on success or a typed reason on failure. `region` is
    /// validated as warn-only by the caller (this function still
    /// reports the region byte through `header.region` so the
    /// caller can compare and log).
    pub fn decode_and_validate<'a>(
        bytes: &'a [u8],
        expected_channel: Channel,
        expected_mapper_id: u16,
        expected_submapper: u8,
        expected_cart_crc32: u32,
    ) -> DecodeOutcome<'a> {
        if bytes.len() < HEADER_LEN {
            return DecodeOutcome::Rejected(DecodeError::Truncated {
                actual: bytes.len(),
            });
        }
        if &bytes[0..4] != MAGIC {
            return DecodeOutcome::Rejected(DecodeError::BadMagic);
        }
        let version = bytes[4];
        if version != VERSION {
            return DecodeOutcome::Rejected(DecodeError::UnsupportedVersion(version));
        }
        let Some(channel) = Channel::from_byte(bytes[5]) else {
            return DecodeOutcome::Rejected(DecodeError::UnknownChannel(bytes[5]));
        };
        if channel != expected_channel {
            return DecodeOutcome::Rejected(DecodeError::WrongChannel {
                expected: expected_channel,
                found: channel,
            });
        }
        let region = match bytes[6] {
            0 => Region::Ntsc,
            _ => Region::Pal, // any non-zero byte falls into PAL
        };
        let submapper = bytes[7] & 0x0F;
        let mapper_id = u16::from_le_bytes([bytes[8], bytes[9]]);
        // bytes[10..12] reserved
        let cart_crc32 = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
        let payload_crc32 =
            u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        let payload_len =
            u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
        let timestamp_unix_ms = u64::from_le_bytes([
            bytes[24], bytes[25], bytes[26], bytes[27], bytes[28], bytes[29], bytes[30],
            bytes[31],
        ]);
        let mut emulator_version = [0u8; EMU_VERSION_LEN];
        emulator_version.copy_from_slice(&bytes[32..48]);

        if mapper_id != expected_mapper_id || submapper != (expected_submapper & 0x0F) {
            return DecodeOutcome::Rejected(DecodeError::WrongMapper {
                expected: (expected_mapper_id, expected_submapper & 0x0F),
                found: (mapper_id, submapper),
            });
        }
        if cart_crc32 != expected_cart_crc32 {
            return DecodeOutcome::Rejected(DecodeError::WrongCart {
                expected: expected_cart_crc32,
                found: cart_crc32,
            });
        }

        let payload = &bytes[HEADER_LEN..];
        if payload.len() != payload_len as usize {
            return DecodeOutcome::Rejected(DecodeError::LengthMismatch {
                declared: payload_len,
                actual: payload.len(),
            });
        }
        if payload_crc32 != 0 {
            let computed = crate::crc32::crc32(payload);
            if computed != payload_crc32 {
                return DecodeOutcome::Rejected(DecodeError::PayloadCrcMismatch {
                    declared: payload_crc32,
                    computed,
                });
            }
        }

        DecodeOutcome::Accepted {
            header: SaveHeader {
                version,
                channel,
                region,
                mapper_id,
                submapper,
                cart_crc32,
                payload_len,
                payload_crc32,
                timestamp_unix_ms,
                emulator_version,
            },
            payload,
        }
    }
}

fn region_byte(r: Region) -> u8 {
    match r {
        Region::Ntsc => 0,
        Region::Pal => 1,
    }
}

/// NUL-padded ASCII rendering of the build's crate version.
/// Truncates at 16 bytes, which fits any sane semver string we'll
/// ever ship (`vibenes-99.99.99` is 16 chars).
fn emu_version_bytes() -> [u8; EMU_VERSION_LEN] {
    let mut out = [0u8; EMU_VERSION_LEN];
    let s = concat!("vibenes-", env!("CARGO_PKG_VERSION"));
    let bytes = s.as_bytes();
    let n = bytes.len().min(EMU_VERSION_LEN);
    out[..n].copy_from_slice(&bytes[..n]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn battery_payload() -> Vec<u8> {
        (0u8..32).collect()
    }

    fn write_then_decode(channel: Channel, region: Region) -> (SaveHeader, Vec<u8>) {
        let payload = battery_payload();
        let header = SaveHeader::with_payload(channel, region, 1, 5, 0xCAFEBABE, &payload);
        let encoded = header.encode_with_payload(&payload);
        let outcome = SaveHeader::decode_and_validate(
            &encoded,
            channel,
            1,
            5,
            0xCAFEBABE,
        );
        match outcome {
            DecodeOutcome::Accepted { header, payload } => {
                (header, payload.to_vec())
            }
            DecodeOutcome::Rejected(e) => panic!("unexpected reject: {e}"),
        }
    }

    #[test]
    fn header_serialized_size_is_constant() {
        let payload = battery_payload();
        let header = SaveHeader::with_payload(
            Channel::Battery,
            Region::Ntsc,
            1,
            0,
            0xDEADBEEF,
            &payload,
        );
        let encoded = header.encode_with_payload(&payload);
        assert_eq!(encoded.len(), HEADER_LEN + payload.len());
    }

    #[test]
    fn round_trip_preserves_all_fields() {
        let (header, payload) = write_then_decode(Channel::Battery, Region::Ntsc);
        assert_eq!(header.version, VERSION);
        assert_eq!(header.channel, Channel::Battery);
        assert_eq!(header.region, Region::Ntsc);
        assert_eq!(header.mapper_id, 1);
        assert_eq!(header.submapper, 5);
        assert_eq!(header.cart_crc32, 0xCAFEBABE);
        assert_eq!(header.payload_len, 32);
        assert_ne!(header.payload_crc32, 0); // computed by encoder
        assert_ne!(header.timestamp_unix_ms, 0);
        assert!(header.emulator_version.starts_with(b"vibenes-"));
        assert_eq!(payload, battery_payload());
    }

    #[test]
    fn flash_channel_round_trips() {
        let (header, payload) = write_then_decode(Channel::Flash, Region::Pal);
        assert_eq!(header.channel, Channel::Flash);
        assert_eq!(header.region, Region::Pal);
        assert_eq!(payload, battery_payload());
    }

    #[test]
    fn rejects_truncated_file() {
        let outcome = SaveHeader::decode_and_validate(
            &[0u8; 4],
            Channel::Battery,
            0,
            0,
            0,
        );
        assert!(matches!(
            outcome,
            DecodeOutcome::Rejected(DecodeError::Truncated { actual: 4 })
        ));
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = vec![0u8; HEADER_LEN];
        bytes[0..4].copy_from_slice(b"NOPE");
        let outcome = SaveHeader::decode_and_validate(
            &bytes,
            Channel::Battery,
            0,
            0,
            0,
        );
        assert!(matches!(
            outcome,
            DecodeOutcome::Rejected(DecodeError::BadMagic)
        ));
    }

    #[test]
    fn rejects_unsupported_version() {
        let payload = battery_payload();
        let mut encoded = SaveHeader::with_payload(
            Channel::Battery,
            Region::Ntsc,
            1,
            0,
            0xCAFEBABE,
            &payload,
        )
        .encode_with_payload(&payload);
        encoded[4] = 0xFF;
        let outcome = SaveHeader::decode_and_validate(
            &encoded,
            Channel::Battery,
            1,
            0,
            0xCAFEBABE,
        );
        assert!(matches!(
            outcome,
            DecodeOutcome::Rejected(DecodeError::UnsupportedVersion(0xFF))
        ));
    }

    #[test]
    fn rejects_wrong_channel() {
        let payload = battery_payload();
        let encoded = SaveHeader::with_payload(
            Channel::Battery,
            Region::Ntsc,
            1,
            0,
            0xCAFEBABE,
            &payload,
        )
        .encode_with_payload(&payload);
        let outcome = SaveHeader::decode_and_validate(
            &encoded,
            Channel::Flash,
            1,
            0,
            0xCAFEBABE,
        );
        assert!(matches!(
            outcome,
            DecodeOutcome::Rejected(DecodeError::WrongChannel { .. })
        ));
    }

    #[test]
    fn rejects_wrong_cart_crc() {
        let payload = battery_payload();
        let encoded = SaveHeader::with_payload(
            Channel::Battery,
            Region::Ntsc,
            1,
            0,
            0xCAFEBABE,
            &payload,
        )
        .encode_with_payload(&payload);
        let outcome = SaveHeader::decode_and_validate(
            &encoded,
            Channel::Battery,
            1,
            0,
            0xDEADBEEF,
        );
        assert!(matches!(
            outcome,
            DecodeOutcome::Rejected(DecodeError::WrongCart { .. })
        ));
    }

    #[test]
    fn rejects_wrong_mapper_or_submapper() {
        let payload = battery_payload();
        let encoded = SaveHeader::with_payload(
            Channel::Battery,
            Region::Ntsc,
            4,
            0,
            0xCAFEBABE,
            &payload,
        )
        .encode_with_payload(&payload);
        let outcome = SaveHeader::decode_and_validate(
            &encoded,
            Channel::Battery,
            5,
            0,
            0xCAFEBABE,
        );
        assert!(matches!(
            outcome,
            DecodeOutcome::Rejected(DecodeError::WrongMapper { .. })
        ));
        let outcome = SaveHeader::decode_and_validate(
            &encoded,
            Channel::Battery,
            4,
            1,
            0xCAFEBABE,
        );
        assert!(matches!(
            outcome,
            DecodeOutcome::Rejected(DecodeError::WrongMapper { .. })
        ));
    }

    #[test]
    fn rejects_truncated_payload() {
        let payload = battery_payload();
        let mut encoded = SaveHeader::with_payload(
            Channel::Battery,
            Region::Ntsc,
            1,
            0,
            0xCAFEBABE,
            &payload,
        )
        .encode_with_payload(&payload);
        encoded.truncate(HEADER_LEN + 8); // only 8 of 32 payload bytes
        let outcome = SaveHeader::decode_and_validate(
            &encoded,
            Channel::Battery,
            1,
            0,
            0xCAFEBABE,
        );
        assert!(matches!(
            outcome,
            DecodeOutcome::Rejected(DecodeError::LengthMismatch { .. })
        ));
    }

    #[test]
    fn rejects_corrupted_payload_when_crc_is_set() {
        let payload = battery_payload();
        let mut encoded = SaveHeader::with_payload(
            Channel::Battery,
            Region::Ntsc,
            1,
            0,
            0xCAFEBABE,
            &payload,
        )
        .encode_with_payload(&payload);
        // Flip a payload byte (after the header).
        encoded[HEADER_LEN] ^= 0xFF;
        let outcome = SaveHeader::decode_and_validate(
            &encoded,
            Channel::Battery,
            1,
            0,
            0xCAFEBABE,
        );
        assert!(matches!(
            outcome,
            DecodeOutcome::Rejected(DecodeError::PayloadCrcMismatch { .. })
        ));
    }

    #[test]
    fn zero_payload_crc_skips_validation() {
        let payload = battery_payload();
        let mut header = SaveHeader::with_payload(
            Channel::Battery,
            Region::Ntsc,
            1,
            0,
            0xCAFEBABE,
            &payload,
        );
        header.payload_crc32 = 0;
        let mut encoded = header.encode_with_payload(&payload);
        // Even with corrupted payload, zero CRC means "don't check."
        encoded[HEADER_LEN] ^= 0xFF;
        let outcome = SaveHeader::decode_and_validate(
            &encoded,
            Channel::Battery,
            1,
            0,
            0xCAFEBABE,
        );
        assert!(matches!(outcome, DecodeOutcome::Accepted { .. }));
    }

    #[test]
    fn submapper_high_nibble_is_zeroed() {
        // Caller supplies a full byte; encoder masks to low nibble
        // since NES 2.0 submappers are 4 bits.
        let payload = battery_payload();
        let header = SaveHeader::with_payload(
            Channel::Battery,
            Region::Ntsc,
            1,
            0xF7,
            0xCAFEBABE,
            &payload,
        );
        assert_eq!(header.submapper, 0x07);
    }
}
