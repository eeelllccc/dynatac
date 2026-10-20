//! PDU codec tests: published example PDUs, hand-built edge cases, and
//! round trips through `encode_submit` → `decode`.

use super::*;

fn deliver(hex: &str) -> Deliver {
    match decode(hex) {
        Ok(Pdu::Deliver(d)) => d,
        other => panic!("{other:?}"),
    }
}

fn submit(hex: &str) -> Submit {
    match decode(hex) {
        Ok(Pdu::Submit(s)) => s,
        other => panic!("{other:?}"),
    }
}

fn text(b: &Body) -> &str {
    match b {
        Body::Text(t) => t,
        Body::Data(d) => panic!("data {d:?}"),
    }
}

// --- decoding: published examples ---

/// The widely published SMS-DELIVER example ("hellohello" from
/// +27838890001 via SMSC +27381000015, 29 Mar 1999 15:16:59 UTC+2).
#[test]
fn textbook_deliver() {
    let d = deliver("07917283010010F5040BC87238880900F10000993092516195800AE8329BFD4697D9EC37");
    // Type C8: national number, so no `+`.
    assert_eq!(d.from, "27838890001");
    assert_eq!(
        d.sent_at,
        Timestamp { year: 99, month: 3, day: 29, hour: 15, minute: 16, second: 59, tz_quarters: 8 }
    );
    assert_eq!(d.body, Body::Text("hellohello".into()));
    assert_eq!(d.concat, None);
}

/// The matching published SMS-SUBMIT example (to +46708251358, VP 4 days):
/// exactly what `encode_submit` produces.
#[test]
fn textbook_submit_is_what_we_encode() {
    let hex = "0011000B916407281553F80000AA0AE8329BFD4697D9EC37";
    let s = submit(hex);
    assert_eq!((s.to.as_str(), text(&s.body)), ("+46708251358", "hellohello"));
    let out = encode_submit("+46708251358", "hellohello", 0).unwrap();
    assert_eq!(out, [SubmitPdu { hex: hex.into(), tpdu_len: hex.len() / 2 - 1 }]);
}

// --- decoding: hand-built cases ---

/// SMSC 00 (none), international sender +447700900123, UCS2 body.
#[test]
fn ucs2_deliver_with_international_sender() {
    // "Hi 👋" = 0048 0069 0020 D83D DC4B (UTF-16BE, emoji as surrogate pair).
    let d = deliver("00040C914477000910320008624070912134000A004800690020D83DDC4B");
    assert_eq!(d.from, "+447700900123");
    assert_eq!(text(&d.body), "Hi 👋");
    assert_eq!(
        d.sent_at,
        Timestamp { year: 26, month: 4, day: 7, hour: 19, minute: 12, second: 43, tz_quarters: 0 }
    );
}

#[test]
fn negative_time_zone() {
    // Zone octet 0x4A: low semi-octet A = sign bit + 2 → -(2*10 + 4) = -24 (UTC-6).
    let d = deliver("00040C9144770009103200006240709121344A0141");
    assert_eq!(d.sent_at.tz_quarters, -24);
    assert_eq!(text(&d.body), "A");
}

/// Concatenated part, 8-bit reference: UDH 05 00 03 01 02 01, then "A"
/// after one fill bit (septets start at bit 49): 0x41 << 1 = 0x82.
#[test]
fn concatenated_part_with_fill_bit() {
    let d = deliver("00440C914477000910320000624070912134000805000301020182");
    assert_eq!(text(&d.body), "A");
    assert_eq!(d.concat, Some(Concat { reference: 1, total: 2, seq: 1 }));
}

#[test]
fn concatenated_part_with_16_bit_reference() {
    // UDH 06 08 04 12 34 03 02, UCS2 "ok".
    let d = deliver("00440C914477000910320008624070912134000B06080412340302006F006B");
    assert_eq!(text(&d.body), "ok");
    assert_eq!(d.concat, Some(Concat { reference: 0x1234, total: 3, seq: 2 }));
}

