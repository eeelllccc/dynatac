//! Replies of the SMS storage commands in PDU mode: message listings
//! (`+CMGL`, `+CMGR`) and storage usage (`+CPMS?`).
//!
//! Formats, as captured (`captures/README.md`, `core/tests/golden_traces.rs`):
//!   - `AT+CMGL=4` → for each message `+CMGL: <index>,<stat>,,<length>`
//!     then the PDU on its own line; then `OK`. Nothing stored → just `OK`.
//!   - `AT+CMGR=<index>` → `+CMGR: <stat>,"",<length>`, the PDU, `OK`
//!     (the empty alpha field is quoted here, not in `+CMGL`).
//!   - `AT+CPMS?` → `+CPMS: "SM",7,10,"SM",7,10,"SM",7,10`: read/delete,
//!     write/send and receive storage, each with used and total slots.
//!
//! `<length>` is the TPDU length: the PDU's octets after the SMSC address.
//!
//! Invariants (callee):
//!   - A listing parses only if it is exactly header/PDU pairs and every
//!     PDU's length matches its header; anything else (a lost or damaged
//!     line) is `None`, never a partial list.
//!   - The PDU is not decoded here (see `sms_pdu`): a listing stays usable
//!     when one message has a coding we don't understand.

use super::sms_pdu::is_hex_line;

/// Message slots on the SIM (captured: `+CPMS: "SM",7,10,…`).
pub const SIM_SLOTS: u32 = 10;

/// The longest `AT+CMGL` reply: a full SIM of the longest PDUs (12-octet
/// SMSC address field, 164-octet TPDU), each with the longest header.
pub const MAX_LIST_REPLY_BYTES: usize = {
    let header = "\r\n+CMGL: 10,1,,164\r\n".len();
    let pdu = 2 * (12 + 164) + "\r\n".len();
    SIM_SLOTS as usize * (header + pdu) + "\r\nOK\r\n".len()
};

// A full SIM's listing must fit the L0 queue in one burst with room to
// spare, or it would be lost on every attempt (found by the SMS store's
// fault soak with a 512-byte queue).
const _: () = assert!(2 * MAX_LIST_REPLY_BYTES <= super::transport::RX_QUEUE_BYTES);

/// `<stat>` (3GPP TS 27.005 §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Received, not yet listed or read. Listing it marks it read.
    Unread,
    Read,
    /// Stored outgoing message, not sent.
    Unsent,
    Sent,
}

/// One message in the modem's storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    pub index: u32,
    pub status: Status,
    /// The PDU as hex, SMSC address first (for `sms_pdu::decode`).
    pub pdu: String,
}

/// Used and total slots of one storage (`"SM"` = the SIM: 10 slots).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Usage {
    pub storage: String,
    pub used: u32,
    pub total: u32,
}

fn status(s: &str) -> Option<Status> {
    Some(match s {
        "0" => Status::Unread,
        "1" => Status::Read,
        "2" => Status::Unsent,
        "3" => Status::Sent,
        _ => return None,
    })
}

/// Whether `pdu`'s octets after its SMSC address number `tpdu_len`.
fn length_matches(pdu: &str, tpdu_len: usize) -> bool {
    let Some(sca) = pdu.get(..2).and_then(|h| usize::from_str_radix(h, 16).ok()) else { return false };
    pdu.len() / 2 == 1 + sca + tpdu_len
}

/// The header fields `(index, stat, length)` of a `+CMGL:` line.
fn list_header(line: &str) -> Option<(u32, Status, usize)> {
    let f: Vec<_> = line.strip_prefix("+CMGL: ")?.split(',').collect();
    match f[..] {
        [index, stat, "", len] => Some((index.parse().ok()?, status(stat)?, len.parse().ok()?)),
        _ => None,
    }
}

/// The header fields `(stat, length)` of a `+CMGR:` line.
fn read_header(line: &str) -> Option<(Status, usize)> {
    let f: Vec<_> = line.strip_prefix("+CMGR: ")?.split(',').collect();
    match f[..] {
        [stat, "\"\"", len] => Some((status(stat)?, len.parse().ok()?)),
        _ => None,
    }
}

/// Whether a line belongs to a `+CMGL` reply: a header or a PDU.
pub fn is_list_line(line: &str) -> bool {
    line.starts_with("+CMGL: ") || is_hex_line(line)
}

/// Whether a line belongs to a `+CMGR` reply: a header or a PDU.
pub fn is_read_line(line: &str) -> bool {
    line.starts_with("+CMGR: ") || is_hex_line(line)
}

/// The info lines of `AT+CMGL=<stat>`.
pub fn parse_list(info: &[String]) -> Option<Vec<Stored>> {
    info.chunks(2)
        .map(|pair| match pair {
            [head, pdu] => {
                let (index, status, len) = list_header(head)?;
                (is_hex_line(pdu) && length_matches(pdu, len)).then(|| Stored { index, status, pdu: pdu.clone() })
            }
            _ => None,
        })
        .collect()
}

