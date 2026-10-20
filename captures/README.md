# Modem captures

Raw A7682E UART traces recorded on hardware with `device/examples/modem_capture.rs`.
They are the ground truth the modem simulator is checked against (see `todo.md`,
"Modem stack rework"). Format: `core/src/modem/trace.rs`.

## Fixtures

`./captures/make-fixtures.sh` turns these logs into `core/tests/fixtures/modem/*.trace`:
TRACE records only, capture-tool diagnostics dropped, the dialled number replaced by
`07700900123` and the IMEI zeroed. `core/tests/golden_traces.rs` asserts each finding
below against them. Add new captures to the script's list.

## Recording

```
./flash-example.sh modem_capture | tee captures/<NN>-<scenario>.log
```

Wait for the boot output to settle (~15 s), press `i` (init), then follow a script
below. Press `m` and type a marker before each step so the trace is self-describing.
One scenario per file. Have a second phone ready.

## Scenarios

| File | Steps (marker → action) |
|---|---|
| `01-boot` | Do nothing for 30 s after power-on, then `i`. Captures the boot output and the init replies (incl. firmware version). |
| `02-out-answered-remote-hangup` | `m dial` → `c` number → `m answered` when they pick up → wait 10 s → `m remote hangs up` → other phone hangs up → wait 10 s → `l`. |
| `03-out-answered-local-hangup` | `m dial` → `c` → `m answered` → wait 10 s → `m local hangup` → `h` → wait 5 s → `l`. |
| `04-out-rejected` | `m dial` → `c` → `m rejecting` → other phone rejects the call → wait 10 s → `l`. |
| `05-out-unanswered` | `m dial` → `c` → let it ring out (don't answer, ~60 s) → wait 10 s → `l`. |
| `06-out-busy-or-unreachable` | `m dial` → `c` a number that is off or busy → wait 30 s → `l`. |
| `07-in-answered` | `m incoming` → call the device → `l` while ringing → `m answer` → `a` → wait 10 s → `m remote hangs up` → hang up the other phone → wait 10 s. |
| `08-in-missed` | `m incoming` → call the device, let it ring 3–4 times → `m caller gives up` → hang up the other phone → wait 10 s → `l`. |
| `09-sms-during-call` | Start a call as in 03 → `m sms` → text the device from another phone → wait 10 s → `h`. |
| `10-soft-power-cycle` | `m off` → `o` → wait 10 s → `m on` → `n` → wait 30 s → `i`. |
| `11-hard-power-cut` | `m cut` → `x` → wait 30 s → `i`. |
| `12-command-during-urc` | Start a call as in 03 → press `l` repeatedly (5–10×) around the moment the other side answers and again when they hang up. Shows whether URCs land inside replies. |
| `13-sms-state` | Boot, wait 30 s → `i` → `m state` → `s` → `m pdu list` → `p` → `g`. Shows the SMS settings as stored (`CNMI`/`CPMS` are `AUTO_SAVE`) and whether the text from `09` was stored silently. |
| `14-sms-receive-idle` | `i` → `p` → `m clear` → `e` (empty the SIM) → `g` → `m text 1` → text the device **`dynatac in 1`** → wait 15 s → `m text 2` → text **`dynatac in 2`** → wait 15 s → `m long` → text a made-up message of 200+ characters containing an emoji → wait 30 s → `g`. |
| `15-sms-receive-in-call` | `i` → `p` → start a call as in 03 → `m sms` → text **`dynatac in call`** → wait 15 s → `m hangup` → `h` → wait 15 s → `g`. |
| `16-sms-send` | `i` → `m short` → `w` + the other phone's number → wait 30 s → `m long` → `u` + the same number → wait 30 s → `g` → `m read` → `v` → `m delete` → `r` → `g`. |

SMS keys: `s` reads the SMS settings, `p` selects PDU mode (`AT+CMGF=0`), `k` turns on
new-message indications (`AT+CNMI=2,1,0,0,0`), `g` lists every stored message (`AT+CMGL=4`),
`d` reads registration / network-mode / IMS diagnostics, `v` reads message 1 (`AT+CMGR=1`),
`r` deletes message 1 (`AT+CMGD=1`), `e` deletes every stored message (`AT+CMGD=1,4`), `w` / `u` send a short GSM-alphabet text / a two-part UCS2 text (PDUs from
`core/src/modem/sms_pdu.rs`, written at the `> ` prompt). Use only the texts given above
(they end up in the fixtures); the scrub replaces the phone number in PDUs too.

**Send every text as plain SMS**: turn RCS chat off on the sending phone for the session
(or long-press → resend as text). RCS messages never reach the modem.

## Findings so far

Firmware `A7682M7_V1.11.1` (`+CGMR: A011B15A7682M7`). All timings below are from these
traces; "t+" is relative to the triggering event.

### Boot, power and restart (`01`, `10`, `11`)
- Boot output, identical after power-on, soft power cycle and hard cut: `*ATREADY: 1`
  (t+6.0 s after the PWRKEY pulse ends) → `+CPIN: READY` (+2 s) → `SMS DONE` (RI low
  ~110 ms) → `+CGEV: EPS PDN ACT 1` → `PB DONE` (~t+18 s). **`*ATREADY: 1` is the restart
  marker**: seeing it means every `NO_SAVE` setting is gone.
- Soft power-off (3 s PWRKEY): only `+CGEV: ME DETACH`, ~0.4 s after the pulse. No
  `NORMAL POWER DOWN`. Hard cut (POWER_EN low): nothing at all. So power loss is only
  detectable by silence (commands time out), and power return by `*ATREADY`.

### Init
- `ATE0` echoes itself (`ATE0\r\r\nOK`) before echo turns off.
- **`AT+CLCC=1` returns `ERROR`** every time, yet `+CLCC:` URCs are sent on every call
  state change anyway: reporting is on by default. `configure()` must not treat this as fatal.
- `ATI` reply is split across reads mid line-ending (`…+DS\r` | `\n\r\nOK`) in every trace.

### Outgoing calls (`02`–`06`, `09`, `12`)
- `ATD<n>;` → `OK` ~0.5 s later. `+CGEV: NW ACT 8,10` (VoLTE bearer) arrives within ±0.1 s
  of that `OK`: **before it in 02/05/06 (URC inside the reply), after it in 03/04/09/12.**
- Then `+CLCC` stat 2 (dialling, +1.5–2 s) → stat 3 (alerting, +1 s).
- On connect, one burst: `VOICE CALL: BEGIN` + `+CLCC` stat 0, then `+COLP: "<n>",129`
  (unrequested).
- **Rejected / unanswered / unreachable (`04`/`05`/`06`) all ended up connected to
  voicemail** (`VOICE CALL: BEGIN`, stat 0), immediately or after ~15 s of ringing.
  `BUSY` / `NO ANSWER` never appeared. From the modem's side these are answered calls; the
  code must still handle those final codes, but there are no traces of them.

### Hang-up
- **Remote** (`02`, `03-3`, `07`, `12`): `+CGEV: NW DEACT 8,10` → `+CLCC` stat 6 (+0.1 s) →
  `VOICE CALL: END: HHMMSS` + `NO CARRIER` in one burst (+0.25 s).
- **Local, `AT+CHUP`** (`03-4`, `04`, `05`, `06`, `09`): the same sequence starts ~0.4 s after
  the command, and **`CHUP`'s own `OK` comes last, after `NO CARRIER`**. So `NO CARRIER`
  during `CHUP` is a URC, not the final code. The capture tool's `is_final` (and the old
  driver) get this wrong; the L1 per-command final-code sets fix it.
