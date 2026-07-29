//! Frozen differential-parity replay.
//!
//! `tests/data/parity_expected.txt` holds a curated corpus of Scala programs and
//! the stdout each produces, captured byte-for-byte from a reference `scala`
//! during authoring (the same programs the `parity-fuzz` binary diffs live). This
//! test replays every program through the built `scala` frontend and asserts the
//! frozen output, so the parity-critical behaviors — Int-vs-Double division,
//! `Double.toString` notation, `+` concatenation rules, structural `==`, range
//! `for` and its `by` step, IEEE division by zero, and
//! `try`/`catch`/`finally`/`throw` unwinding (including the exact JDK exception
//! messages) — stay locked WITHOUT any Scala toolchain installed. CI runs this;
//! the live `parity-fuzz` differential harness is a developer tool.
//!
//! Format: one record per line, `program<TAB>expected`, with `\n` in `expected`
//! encoded as the two characters backslash-n.

use std::process::Command;

fn run(src: &str) -> (String, bool) {
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
    let mut n = 0;
    for (i, line) in data.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let (prog, expected_enc) = line
            .split_once('\t')
            .unwrap_or_else(|| panic!("record {} has no TAB separator", i + 1));
        let expected = expected_enc.replace("\\n", "\n");
        let (got, ok) = run(prog);
        assert!(
            ok,
            "record {}: scalars rejected a valid program:\n  {prog}",
            i + 1
        );
        assert_eq!(
            got,
            expected,
            "record {}: output diverged from reference scala\n  program : {prog}",
            i + 1
        );
        n += 1;
    }
    assert!(n >= 15, "expected a non-trivial corpus, got {n} records");
}
