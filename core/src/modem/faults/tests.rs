//! The step-5 fault tests (see `todo.md`). Each names the fault class it
//! covers; all check the full oracle unless noted.

use super::*;

const MIN: Millis = 60_000;

fn run(cfg: SimConfig, faults: Faults, seed: u64, ms: Millis, calls: bool) -> Session {
    let mut s = Session::new(cfg, faults, seed);
    if let Err(e) = s.soak(ms, calls) {
        panic!("seed {seed}: {e}");
    }
    s
}

/// Baseline: no faults at all, the oracle sees only correct answers and
/// nothing is ever `Unknown`.
#[test]
fn clean_session() {
    let s = run(SimConfig::default(), Faults::none(), 1, 10 * MIN, true);
    assert_eq!(s.oracle.unknowns, 0);
    assert!(s.oracle.oks > 100, "workload too thin: {} oks", s.oracle.oks);
}

/// Every byte stream split at every boundary (one byte per read), and at
/// random points.
#[test]
fn split_at_every_byte_boundary() {
    for split in [Split::Bytes, Split::Random] {
        let s = run(SimConfig::default(), Faults { split, ..Faults::none() }, 2, 5 * MIN, true);
        assert_eq!(s.oracle.unknowns, 0, "{split:?}");
    }
}

/// URCs injected between lines of replies, including before the first and
/// after the final code.
#[test]
fn urcs_between_every_pair_of_reply_lines() {
    let s = run(SimConfig::default(), Faults { urc: 0.3, ..Faults::none() }, 3, 5 * MIN, true);
    assert_eq!(s.oracle.unknowns, 0);
    assert!(s.link.urcs_sent.iter().filter(|l| *l == "RING").count() > 50);
}

/// Output stalled past the command timeout: the late final lands in the
/// probe window or during the next command and must never be credited.
#[test]
fn replies_delayed_past_the_timeout() {
    let s = run(SimConfig::default(), Faults { stall: 0.05, ..Faults::none() }, 4, 20 * MIN, true);
    assert!(s.oracle.unknowns > 0, "no stall hit a command");
}

/// Dropped bytes with loss markers at arbitrary points.
#[test]
fn dropped_bytes_with_loss_markers() {
    for split in [Split::Whole, Split::Random] {
        let s = run(SimConfig::default(), Faults { split, drop: 0.03, ..Faults::none() }, 5, 15 * MIN, true);
        assert!(s.link.losses > 10, "{split:?}: too few losses");
    }
}

/// Junk lines, empty lines, stray CR/LF runs and stray `> ` prompts. None
/// of these damage real content, so nothing may be `Unknown`.
#[test]
fn junk_blank_lines_and_stray_prompts() {
    let f = Faults { split: Split::Random, junk: 0.1, blank: 0.2, prompt: 0.05, ..Faults::none() };
    let s = run(SimConfig::default(), f, 6, 10 * MIN, true);
    assert_eq!(s.oracle.unknowns, 0);
}

/// Hard power cuts at random moments (mid-command, mid-call, mid-config).
#[test]
fn modem_restarts_mid_command_and_mid_call() {
    let s = run(SimConfig::default(), Faults { restart: 0.01, ..Faults::none() }, 7, 30 * MIN, true);
    assert!(s.oracle.restarts >= 5, "only {} restarts", s.oracle.restarts);
    // Settings are back after the last restart.
    assert_eq!(s.link.sim.cvhu(), 0);
}

/// Bursts larger than the L0 queue while the consumer is paused.
#[test]
fn bursts_larger_than_the_buffer() {
    let f = Faults { pause: 0.1, queue: 96, ..Faults::none() };
    let s = run(SimConfig::default(), f, 8, 15 * MIN, true);
    assert!(s.link.log.iter().any(|(_, l)| l == "queue overflow"), "no overflow happened");
}

/// Every observed race, in every order: the bearer-up URC swept across
/// `ATD`'s `OK` and `ATA`'s `BEGIN`/`OK`; bearer-down before and after the
/// `+CLCC` stat 6 on hang-up. Each variant runs a call-heavy session.
#[test]
fn every_observed_race_in_every_order() {
    for (i, ms) in (400..=760).step_by(40).enumerate() {
        for bearer_down_first in [true, false] {
            let cfg = SimConfig { dial_bearer_ms: ms, answer_bearer_ms: ms, bearer_down_first, ..SimConfig::default() };
            let s = run(cfg, Faults::none(), 100 + i as u64, 5 * MIN, true);
            assert_eq!(s.oracle.unknowns, 0, "bearer at {ms} ms, down-first {bearer_down_first}");
        }
    }
}

/// Everything at once, many seeds, long sessions.
#[test]
fn randomised_fault_schedules() {
    let f = Faults {
        split: Split::Random,
        drop: 0.01,
        junk: 0.03,
        blank: 0.05,
        prompt: 0.01,
        urc: 0.05,
        stall: 0.005,
        restart: 0.002,
        pause: 0.01,
        corrupt: 0.0,
        queue: 512,
    };
    for seed in 1000..1100 {
        run(SimConfig::default(), f, seed, 20 * MIN, true);
    }
}

/// What broke the old quiet-window resync: losses force resyncs while
/// reader pauses and modem stalls delay the probe replies arbitrarily.
/// With the probe's read-back, no delay can shift a reply.
#[test]
fn resync_survives_delayed_probe_replies() {
    let f = Faults { split: Split::Random, drop: 0.04, pause: 0.2, stall: 0.02, ..Faults::none() };
    for seed in 3000..3030 {
        let s = run(SimConfig::default(), f, seed, 10 * MIN, true);
        assert!(s.oracle.configured >= 2, "seed {seed}: no resync happened");
    }
}

/// Undetectable damage (silent drops, bit flips): truth can't be checked,
/// but the contract and liveness must hold — no panic, one response per
/// request, and service resumes once the damage stops.
#[test]
fn undetectable_corruption_keeps_contract_and_liveness() {
    let f = Faults { split: Split::Random, corrupt: 0.02, ..Faults::none() };
    for seed in 2000..2020 {
        let mut s = Session::new(SimConfig::default(), f, seed);
        let r = s.run(10 * MIN, true).and_then(|_| s.settle());
        if let Err(e) = r {
            panic!("seed {seed}: {}", s.diagnose(e));
        }
    }
}

/// The harness must itself catch a misattributed reply: credit the first
/// gain query with a wrong value and the oracle must object.
#[test]
fn oracle_catches_a_wrong_answer() {
    let mut o = Oracle::new();
    o.allowed_gain = [3].into();
    assert!(o.check_truth(&Request::QueryOutputGain, Response::Gain(Gain::new(5).unwrap())).is_err());
    assert!(o.check_truth(&Request::Firmware, Response::Lines(vec![])).is_err());
}
