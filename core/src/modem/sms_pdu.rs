//! SMS PDUs (3GPP TS 23.040), as the modem gives and takes them in PDU mode
//! (`AT+CMGF=0`): hex text, starting with the SMSC address.
//!
//! - [`decode`]: a PDU from `+CMGL` / `+CMGR` → [`Pdu`]. Received messages
//!   (SMS-DELIVER) are decoded in full; stored outgoing ones (SMS-SUBMIT)
//!   too; anything else (status reports) is reported by type only.
//! - [`encode_submit`]: text → one or more SMS-SUBMIT PDUs ready for
//!   `AT+CMGS=<len>`. GSM 7-bit if every character is in the GSM alphabet,
//!   else UCS2 (UTF-16). Long texts become a concatenated message
//!   (8-bit reference) without splitting an escape pair or a surrogate pair.
//!
//! PDU mode rather than text mode: every body is one hex line, so the link
//! engine can't mistake a message for a URC or absorb a URC into one, and
//! Unicode, multipart messages and exact timestamps come for free.
//!
//! Invariants (callee):
//!   - `decode` never panics; malformed input (truncated, trailing octets,
//!     impossible lengths or concatenation numbers) is an `Err`.
//!   - Every part from `encode_submit` fits one SMS (140 octets of user
//!     data) and decodes back to its share of the text, in order.
//!
//! Not decoded as text: 8-bit data and compressed text (kept as
//! `Body::Data`). Status reports are not decoded.

use super::gsm7;

/// SMS-SUBMIT validity period: relative, 4 days (`0xAA`), the usual default.
const VALIDITY_4_DAYS: u8 = 0xAA;
/// Information element: concatenated message, 8-bit / 16-bit reference.
const IE_CONCAT_8: u8 = 0x00;
const IE_CONCAT_16: u8 = 0x08;
/// User data is at most 140 octets.
const MAX_UD: usize = 140;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pdu {
    Deliver(Deliver),
    Submit(Submit),
    /// Another TPDU type (e.g. a status report, `mti` 2): not decoded.
    Other { mti: u8 },
}

/// A received message (SMS-DELIVER).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deliver {
    /// Sender: a number (`+` if international) or an alphanumeric name.
    pub from: String,
    pub sent_at: Timestamp,
    pub body: Body,
    pub concat: Option<Concat>,
}

/// A stored outgoing message (SMS-SUBMIT).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submit {
    pub to: String,
    pub body: Body,
    pub concat: Option<Concat>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    Text(String),
    /// 8-bit data (or a compressed / unknown coding): raw octets.
    Data(Vec<u8>),
}

/// This part's place in a concatenated message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Concat {
    /// Same for every part of one message (from one sender).
    pub reference: u16,
    /// Number of parts, ≥ 1.
    pub total: u8,
    /// This part, `1..=total`.
    pub seq: u8,
}

/// Service-centre timestamp: local time plus its offset from UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timestamp {
    /// Two digits (`26` = 2026).
    pub year: u8,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    /// Offset from UTC in quarter hours (`4` = UTC+1).
    pub tz_quarters: i8,
}

/// One SMS-SUBMIT, for `AT+CMGS=<tpdu_len>` then `hex` at the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitPdu {
    /// The whole PDU, SMSC field (`00`: the SIM's default) included.
    pub hex: String,
    /// Octets after the SMSC field: the length `AT+CMGS` wants.
    pub tpdu_len: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PduError {
    /// Not an even number of hex digits.
    NotHex,
    /// Ends before a field it announces.
    Truncated,
    /// A field holds an impossible value.
    Invalid(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    /// The destination has a character that isn't a digit, `*`, `#` or a
    /// leading `+`, or is empty or longer than 20 digits.
    BadNumber,
    Empty,
    /// More than 255 parts.
    TooLong,
}

// --- decoding ----------------------------------------------------------------

/// A cursor over the PDU's octets.
struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn u8(&mut self) -> Result<u8, PduError> {
        let v = *self.b.get(self.at).ok_or(PduError::Truncated)?;
        self.at += 1;
        Ok(v)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], PduError> {
        let s = self.b.get(self.at..self.at + n).ok_or(PduError::Truncated)?;
        self.at += n;
        Ok(s)
    }

    fn rest(&self) -> &'a [u8] {
        &self.b[self.at..]
    }
}

