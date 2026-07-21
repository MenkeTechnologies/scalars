//! Differential parity fuzzer: reference `scala <file>` vs our `scala <file>`.
//!
//! Generates grammar-driven, deterministic-output Scala programs, runs each
//! through both the reference Scala toolchain and this frontend, and reports
//! every case whose stdout OR success/failure diverges. Each program is produced
//! from a per-index seed so any divergence replays exactly:
//! `parity-fuzz --seed <N> --once`.
//!
//! The reference `scala` pays a ~3s JVM/compile cost per invocation, so every
//! program packs many independent probe statements (`--probes`, default 40) into
//! one `object T extends App { … }`; a single invocation therefore exercises
//! dozens of probes. On divergence, [`minimize`] bisects the probe list down to
//! the single offending probe before reporting.
//!
//! The generator is biased toward the historically weak areas of a from-scratch
//! Scala frontend: `Int`-vs-`Double` division dispatch (`7/2==3`, `7/2.0==3.5`),
//! `+` string-concatenation coercion, structural `==`/`!=`, `Double.toString`
//! notation (the decimal/scientific threshold), and the range `for`. Pure random
//! bytes only produce mutual parse errors that agree on both sides and teach
//! nothing.
//!
//! Scope + determinism invariants (mirroring the node-js/pythonrs harnesses):
//!   * Only constructs scalars actually implements are emitted — an unsupported
//!     construct would be a known gap, not a parity signal.
//!   * No nondeterministic output (no `Random`, `System.currentTimeMillis`,
//!     identity hashes, unordered collections). Every probe's output is a pure
//!     function of its source.
//!   * Documented known gaps are NOT generated: integer overflow (operands and
//!     products are kept well inside `Int` range) and integer division/modulo by
//!     zero (divisors are always non-zero). Generating them would only reproduce
//!     `BUGS.md` entries.
//!
//! Subprocess-only: this binary never links the scalars library — it compares
//! two `scala` processes, exactly as a user would observe them.
//!
//! Build:  cargo build --bin parity-fuzz
//! Run:    ./target/debug/parity-fuzz --count 300 --probes 40

use std::io::Read as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

// ───────────────────────── deterministic PRNG (splitmix64) ─────────────────

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }
}

fn pick<'a, T>(rng: &mut Rng, xs: &'a [T]) -> &'a T {
    &xs[rng.below(xs.len() as u64) as usize]
}

// ───────────────────────── value pools ─────────────────────────────────────

// Small ints so arithmetic stays well inside 32-bit `Int` (no overflow gap).
const INTS: &[&str] = &[
    "0", "1", "2", "3", "5", "7", "9", "-1", "-3", "-7", "12", "42", "-42",
];
// Non-zero divisors (avoids the documented integer /0 gap).
const DIVS: &[&str] = &["1", "2", "3", "4", "5", "-2", "-3", "7"];
// Doubles spanning the decimal and scientific `Double.toString` ranges, plus
// repr edge cases.
const DBLS: &[&str] = &[
    "0.0",
    "-0.0",
    "0.5",
    "1.5",
    "2.5",
    "3.0",
    "3.14",
    "10.0",
    "-1.5",
    "100.0",
    "0.1",
    "0.2",
    "0.001",
    "0.0001",
    "1e7",
    "1e-4",
    "1e21",
    "1.5e300",
    "123456789.0",
    "9999999.0",
    "1e-300",
    "6.022e23",
];
const STRS: &[&str] = &[
    "\"hi\"",
    "\"World\"",
    "\"abc\"",
    "\"\"",
    "\"a\"",
    "\"x=\"",
    "\"café\"",
];
const BOOLS: &[&str] = &["true", "false"];
const AOPS: &[&str] = &["+", "-", "*"];
const FOPS: &[&str] = &["+", "-", "*", "/"];
const DIVMOD: &[&str] = &["/", "%"];
const CMPOPS: &[&str] = &["==", "!=", "<", ">", "<=", ">="];
const EQOPS: &[&str] = &["==", "!="];
const LOGOPS: &[&str] = &["&&", "||"];

