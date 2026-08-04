//! Vehicle-identity decoding: the ECU-name overlay for the gateway SVT list, and
//! the FA (Fahrzeugauftrag / vehicle-order) decode.
//!
//! The SVT read gives diagnostic addresses only; the gateway's own name table is
//! coarse and stale (its 0x40 says "CAS", wrong for many cars). So names come from
//! the ISTA-derived semantic DB (`Catalog::ecus()`) — generic across BMW — and an
//! address the DB doesn't know keeps a raw-hex name rather than being dropped.

use crate::catalog::{Catalog, EcuSlot};

/// One installed ECU with a DB-resolved name, or a raw-hex fallback name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedEcu {
    /// The diagnostic address from the SVT.
    pub address: u8,
    /// The ISTA group name for this address, or `None` if the DB lacks it.
    pub name: Option<String>,
    /// A human title for the address, if the DB has one.
    pub title: Option<String>,
}

/// Overlay DB names onto SVT addresses. Order and multiplicity follow `addresses`.
///
/// With no catalog, or for an address the DB does not know, `name`/`title` are
/// `None` — the address is always kept (never dropped or guessed).
pub fn name_ecu_list(catalog: Option<&Catalog>, addresses: &[u8]) -> Vec<NamedEcu> {
    let slots: Vec<EcuSlot> = catalog.and_then(|c| c.ecus().ok()).unwrap_or_default();
    addresses
        .iter()
        .map(|&address| {
            let slot = slots.iter().find(|s| s.address == address);
            NamedEcu {
                address,
                name: slot.map(|s| s.group_name.clone()),
                title: slot.and_then(|s| s.title.clone()),
            }
        })
        .collect()
}

/// The decoded vehicle order (Fahrzeugauftrag / FA) from gateway DID 0x3F06.
///
/// Decoded per ISTA's own implementation of the same encoding — `FaDecodeHelper`
/// `DecodeVCMBackupFA` (`RheingoldDiagnostics`) and `FormatConverter`
/// `Convert6BitNibblesTo4DigitString` / `DecodeFAChar` (`RheingoldCoreFramework`) —
/// cross-checked against the gateway's own bytecode (`zgw_01.prg`
/// `STATUS_VCM_GET_FA`, whose header offsets are hardcoded immediates and whose
/// alphabet is its `TABKOMPRIMIERUNG` table) and against real captures.
///
/// Fields are `None` only when the region is too short or a character fails to
/// decode; `raw` is always kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VehicleOrder {
    /// FA format version (`STAT_VERSION`). Real cars report 3.
    pub version: Option<u16>,
    /// Series (`STAT_BAUREIHE` / `BR`), e.g. `F025`.
    pub baureihe: Option<String>,
    /// Type key (`STAT_TYP_SCHLUESSEL` / `TYPE`).
    pub typ_schluessel: Option<String>,
    /// Paint code (`STAT_LACKCODE` / `LACK`).
    pub lackcode: Option<String>,
    /// Upholstery code (`STAT_POLSTERCODE` / `POLSTER`).
    pub polstercode: Option<String>,
    /// Build date (`STAT_ZEIT_KRITERIUM` / `C_DATE`), the raw `MMyy` string —
    /// ISTA parses it with exactly that format (`FormatConverterBase`
    /// `C_DATE2DateTime`), so `0317` is March 2017.
    pub build_date: Option<String>,
    /// SA (SALAPA) option codes, 3 characters each, in wire order.
    pub options: Vec<String>,
    /// E-Worte, 4 characters each.
    pub e_worte: Vec<String>,
    /// HO-Worte, 4 characters each.
    pub ho_worte: Vec<String>,
    /// The whole data region, verbatim.
    pub raw: Vec<u8>,
}

impl VehicleOrder {
    /// ISTA's canonical `STANDARD_FA` string, built exactly as both of its own
    /// implementations build it (`RheingoldDiagnostics` :234118 and :224315):
    /// `{BR}#{C_DATE}*{TYPE}%{LACK}&{POLSTER}`, then `$SA`, `-EW`, `+HO`.
    ///
    /// `None` unless all five header fields decoded.
    #[must_use]
    pub fn standard_fa(&self) -> Option<String> {
        let (br, date, ty, lack, polster) = (
            self.baureihe.as_deref()?,
            self.build_date.as_deref()?,
            self.typ_schluessel.as_deref()?,
            self.lackcode.as_deref()?,
            self.polstercode.as_deref()?,
        );
        let mut out = format!("{br}#{date}*{ty}%{lack}&{polster}");
        for sa in &self.options {
            out.push('$');
            out.push_str(sa);
        }
        for ew in &self.e_worte {
            out.push('-');
            out.push_str(ew);
        }
        for ho in &self.ho_worte {
            out.push('+');
            out.push_str(ho);
        }
        Some(out)
    }
}

