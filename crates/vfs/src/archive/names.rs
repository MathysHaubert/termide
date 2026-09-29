//! Decoding zip entry names written without the UTF-8 flag.
//!
//! The zip spec says such names are CP437, but tools on Windows write the
//! OEM code page instead — CP866 for Russian systems — and macOS writes UTF-8
//! without setting the flag. A name that is valid UTF-8 is taken as such;
//! for the rest, the whole archive is judged once, so every name in it is
//! decoded the same way.

/// Legacy encoding of the names in one archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LegacyNames {
    Cp437,
    Cp866,
}

/// CP866 0xB0–0xFF: box drawing, the rest of the Cyrillic alphabet and a
/// few symbols (0x80–0xAF and 0xE0–0xEF are computed).
const CP866_B0_DF: &str = "░▒▓│┤╡╢╖╕╣║╗╝╜╛┐└┴┬├─┼╞╟╚╔╩╦╠═╬╧╨╤╥╙╘╒╓╫╪┘┌█▄▌▐▀";
const CP866_F0_FF: &str = "ЁёЄєЇїЎў°∙·√№¤■\u{a0}";

/// Whether `byte` is a Cyrillic letter in CP866.
fn is_cp866_letter(byte: u8) -> bool {
    matches!(byte, 0x80..=0xAF | 0xE0..=0xF7)
}

/// Pick the legacy encoding for an archive from the raw names that are not
/// valid UTF-8. CP866 wins when every non-ASCII byte is a CP866 Cyrillic
/// letter and they are at least as many as the ASCII letters: Cyrillic names
/// are mostly non-ASCII, while an accented Latin name in CP437 (`café`) has
/// a few high bytes among ASCII letters.
pub(crate) fn guess<'a>(raw_names: impl IntoIterator<Item = &'a [u8]>) -> LegacyNames {
    let (mut high, mut letters, mut ascii_letters) = (0usize, 0usize, 0usize);
    for name in raw_names {
        if std::str::from_utf8(name).is_ok() {
            continue;
        }
        for &byte in name {
            if byte >= 0x80 {
                high += 1;
                letters += usize::from(is_cp866_letter(byte));
            } else if byte.is_ascii_alphabetic() {
                ascii_letters += 1;
            }
        }
    }
    if high > 0 && letters == high && high >= ascii_letters {
        LegacyNames::Cp866
    } else {
        LegacyNames::Cp437
    }
}

/// The name of an entry: UTF-8 when it is valid UTF-8, else CP866 when the
/// archive was judged so, else `cp437` (the zip crate's own decoding).
pub(crate) fn decode(raw: &[u8], cp437: &str, legacy: LegacyNames) -> String {
    if let Ok(name) = std::str::from_utf8(raw) {
        return name.to_owned();
    }
    match legacy {
        LegacyNames::Cp437 => cp437.to_owned(),
        LegacyNames::Cp866 => raw.iter().map(|&b| cp866_char(b)).collect(),
    }
}

fn cp866_char(byte: u8) -> char {
    let offset = |base: u32, from: u8| char::from_u32(base + u32::from(byte - from));
    match byte {
        0x00..=0x7F => Some(char::from(byte)),
        0x80..=0xAF => offset(0x0410, 0x80),
        0xB0..=0xDF => CP866_B0_DF.chars().nth(usize::from(byte - 0xB0)),
        0xE0..=0xEF => offset(0x0440, 0xE0),
        0xF0..=0xFF => CP866_F0_FF.chars().nth(usize::from(byte - 0xF0)),
    }
    .unwrap_or('\u{FFFD}')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "Документы" and "отчёт" in CP866.
    const DOCUMENTS: &[u8] = &[0x84, 0xAE, 0xAA, 0xE3, 0xAC, 0xA5, 0xAD, 0xE2, 0xEB];
    const REPORT_TXT: &[u8] = &[0xAE, 0xE2, 0xE7, 0xF1, 0xE2, b'.', b't', b'x', b't'];

    #[test]
    fn the_tables_cover_every_high_byte() {
        assert_eq!(CP866_B0_DF.chars().count(), 0x30);
        assert_eq!(CP866_F0_FF.chars().count(), 0x10);
        assert!((0x80..=0xFFu8).all(|b| cp866_char(b) != '\u{FFFD}'));
    }

    #[test]
    fn russian_windows_names_decode_as_cp866() {
        let legacy = guess([DOCUMENTS, REPORT_TXT]);
        assert_eq!(legacy, LegacyNames::Cp866);
        assert_eq!(decode(DOCUMENTS, "ignored", legacy), "Документы");
        assert_eq!(decode(REPORT_TXT, "ignored", legacy), "отчёт.txt");
    }

    #[test]
    fn accented_latin_names_stay_cp437() {
        // "café.txt" and "résumé.pdf" in CP437.
        let cafe: &[u8] = b"caf\x82.txt";
        let resume: &[u8] = b"r\x82sum\x82.pdf";
        assert_eq!(guess([cafe, resume]), LegacyNames::Cp437);
        assert_eq!(decode(cafe, "café.txt", LegacyNames::Cp437), "café.txt");
        // Box drawing is not a letter: not Cyrillic text.
        assert_eq!(guess([b"\xC4\xC4\xC4".as_slice()]), LegacyNames::Cp437);
    }

    #[test]
    fn valid_utf8_wins_without_the_flag() {
        let utf8 = "Документы".as_bytes();
        assert_eq!(
            guess([utf8]),
            LegacyNames::Cp437,
            "UTF-8 names are not judged"
        );
        assert_eq!(decode(utf8, "mojibake", LegacyNames::Cp866), "Документы");
    }
}