// ───────────────────────── generators (one probe each) ─────────────────────
//
// Each returns ONE self-contained statement whose stdout is deterministic and
// which scalars implements. Probes never share state, so [`minimize`] can drop
// any subset and each survivor still reproduces.

fn g_arith(r: &mut Rng) -> String {
    // small operands, depth<=3 -> products stay < 2^31
    let a = pick(r, INTS);
    let b = pick(r, INTS);
    let c = pick(r, INTS);
    let (o1, o2) = (pick(r, AOPS), pick(r, AOPS));
    match r.below(4) {
        0 => format!("println({a} {o1} {b} {o2} {c})"),
        1 => format!("println(({a} {o1} {b}) {o2} {c})"),
        2 => format!("println({a} {o1} ({b} {o2} {c}))"),
        _ => format!("println(-{a} {o1} {b})", a = a.trim_start_matches('-')),
    }
}

fn g_intdiv(r: &mut Rng) -> String {
    let a = pick(r, INTS);
    let b = pick(r, DIVS);
    let op = pick(r, DIVMOD);
    format!("println({a} {op} {b})")
}

fn g_floatarith(r: &mut Rng) -> String {
    let a = pick(r, DBLS);
    let b = pick(r, DBLS);
    let op = pick(r, FOPS);
    // one mixed Int/Double form to exercise the division dispatch
    if r.below(3) == 0 {
        let i = pick(r, INTS);
        let d = pick(r, DBLS);
        return format!("println({i} {op} {d})");
    }
    format!("println({a} {op} {b})")
}

fn g_floatfmt(r: &mut Rng) -> String {
    // raw literal, or a whole-number-producing op, or negation
    let a = pick(r, DBLS);
    match r.below(4) {
        0 => format!("println({a})"),
        1 => format!("println(-{})", pick(r, DBLS).trim_start_matches('-')),
        2 => format!("println({a} + 0.0)"),
        _ => format!("println({} * 2.0)", pick(r, DBLS)),
    }
}

fn g_concat(r: &mut Rng) -> String {
    // Scala 3 has no universal `any2stringadd`: `String + Any` and
    // `numeric + String` are the only valid mixed `+`. `Boolean + String` and
    // `null + String` do NOT compile, so never put a non-numeric value on the
    // LEFT of a `+ String` — that would be invalid Scala, not a parity probe.
    let s = *pick(r, STRS);
    // Any value is legal on the RIGHT of `String +`.
    let any = match r.below(4) {
        0 => *pick(r, INTS),
        1 => *pick(r, DBLS),
        2 => *pick(r, BOOLS),
        _ => "null",
    };
    // Only a numeric value is legal on the LEFT of `+ String`.
    let numeric = if r.below(2) == 0 {
        *pick(r, INTS)
    } else {
        *pick(r, DBLS)
    };
    match r.below(3) {
        0 => format!("println({s} + {any})"),
        1 => format!("println({numeric} + {s})"),
        _ => format!("println({s} + {any} + {s})"),
    }
}

fn g_compare(r: &mut Rng) -> String {
    let op = pick(r, CMPOPS);
    match r.below(3) {
        0 => format!("println({} {op} {})", pick(r, INTS), pick(r, INTS)),
        1 => format!("println({} {op} {})", pick(r, DBLS), pick(r, DBLS)),
        _ => {
            let eq = pick(r, EQOPS);
            format!("println({} {eq} {})", pick(r, STRS), pick(r, STRS))
        }
    }
}

fn g_bool(r: &mut Rng) -> String {
    let a = pick(r, BOOLS);
    let b = pick(r, BOOLS);
    let op = pick(r, LOGOPS);
    match r.below(3) {
        0 => format!("println({a} {op} {b})"),
        1 => format!("println(!{a} {op} {b})"),
        _ => {
            // comparison-driven booleans
            let x = pick(r, INTS);
            let y = pick(r, INTS);
            format!("println({x} > {y} {op} {x} < {y})")
        }
    }
}

