//! Windows-1252 (CP1252) byte codec, as EDIABAS stores its text.
//!
//! SGBD strings are CP1252, not UTF-8: the German descriptions and unit symbols
//! (`°`, `µ`, umlauts) live in the `0xA0..=0xFF` Latin-1 range, with a handful of
//! typographic characters in `0x80..=0x9F`. [`decode`] turns those bytes into a
//! `String` and [`encode`] turns a `String` back into one byte per char — the
//! exact inverse over the CP1252 repertoire, so text survives a round trip
//! through EDIABAS's byte-oriented string registers. This codec is
//! dependency-free; the `0x80..=0x9F` block uses the standard CP1252 mapping and
//! every other byte maps to its Latin-1 (identity) code point.

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
pub fn decode(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| decode_byte(b)).collect()
}

/// Encode a `&str` into CP1252 bytes — EDIABAS's on-disk text encoding.
///
/// The inverse of [`decode`] over the CP1252 repertoire: ASCII and Latin-1
/// (`0xA0..=0xFF`) code points map to their identity byte, and the `0x80..=0x9F`
/// block reverses [`decode`]'s table. A code point CP1252 cannot represent maps
/// to `?` — the `Encoding.GetEncoding(1252)` default fallback — but text that
/// came through [`decode`] (every SGBD string) always round-trips exactly.
///
/// # Examples
///
/// ```
/// use klartext_sgbd::cp1252::encode;
/// assert_eq!(encode("Öl"), vec![0xD6, 0x6C]); // one byte per char, not UTF-8
/// assert_eq!(encode("€"), vec![0x80]); // the 0x80..=0x9F block round-trips
/// ```
pub fn encode(text: &str) -> Vec<u8> {
    text.chars().map(encode_char).collect()
}

/// Map a single CP1252 byte to its Unicode `char`.
fn decode_byte(b: u8) -> char {
    match b {
        0x80..=0x9F => HIGH_CONTROL_MAP[usize::from(b - 0x80)],
        // ASCII (0x00..=0x7F) and Latin-1 (0xA0..=0xFF) are identity code points.
        other => char::from(other),
    }
}

/// Map a single `char` to its CP1252 byte, the inverse of [`decode_byte`].
///
/// `0x3F` (`?`) is the `Encoding.GetEncoding(1252)` replacement byte for a code
/// point the encoding cannot represent; [`encode`] documents why SGBD text never
/// reaches it.
fn encode_char(c: char) -> u8 {
    match c {
        // ASCII (0x00..=0x7F) and Latin-1 (0xA0..=0xFF) are identity code points.
        '\u{00}'..='\u{7F}' | '\u{A0}'..='\u{FF}' => c as u8,
        // The 0x80..=0x9F block: find `c` in the decode table and re-add 0x80.
        _ => HIGH_CONTROL_MAP
            .iter()
            .position(|&mapped| mapped == c)
            .map_or(b'?', |i| 0x80 + i as u8),
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

    #[test]
    fn encode_is_one_byte_per_char_not_utf8() {
        // The Task 10 bug: `Ö` must be the single CP1252 byte 0xD6, not the two
        // UTF-8 bytes 0xC3 0x96 that `str::bytes` (UTF-8) would emit.
        assert_eq!(encode("Ö"), vec![0xD6]);
        assert_eq!(encode("Öl"), vec![0xD6, b'l']);
        assert_eq!(encode("degC"), b"degC");
    }

    #[test]
    fn encode_reverses_the_cp1252_high_block() {
        assert_eq!(encode("€"), vec![0x80]); // 0x80, not Latin-1 U+0080
        assert_eq!(encode("\u{2019}"), vec![0x92]); // right single quote
        assert_eq!(encode("\u{0081}"), vec![0x81]); // an identity C1 position
    }

    #[test]
    fn encode_round_trips_every_byte_through_decode() {
        // encode(decode(b)) == b for all 256 bytes: the two are exact inverses,
        // so a String that came from `decode` (every SGBD cell) survives a
        // write-to-register (`encode`) and read-back (`decode`) losslessly.
        for b in 0u8..=0xFF {
            let text = decode(&[b]);
            assert_eq!(encode(&text), vec![b], "byte {b:#04X} did not round-trip");
        }
    }

    #[test]
    fn encode_replaces_an_unrepresentable_char_with_question_mark() {
        // A code point outside the CP1252 repertoire falls back to '?' (0x3F),
        // matching .NET's `Encoding.GetEncoding(1252)`; SGBD text never hits it.
        assert_eq!(encode("\u{1F600}"), vec![b'?']);
    }
}