- Hardware check 2026-10-20: on a local hang-up, `+CLCC` stat 6 once came 60 ms *before*
  `+CGEV: NW DEACT`. So the bearer-down URC races the call-state URC too.
- Afterwards `AT+CLCC` returns a bare `OK`.

### Incoming calls (`07`, `08`)
- First ring: `RING` → `+CLIP: "<n>",129` → `+CLCC: 1,1,4,…` (dir 1 = incoming, stat 4).
  Later rings, every 6.0 s: `RING` + `+CLIP` only.
- `ATA` → `+CGEV: NW ACT` → `VOICE CALL: BEGIN` + `+CLCC` stat 0 → **`OK` last**, ~0.7 s
  after the command. Again URCs inside a reply.
  (Hardware check 2026-10-20: the bearer URC also came *after* `ATA`'s `OK` once, so it races
  like the `ATD` one.)
- Missed (caller gives up): one burst `+CLCC` stat 6 + `VOICE CALL: END` (**no duration**)
  + `NO CARRIER`. No `+CGEV` (no bearer was set up).
- `AT+CLCC` while ringing returns the stat 4 line, as expected.

### RI pin
- Goes low within 3 ms of `RING` (sometimes just before the bytes, sometimes just after),
  stays low 5.9 s, high 0.1 s, repeats with each `RING`. After `ATA` it finishes the
  current 5.9 s cycle; when the caller gives up it goes high immediately.
- `SMS DONE` at boot: ~110 ms low pulse. `+CLCC`, `+CGEV`, `VOICE CALL` URCs: no pulse
  (default `CFGRI`).