fn g_cond(r: &mut Rng) -> String {
    let n = pick(r, INTS);
    let a = pick(r, STRS);
    let b = pick(r, STRS);
    let c = pick(r, STRS);
    format!("{{ val n = {n}; if (n < 0) println({a}) else if (n == 0) println({b}) else println({c}) }}")
}

fn g_loop(r: &mut Rng) -> String {
    let lo = r.below(4) as i64;
    let hi = lo + 1 + r.below(6) as i64;
    match r.below(3) {
        0 => format!("{{ var s = 0; for (i <- {lo} until {hi}) {{ s += i }}; println(s) }}"),
        1 => format!("{{ var s = 0; for (i <- {lo} to {hi}) {{ s += i * i }}; println(s) }}"),
        _ => format!(
            "{{ var s = 0; var i = {lo}; while (i < {hi}) {{ s += i; i += 1 }}; println(s) }}"
        ),
    }
}

fn g_mixed(r: &mut Rng) -> String {
    let x = pick(r, INTS);
    let y = pick(r, DIVS);
    format!(
        "{{ val x = {x}; val y = {y}; val q = x / y; val rem = x % y; println(\"q=\" + q + \" r=\" + rem) }}"
    )
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    All,
    Arith,
    IntDiv,
    FloatArith,
    FloatFmt,
    Concat,
    Compare,
    Bool,
    Cond,
    Loop,
    Mixed,
}

fn mode_name(m: Mode) -> &'static str {
    match m {
        Mode::All => "all",
        Mode::Arith => "arith",
        Mode::IntDiv => "intdiv",
        Mode::FloatArith => "floatarith",
        Mode::FloatFmt => "floatfmt",
        Mode::Concat => "concat",
        Mode::Compare => "compare",
        Mode::Bool => "bool",
        Mode::Cond => "cond",
        Mode::Loop => "loop",
        Mode::Mixed => "mixed",
    }
}

fn parse_mode(s: &str) -> Option<Mode> {
    Some(match s {
        "all" => Mode::All,
        "arith" => Mode::Arith,
        "intdiv" => Mode::IntDiv,
        "floatarith" => Mode::FloatArith,
        "floatfmt" => Mode::FloatFmt,
        "concat" => Mode::Concat,
        "compare" => Mode::Compare,
        "bool" => Mode::Bool,
        "cond" => Mode::Cond,
        "loop" => Mode::Loop,
        "mixed" => Mode::Mixed,
        _ => return None,
    })
}

const CONCRETE: &[Mode] = &[
    Mode::Arith,
    Mode::IntDiv,
    Mode::FloatArith,
    Mode::FloatFmt,
    Mode::Concat,
    Mode::Compare,
    Mode::Bool,
    Mode::Cond,
    Mode::Loop,
    Mode::Mixed,
];

fn gen_probe(r: &mut Rng, mode: Mode) -> String {
    let m = if mode == Mode::All {
        *pick(r, CONCRETE)
    } else {
        mode
    };
    match m {
        Mode::Arith => g_arith(r),
        Mode::IntDiv => g_intdiv(r),
        Mode::FloatArith => g_floatarith(r),
        Mode::FloatFmt => g_floatfmt(r),
        Mode::Concat => g_concat(r),
        Mode::Compare => g_compare(r),
        Mode::Bool => g_bool(r),
        Mode::Cond => g_cond(r),
        Mode::Loop => g_loop(r),
        Mode::Mixed => g_mixed(r),
        Mode::All => unreachable!(),
    }
}

/// Generate a program's probe list for a seed (each probe is one line).
fn gen_probes(seed: u64, mode: Mode, n: usize) -> Vec<String> {
    let r = &mut Rng::new(seed);
    (0..n).map(|_| gen_probe(r, mode)).collect()
}

fn build_program(probes: &[String]) -> String {
    let mut s = String::from("object T extends App {\n");
    for p in probes {
        s.push_str("  ");
        s.push_str(p);
        s.push('\n');
    }
    s.push_str("}\n");
    s
}

// ───────────────────────── process invocation ──────────────────────────────

struct RunOut {
    stdout: Vec<u8>,
    exit: i32,
    timed_out: bool,
}

