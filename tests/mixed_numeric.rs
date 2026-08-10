//! Mixed `Int`/`Float` operands at the strict numeric hook.
//!
//! fusevm computes a mixed pair natively only while the integer is one an `f64`
//! holds exactly. Past `2^53` the conversion rounds — `16677181699666569`
//! collapses onto its neighbour `16677181699666568` — so strict numeric mode
//! hands the two operands to [`scalars::host::numeric_hook`] instead of an
//! answer computed on a rounded value. Every arithmetic and comparison
//! operation on such a pair therefore arrives at the hook.
//!
//! Scala's answer for that pair is the *promoted* one: binary numeric promotion
//! widens the `Long` to `Double` first, so `16677181699666569L ==
//! 1.6677181699666568E16` is `true` and `16677181699666569L % 2.5` is `0.5`.
//! The hook has to return that deliberately — before this test it rejected the
//! arithmetic (`operator ... is not defined for operands`) and answered the six
//! comparisons by comparing the two operands' *rendered strings*
//! LEXICOGRAPHICALLY.
//!
//! `data/mixed_numeric_expected.txt` is the verbatim stdout of a reference
//! `scala` (Scala code runner 1.16.0, Scala 3.8.4, JDK 26.0.2) on a generated
//! program that prints `<pair>.<order>.<op><TAB><result>` for every combination
//! below, both operand orders, five arithmetic operations and all six
//! comparisons. Nothing here is derived from what this frontend does.
//!
//! `Div` was absent from the capture until this round: the value assertions
//! covered `+`, `-`, `*` and `%` while `Div` appeared only in
//! [`mixed_int_float_pairs_are_never_rejected`], which checks that the hook
//! answers a `Float` and not WHICH `Float`. A `/` that answered the reciprocal,
//! or that computed on the rounded integer, passed the whole file. The twelve
//! `div` records close that.

use fusevm::{NumOp, Value};
use scalars::host::numeric_hook;

/// The operand pairs, matching the `val`s of the reference program by name.
/// `P0` is the in-range pair (fusevm answers it natively today, but the hook
/// must still be right); `P1` pairs the integer with its own rounded `f64`
/// image; `P2` with a small value; `P3` with the next `f64` above that image;
/// `P4` is the negated pair; `P5` is `2^53 + 1` against `2^53`.
const PAIRS: &[(&str, i64, f64)] = &[
    ("P0", 7, 2.5),
    ("P1", 16677181699666569, 1.6677181699666568E16),
    ("P2", 16677181699666569, 2.5),
    ("P3", 16677181699666569, 1.667718169966657E16),
    ("P4", -16677181699666569, -2.5),
    ("P5", 9007199254740993, 9.007199254740992E15),
];

fn op_of(name: &str) -> NumOp {
    match name {
        "add" => NumOp::Add,
        "sub" => NumOp::Sub,
        "mul" => NumOp::Mul,
        "div" => NumOp::Div,
        "mod" => NumOp::Mod,
        "lt" => NumOp::Lt,
        "gt" => NumOp::Gt,
        "le" => NumOp::Le,
        "ge" => NumOp::Ge,
        "eq" => NumOp::Eq,
        "ne" => NumOp::Ne,
        other => panic!("unknown operation `{other}` in the reference capture"),
    }
}

/// Render a hook answer the way the reference program printed it: `"" + x`,
/// i.e. `Boolean.toString` / `Double.toString`. An error is rendered as the
/// message, which can never equal a Scala result and so always reports as a
/// divergence.
fn render(got: &Result<Value, String>) -> String {
    match got {
        Ok(Value::Bool(b)) => b.to_string(),
        Ok(Value::Float(f)) => scalars::host::scala_str(&Value::Float(*f)),
        Ok(v) => format!("<non-Scala result {v:?}>"),
        Err(e) => format!("<error: {e}>"),
    }
}

#[test]
fn mixed_int_float_pairs_match_reference_scala() {
    let data = include_str!("data/mixed_numeric_expected.txt");
    let mut checked = 0usize;
    let mut bad: Vec<String> = Vec::new();
    for (i, line) in data.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let (key, expected) = line
            .split_once('\t')
            .unwrap_or_else(|| panic!("record {} has no TAB separator", i + 1));
        let mut parts = key.split('.');
        let pair = parts.next().expect("record key names a pair");
        let order = parts.next().expect("record key names an operand order");
        let op_name = parts.next().expect("record key names an operation");
        let &(_, x, y) = PAIRS
            .iter()
            .find(|(n, _, _)| *n == pair)
            .unwrap_or_else(|| panic!("record {} names unknown pair `{pair}`", i + 1));
        let (a, b) = match order {
            "IF" => (Value::Int(x), Value::Float(y)),
            "FI" => (Value::Float(y), Value::Int(x)),
            other => panic!("record {} has unknown operand order `{other}`", i + 1),
        };
        let got = numeric_hook(op_of(op_name), &a, &b);
        let shown = render(&got);
        if shown != expected {
            bad.push(format!("  {key}: scala {expected:?}, scalars {shown:?}"));
        }
        checked += 1;
    }
    assert!(
        checked >= 132,
        "expected the full reference capture, checked only {checked} records"
    );
    assert!(
        bad.is_empty(),
        "{} of {checked} mixed Int/Float operations diverge from reference scala:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

/// Both operand orders reach the hook for every operation the reference covers,
/// and none of them is an error return. Kept separate from the value assertions
/// so a regression that reintroduces the `not defined for operands` rejection is
/// reported as such even if the expectation file is ever narrowed.
#[test]
fn mixed_int_float_pairs_are_never_rejected() {
    let ops = [
        NumOp::Add,
        NumOp::Sub,
        NumOp::Mul,
        NumOp::Mod,
        NumOp::Div,
        NumOp::Lt,
        NumOp::Gt,
        NumOp::Le,
        NumOp::Ge,
        NumOp::Eq,
        NumOp::Ne,
    ];
    for &(name, x, y) in PAIRS {
        for op in ops {
            for (a, b) in [
                (Value::Int(x), Value::Float(y)),
                (Value::Float(y), Value::Int(x)),
            ] {
                let got = numeric_hook(op, &a, &b);
                assert!(
                    got.is_ok(),
                    "{name} {op:?} {a:?} {b:?} was rejected: {}",
                    got.unwrap_err()
                );
                let kind_ok = match op {
                    NumOp::Lt | NumOp::Gt | NumOp::Le | NumOp::Ge | NumOp::Eq | NumOp::Ne => {
                        matches!(got, Ok(Value::Bool(_)))
                    }
                    _ => matches!(got, Ok(Value::Float(_))),
                };
                assert!(kind_ok, "{name} {op:?} answered the wrong kind: {got:?}");
            }
        }
    }
}