fn from_hex(s: &str) -> Result<Vec<u8>, PduError> {
    let s = s.trim().as_bytes();
    if !s.len().is_multiple_of(2) {
        return Err(PduError::NotHex);
    }
    let nibble = |c: u8| (c as char).to_digit(16).map(|d| d as u8).ok_or(PduError::NotHex);
    s.chunks(2).map(|p| Ok(nibble(p[0])? << 4 | nibble(p[1])?)).collect()
}

fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

/// Whether a reply line is a PDU (hex digits only): how `+CMGL` / `+CMGR`
/// bodies are told apart from URCs.
pub fn is_hex_line(line: &str) -> bool {
    !line.is_empty() && line.len().is_multiple_of(2) && line.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn decode(hex: &str) -> Result<Pdu, PduError> {
    let bytes = from_hex(hex)?;
    let mut r = Reader { b: &bytes, at: 0 };
    let smsc_len = r.u8()? as usize;
    r.take(smsc_len)?;
    let first = r.u8()?;
    let udhi = first & 0x40 != 0;
    match first & 0x03 {
        0 => {
            let from = address(&mut r)?;
            let _pid = r.u8()?;
            let dcs = r.u8()?;
            let sent_at = timestamp(r.take(7)?)?;
            let (body, concat) = user_data(&mut r, dcs, udhi)?;
            Ok(Pdu::Deliver(Deliver { from, sent_at, body, concat }))
        }
        1 => {
            let _mr = r.u8()?;
            let to = address(&mut r)?;
            let _pid = r.u8()?;
            let dcs = r.u8()?;
            match (first >> 3) & 0x03 {
                0 => {}
                2 => {
                    r.u8()?;
                }
                _ => {
                    r.take(7)?;
                }
            }
            let (body, concat) = user_data(&mut r, dcs, udhi)?;
            Ok(Pdu::Submit(Submit { to, body, concat }))
        }
        mti => Ok(Pdu::Other { mti }),
    }
}

/// TP-OA / TP-DA: digit count, type of address, digits.
fn address(r: &mut Reader) -> Result<String, PduError> {
    let digits = r.u8()? as usize;
    let toa = r.u8()?;
    let octets = r.take(digits.div_ceil(2))?;
    if toa & 0x70 == 0x50 {
        // Alphanumeric: GSM 7-bit, `digits` counts semi-octets.
        return Ok(gsm7::decode(&gsm7::unpack(octets, 0, digits * 4 / 7)));
    }
    let mut out = String::with_capacity(digits + 1);
    if toa & 0x70 == 0x10 {
        out.push('+');
    }
    for i in 0..digits {
        let o = octets[i / 2];
        out.push(bcd_digit(if i % 2 == 0 { o & 0x0F } else { o >> 4 })?);
    }
    Ok(out)
}

fn bcd_digit(n: u8) -> Result<char, PduError> {
    match n {
        0..=9 => Ok((b'0' + n) as char),
        0x0A => Ok('*'),
        0x0B => Ok('#'),
        0x0C => Ok('a'),
        0x0D => Ok('b'),
        0x0E => Ok('c'),
        _ => Err(PduError::Invalid("address digit")),
    }
}

/// Swapped-nibble BCD octet (`0x21` → 12).
fn swapped(o: u8) -> Result<u8, PduError> {
    let (lo, hi) = (o & 0x0F, o >> 4);
    if lo > 9 || hi > 9 {
        return Err(PduError::Invalid("timestamp digit"));
    }
    Ok(lo * 10 + hi)
}

fn timestamp(t: &[u8]) -> Result<Timestamp, PduError> {
    // The zone's sign is bit 3 of the first (low) semi-octet.
    let tz = t[6];
    let quarters = ((tz & 0x07) * 10 + (tz >> 4)) as i8;
    if tz >> 4 > 9 {
        return Err(PduError::Invalid("timestamp zone"));
    }
    Ok(Timestamp {
        year: swapped(t[0])?,
        month: swapped(t[1])?,
        day: swapped(t[2])?,
        hour: swapped(t[3])?,
        minute: swapped(t[4])?,
        second: swapped(t[5])?,
        tz_quarters: if tz & 0x08 != 0 { -quarters } else { quarters },
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Coding {
    Gsm7,
    Data,
    Ucs2,
}

/// TS 23.038 §4: the alphabet a data coding scheme selects.
fn coding(dcs: u8) -> Coding {
    let by_bits = |b: u8| match (b >> 2) & 0x03 {
        0 => Coding::Gsm7,
        2 => Coding::Ucs2,
        _ => Coding::Data,
    };
    match dcs >> 4 {
        // General data coding / automatic deletion; bit 5 = compressed.
        0x0..=0x7 if dcs & 0x20 != 0 => Coding::Data,
        0x0..=0x7 => by_bits(dcs),
        // Message waiting indication groups.
        0xC | 0xD => Coding::Gsm7,
        0xE => Coding::Ucs2,
        // Data coding / message class.
        0xF if dcs & 0x04 != 0 => Coding::Data,
        0xF => Coding::Gsm7,
        // Reserved groups: treat as GSM 7-bit (TS 23.038).
        _ => Coding::Gsm7,
    }
}

/// TP-UDL + TP-UD → body and concatenation info.
fn user_data(r: &mut Reader, dcs: u8, udhi: bool) -> Result<(Body, Option<Concat>), PduError> {
    let udl = r.u8()? as usize;
    let coding = coding(dcs);
    let ud_octets = match coding {
        Coding::Gsm7 => (udl * 7).div_ceil(8),
        _ => udl,
    };
    if ud_octets > MAX_UD {
        return Err(PduError::Invalid("user data length"));
    }
    let ud = r.take(ud_octets)?;
    if !r.rest().is_empty() {
        return Err(PduError::Invalid("trailing octets"));
    }
    let (header_len, concat) = if udhi {
        let udhl = *ud.first().ok_or(PduError::Truncated)? as usize;
        let header = ud.get(1..1 + udhl).ok_or(PduError::Truncated)?;
        (1 + udhl, concat_of(header)?)
    } else {
        (0, None)
    };
    let body = match coding {
        Coding::Gsm7 => {
            let header_septets = (header_len * 8).div_ceil(7);
            let fill = header_septets * 7 - header_len * 8;
            let n = udl.checked_sub(header_septets).ok_or(PduError::Invalid("header longer than data"))?;
            let septets = gsm7::unpack(&ud[header_len..], fill, n);
            if septets.len() != n {
                return Err(PduError::Truncated);
            }
            Body::Text(gsm7::decode(&septets))
        }
        Coding::Ucs2 => {
            let b = ud.get(header_len..).ok_or(PduError::Truncated)?;
            let units: Vec<u16> = b.chunks(2).map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)])).collect();
            Body::Text(String::from_utf16_lossy(&units))
        }
        Coding::Data => Body::Data(ud.get(header_len..).ok_or(PduError::Truncated)?.to_vec()),
    };
    Ok((body, concat))
}