static TMP_CTR: AtomicU64 = AtomicU64::new(0);

/// Run `<prog> <tempfile.scala>` under a wall-clock timeout, capturing stdout.
fn run_prog(prog: &Path, src: &str, timeout: Duration) -> RunOut {
    let id = TMP_CTR.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("scalars_fuzz_{}_{}.scala", std::process::id(), id));
    if std::fs::write(&path, src).is_err() {
        return RunOut {
            stdout: Vec::new(),
            exit: -1,
            timed_out: false,
        };
    }

    let mut cmd = Command::new(prog);
    cmd.arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => {
            let _ = std::fs::remove_file(&path);
            return RunOut {
                stdout: Vec::new(),
                exit: -1,
                timed_out: false,
            };
        }
    };

    let mut out_h = child.stdout.take().map(|mut o| {
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = o.read_to_end(&mut b);
            b
        })
    });
    // drain stderr so a chatty compiler can't deadlock on a full pipe
    let mut err_h = child.stderr.take().map(|mut e| {
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = e.read_to_end(&mut b);
            b
        })
    });

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let exit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                exit = status.code().unwrap_or(-1);
                break;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let s = child.wait().ok();
                    exit = s.and_then(|s| s.code()).unwrap_or(-1);
                    timed_out = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => {
                exit = -1;
                break;
            }
        }
    }

    let stdout = out_h.take().and_then(|h| h.join().ok()).unwrap_or_default();
    let _ = err_h.take().and_then(|h| h.join().ok());
    let _ = std::fs::remove_file(&path);
    RunOut {
        stdout,
        exit,
        timed_out,
    }
}

/// A parity gap: stdout bytes differ, OR one side accepted the program (exit 0)
/// while the other rejected it. Exact exit codes are not compared — a
/// from-scratch frontend is free to pick its own non-zero code.
fn differs(oracle: &RunOut, ours: &RunOut) -> bool {
    (oracle.exit == 0) != (ours.exit == 0) || oracle.stdout != ours.stdout
}

fn render(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim_end_matches('\n');
    if std::str::from_utf8(bytes).is_err() {
        let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
        return format!("{text}\n  (hex) {}", hex.join(" "));
    }
    text.to_string()
}

fn diverges(probes: &[String], ours: &Path, oracle: &Path, timeout: Duration) -> bool {
    let src = build_program(probes);
    let o = run_prog(oracle, &src, timeout);
    if o.timed_out {
        return false; // oracle-side pathology, not a parity gap
    }
    let r = run_prog(ours, &src, timeout);
    differs(&o, &r)
}

/// Shrink a diverging program to the smallest still-diverging probe subset.
/// Probes are independent, so a single offending probe almost always reproduces;
/// fall back to a linear "remove one at a time" pass otherwise.
fn minimize(probes: &[String], ours: &Path, oracle: &Path, timeout: Duration) -> Vec<String> {
    // 1. Try each probe alone.
    for p in probes {
        let one = vec![p.clone()];
        if diverges(&one, ours, oracle, timeout) {
            return one;
        }
    }
    // 2. Interaction case: greedily drop probes that are not needed.
    let mut cur = probes.to_vec();
    let mut i = 0;
    while i < cur.len() && cur.len() > 1 {
        let mut trial = cur.clone();
        trial.remove(i);
        if diverges(&trial, ours, oracle, timeout) {
            cur = trial;
        } else {
            i += 1;
        }
    }
    cur
}

// ───────────────────────── binary resolution ───────────────────────────────

/// Our built `scala` binary — the sibling of this harness binary. NEVER the
/// system `scala` (same file name!), so we always use an absolute path into the
/// build directory rather than a PATH lookup.
fn ours_bin() -> PathBuf {
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_scala") {
        return PathBuf::from(p);
    }
    if let Some(d) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        let cand = d.join("scala");
        if cand.exists() {
            return cand;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("debug")
        .join("scala")
}

