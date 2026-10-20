//! The GSM 7-bit default alphabet (3GPP TS 23.038 §6.2.1) and septet
//! packing (TS 23.038 §6.1.2.1), as used in SMS PDUs.
//!
//! - [`encode`] / [`decode`] map text ↔ septets (values `0..=127`). The
//!   extension table is reached through the escape septet `0x1B`, so `€`
//!   is two septets: `1B 65`.
//! - [`pack`] / [`unpack`] map septets ↔ octets, least significant bit
//!   first. A message with a user-data header starts its septets after
//!   `fill` padding bits, so they begin on a septet boundary.
//!
//! Invariants (callee):
//!   - `decode(&encode(s)?) == s` for every `s` that `encode` accepts.
//!   - `unpack(&pack(x, f), f, x.len()) == x` for septets `x`.
//!   - `decode` never fails: an unknown extension code decodes as its
//!     default-table character (TS 23.038 §6.2.1.1), a trailing escape
//!     as a space.

/// The escape to the extension table.
pub const ESC: u8 = 0x1B;

/// Default alphabet, indexed by septet. `0x1B` (escape) is never looked up.
const DEFAULT: [char; 128] = [
    '@', '£', '$', '¥', 'è', 'é', 'ù', 'ì', 'ò', 'Ç', '\n', 'Ø', 'ø', '\r', 'Å', 'å', //
    'Δ', '_', 'Φ', 'Γ', 'Λ', 'Ω', 'Π', 'Ψ', 'Σ', 'Θ', 'Ξ', ' ', 'Æ', 'æ', 'ß', 'É', //
    ' ', '!', '"', '#', '¤', '%', '&', '\'', '(', ')', '*', '+', ',', '-', '.', '/', //
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', ':', ';', '<', '=', '>', '?', //
    '¡', 'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', //
    'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z', 'Ä', 'Ö', 'Ñ', 'Ü', '§', //
    '¿', 'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', //
    'p', 'q', 'r', 's', 't', 'u', 'v', 'w', 'x', 'y', 'z', 'ä', 'ö', 'ñ', 'ü', 'à', //
];

/// Extension table: (septet after `ESC`, character).
const EXTENSION: [(u8, char); 10] = [
    (0x0A, '\u{0C}'),
    (0x14, '^'),
    (0x28, '{'),
    (0x29, '}'),
    (0x2F, '\\'),
    (0x3C, '['),
    (0x3D, '~'),
    (0x3E, ']'),
    (0x40, '|'),
    (0x65, '€'),
];

/// Septets for one character, or `None` if it isn't in the alphabet.
fn septets_of(c: char) -> Option<([u8; 2], usize)> {
    if let Some(&(code, _)) = EXTENSION.iter().find(|&&(_, e)| e == c) {
        return Some(([ESC, code], 2));
    }
    // 0x1B in the default table is the escape, not a character.
    let i = DEFAULT.iter().enumerate().position(|(i, &d)| d == c && i != ESC as usize)?;
    Some(([i as u8, 0], 1))
}

/// Text → septets, or `None` if a character isn't in the GSM alphabet
/// (then the message must be sent as UCS2).
pub fn encode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len());
    for c in text.chars() {
        let (s, n) = septets_of(c)?;
        out.extend_from_slice(&s[..n]);
    }
    Some(out)
}

/// Septets one character takes (1, or 2 for the extension table), or `None`.
pub fn septet_len(c: char) -> Option<usize> {
    septets_of(c).map(|(_, n)| n)
}

/// Septets → text.
pub fn decode(septets: &[u8]) -> String {
    let mut out = String::with_capacity(septets.len());
    let mut it = septets.iter().map(|s| s & 0x7F);
    while let Some(s) = it.next() {
        if s != ESC {
            out.push(DEFAULT[s as usize]);
            continue;
        }
        match it.next() {
            Some(e) => match EXTENSION.iter().find(|&&(code, _)| code == e) {
                Some(&(_, c)) => out.push(c),
                None if e == ESC => out.push(' '),
                None => out.push(DEFAULT[e as usize]),
            },
            None => out.push(' '),
        }
    }
    out
}

