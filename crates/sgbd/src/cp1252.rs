//! Windows-1252 (CP1252) byte decoding, as EDIABAS stores its text.
//!
//! SGBD strings are CP1252, not UTF-8: the German descriptions and unit symbols
//! (`°`, `µ`, umlauts) live in the `0xA0..=0xFF` Latin-1 range, with a handful of
//! typographic characters in `0x80..=0x9F`. This decoder is dependency-free; the
//! `0x80..=0x9F` block uses the standard CP1252 mapping and every other byte maps
//! to its Latin-1 (identity) code point.

/// CP1252 code points for the `0x80..=0x9F` block (Latin-1 differs only here).
///
/// The five positions CP1252 leaves undefined (`0x81`, `0x8D`, `0x8F`, `0x90`,
/// `0x9D`) map to their identity code point, matching the WHATWG decoder.
const HIGH_CONTROL_MAP: [char; 32] = [
    '\u{20AC}', '\u{0081}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{008D}', '\u{017D}', '\u{008F}',
    '\u{0090}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}', '\u{0153}', '\u{009D}', '\u{017E}', '\u{0178}',
];

/// Decode a CP1252 byte slice into an owned `String`.
///
/// Total and lossless: every byte yields exactly one `char`, so this never fails.
pub(crate) fn decode(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| decode_byte(b)).collect()
}

/// Map a single CP1252 byte to its Unicode `char`.
fn decode_byte(b: u8) -> char {
    match b {
        0x80..=0x9F => HIGH_CONTROL_MAP[usize::from(b - 0x80)],
        // ASCII (0x00..=0x7F) and Latin-1 (0xA0..=0xFF) are identity code points.
        other => char::from(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_is_identity() {
        assert_eq!(decode(b"STATUS_MOTORTEMPERATUR"), "STATUS_MOTORTEMPERATUR");
    }

    #[test]
    fn latin1_high_range_maps_to_same_code_point() {
        // 0xB0 = '°', 0xE4 = 'ä', 0xFC = 'ü', 0xDF = 'ß' — common in SGBD text.
        assert_eq!(decode(&[0xB0]), "°");
        assert_eq!(decode(&[0xE4]), "ä");
        assert_eq!(decode(&[0xFC]), "ü");
        assert_eq!(decode(&[0xDF]), "ß");
    }

    #[test]
    fn cp1252_specials_differ_from_latin1() {
        // 0x80 is '€' in CP1252 (a C1 control in Latin-1) — the case that proves
        // we are decoding CP1252 and not just casting bytes to chars.
        assert_eq!(decode(&[0x80]), "€");
        assert_eq!(decode(&[0x92]), "\u{2019}"); // right single quote
    }

    #[test]
    fn undefined_positions_are_identity() {
        assert_eq!(decode(&[0x81]), "\u{0081}");
        assert_eq!(decode(&[0x8D]), "\u{008D}");
    }
}