/// The ORACLE — the reference Scala toolchain. `SCALARS_FUZZ_SCALA` names it
/// explicitly; otherwise probe the usual locations. It must NOT resolve to our
/// own build output, and it must look like a real Scala (`--version` mentions
/// "Scala"). A misconfigured oracle is a HARD error — silently comparing against
/// the wrong thing answers a different question.
fn resolve_oracle(ours: &Path) -> PathBuf {
    let candidates: Vec<String> = if let Ok(p) = std::env::var("SCALARS_FUZZ_SCALA") {
        vec![p]
    } else {
        vec![
            "/opt/homebrew/bin/scala".into(),
            "/usr/local/bin/scala".into(),
            "/usr/bin/scala".into(),
            "scala".into(),
        ]
    };
    for c in &candidates {
        let cp = PathBuf::from(c);
        if cp.canonicalize().ok() == ours.canonicalize().ok() {
            continue; // never the frontend under test
        }
        if scala_version(&cp).is_some() {
            return cp;
        }
    }
    eprintln!("parity-fuzz: no reference scala found; set SCALARS_FUZZ_SCALA to a real `scala`");
    std::process::exit(2);
}

fn scala_version(prog: &Path) -> Option<String> {
    let o = Command::new(prog).arg("-version").output().ok()?;
    let s = format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    if s.to_lowercase().contains("scala") {
        Some(s.trim().to_string())
    } else {
        None
    }
}

// ───────────────────────── CLI + driver ────────────────────────────────────

struct Args {
    count: u64,
    probes: usize,
    base_seed: u64,
    jobs: usize,
    mode: Mode,
    once: bool,
    timeout: Duration,
    max_report: usize,
    out_path: PathBuf,
}

fn parse_args() -> Args {
    let mut a = Args {
        count: 200,
        probes: 40,
        base_seed: 1,
        jobs: (std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            / 2)
        .clamp(1, 8),
        mode: Mode::All,
        once: false,
        timeout: Duration::from_secs(40),
        max_report: 40,
        out_path: PathBuf::from("target/parity-divergences.txt"),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = || it.next().expect("missing value for flag");
        match arg.as_str() {
            "--count" => a.count = val().parse().expect("--count"),
            "--probes" => a.probes = val().parse().expect("--probes"),
            "--seed" => a.base_seed = val().parse().expect("--seed"),
            "--jobs" => a.jobs = val().parse::<usize>().expect("--jobs").max(1),
            "--mode" => a.mode = parse_mode(&val()).expect("unknown --mode"),
            "--once" => a.once = true,
            "--timeout" => a.timeout = Duration::from_secs(val().parse().expect("--timeout")),
            "--max-report" => a.max_report = val().parse().expect("--max-report"),
            "--out" => a.out_path = PathBuf::from(val()),
            other => {
                eprintln!("parity-fuzz: unknown flag `{other}`");
                std::process::exit(2);
            }
        }
    }
    a
}