/// Pack septets into octets, after `fill` zero bits (`fill < 7`).
pub fn pack(septets: &[u8], fill: usize) -> Vec<u8> {
    let bits = fill + septets.len() * 7;
    let mut out = vec![0u8; bits.div_ceil(8)];
    for (i, &s) in septets.iter().enumerate() {
        let at = fill + i * 7;
        let v = ((s & 0x7F) as u16) << (at % 8);
        out[at / 8] |= v as u8;
        if v > 0xFF {
            out[at / 8 + 1] |= (v >> 8) as u8;
        }
    }
    out
}

/// Unpack `count` septets that start `fill` bits into `octets`. Septets
/// beyond the data read as missing (the result is shorter).
pub fn unpack(octets: &[u8], fill: usize, count: usize) -> Vec<u8> {
    (0..count)
        .map_while(|i| {
            let at = fill + i * 7;
            let lo = *octets.get(at / 8)? as u16;
            let hi = octets.get(at / 8 + 1).copied().unwrap_or(0) as u16;
            // The septet may end in the octet after the last one only if
            // it fits in the bits that exist.
            if at % 8 > 1 && at / 8 + 1 >= octets.len() {
                return None;
            }
            Some((((hi << 8 | lo) >> (at % 8)) & 0x7F) as u8)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02X}")).collect()
    }

    /// The textbook example (also in the 3GPP-derived PDU guides).
    #[test]
    fn hellohello_packs_to_the_known_octets() {
        let s = encode("hellohello").unwrap();
        assert_eq!(hex(&pack(&s, 0)), "E8329BFD4697D9EC37");
        assert_eq!(decode(&unpack(&pack(&s, 0), 0, 10)), "hellohello");
    }

    #[test]
    fn eight_septets_fill_seven_octets_exactly() {
        let s = encode("12345678").unwrap();
        let p = pack(&s, 0);
        assert_eq!(p.len(), 7);
        assert_eq!(unpack(&p, 0, 8), s);
    }

    #[test]
    fn fill_bits_shift_the_septets() {
        // After a 6-octet UDH, septets start at bit 49: one fill bit.
        assert_eq!(pack(&[0x41], 1), [0x82]);
        assert_eq!(unpack(&[0x82], 1, 1), [0x41]);
        let s = encode("concatenated").unwrap();
        for fill in 0..7 {
            assert_eq!(unpack(&pack(&s, fill), fill, s.len()), s, "fill {fill}");
        }
    }

    #[test]
    fn every_character_round_trips() {
        let all: String = DEFAULT
            .iter()
            .enumerate()
            .filter(|&(i, _)| i != ESC as usize)
            .map(|(_, &c)| c)
            .chain(EXTENSION.iter().map(|&(_, c)| c))
            .collect();
        let s = encode(&all).unwrap();
        assert_eq!(s.len(), 127 + 2 * 10);
        assert_eq!(decode(&s), all);
        assert_eq!(decode(&unpack(&pack(&s, 3), 3, s.len())), all);
    }

    #[test]
    fn extension_characters_take_two_septets() {
        assert_eq!(encode("€").unwrap(), [ESC, 0x65]);
        assert_eq!(encode("[x]").unwrap(), [ESC, 0x3C, 0x78, ESC, 0x3E]);
        assert_eq!(septet_len('€'), Some(2));
        assert_eq!(septet_len('a'), Some(1));
    }

    #[test]
    fn characters_outside_the_alphabet_are_refused() {
        assert_eq!(encode("naïve"), None);
        assert_eq!(encode("hi 👋"), None);
        assert_eq!(septet_len('ï'), None);
    }

    #[test]
    fn odd_escapes_decode_leniently() {
        assert_eq!(decode(&[ESC, 0x41]), "A"); // unknown extension: default char
        assert_eq!(decode(&[0x41, ESC]), "A "); // trailing escape
        assert_eq!(decode(&[ESC, ESC]), " ");
    }

    #[test]
    fn unpack_stops_at_the_end_of_the_data() {
        assert_eq!(unpack(&[0xE8], 0, 5).len(), 1);
        assert_eq!(unpack(&[], 0, 3), Vec::<u8>::new());
    }
}