/// The concatenation element of a user-data header, if any.
fn concat_of(header: &[u8]) -> Result<Option<Concat>, PduError> {
    let mut found = None;
    let mut i = 0;
    while i < header.len() {
        let id = header[i];
        let len = *header.get(i + 1).ok_or(PduError::Truncated)? as usize;
        let data = header.get(i + 2..i + 2 + len).ok_or(PduError::Truncated)?;
        let parsed = match (id, data) {
            (IE_CONCAT_8, &[r, total, seq]) => Some((r as u16, total, seq)),
            (IE_CONCAT_16, &[hi, lo, total, seq]) => Some((u16::from_be_bytes([hi, lo]), total, seq)),
            (IE_CONCAT_8 | IE_CONCAT_16, _) => return Err(PduError::Invalid("concatenation element")),
            _ => None,
        };
        if let Some((reference, total, seq)) = parsed {
            if total == 0 || seq == 0 || seq > total {
                return Err(PduError::Invalid("concatenation numbers"));
            }
            found = Some(Concat { reference, total, seq });
        }
        i += 2 + len;
    }
    Ok(found)
}

// --- encoding ----------------------------------------------------------------

/// TP-DA for `number`: digit count, type of address, swapped BCD.
fn encode_address(number: &str) -> Result<Vec<u8>, EncodeError> {
    let (toa, digits) = match number.strip_prefix('+') {
        Some(d) => (0x91, d),
        None => (0x81, number),
    };
    if digits.is_empty() || digits.len() > 20 {
        return Err(EncodeError::BadNumber);
    }
    let nibbles = digits
        .chars()
        .map(|c| match c {
            '0'..='9' => Ok(c as u8 - b'0'),
            '*' => Ok(0x0A),
            '#' => Ok(0x0B),
            _ => Err(EncodeError::BadNumber),
        })
        .collect::<Result<Vec<u8>, _>>()?;
    let mut out = vec![digits.len() as u8, toa];
    out.extend(nibbles.chunks(2).map(|p| p[0] | p.get(1).copied().unwrap_or(0x0F) << 4));
    Ok(out)
}