### SMS during a call (`09`): not captured
- No `+CMTI`, no RI pulse, nothing, even 17 s after hang-up. `AT+CNMI` is `AUTO_SAVE`, so
  its current value is unknown (manual default `2,1,0,0,0`). Next: check `AT+CNMI?`,
  `AT+CPMS?` and `AT+CMGL="ALL"` to see whether the text was stored silently, then repeat
  with `AT+CNMI=2,1` set explicitly. Scenarios `13`–`16`.

### SMS state and storage (`13`, `14`)
- Stored settings (`AUTO_SAVE`): `+CNMI: 2,1,0,0,0` (new SMS stored + `+CMTI` already on), `+CSMS: 0,1,1,1`,
  `+CSCA: "+44…",145`. `+CMGF: 1` (text) at boot: **`CMGF` is not saved**, set `AT+CMGF=0` after every boot.
- Storage: `+CPMS=?` offers `"ME"` and `"SM"`; in use `"SM"` for all three, **7 of 10 slots used**. The SIM
  holds only 10 messages: the device must keep its own copy and delete from the SIM, or it fills up.
- `+CMTI: "SM",<index>` with a ~115 ms RI low pulse just before it (`13`: two messages, 38 s after power-on).
- `AT+CMGL=4` (PDU, all): `+CMGL: <index>,<stat>,,<tpdu length>` + one hex line per message, `OK` last. Long
  lines arrive split across reads. **Listing marks unread messages read** (stat 0 → 1 between `13` and `14`),
  so the modem's read flag can't be the store's truth.
- All 7 stored PDUs (intl. and alphanumeric senders, 7-bit text, 8-bit WAP push) decode with
  `core/src/modem/sms_pdu.rs`, and every `<tpdu length>` matches.
- The texts in the first runs of `09` and `14` (and a since-dropped `14b`) never arrived because the sending
  phone sent them as **RCS**, not SMS. Not a modem problem. `14` is redone with plain SMS; `15` supersedes `09`.
  `14b`'s `d` diagnostics, for reference: `CREG`/`CEREG`/`CGREG` `0,1`, `+COPS: 0,2,"23430",7` (LTE),
  `+CNMP: 2`, `+CEMODE: 2`, `+CIREG: 1,1,15` (IMS registered), `+CGSMS: 3`.
- Messages 4 and 6 have `TP-PID 0x40` (Short Message Type 0: the receiver should acknowledge and discard
  them). The store must drop these, not show them.
- Privacy: these logs hold other people's numbers, message bodies and an MMS link. They need a PDU-aware
  scrub (sender and body replaced, lengths kept) before becoming fixtures; not in `make-fixtures.sh` yet.

### SMS receive and send (`14`–`16`, plain SMS)
Pinned in `core/tests/golden_traces.rs` (SMS section).
- `+CMTI: "SM",<index>` ~20 s after sending from the phone, idle or in a call (the call is unaffected), at the
  end of a 100–140 ms RI low pulse. A 3-part UCS2 text arrived as three `+CMTI`s within 1.2 s and was stored
  **out of order** (indices 3, 4, 5 = parts 1, 3, 2): reassemble by the concatenation header.
- `AT+CMGR=<i>`: `+CMGR: <stat>,"",<len>` (alpha quoted, unlike `CMGL`), the PDU, `OK`. Doesn't delete.
- `AT+CMGD=<i>` deletes one (others keep their indices); `AT+CMGD=1,4` deletes all; empty `CMGL` is bare `OK`.
- `AT+CMGS=<len>` → `> ` in ~25 ms → PDU + Ctrl-Z → `+CMGS: <mr>` + `OK` in 0.9–1.4 s. The message
  reference counts up per part (24, 25, 26). The sent PDUs are byte-for-byte `encode_submit`'s output.

### Concurrency (`12`)
- 30+ back-to-back `AT+CLCC`s around connect and hang-up: replies were always
  `+CLCC` line(s) + `OK`, ~20–30 ms. Call URCs landed between commands, never inside a
  `CLCC` reply in this run (but they do inside `ATD`, `ATA` and `CHUP` replies, above).

### Capture-tool note
- `03-1`/`03-2`/`03-3` stop at connect because the Sym toggle was still on after typing
  the number, so later keys were symbols the menu ignores. Not a bug in the hardware or
  driver. They're kept as repeat runs.

## What we want to learn

- What a remote hang-up emits (`VOICE CALL: END`? `NO CARRIER`? `+CLCC` stat 6? order?)
- When `VOICE CALL: BEGIN` arrives relative to the `+CLCC` state changes
- Whether URCs ever appear between the lines of a command's reply
- How the modem's boot / restart shows up on the UART (for restart detection)
- RI pin timing for URCs, SMS and incoming calls