fn main() {
    let args = parse_args();
    let ours = ours_bin();
    if !ours.exists() {
        eprintln!(
            "parity-fuzz: our `scala` not found at {} (run `cargo build` first)",
            ours.display()
        );
        std::process::exit(2);
    }
    let oracle = resolve_oracle(&ours);

    if args.once {
        let probes = gen_probes(args.base_seed, args.mode, args.probes);
        let full = build_program(&probes);
        let o = run_prog(&oracle, &full, args.timeout);
        let r = run_prog(&ours, &full, args.timeout);
        let diverged = !o.timed_out && differs(&o, &r);
        let show = if diverged {
            minimize(&probes, &ours, &oracle, args.timeout)
        } else {
            probes.clone()
        };
        let ms = build_program(&show);
        let mo = run_prog(&oracle, &ms, args.timeout);
        let mr = run_prog(&ours, &ms, args.timeout);
        println!("seed   : {}", args.base_seed);
        println!("mode   : {}", mode_name(args.mode));
        println!("program:\n{ms}");
        println!(
            "--- scala(ref) exit={} timeout={} ---",
            mo.exit, mo.timed_out
        );
        println!("{}", render(&mo.stdout));
        println!(
            "--- scalars    exit={} timeout={} ---",
            mr.exit, mr.timed_out
        );
        println!("{}", render(&mr.stdout));
        println!("--- {} ---", if diverged { "DIVERGE" } else { "match" });
        std::process::exit(if diverged { 1 } else { 0 });
    }

    let next = AtomicU64::new(0);
    let checked = AtomicU64::new(0);
    let timeouts = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    let divergences: Mutex<Vec<(u64, String)>> = Mutex::new(Vec::new());
    let start = Instant::now();

    eprintln!(
        "oracle : {}",
        scala_version(&oracle)
            .unwrap_or_default()
            .lines()
            .next()
            .unwrap_or("")
            .trim()
    );
    eprintln!("ours   : {}", ours.display());
    eprintln!(
        "fuzzing {} programs x {} probes ({}) across {} workers…",
        args.count,
        args.probes,
        mode_name(args.mode),
        args.jobs
    );

    std::thread::scope(|scope| {
        for _ in 0..args.jobs {
            scope.spawn(|| loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let idx = next.fetch_add(1, Ordering::Relaxed);
                if idx >= args.count {
                    break;
                }
                let seed = args.base_seed.wrapping_add(idx);
                let probes = gen_probes(seed, args.mode, args.probes);
                let src = build_program(&probes);
                let o = run_prog(&oracle, &src, args.timeout);
                let r = run_prog(&ours, &src, args.timeout);
                let done = checked.fetch_add(1, Ordering::Relaxed) + 1;
                if o.timed_out || r.timed_out {
                    timeouts.fetch_add(1, Ordering::Relaxed);
                }
                if !o.timed_out && differs(&o, &r) {
                    let minimal = minimize(&probes, &ours, &oracle, args.timeout);
                    // re-verify the shrunk case actually reproduces
                    if !diverges(&minimal, &ours, &oracle, args.timeout) {
                        continue;
                    }
                    let ms = build_program(&minimal);
                    let mo = run_prog(&oracle, &ms, args.timeout);
                    let mr = run_prog(&ours, &ms, args.timeout);
                    let rec = format!(
                        "==== seed {seed} ====\n\
                         probe   : {}\n\
                         scala   : exit={} {}\n\
                         scalars : exit={} {}\n",
                        minimal.join(" ; "),
                        mo.exit,
                        render(&mo.stdout).replace('\n', " | "),
                        mr.exit,
                        render(&mr.stdout).replace('\n', " | "),
                    );
                    let mut d = divergences.lock().unwrap();
                    d.push((seed, rec));
                    if d.len() >= args.max_report {
                        stop.store(true, Ordering::Relaxed);
                    }
                }
                if done % 20 == 0 {
                    let n = divergences.lock().unwrap().len();
                    eprintln!(
                        "  {done}/{} programs, {n} divergences, {:.1} prog/s",
                        args.count,
                        done as f64 / start.elapsed().as_secs_f64().max(0.001)
                    );
                }
            });
        }
    });

    let checked = checked.load(Ordering::Relaxed);
    let timeouts = timeouts.load(Ordering::Relaxed);
    let mut divergences: Vec<(u64, String)> = divergences.into_inner().unwrap();
    divergences.sort_by_key(|(seed, _)| *seed);
    let divergences: Vec<String> = divergences.into_iter().map(|(_, r)| r).collect();
    let elapsed = start.elapsed();

    println!(
        "\nfuzzed {checked} programs ({} probes) in {:.1}s\n\
         divergences : {}\n\
         timeouts    : {}",
        checked as usize * args.probes,
        elapsed.as_secs_f64(),
        divergences.len(),
        timeouts,
    );

    if !divergences.is_empty() {
        if let Some(parent) = args.out_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut f) = std::fs::File::create(&args.out_path) {
            for d in &divergences {
                let _ = writeln!(f, "{d}");
            }
        }
        println!(
            "wrote {} divergences to {}",
            divergences.len(),
            args.out_path.display()
        );
        for d in divergences.iter().take(10) {
            println!("\n{d}");
        }
        std::process::exit(1);
    }
    println!("no divergences — scalars matches reference scala across all probes ✓");
}