/// Split `text` into the parts it needs: per part, its characters. Single
/// message limits: 160 septets / 70 UTF-16 units; with the 6-octet
/// concatenation header: 153 / 67.
fn split(text: &str, gsm: bool) -> Vec<String> {
    let cost = |c: char| if gsm { gsm7::septet_len(c).unwrap_or(1) } else { c.len_utf16() };
    let (single, part) = if gsm { (160, 153) } else { (70, 67) };
    if text.chars().map(cost).sum::<usize>() <= single {
        return vec![text.to_string()];
    }
    let mut parts = vec![String::new()];
    let mut used = 0;
    for c in text.chars() {
        if used + cost(c) > part {
            parts.push(String::new());
            used = 0;
        }
        used += cost(c);
        parts.last_mut().unwrap().push(c);
    }
    parts
}

/// SMS-SUBMIT PDUs for `text` to `number` (digits, `*`, `#`, optional
/// leading `+`). A text needing several parts is sent as one concatenated
/// message with `reference` (choose a different one per message).
pub fn encode_submit(number: &str, text: &str, reference: u8) -> Result<Vec<SubmitPdu>, EncodeError> {
    if text.is_empty() {
        return Err(EncodeError::Empty);
    }
    let da = encode_address(number)?;
    let gsm = gsm7::encode(text).is_some();
    let parts = split(text, gsm);
    if parts.len() > 255 {
        return Err(EncodeError::TooLong);
    }
    let total = parts.len() as u8;
    Ok(parts
        .iter()
        .enumerate()
        .map(|(i, part)| {
            let header = (total > 1).then(|| [5u8, IE_CONCAT_8, 3, reference, total, i as u8 + 1]);
            let header: &[u8] = header.as_ref().map_or(&[], |h| h);
            let (dcs, udl, ud) = if gsm {
                let septets = gsm7::encode(part).expect("checked above");
                let header_septets = (header.len() * 8).div_ceil(7);
                let fill = header_septets * 7 - header.len() * 8;
                let mut ud = header.to_vec();
                ud.extend(gsm7::pack(&septets, fill));
                (0x00, header_septets + septets.len(), ud)
            } else {
                let mut ud = header.to_vec();
                ud.extend(part.encode_utf16().flat_map(u16::to_be_bytes));
                (0x08, ud.len(), ud)
            };
            debug_assert!(ud.len() <= MAX_UD);
            let first = 0x01 | 0x10 | if header.is_empty() { 0 } else { 0x40 };
            let mut tpdu = vec![first, 0x00];
            tpdu.extend(&da);
            tpdu.extend([0x00, dcs, VALIDITY_4_DAYS, udl as u8]);
            tpdu.extend(ud);
            SubmitPdu { hex: format!("00{}", to_hex(&tpdu)), tpdu_len: tpdu.len() }
        })
        .collect())
}

#[cfg(test)]
mod tests;