#[test]
fn other_header_elements_are_skipped() {
    // UDH: 0A 02 xx xx (text formatting, ignored), then concat 00 03 07 02 02. UCS2 "x".
    let d = deliver("00440C914477000910320008624070912134000C090A02000000030702020078");
    assert_eq!(text(&d.body), "x");
    assert_eq!(d.concat, Some(Concat { reference: 7, total: 2, seq: 2 }));
}

/// Alphanumeric sender ("Bank" as GSM 7-bit: 4 septets = 4 octets = 7 semi-octets).
#[test]
fn alphanumeric_sender() {
    let packed = gsm7::pack(&gsm7::encode("Bank").unwrap(), 0);
    assert_eq!(packed.len(), 4);
    let hex = format!("000407D0{}0000624070912134000141", to_hex(&packed));
    // 7 semi-octets → 4 octets read; 7*4/7 = 4 septets.
    let d = deliver(&hex);
    assert_eq!(d.from, "Bank");
    assert_eq!(text(&d.body), "A");
}

#[test]
fn eight_bit_data_is_kept_as_bytes() {
    let d = deliver("00040C9144770009103200046240709121340003DEADBE");
    assert_eq!(d.body, Body::Data(vec![0xDE, 0xAD, 0xBE]));
}

#[test]
fn message_class_and_waiting_groups_pick_the_alphabet() {
    assert!(matches!(coding(0xF0), Coding::Gsm7)); // class 0 ("flash")
    assert!(matches!(coding(0xF4), Coding::Data));
    assert!(matches!(coding(0x18), Coding::Ucs2)); // class 0, UCS2
    assert!(matches!(coding(0xC8), Coding::Gsm7)); // voicemail waiting, discard
    assert!(matches!(coding(0xE0), Coding::Ucs2));
    assert!(matches!(coding(0x20), Coding::Data)); // compressed
    assert!(matches!(coding(0x48), Coding::Ucs2)); // auto-deletion group
}

#[test]
fn status_report_is_reported_by_type() {
    assert_eq!(decode("0006"), Ok(Pdu::Other { mti: 2 }));
}

#[test]
fn malformed_pdus_are_errors() {
    let good = "07917283010010F5040BC87238880900F10000993092516195800AE8329BFD4697D9EC37";
    assert_eq!(decode("0"), Err(PduError::NotHex));
    assert_eq!(decode("0G"), Err(PduError::NotHex));
    assert_eq!(decode(""), Err(PduError::Truncated));
    // Every truncation of a valid PDU is an error, never a panic.
    for n in (0..good.len()).step_by(2) {
        assert!(decode(&good[..n]).is_err(), "{n}");
    }
    assert_eq!(decode(&format!("{good}00")), Err(PduError::Invalid("trailing octets")));
    // UDL claiming more than 140 octets.
    assert_eq!(decode("00040C91447700091032000862407091213400FF"), Err(PduError::Invalid("user data length")));
    // Concatenation numbers out of range.
    assert_eq!(
        decode("00440C9144770009103200086240709121340006050003010203"),
        Err(PduError::Invalid("concatenation numbers"))
    );
}

/// Arbitrary junk never panics.
#[test]
fn junk_never_panics() {
    let mut x: u64 = 0x1234_5678_9ABC_DEF1;
    for _ in 0..20_000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let len = (x % 60) as usize;
        let bytes: Vec<u8> = (0..len).map(|i| (x >> (i % 8 * 8)) as u8 ^ i as u8).collect();
        let _ = decode(&to_hex(&bytes));
    }
}

#[test]
fn hex_lines() {
    assert!(is_hex_line("0011000B91"));
    assert!(!is_hex_line("RING"));
    assert!(!is_hex_line("ABC"));
    assert!(!is_hex_line(""));
    assert!(!is_hex_line("+CMGL: 1,0,,23"));
}

// --- encoding ---

