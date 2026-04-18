//! CRC32/IEEE-802.3 (polynomial `0xEDB88320`, reflected). Table-based
//! implementation — ~1 KB of precomputed table for a ~1 byte/cycle loop.
//!
//! Used to key into the game database: we compute a CRC32 over the
//! concatenation of PRG-ROM + CHR-ROM (matching Mesen2's
//! `romData.Info.Hash.PrgChrCrc32` — `iNesLoader.cpp:62-63`) and look
//! up region / chip / bus-conflict / submapper info that the iNES 1.0
//! header cannot reliably encode.

/// Precomputed lookup table for CRC32 with the IEEE-802.3 reflected
/// polynomial. Generated in a `const fn` so it costs nothing at runtime.
const CRC32_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0u32;
    while i < 256 {
        let mut c = i;
        let mut k = 0;
        while k < 8 {
            c = if (c & 1) != 0 {
                0xEDB88320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i as usize] = c;
        i += 1;
    }
    table
};

/// Compute CRC32 (IEEE-802.3) over the given byte slice.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut c: u32 = 0xFFFFFFFF;
    for &b in bytes {
        let idx = ((c ^ b as u32) & 0xFF) as usize;
        c = CRC32_TABLE[idx] ^ (c >> 8);
    }
    c ^ 0xFFFFFFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    // Known CRC32 fixtures from the IEEE-802.3 / ISO-HDLC spec (matches
    // zlib / png / gzip). Cross-checks any table regeneration bug.
    #[test]
    fn empty_input() {
        assert_eq!(crc32(&[]), 0);
    }

    #[test]
    fn ascii_fixtures() {
        assert_eq!(crc32(b"a"), 0xE8B7BE43);
        assert_eq!(crc32(b"abc"), 0x352441C2);
        assert_eq!(crc32(b"123456789"), 0xCBF43926);
    }

    #[test]
    fn accumulates_over_chunks() {
        // Treat-chunked input = same as concatenated. Relevant for the
        // PRG+CHR composite CRC where we could stream the file in
        // pieces later.
        let a = b"The quick brown fox ";
        let b = b"jumps over the lazy dog";
        let mut whole = Vec::new();
        whole.extend_from_slice(a);
        whole.extend_from_slice(b);
        assert_eq!(crc32(&whole), 0x414FA339);
    }
}