/// The info lines of `AT+CMGR=<index>`: the message, or `None` inside
/// `Some` when the slot is empty (no info lines).
pub fn parse_read(index: u32, info: &[String]) -> Option<Option<Stored>> {
    match info {
        [] => Some(None),
        [head, pdu] => {
            let (status, len) = read_header(head)?;
            (is_hex_line(pdu) && length_matches(pdu, len)).then(|| Some(Stored { index, status, pdu: pdu.clone() }))
        }
        _ => None,
    }
}

/// The info line of `AT+CPMS?`: one [`Usage`] per storage role, in the
/// order read/delete, write/send, receive.
pub fn parse_usage(line: &str) -> Option<Vec<Usage>> {
    let f: Vec<_> = line.strip_prefix("+CPMS: ")?.split(',').collect();
    if f.len() != 9 {
        return None;
    }
    f.chunks(3)
        .map(|c| {
            let storage = c[0].strip_prefix('"')?.strip_suffix('"')?.to_string();
            Some(Usage { storage, used: c[1].parse().ok()?, total: c[2].parse().ok()? })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(ls: &[&str]) -> Vec<String> {
        ls.iter().map(|s| s.to_string()).collect()
    }

    /// From `14-sms-receive-idle` (number scrubbed).
    const PDU_1: &str = "0791449785788997040C914477000910320000620102516080400CC4BC3B4C0F8F4169372806";
    const PDU_2: &str = "0791449785788997040C914477000910320000620102516034400CC4BC3B4C0F8F4169374806";

    #[test]
    fn listing_pairs_headers_with_pdus() {
        let info = lines(&["+CMGL: 1,0,,30", PDU_1, "+CMGL: 4,1,,30", PDU_2]);
        assert_eq!(
            parse_list(&info),
            Some(vec![
                Stored { index: 1, status: Status::Unread, pdu: PDU_1.into() },
                Stored { index: 4, status: Status::Read, pdu: PDU_2.into() },
            ])
        );
        assert_eq!(parse_list(&[]), Some(vec![]));
    }

    /// A lost or damaged line spoils the whole listing, never shifts PDUs
    /// onto the wrong index.
    #[test]
    fn damaged_listings_are_rejected() {
        let bad: &[&[&str]] = &[
            &["+CMGL: 1,0,,30"],                             // PDU lost
            &[PDU_1],                                        // header lost
            &["+CMGL: 1,0,,30", "+CMGL: 2,0,,30", PDU_2],    // PDU lost mid-list
            &["+CMGL: 1,0,,30", PDU_1, PDU_2],               // header lost mid-list
            &["+CMGL: 1,0,,31", PDU_1],                      // length mismatch
            &["+CMGL: 1,0,,30", &PDU_1[..PDU_1.len() - 2]],  // PDU truncated
            &["+CMGL: 1,7,,30", PDU_1],                      // unknown status
            &["+CMGL: x,0,,30", PDU_1],
            &["+CMGL: 1,0,30", PDU_1],
            &["+CMGL: 1,0,\"\",30", PDU_1],
        ];
        for case in bad {
            assert_eq!(parse_list(&lines(case)), None, "{case:?}");
        }
    }

    #[test]
    fn read_one() {
        let info = lines(&["+CMGR: 1,\"\",30", PDU_1]);
        assert_eq!(parse_read(3, &info), Some(Some(Stored { index: 3, status: Status::Read, pdu: PDU_1.into() })));
        assert_eq!(parse_read(3, &[]), Some(None));
        assert_eq!(parse_read(3, &lines(&["+CMGR: 1,,30", PDU_1])), None);
        assert_eq!(parse_read(3, &lines(&["+CMGR: 1,\"\",29", PDU_1])), None);
        assert_eq!(parse_read(3, &lines(&["+CMGR: 1,\"\",30"])), None);
    }

    #[test]
    fn reply_lines_are_recognised() {
        assert!(is_list_line("+CMGL: 1,0,,30") && is_list_line(PDU_1));
        assert!(is_read_line("+CMGR: 1,\"\",30") && is_read_line(PDU_1));
        for urc in ["+CMTI: \"SM\",3", "RING", "+CLCC: 1,0,0,0,0,\"07700900123\",129,\"\"", "SMS DONE"] {
            assert!(!is_list_line(urc) && !is_read_line(urc), "{urc}");
        }
    }

    #[test]
    fn longest_listing() {
        assert_eq!(MAX_LIST_REPLY_BYTES, 3746);
    }

    #[test]
    fn storage_usage() {
        let u = |s: &str, used, total| Usage { storage: s.into(), used, total };
        assert_eq!(
            parse_usage("+CPMS: \"SM\",7,10,\"SM\",7,10,\"ME\",0,23"),
            Some(vec![u("SM", 7, 10), u("SM", 7, 10), u("ME", 0, 23)])
        );
        assert_eq!(parse_usage("+CPMS: \"SM\",7,10"), None);
        assert_eq!(parse_usage("+CPMS: SM,7,10,SM,7,10,SM,7,10"), None);
        assert_eq!(parse_usage("+CPMS: 7,10,7,10,7,10"), None);
    }
}