fn round_trip(number: &str, msg: &str) -> Vec<Submit> {
    let pdus = encode_submit(number, msg, 42).unwrap();
    let parts: Vec<Submit> = pdus
        .iter()
        .map(|p| {
            assert_eq!(p.hex.len() / 2 - 1, p.tpdu_len, "length excludes the SMSC octet");
            submit(&p.hex)
        })
        .collect();
    let joined: String = parts.iter().map(|s| text(&s.body)).collect();
    assert_eq!(joined, msg);
    for (i, s) in parts.iter().enumerate() {
        assert_eq!(s.to, number);
        if parts.len() > 1 {
            assert_eq!(s.concat, Some(Concat { reference: 42, total: parts.len() as u8, seq: i as u8 + 1 }));
        } else {
            assert_eq!(s.concat, None);
        }
    }
    parts
}

#[test]
fn national_number_and_odd_digit_count() {
    let p = encode_submit("07700900123", "x", 0).unwrap();
    assert!(p[0].hex.starts_with("0011000B817007900021F3"), "{}", p[0].hex);
    round_trip("07700900123", "hello");
    round_trip("*100#", "hi");
}

#[test]
fn single_part_limits() {
    assert_eq!(round_trip("+447700900123", &"a".repeat(160)).len(), 1);
    assert_eq!(round_trip("+447700900123", &"a".repeat(161)).len(), 2);
    assert_eq!(round_trip("+447700900123", &"é".repeat(160)).len(), 1); // é is GSM
    assert_eq!(round_trip("+447700900123", &"ï".repeat(70)).len(), 1); // UCS2
    assert_eq!(round_trip("+447700900123", &"ï".repeat(71)).len(), 2);
    // Extension characters count double: 80 × € = 160 septets.
    assert_eq!(round_trip("+447700900123", &"€".repeat(80)).len(), 1);
    assert_eq!(round_trip("+447700900123", &"€".repeat(81)).len(), 2);
}

#[test]
fn multipart_sizes() {
    // 153 septets per part with the header.
    let parts = round_trip("+447700900123", &"a".repeat(306));
    assert_eq!(parts.len(), 2);
    assert_eq!(round_trip("+447700900123", &"a".repeat(307)).len(), 3);
    // 67 UTF-16 units per part.
    assert_eq!(round_trip("+447700900123", &"ï".repeat(134)).len(), 2);
    assert_eq!(round_trip("+447700900123", &"ï".repeat(135)).len(), 3);
}

/// An escape pair or a surrogate pair is never split across parts.
#[test]
fn pairs_are_not_split() {
    let msg = format!("{}€€€€€", "a".repeat(152)); // 162 septets
    let parts = round_trip("+447700900123", &msg);
    assert_eq!(text(&parts[0].body), "a".repeat(152)); // € wouldn't fit in the last septet
    let msg = format!("{}👋👋👋", "ï".repeat(66)); // 72 units
    let parts = round_trip("+447700900123", &msg);
    assert_eq!(text(&parts[0].body), "ï".repeat(66));
}

#[test]
fn every_part_fits_one_sms() {
    for msg in ["a".repeat(1000), "€".repeat(500), "👋".repeat(300), "x€ï👋".repeat(100)] {
        for p in encode_submit("+447700900123", &msg, 1).unwrap() {
            assert!(p.tpdu_len <= 1 + 1 + 12 + 1 + 1 + 1 + 1 + 140, "{}", p.tpdu_len);
        }
        round_trip("+447700900123", &msg);
    }
}

#[test]
fn bad_input_is_refused() {
    assert_eq!(encode_submit("", "x", 0), Err(EncodeError::BadNumber));
    assert_eq!(encode_submit("+", "x", 0), Err(EncodeError::BadNumber));
    assert_eq!(encode_submit("077009001x3", "x", 0), Err(EncodeError::BadNumber));
    assert_eq!(encode_submit("+44+77", "x", 0), Err(EncodeError::BadNumber));
    assert_eq!(encode_submit(&"1".repeat(21), "x", 0), Err(EncodeError::BadNumber));
    assert_eq!(encode_submit("123", "", 0), Err(EncodeError::Empty));
    assert_eq!(encode_submit("123", &"a".repeat(153 * 256), 0), Err(EncodeError::TooLong));
}
