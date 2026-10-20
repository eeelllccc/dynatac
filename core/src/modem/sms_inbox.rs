//! Received messages as the user sees them: parts joined into texts, each
//! with a "seen" flag kept on the device (listing on the modem marks
//! everything read, so the modem's flag means nothing).
//!
//! [`Inbox`] is pure and holds no storage: the owner saves each PDU from
//! `SmsStore` (and each message's seen flag) and rebuilds the inbox at boot
//! by adding the saved PDUs again, in the order they were received.
//!
//! Joining (TS 23.040 §9.2.3.24.1): parts of one text share the sender, the
//! reference and the part count. Parts arrive in any order (captured: 1, 3,
//! 2), possibly across restarts. A part joins the oldest *incomplete* text
//! with the same sender, reference and count that lacks its number; a
//! complete text never takes more parts, so a reused reference starts a
//! new text.
//!
//! Invariants (callee):
//!   - Adding the same PDU twice changes nothing ([`Added::Duplicate`]), so
//!     a message offered again after a restart is harmless.
//!   - Every decodable SMS-DELIVER is in exactly one [`Message`].
//!   - [`Message::text`] has the parts in order; missing parts show as `…`.

use super::sms_pdu::{self, Body, Pdu, Timestamp};

/// Position in [`Inbox::messages`] (stable: messages are never removed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MessageId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Added {
    /// A new text.
    New(MessageId),
    /// A further part of this text.
    Joined(MessageId),
    /// Already in the inbox.
    Duplicate,
    /// Not a received text (a status report, or not decodable): nothing to
    /// show. The owner may still save it, to free the SIM.
    Ignored,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Part {
    pdu: String,
    seq: u8,
    body: Body,
    sent_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    from: String,
    /// `(reference, total)` for a multipart text.
    concat: Option<(u16, u8)>,
    parts: Vec<Part>,
    seen: bool,
}

/// A text, for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub id: MessageId,
    pub from: String,
    /// When the first part present was sent (service centre time).
    pub sent_at: Timestamp,
    pub text: String,
    /// All parts present.
    pub complete: bool,
    pub seen: bool,
}

#[derive(Debug, Default)]
pub struct Inbox {
    entries: Vec<Entry>,
}

const MISSING: &str = "…";
const DATA: &str = "[data message]";

impl Inbox {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, pdu: &str) -> Added {
        let Ok(Pdu::Deliver(d)) = sms_pdu::decode(pdu) else { return Added::Ignored };
        if self.entries.iter().any(|e| e.parts.iter().any(|p| p.pdu == pdu)) {
            return Added::Duplicate;
        }
        let concat = d.concat.filter(|c| c.total > 1);
        let part = Part { pdu: pdu.to_string(), seq: concat.map_or(1, |c| c.seq), body: d.body, sent_at: d.sent_at };
        let key = concat.map(|c| (c.reference, c.total));
        if key.is_some() {
            let open = self.entries.iter().position(|e| {
                e.from == d.from && e.concat == key && !is_complete(e) && e.parts.iter().all(|p| p.seq != part.seq)
            });
            if let Some(i) = open {
                let e = &mut self.entries[i];
                e.parts.push(part);
                e.parts.sort_by_key(|p| p.seq);
                return Added::Joined(MessageId(i));
            }
        }
        self.entries.push(Entry { from: d.from, concat: key, parts: vec![part], seen: false });
        Added::New(MessageId(self.entries.len() - 1))
    }

    /// Every text, oldest first.
    pub fn messages(&self) -> Vec<Message> {
        (0..self.entries.len()).map(|i| self.message(MessageId(i)).expect("in range")).collect()
    }

    pub fn message(&self, id: MessageId) -> Option<Message> {
        let e = self.entries.get(id.0)?;
        let total = e.concat.map_or(1, |(_, t)| t);
        let text = (1..=total)
            .map(|seq| match e.parts.iter().find(|p| p.seq == seq) {
                Some(Part { body: Body::Text(t), .. }) => t.as_str(),
                Some(Part { body: Body::Data(_), .. }) => DATA,
                None => MISSING,
            })
            .collect();
        Some(Message {
            id,
            from: e.from.clone(),
            sent_at: e.parts[0].sent_at,
            text,
            complete: is_complete(e),
            seen: e.seen,
        })
    }

    /// The PDUs a text is made of (to persist its seen flag alongside).
    pub fn pdus(&self, id: MessageId) -> Vec<&str> {
        self.entries.get(id.0).map_or(vec![], |e| e.parts.iter().map(|p| p.pdu.as_str()).collect())
    }

    pub fn mark_seen(&mut self, id: MessageId) {
        if let Some(e) = self.entries.get_mut(id.0) {
            e.seen = true;
        }
    }

    pub fn unseen(&self) -> usize {
        self.entries.iter().filter(|e| !e.seen).count()
    }
}