/// Payload offsets, i.e. into the region AFTER the `62 3F 06` echo.
///
/// The SGBD's immediates (`move L0,#5/#6/#9/#12/#15/#18/#21`) index the response
/// INCLUDING that 3-byte echo, so each is 3 lower here. Reading the SGBD's numbers
/// against the stripped payload is what made klartext report version 87 — it read
/// `payload[5]`, three bytes past the version.
const FA_LENGTH: usize = 0;
const FA_VERSION: usize = 2;
const FA_ZEIT_KRITERIUM: usize = 3;
const FA_BAUREIHE: usize = 6;
const FA_TYP_SCHLUESSEL: usize = 9;
const FA_LACKCODE: usize = 12;
const FA_POLSTERCODE: usize = 15;
const FA_OPTIONS: usize = 18;

/// Decode one 6-bit FA character.
///
/// `DecodeFAChar` (`RheingoldCoreFramework`) switches on the top two bits and ORs a
/// base of 0x30/0x40/0x50 onto the low nibble — which for any value in `0x10..=0x3F`
/// is exactly `value + 0x20`, plain ASCII-minus-space. A value below `0x10` has no
/// mapping: the reference logs "unknown encoding" and it doubles as the list
/// terminator, so it is `None` here rather than a guessed character.
fn decode_fa_char(value: u8) -> Option<char> {
    (0x10..=0x3F)
        .contains(&value)
        .then(|| char::from(value + 0x20))
}

/// Unpack the 4 characters packed MSB-first into the 3 bytes at `offset`.
///
/// `Convert6BitNibblesTo4DigitString` (`RheingoldCoreFramework`). `None` if the
/// region is short or any character is unmapped.
fn unpack_four(region: &[u8], offset: usize) -> Option<String> {
    let bytes: [u8; 3] = region.get(offset..offset + 3)?.try_into().ok()?;
    let sixbits = [
        bytes[0] >> 2,
        ((bytes[0] & 0x03) << 4) | (bytes[1] >> 4),
        ((bytes[1] & 0x0F) << 2) | (bytes[2] >> 6),
        bytes[2] & 0x3F,
    ];
    sixbits.iter().map(|&v| decode_fa_char(v)).collect()
}

/// An MSB-first bit reader over the FA's tagged option stream.
struct BitReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl BitReader<'_> {
    /// Reads `count` bits MSB-first, or `None` past the end.
    fn take(&mut self, count: usize) -> Option<u32> {
        let mut value = 0u32;
        for _ in 0..count {
            let byte = self.bytes.get(self.pos / 8)?;
            value = (value << 1) | u32::from((byte >> (7 - self.pos % 8)) & 1);
            self.pos += 1;
        }
        Some(value)
    }

    /// Peeks the next `count` bits without consuming them.
    fn peek(&mut self, count: usize) -> Option<u32> {
        let mark = self.pos;
        let value = self.take(count);
        self.pos = mark;
        value
    }

    /// Reads one list of `chars`-character entries until the terminator.
    ///
    /// Per `DecodeVCMBackupFA`: each entry is `chars` × 6 bits; before each, the
    /// next 6 bits are inspected and a value below `0x10` (top two bits `00`) ends
    /// the list, consuming **only 2** bits so the following 4 are the next tag.
    fn read_list(&mut self, chars: usize) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        loop {
            let Some(first) = self.peek(6) else {
                return out;
            };
            if first < 0x10 {
                self.pos += 2; // the terminator is 2 bits, not 6
                return out;
            }
            let mut word = String::with_capacity(chars);
            for _ in 0..chars {
                match self.take(6).and_then(|v| decode_fa_char(v as u8)) {
                    Some(c) => word.push(c),
                    None => return out,
                }
            }
            if !out.contains(&word) {
                out.push(word); // AddIfNotContains
            }
        }
    }
}

