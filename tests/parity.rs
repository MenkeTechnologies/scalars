//! Frozen differential-parity replay.
//!
//! `tests/data/parity_expected.txt` holds a curated corpus of Scala programs and
//! what each one does — the stdout it produces, and for the programs that abort,
//! the exception that stops them — captured from a reference `scala` during
//! authoring (the same programs the `parity-fuzz` binary diffs live). This test
//! replays every program through the built `scala` frontend and asserts the
//! frozen result, so the parity-critical behaviors — Int-vs-Double division,
//! `Double.toString` notation, `+` concatenation rules, structural `==`, range
//! `for` and its `by` step, IEEE division by zero, per-collection out-of-bounds
//! and empty-receiver messages, and `try`/`catch`/`finally`/`throw` unwinding
//! (including the exact JDK exception messages) — stay locked WITHOUT any Scala
//! toolchain installed. CI runs this; the live `parity-fuzz` differential
//! harness is a developer tool.
//!
//! Format: one record per line, TAB-separated, with `\n` in any field encoded as
//! the two characters backslash-n.
//!
//! * `program<TAB>stdout` — the program must SUCCEED and print exactly `stdout`.
//! * `program<TAB>stdout<TAB>exception` — the program must FAIL, print exactly
//!   `stdout` before it does, and report `exception`.
//!
//! The third field closed this file's largest blind spot: until it existed, a
//! record could only say "this runs and prints X", so NO failing program could
//! be frozen here at all. Every exception message — the whole
//! `NoSuchElementException` / `IndexOutOfBoundsException` / `MatchError` surface,
//! which is where a hand-written string is most likely to be wrong and least
//! likely to be noticed — was reachable only through `tests/eval.rs`, and only
//! when someone thought to wrap the expression in a `catch`. A program that
//! ABORTS could not be described.
//!
//! What the third field holds is the reference's exception LINE
//! (`<fqcn>: <message>`), not its stderr. A failing `scala` run writes ANSI
//! build chatter, then a stack trace whose frames name a temp file, and wraps
//! anything raised from an `extends App` body in an `ExceptionInInitializerError`
//! — so the capture takes the innermost `Caused by:` when there is one and the
//! `Exception in thread "main"` line otherwise. That text is the only part of a
//! failing reference run that is comparable at all; this frontend's own stderr
//! must CONTAIN it (ours prefixes `scalars: `). The stdout field stays
//! byte-for-byte: how much output escaped before the abort is exactly as
//! observable as the message, and a frontend that raised too early or too late
//! would still match on the message alone.
//!
//! Exit codes are not compared beyond zero/non-zero, for the same reason
//! `parity-fuzz` does not compare them: a from-scratch frontend picks its own.

use std::process::Command;

fn run(src: &str) -> (String, bool) {
    let (out, _err, ok) = run_full(src);
    (out, ok)
}

fn run_full(src: &str) -> (String, String, bool) {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("scalars_parity_{}.scala", fnv1a(src)));
    std::fs::write(&path, src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_scala"))
        .arg(&path)
        .output()
        .expect("spawn scala");
    let _ = std::fs::remove_file(&path);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[test]
fn frozen_corpus_matches_reference_scala() {
    let data = include_str!("data/parity_expected.txt");
    let (mut n, mut failing) = (0, 0);
    for (i, line) in data.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let rec = i + 1;
        let fields: Vec<&str> = line.split('\t').collect();
        let (prog, expected_enc, want_exc) = match fields.as_slice() {
            [p, e] => (*p, *e, None),
            [p, e, x] => (*p, *e, Some(*x)),
            _ => panic!(
                "record {rec}: want 2 or 3 TAB-separated fields, got {}",
                fields.len()
            ),
        };
        let expected = expected_enc.replace("\\n", "\n");
        let (got, err, ok) = run_full(prog);

        match want_exc {
            None => {
                assert!(
                    ok,
                    "record {rec}: scalars rejected a valid program:\n  {prog}\n  {err}"
                );
                assert_eq!(
                    got, expected,
                    "record {rec}: output diverged from reference scala\n  program : {prog}"
                );
            }
            Some(exc) => {
                assert!(
                    !exc.is_empty(),
                    "record {rec}: an expected-failure record needs a non-empty exception field"
                );
                failing += 1;
                assert!(
                    !ok,
                    "record {rec}: this program ABORTS under reference scala with {exc}, \
                     but scalars ran it to completion and printed {got:?}\n  {prog}"
                );
                assert_eq!(
                    got, expected,
                    "record {rec}: the output BEFORE the abort diverged \
                     (the program failed on both sides, but not at the same point)\n  \
                     program : {prog}"
                );
                assert!(
                    err.contains(exc),
                    "record {rec}: reference scala reports {exc:?}; scalars reported {err:?}\n  \
                     program : {prog}"
                );
            }
        }
        n += 1;
    }
    assert!(n >= 15, "expected a non-trivial corpus, got {n} records");
    // Without this the three-field form could be dropped from the data and the
    // whole failure axis would stop being exercised with the test still green.
    assert!(
        failing >= 20,
        "the expected-FAILURE half of the corpus has thinned out: {failing} records"
    );
}