fn is_complete(e: &Entry) -> bool {
    e.parts.len() == e.concat.map_or(1, |(_, t)| t as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SMS-DELIVER from `from_bcd` (a 12-digit international number as
    /// swapped BCD), sent at second `sec`, with user data `ud` (hex, UDL
    /// first) and first octet `fo`.
    fn deliver(from_bcd: &str, sec: u32, fo: &str, dcs: &str, ud: &str) -> String {
        format!("00{fo}0C91{from_bcd}00{dcs}6201020000{}{}40{ud}", sec % 10, sec / 10)
    }

    const ALICE: &str = "447700091032"; // +447700900123
    const BOB: &str = "447700091042"; // +447700900124

    /// One 7-bit part "x" of a concatenated text (UDH 05 00 03 ref total
    /// seq; one fill bit, so 'x' = 0x78 << 1 = 0xF0).
    fn part(from: &str, sec: u32, reference: u8, total: u8, seq: u8) -> String {
        deliver(from, sec, "44", "00", &format!("08050003{reference:02X}{total:02X}{seq:02X}F0"))
    }

    /// A UCS2 part with text `t` (BMP only).
    fn ucs2_part(sec: u32, reference: u8, total: u8, seq: u8, t: &str) -> String {
        let units: String = t.encode_utf16().map(|u| format!("{u:04X}")).collect();
        let ud = format!("050003{reference:02X}{total:02X}{seq:02X}{units}");
        deliver(ALICE, sec, "44", "08", &format!("{:02X}{ud}", ud.len() / 2))
    }

    /// Single-part GSM text "hellohello" (the textbook packing).
    fn hello(from: &str, sec: u32) -> String {
        deliver(from, sec, "04", "00", "0AE8329BFD4697D9EC37")
    }

    #[test]
    fn single_part_text() {
        let mut inbox = Inbox::new();
        assert_eq!(inbox.add(&hello(ALICE, 1)), Added::New(MessageId(0)));
        let m = inbox.message(MessageId(0)).unwrap();
        assert_eq!((m.from.as_str(), m.text.as_str(), m.complete, m.seen), ("+447700900123", "hellohello", true, false));
        assert_eq!(m.sent_at.second, 1);
    }

    /// As captured in 14: parts stored 1, 3, 2.
    #[test]
    fn parts_out_of_order_are_joined_in_order() {
        let mut inbox = Inbox::new();
        assert_eq!(inbox.add(&ucs2_part(1, 0x50, 3, 1, "The ")), Added::New(MessageId(0)));
        assert!(!inbox.message(MessageId(0)).unwrap().complete);
        assert_eq!(inbox.message(MessageId(0)).unwrap().text, "The ……");
        assert_eq!(inbox.add(&ucs2_part(2, 0x50, 3, 3, "price ✅")), Added::Joined(MessageId(0)));
        assert_eq!(inbox.message(MessageId(0)).unwrap().text, "The …price ✅");
        assert_eq!(inbox.add(&ucs2_part(1, 0x50, 3, 2, "right ")), Added::Joined(MessageId(0)));
        let m = inbox.message(MessageId(0)).unwrap();
        assert_eq!((m.text.as_str(), m.complete), ("The right price ✅", true));
        assert_eq!(inbox.pdus(MessageId(0)).len(), 3);
    }

    #[test]
    fn same_reference_from_different_senders_stays_apart() {
        let mut inbox = Inbox::new();
        inbox.add(&part(ALICE, 1, 7, 2, 1));
        assert_eq!(inbox.add(&part(BOB, 2, 7, 2, 2)), Added::New(MessageId(1)));
        assert_eq!(inbox.add(&part(ALICE, 3, 7, 2, 2)), Added::Joined(MessageId(0)));
        assert_eq!(inbox.add(&part(BOB, 4, 7, 2, 1)), Added::Joined(MessageId(1)));
        assert!(inbox.messages().iter().all(|m| m.complete && m.text == "xx"));
    }

    /// A reference used again after its text was complete starts a new text.
    #[test]
    fn reused_reference_starts_a_new_text() {
        let mut inbox = Inbox::new();
        inbox.add(&part(ALICE, 1, 9, 2, 1));
        inbox.add(&part(ALICE, 2, 9, 2, 2));
        assert_eq!(inbox.add(&part(ALICE, 30, 9, 2, 2)), Added::New(MessageId(1)));
        assert_eq!(inbox.add(&part(ALICE, 31, 9, 2, 1)), Added::Joined(MessageId(1)));
    }

    /// A second copy of a part number the open text already has (a
    /// different PDU) can't join it.
    #[test]
    fn a_part_number_already_present_starts_a_new_text() {
        let mut inbox = Inbox::new();
        inbox.add(&part(ALICE, 1, 9, 3, 1));
        assert_eq!(inbox.add(&part(ALICE, 2, 9, 3, 1)), Added::New(MessageId(1)));
        assert_eq!(inbox.add(&part(ALICE, 3, 9, 3, 2)), Added::Joined(MessageId(0)), "the oldest open text");
    }

    #[test]
    fn duplicates_change_nothing() {
        let mut inbox = Inbox::new();
        inbox.add(&hello(ALICE, 1));
        inbox.add(&part(ALICE, 2, 1, 2, 1));
        assert_eq!(inbox.add(&hello(ALICE, 1)), Added::Duplicate);
        assert_eq!(inbox.add(&part(ALICE, 2, 1, 2, 1)), Added::Duplicate);
        assert_eq!(inbox.messages().len(), 2);
    }

    /// "One part of one" is an ordinary text.
    #[test]
    fn single_part_concatenation_header() {
        let mut inbox = Inbox::new();
        inbox.add(&part(ALICE, 1, 3, 1, 1));
        assert_eq!(inbox.add(&part(ALICE, 2, 3, 1, 1)), Added::New(MessageId(1)));
        assert!(inbox.messages().iter().all(|m| m.complete && m.text == "x"));
    }

    #[test]
    fn data_and_non_texts() {
        let mut inbox = Inbox::new();
        assert_eq!(inbox.add(&deliver(ALICE, 1, "04", "04", "02CAFE")), Added::New(MessageId(0)));
        assert_eq!(inbox.message(MessageId(0)).unwrap().text, DATA);
        assert_eq!(inbox.add("0006"), Added::Ignored); // status report
        assert_eq!(inbox.add("zz"), Added::Ignored);
        assert_eq!(inbox.add(""), Added::Ignored);
        assert_eq!(inbox.messages().len(), 1);
    }

    #[test]
    fn seen_is_kept_per_text() {
        let mut inbox = Inbox::new();
        inbox.add(&hello(ALICE, 1));
        inbox.add(&hello(BOB, 2));
        assert_eq!(inbox.unseen(), 2);
        inbox.mark_seen(MessageId(1));
        inbox.mark_seen(MessageId(9));
        assert_eq!(inbox.unseen(), 1);
        assert!(inbox.message(MessageId(1)).unwrap().seen);
        assert!(!inbox.message(MessageId(0)).unwrap().seen);
        assert_eq!(inbox.message(MessageId(9)), None);
    }
}
