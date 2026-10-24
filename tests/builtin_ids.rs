//! Source-level invariants on the builtin id space.
//!
//! Every host builtin is a hand-assigned `pub const NAME: u16 = N;` in
//! `src/host.rs`, handed to fusevm as `Op::CallBuiltin(N, argc)` and installed
//! with `VM::register_builtin(N, handler)`. Nothing in the compiler checks the
//! numbers: two constants can carry the same id, and `register_builtin` keeps
//! the LAST handler registered for it, so every call site compiled against the
//! other constant silently runs the wrong builtin. Two sessions adding a
//! builtin each merge cleanly (each side only adds a line), which is how
//! `MAIN_REST` and `MAKE_NUMERIC` once both took 837 and `implicitly[Numeric[
//! Int]]` printed `List()`. These tests read the constants and registrations
//! back out of the source so a collision fails the build instead.

use std::collections::HashMap;
use std::path::PathBuf;

fn read(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Every `pub const NAME: u16 = N;` in `src/host.rs`, as (name, id, line).
fn builtin_ids() -> Vec<(String, u16, usize)> {
    let mut out = Vec::new();
    for (i, line) in read("src/host.rs").lines().enumerate() {
        let Some(rest) = line.trim().strip_prefix("pub const ") else {
            continue;
        };
        let Some((name, ty_value)) = rest.split_once(':') else {
            continue;
        };
        let Some(value) = ty_value
            .trim()
            .strip_prefix("u16")
            .and_then(|v| v.trim().strip_prefix('='))
            .and_then(|v| v.trim().strip_suffix(';'))
        else {
            continue;
        };
        let id: u16 = value
            .trim()
            .parse()
            .unwrap_or_else(|e| panic!("src/host.rs:{}: unparsable builtin id: {e}", i + 1));
        out.push((name.trim().to_string(), id, i + 1));
    }
    out
}

/// The constant each `register_builtin(NAME, …)` call names, as (name, file:line).
fn registrations() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for file in ["src/host.rs", "src/lib.rs"] {
        for (i, line) in read(file).lines().enumerate() {
            let Some((_, after)) = line.split_once("register_builtin(") else {
                continue;
            };
            let Some((name, _)) = after.split_once(',') else {
                continue;
            };
            let name = name.trim().rsplit("::").next().unwrap_or("").to_string();
            out.push((name, format!("{file}:{}", i + 1)));
        }
    }
    out
}

#[test]
fn no_two_builtins_share_an_id() {
    let ids = builtin_ids();
    // A floor, so a parser that silently stopped matching cannot pass this.
    assert!(ids.len() >= 100, "only {} builtin ids parsed", ids.len());
    let mut seen: HashMap<u16, (String, usize)> = HashMap::new();
    let mut clashes = Vec::new();
    for (name, id, line) in ids {
        if let Some((other, other_line)) = seen.get(&id) {
            clashes.push(format!(
                "id {id}: {other} (src/host.rs:{other_line}) and {name} (src/host.rs:{line})"
            ));
        } else {
            seen.insert(id, (name, line));
        }
    }
    assert!(
        clashes.is_empty(),
        "builtin ids collide:\n{}",
        clashes.join("\n")
    );
}

#[test]
fn every_registration_names_a_distinct_builtin() {
    let ids: HashMap<String, u16> = builtin_ids()
        .into_iter()
        .map(|(n, id, _)| (n, id))
        .collect();
    let regs = registrations();
    assert!(
        regs.len() >= 100,
        "only {} registrations parsed",
        regs.len()
    );
    let mut by_id: HashMap<u16, (String, String)> = HashMap::new();
    let mut problems = Vec::new();
    for (name, at) in regs {
        let Some(&id) = ids.get(&name) else {
            problems.push(format!(
                "{at}: registers {name}, which is not a builtin id constant"
            ));
            continue;
        };
        if let Some((other, other_at)) = by_id.get(&id) {
            problems.push(format!(
                "id {id} registered twice: {other} ({other_at}) and {name} ({at})"
            ));
        } else {
            by_id.insert(id, (name, at));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
