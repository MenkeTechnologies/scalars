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
                    well_formed_exception(exc),
                    "record {rec}: the exception field must be a qualified JDK throwable line \
                     (`java.…Exception`/`scala.…Error`, optionally followed by `: message`), \
                     got {exc:?}. A non-empty check is not enough: `err.contains(\" \")` is true \
                     of almost any stderr, so a degenerate field would let the record assert \
                     nothing while still counting toward the expected-failure floor below."
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
    // The floors are the counts the file ACTUALLY holds, not a token minimum.
    // `n >= 15` against a 542-record corpus is not a floor: 527 records could be
    // deleted — every Double-notation pin, the whole exception surface, all of
    // it — and this test would still pass, reporting that parity is locked. A
    // frozen corpus's only defence against silent shrinkage is a number that
    // moves with it, so raise both whenever the corpus grows.
    assert!(
        n >= 542,
        "the frozen corpus has shrunk: {n} records, expected at least 542"
    );
    // Without this the three-field form could be dropped from the data and the
    // whole failure axis would stop being exercised with the test still green.
    assert!(
        failing >= 37,
        "the expected-FAILURE half of the corpus has thinned out: {failing} records"
    );
}

/// Whether an expected-failure record's third field is a real JDK throwable
/// line: a dotted, `java.`/`scala.`-rooted class whose last segment ends in
/// `Exception` or `Error`, optionally followed by `: <message>`.
///
/// The check exists because `err.contains(exc)` is only as strong as `exc`: a
/// field of `" "`, or `"Exception"`, or a stray word is non-empty — which is all
/// the record format used to require — matches nearly any stderr, and would let
/// a record report a PASS while asserting nothing about which exception stopped
/// the program. That is the "frozen failure read as a pass" shape, and it counts
/// toward the expected-failure floor while doing so.
fn well_formed_exception(exc: &str) -> bool {
    let head = exc.split_once(": ").map_or(exc, |(h, _)| h);
    let Some(last) = head.rsplit('.').next() else {
        return false;
    };
    head.split('.').count() >= 2
        && (head.starts_with("java.") || head.starts_with("scala."))
        && (last.ends_with("Exception") || last.ends_with("Error"))
        && last.len() > "Exception".len()
}

/// The record-format guard has to REJECT the shapes it exists to reject.
///
/// A validator is itself a place a vacuous pass hides: one written so that
/// everything satisfies it reads exactly like a working check, and the only way
/// to tell is to hand it the degenerate inputs. Each rejected string below is one
/// that satisfied the old `!exc.is_empty()` test and then made
/// `err.contains(exc)` true against essentially any stderr.
#[test]
fn the_expected_failure_record_format_rejects_a_field_that_asserts_nothing() {
    for bad in [
        "",
        " ",
        "\t",
        "boom",
        "Exception",
        "Error",
        "java.lang.Exception",
        "java.lang.Error",
        ": / by zero",
        "/ by zero",
        "ArithmeticException: / by zero",
        "com.example.MyException: x",
        "java.lang.Thing: x",
        "javaException",
    ] {
        assert!(
            !well_formed_exception(bad),
            "the record-format guard accepted {bad:?}, which asserts nothing about which \
             exception stopped the program"
        );
    }
    // …and it must keep accepting every shape the frozen corpus actually uses.
    for good in [
        "java.lang.ArithmeticException: / by zero",
        "java.util.NoSuchElementException: head of empty list",
        "scala.MatchError: 5 (of class java.lang.Integer)",
        "java.lang.StackOverflowError",
        "java.util.regex.PatternSyntaxException: Unclosed character class near index 0",
        "java.util.IllegalFormatConversionException: d != java.lang.String",
    ] {
        assert!(
            well_formed_exception(good),
            "the record-format guard rejected {good:?}, a real JDK throwable line"
        );
    }
}
