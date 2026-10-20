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
- Afterwards `AT+CLCC` returns a bare `OK`.

### Incoming calls (`07`, `08`)
- First ring: `RING` → `+CLIP: "<n>",129` → `+CLCC: 1,1,4,…` (dir 1 = incoming, stat 4).
  Later rings, every 6.0 s: `RING` + `+CLIP` only.
- `ATA` → `+CGEV: NW ACT` → `VOICE CALL: BEGIN` + `+CLCC` stat 0 → **`OK` last**, ~0.7 s
  after the command. Again URCs inside a reply.
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
  with `AT+CNMI=2,1` set explicitly.

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