/// Decode the FA (vehicle order) data region — the payload after the `62 3F 06` echo.
///
/// Header fields are 6-bit packed 4-character strings; the tail is a bit stream of
/// tagged lists (`1000` SA / 3 chars, `0100` E-Worte / 4 chars, `1100` HO-Worte /
/// 4 chars). Two asymmetries are copied deliberately from `DecodeVCMBackupFA`: the
/// FIRST tag is consumed only if it is the SA tag, while the second and third are
/// consumed unconditionally.
///
/// Everything degrades: a short or malformed region yields `None`/empty fields with
/// `raw` intact, never a panic and never a guessed value.
#[must_use]
pub fn decode_vehicle_order(region: &[u8]) -> VehicleOrder {
    // `STAT_FA_LAENGE` counts from the version byte, so it bounds the option
    // stream and keeps the trailing signature block out of it.
    let declared = region
        .get(FA_LENGTH..FA_LENGTH + 2)
        .map(|b| usize::from(u16::from_be_bytes([b[0], b[1]])));
    let end = declared
        .map_or(region.len(), |len| FA_VERSION + len)
        .min(region.len());
    let stream = region.get(FA_OPTIONS..end).unwrap_or(&[]);

    let mut reader = BitReader {
        bytes: stream,
        pos: 0,
    };
    let mut options = Vec::new();
    let mut e_worte = Vec::new();
    let mut ho_worte = Vec::new();
    // The SA tag is PEEKED — only consumed on a match.
    if reader.peek(4) == Some(0b1000) {
        reader.pos += 4;
        options = reader.read_list(3);
    }
    // The other two tags are consumed whether or not they match (the reference
    // slices those 4 bits off unconditionally).
    if reader.take(4) == Some(0b0100) {
        e_worte = reader.read_list(4);
    }
    if reader.take(4) == Some(0b1100) {
        ho_worte = reader.read_list(4);
    }

    VehicleOrder {
        version: region.get(FA_VERSION).map(|&b| u16::from(b)),
        baureihe: unpack_four(region, FA_BAUREIHE),
        typ_schluessel: unpack_four(region, FA_TYP_SCHLUESSEL),
        lackcode: unpack_four(region, FA_LACKCODE),
        polstercode: unpack_four(region, FA_POLSTERCODE),
        build_date: unpack_four(region, FA_ZEIT_KRITERIUM),
        options,
        e_worte,
        ho_worte,
        raw: region.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Packs `text` as FA 6-bit characters (value = ascii - 0x20) into a bit vec.
    fn pack_chars(bits: &mut Vec<u8>, text: &str) {
        for ch in text.chars() {
            let value = (ch as u8) - 0x20;
            for shift in (0..6).rev() {
                bits.push((value >> shift) & 1);
            }
        }
    }

    fn pack_tag(bits: &mut Vec<u8>, tag: u8) {
        for shift in (0..4).rev() {
            bits.push((tag >> shift) & 1);
        }
    }

    /// Builds a whole FA region the way a gateway would, so the decoder is tested
    /// against the ENCODING rather than against itself.
    fn build_fa(
        header: [&str; 5], // date, br, type, lack, polster
        sa: &[&str],
        ew: &[&str],
        ho: &[&str],
    ) -> Vec<u8> {
        let mut bits: Vec<u8> = Vec::new();
        pack_tag(&mut bits, 0b1000);
        for code in sa {
            pack_chars(&mut bits, code);
        }
        bits.extend_from_slice(&[0, 0]); // 2-bit list terminator
        pack_tag(&mut bits, 0b0100);
        for word in ew {
            pack_chars(&mut bits, word);
        }
        bits.extend_from_slice(&[0, 0]);
        pack_tag(&mut bits, 0b1100);
        for word in ho {
            pack_chars(&mut bits, word);
        }
        bits.extend_from_slice(&[0, 0]);
        while !bits.len().is_multiple_of(8) {
            bits.push(0);
        }
        let stream: Vec<u8> = bits
            .chunks(8)
            .map(|c| c.iter().fold(0u8, |acc, &b| (acc << 1) | b))
            .collect();

        let mut head: Vec<u8> = vec![3]; // version
        for field in header {
            let mut fb: Vec<u8> = Vec::new();
            pack_chars(&mut fb, field);
            head.extend(
                fb.chunks(8)
                    .map(|c| c.iter().fold(0u8, |acc, &b| (acc << 1) | b)),
            );
        }
        let body_len = head.len() + stream.len();
        let mut region = (u16::try_from(body_len).unwrap()).to_be_bytes().to_vec();
        region.extend(head);
        region.extend(stream);
        region.extend_from_slice(&[0x00, 0x04, 0xDE, 0xAD, 0xBE, 0xEF]); // signature block
        region
    }

    /// The whole FA decode, against an independently-built encoding.
    #[test]
    fn vehicle_order_decodes_header_fields_and_all_three_option_lists() {
        let region = build_fa(
            ["0317", "F025", "WZ51", "0300", "LUSW"],
            &["1CA", "2TE", "9AA"],
            &["A090"],
            &["HO01"],
        );
        let fa = decode_vehicle_order(&region);

        assert_eq!(
            fa.version,
            Some(3),
            "version is at payload[2], not payload[5]"
        );
        assert_eq!(fa.build_date.as_deref(), Some("0317")); // MMyy
        assert_eq!(fa.baureihe.as_deref(), Some("F025"));
        assert_eq!(fa.typ_schluessel.as_deref(), Some("WZ51"));
        assert_eq!(fa.lackcode.as_deref(), Some("0300"));
        assert_eq!(fa.polstercode.as_deref(), Some("LUSW"));
        assert_eq!(fa.options, vec!["1CA", "2TE", "9AA"]);
        assert_eq!(fa.e_worte, vec!["A090"]);
        assert_eq!(fa.ho_worte, vec!["HO01"]);
        assert_eq!(fa.raw, region, "the region is always kept verbatim");

        // ISTA's canonical STANDARD_FA string.
        assert_eq!(
            fa.standard_fa().as_deref(),
            Some("F025#0317*WZ51%0300&LUSW$1CA$2TE$9AA-A090+HO01")
        );
    }

    /// The declared length bounds the option stream, so the trailing signature
    /// block is never walked as if it were more option bits.
    #[test]
    fn the_declared_length_keeps_the_signature_out_of_the_option_stream() {
        let region = build_fa(["0317", "F025", "WZ51", "0300", "LUSW"], &["1CA"], &[], &[]);
        let fa = decode_vehicle_order(&region);
        assert_eq!(
            fa.options,
            vec!["1CA"],
            "exactly the one SA, not signature noise"
        );
        assert!(fa.e_worte.is_empty() && fa.ho_worte.is_empty());
    }

    /// A short or truncated region degrades to None/empty with `raw` intact.
    #[test]
    fn a_short_vehicle_order_region_degrades_without_panicking() {
        for len in 0..18usize {
            let fa = decode_vehicle_order(&vec![0xAA; len]);
            assert_eq!(fa.raw.len(), len);
            assert!(fa.options.is_empty());
            if len <= FA_VERSION {
                assert_eq!(fa.version, None);
            }
        }
    }

    /// The alphabet is ISTA's: 0x10..=0x3F maps to `value + 0x20`, and anything
    /// below 0x10 has no mapping (it is the list terminator, not a character).
    #[test]
    fn fa_characters_below_the_alphabet_have_no_mapping() {
        assert_eq!(decode_fa_char(0x10), Some('0'));
        assert_eq!(decode_fa_char(0x21), Some('A'));
        assert_eq!(decode_fa_char(0x3F), Some('_'));
        for value in 0..0x10 {
            assert_eq!(decode_fa_char(value), None, "0x{value:02X} must not map");
        }
    }

    #[test]
    fn without_catalog_addresses_keep_raw_names() {
        let ecus = name_ecu_list(None, &[0x10, 0x12]);
        assert_eq!(
            ecus,
            vec![
                NamedEcu {
                    address: 0x10,
                    name: None,
                    title: None
                },
                NamedEcu {
                    address: 0x12,
                    name: None,
                    title: None
                },
            ]
        );
    }

    // Cross-check against the owner's real semantic DB. Ignored by default (BYO data).
    #[test]
    #[ignore = "requires BYO data: data/klartext-semantic.db"]
    fn real_db_names_known_addresses() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../data/klartext-semantic.db");
        let catalog = Catalog::open(&path).expect("open semantic DB");
        // 0x10 is the gateway on F/G-series; the DB should name it, not guess.
        let ecus = name_ecu_list(Some(&catalog), &[0x10]);
        assert_eq!(ecus.len(), 1);
        assert_eq!(ecus[0].address, 0x10);
        assert!(ecus[0].name.is_some(), "DB should name the gateway address");
    }
}

#[cfg(test)]
mod fa_tests {
    use super::*;

    /// The version byte sits at payload[2], NOT payload[5].
    ///
    /// klartext read payload[5] and so reported 87 on a real car whose FA version
    /// is 3. The SGBD's own `move L0,#5` indexes the response INCLUDING the
    /// `62 3F 06` echo, which `read_did` has already stripped — a 3-byte offset
    /// error. This pins the corrected offset.
    #[test]
    fn the_version_byte_is_at_payload_offset_two_not_five() {
        let region = vec![0x00, 0x00, 0x02, 0x11, 0x22, 0x57, 0x33];
        let fa = decode_vehicle_order(&region);
        assert_eq!(
            fa.version,
            Some(2),
            "payload[2], not the 0x57 at payload[5]"
        );
        assert_eq!(fa.raw, region);
    }

    #[test]
    fn short_region_has_no_version_but_keeps_raw() {
        let fa = decode_vehicle_order(&[0x01, 0x02]);
        assert_eq!(fa.version, None);
        assert_eq!(fa.raw, vec![0x01, 0x02]);
    }
}
