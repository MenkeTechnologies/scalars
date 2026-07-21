//! The scalars host: builtin registration, Scala value formatting, and the
//! strict numeric hook.
//!
//! scalars keeps no object heap of its own yet (slice 1 runs on the fusevm value
//! model directly). Two places need Scala semantics that fusevm's default
//! awk/shell flavour does not provide:
//!
//! 1. **Printing.** fusevm's native `PrintLn` renders values shell-style
//!    (`true`→`1`, `3.0`→`3`). Predef's `println`/`print` instead lower to a
//!    registered builtin ([`SPRINTLN`]/[`SPRINT`]) that formats through
//!    [`scala_str`] — `true`/`false`, `3.0`, `null` — matching `scala`.
//! 2. **`+` overloading.** Scala's `+` is string concatenation when either
//!    operand is a `String` (via `Predef.any2stringadd` / `String.+`). fusevm
//!    runs *strict* once a numeric hook is installed, delegating any operation
//!    with a non-numeric operand to [`numeric_hook`], where `+` concatenates via
//!    the same [`scala_str`].

use fusevm::{NumOp, Value, VM};
use std::cell::RefCell;

/// Builtin id for Predef `println` (one Scala-formatted arg + newline).
pub const SPRINTLN: u16 = 700;
/// Builtin id for Predef `print` (one Scala-formatted arg, no newline).
pub const SPRINT: u16 = 701;
/// Builtin id for Scala `/` (type-dispatching division — see `b_div`).
pub const SDIV: u16 = 702;
/// Builtin id for the `scala --dap` per-statement debug marker. Emitted only by
/// `compiler::compile_debug`; registered only on the debug run path
/// (`crate::run_chunk_debug`). It carries no args and returns `Unit`.
pub const DBG_LINE: u16 = 703;
/// Builtin id for compiling + registering an inline `rust { ... }` FFI block.
/// Pops the base64 block body (a `Str`) and hands it to
/// `fusevm::ffi::compile_and_register`. Returns `Unit`.
pub const FFI_COMPILE: u16 = 704;
/// Builtin id for calling an FFI-exported function by name. The stack holds the
/// args (deepest first) with the function name (a `Str`) on top; `argc` is the
/// arg count plus one (the name). Dispatches through `fusevm::ffi::try_call` and
/// returns the result.
pub const FFI_CALL: u16 = 705;
/// Builtin id for universal postfix method dispatch (`s.length`, `n.toString`,
/// `s.substring(1, 3)`). The stack holds the receiver (deepest), then the
/// arguments, then the method name (a `Str`) on top; `argc` is the argument
/// count plus two (receiver + name). Routes to the wired String/Int stdlib in
/// [`b_method`].
pub const SMETHOD: u16 = 706;
/// Builtin id for `f"…"`-interpolator formatting. The stack holds the value
/// (deepest) and the format spec (a `Str`, on top); `argc` is 2. Formats through
/// the Java-`Formatter` subset in [`format_one`].
pub const SFORMAT: u16 = 707;
/// Builtin id for a `match` typed-pattern runtime type test (`case x: String`).
/// The stack holds the value (deepest) and the type name (a `Str`, on top);
/// `argc` is 2. Returns a `Bool`.
pub const SISTYPE: u16 = 708;
/// Builtin id for a non-exhaustive `match` fall-through. Pops the unmatched
/// scrutinee and faults with `scala.MatchError`, matching an uncaught throw.
pub const SMATCHERR: u16 = 709;

thread_local! {
    /// Set by a runtime fault raised inside a builtin (an FFI compile/dispatch
    /// error) so the runner can surface it as an error after `VM::run` returns
    /// (a builtin can only return a `Value`, not an error).
    static FFI_ERROR: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Take and clear any pending FFI-fault message.
pub fn take_error() -> Option<String> {
    FFI_ERROR.with(|e| e.borrow_mut().take())
}

fn fault(vm: &mut VM, msg: impl Into<String>) -> Value {
    FFI_ERROR.with(|e| *e.borrow_mut() = Some(msg.into()));
    vm.request_halt();
    Value::Undef
}

/// Install scalars builtins on a VM: the Scala-formatting print builtins, the
/// type-dispatching division operator, and the inline-Rust FFI bridge. This is
/// the single install choke point later waves (methods, `String`/collection
/// objects) grow into.
pub fn install(vm: &mut VM) {
    vm.register_builtin(SPRINTLN, b_println);
    vm.register_builtin(SPRINT, b_print);
    vm.register_builtin(SDIV, b_div);
    vm.register_builtin(FFI_COMPILE, b_ffi_compile);
    vm.register_builtin(FFI_CALL, b_ffi_call);
    vm.register_builtin(SMETHOD, b_method);
    vm.register_builtin(SFORMAT, b_format);
    vm.register_builtin(SISTYPE, b_istype);
    vm.register_builtin(SMATCHERR, b_matcherr);
}

/// `SFORMAT` builtin: pop the format spec (top) and the value, and format the
/// value per the Java-`Formatter` subset. A malformed/unsupported spec halts the
/// VM with the message parked for the runner.
fn b_format(vm: &mut VM, _argc: u8) -> Value {
    let spec = vm.pop().as_str_cow().into_owned();
    let v = vm.pop();
    match format_one(&spec, &v) {
        Ok(s) => Value::str(s),
        Err(e) => fault(vm, e),
    }
}

/// `SISTYPE` builtin: pop the type name (top) and the value; push whether the
/// value's runtime type matches (for a `match` typed pattern).
fn b_istype(vm: &mut VM, _argc: u8) -> Value {
    let ty = vm.pop().as_str_cow().into_owned();
    let v = vm.pop();
    Value::bool(value_is_type(&v, &ty))
}

/// `SMATCHERR` builtin: pop the unmatched scrutinee and raise `scala.MatchError`,
/// with the boxed class name Scala reports (`java.lang.Integer`, …).
fn b_matcherr(vm: &mut VM, _argc: u8) -> Value {
    let v = vm.pop();
    let class = match &v {
        Value::Int(_) => "java.lang.Integer",
        Value::Float(_) => "java.lang.Double",
        Value::Bool(_) => "java.lang.Boolean",
        Value::Str(_) => "java.lang.String",
        _ => "scala.runtime.Null$",
    };
    fault(
        vm,
        format!("scala.MatchError: {} (of class {class})", scala_str(&v)),
    )
}

/// Whether `v`'s runtime type satisfies the Scala type name `ty` (a `match`
/// typed-pattern test). Covers the primitive/`String` types this frontend
/// models; `Any`/`AnyRef`/`AnyVal` match everything.
fn value_is_type(v: &Value, ty: &str) -> bool {
    match ty {
        "String" | "CharSequence" => matches!(v, Value::Str(_)),
        "Int" | "Integer" | "Long" | "Short" | "Byte" => matches!(v, Value::Int(_)),
        "Double" | "Float" => matches!(v, Value::Float(_)),
        "Boolean" => matches!(v, Value::Bool(_)),
        "Any" | "AnyRef" | "AnyVal" | "Object" => true,
        _ => false,
    }
}

/// Format one value with a Java-`Formatter` conversion spec
/// (`%[flags][width][.precision]conv`). A faithful subset:
///
/// * `s`/`S` — string (`toString`), precision truncates, width pads;
/// * `d` — decimal integer, `0`/`-`/`+`/space flags + width;
/// * `f` — fixed-point float, precision (default 6), `0`/`-`/`+`/space + width;
/// * `b`/`B` — boolean (`false` only for `false`/`null`), width;
/// * `c` — character (from a code point or the first char of a string).
///
/// An unsupported conversion returns an error (the VM faults) rather than
/// emitting something Java would not.
fn format_one(spec: &str, v: &Value) -> Result<String, String> {
    let sb = spec.as_bytes();
    if sb.first() != Some(&b'%') || sb.len() < 2 {
        return Err(format!("scalars: malformed format spec `{spec}`"));
    }
    let conv = sb[sb.len() - 1] as char;
    let mid = &sb[1..sb.len() - 1];

    let (mut left, mut zero, mut plus, mut space) = (false, false, false, false);
    let mut j = 0;
    while j < mid.len() && matches!(mid[j], b'-' | b'0' | b'+' | b' ' | b'#' | b',') {
        match mid[j] {
            b'-' => left = true,
            b'0' => zero = true,
            b'+' => plus = true,
            b' ' => space = true,
            _ => {}
        }
        j += 1;
    }
    let mut width = 0usize;
    while j < mid.len() && mid[j].is_ascii_digit() {
        width = width * 10 + (mid[j] - b'0') as usize;
        j += 1;
    }
    let mut prec: Option<usize> = None;
    if j < mid.len() && mid[j] == b'.' {
        j += 1;
        let mut p = 0usize;
        while j < mid.len() && mid[j].is_ascii_digit() {
            p = p * 10 + (mid[j] - b'0') as usize;
            j += 1;
        }
        prec = Some(p);
    }

    match conv {
        's' | 'S' => {
            let mut s = scala_str(v);
            if let Some(p) = prec {
                s = s.chars().take(p).collect();
            }
            if conv == 'S' {
                s = s.to_uppercase();
            }
            Ok(pad_str(s, left, width))
        }
        'd' => {
            let n = v.to_int();
            let digits = (n as i128).unsigned_abs().to_string();
            Ok(pad_num(digits, n < 0, left, zero, plus, space, width))
        }
        'f' => {
            let x = v.to_float();
            let p = prec.unwrap_or(6);
            let digits = format!("{:.*}", p, x.abs());
            Ok(pad_num(digits, x < 0.0, left, zero, plus, space, width))
        }
        // Radix conversions. Rust's `{:x}`/`{:X}`/`{:o}` on a signed integer
        // format the two's-complement bit pattern with no sign — identical to
        // Java for non-negative values; a negative value renders as 64-bit
        // (Scala `Long`) two's complement (this frontend models every integer as
        // `i64`, so it cannot pick Java's 32-bit `Int` width).
        'x' | 'X' | 'o' => {
            let n = v.to_int();
            let body = match conv {
                'x' => format!("{n:x}"),
                'X' => format!("{n:X}"),
                _ => format!("{n:o}"),
            };
            Ok(pad_num(body, false, left, zero, false, false, width))
        }
        'b' | 'B' => {
            let truthy = match v {
                Value::Bool(b) => *b,
                Value::Undef => false,
                _ => true,
            };
            let mut s = if truthy { "true" } else { "false" }.to_string();
            if conv == 'B' {
                s = s.to_uppercase();
            }
            Ok(pad_str(s, left, width))
        }
        'c' => {
            let ch = match v {
                Value::Str(s) => s.chars().next().unwrap_or('\0'),
                _ => char::from_u32(v.to_int() as u32).unwrap_or('\u{fffd}'),
            };
            Ok(pad_str(ch.to_string(), left, width))
        }
        other => Err(format!("scalars: unsupported format conversion `%{other}`")),
    }
}

/// Pad a string body to `width` (left- or right-justified with spaces).
fn pad_str(s: String, left: bool, width: usize) -> String {
    let len = s.chars().count();
    if len >= width {
        return s;
    }
    let padn = width - len;
    if left {
        format!("{s}{}", " ".repeat(padn))
    } else {
        format!("{}{s}", " ".repeat(padn))
    }
}

/// Pad a numeric body (`digits`, already sign-stripped) to `width`, applying the
/// sign and the `0`/`-` flags (zero-pad goes between sign and digits).
fn pad_num(
    digits: String,
    negative: bool,
    left: bool,
    zero: bool,
    plus: bool,
    space: bool,
    width: usize,
) -> String {
    let sign = if negative {
        "-"
    } else if plus {
        "+"
    } else if space {
        " "
    } else {
        ""
    };
    let total = sign.len() + digits.len();
    if total >= width {
        return format!("{sign}{digits}");
    }
    let padn = width - total;
    if left {
        format!("{sign}{digits}{}", " ".repeat(padn))
    } else if zero {
        format!("{sign}{}{digits}", "0".repeat(padn))
    } else {
        format!("{}{sign}{digits}", " ".repeat(padn))
    }
}

/// `SMETHOD` builtin: universal postfix method dispatch. The stack holds
/// `[recv, arg0 .. arg{k-1}, name]` (name on top) with `argc == k + 2`. Pops the
/// name, the `k` arguments, and the receiver, then routes to a small faithful
/// slice of the Scala `String`/`Int`/`Double`/`Any` stdlib.
///
/// Unknown method / arity / receiver-type combinations halt the VM with a
/// message parked for the runner — the frontend has no exception machinery, so
/// an unresolved call aborts like an uncaught error rather than guessing.
fn b_method(vm: &mut VM, argc: u8) -> Value {
    let name = vm.pop().as_str_cow().into_owned();
    let k = (argc as usize).saturating_sub(2);
    let mut args = Vec::with_capacity(k);
    for _ in 0..k {
        args.push(vm.pop());
    }
    args.reverse();
    let recv = vm.pop();

    match dispatch_method(&recv, &name, &args) {
        Ok(v) => v,
        Err(e) => fault(vm, e),
    }
}

/// Resolve `recv.name(args)` against the wired stdlib, or return the Scala-style
/// error message for an unresolved call. Kept host-only (no VM handle) so it is
/// straightforward to unit-test.
fn dispatch_method(recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    // `toString` is defined on every value (Scala's `Any.toString`).
    if name == "toString" && args.is_empty() {
        return Ok(Value::str(scala_str(recv)));
    }
    match recv {
        Value::Str(s) => string_method(s, name, args),
        Value::Int(n) => int_method(*n, name, args),
        Value::Float(f) => double_method(*f, name, args),
        _ => Err(no_such_method(recv, name)),
    }
}

/// `String` methods (a faithful subset of `java.lang.String` / Scala
/// `StringOps`). Lengths/indices are in `char`s — matching Scala for the BMP
/// text this frontend handles.
fn string_method(s: &str, name: &str, args: &[Value]) -> Result<Value, String> {
    let arity_err = || format!("scalars: String.{name}: wrong number of arguments");
    match (name, args.len()) {
        ("length" | "size", 0) => Ok(Value::int(s.chars().count() as i64)),
        ("isEmpty", 0) => Ok(Value::bool(s.is_empty())),
        ("nonEmpty", 0) => Ok(Value::bool(!s.is_empty())),
        ("toUpperCase", 0) => Ok(Value::str(s.to_uppercase())),
        ("toLowerCase", 0) => Ok(Value::str(s.to_lowercase())),
        ("trim", 0) => Ok(Value::str(s.trim())),
        ("reverse", 0) => Ok(Value::str(s.chars().rev().collect::<String>())),
        ("toInt", 0) => s.trim().parse::<i64>().map(Value::int).map_err(|_| {
            format!("scalars: java.lang.NumberFormatException: For input string: \"{s}\"")
        }),
        ("toDouble", 0) => s.trim().parse::<f64>().map(Value::float).map_err(|_| {
            format!("scalars: java.lang.NumberFormatException: For input string: \"{s}\"")
        }),
        ("charAt", 1) => {
            let i = args[0].to_int();
            let chars: Vec<char> = s.chars().collect();
            if i < 0 || i as usize >= chars.len() {
                Err(format!(
                    "scalars: java.lang.StringIndexOutOfBoundsException: index {i}, length {}",
                    chars.len()
                ))
            } else {
                Ok(Value::str(chars[i as usize].to_string()))
            }
        }
        ("contains", 1) => Ok(Value::bool(s.contains(&*args[0].as_str_cow()))),
        ("startsWith", 1) => Ok(Value::bool(s.starts_with(&*args[0].as_str_cow()))),
        ("endsWith", 1) => Ok(Value::bool(s.ends_with(&*args[0].as_str_cow()))),
        ("substring", 1) => substring(s, args[0].to_int(), s.chars().count() as i64),
        ("substring", 2) => substring(s, args[0].to_int(), args[1].to_int()),
        // A recognized name with the wrong arity is an arity error; an
        // unrecognized name is "no such method".
        (
            "length" | "size" | "isEmpty" | "nonEmpty" | "toUpperCase" | "toLowerCase" | "trim"
            | "reverse" | "toInt" | "toDouble" | "charAt" | "contains" | "startsWith" | "endsWith"
            | "substring",
            _,
        ) => Err(arity_err()),
        _ => Err(no_such_method(&Value::str(s), name)),
    }
}

/// Scala/Java `String.substring(begin, end)` — a half-open `char` slice that
/// throws `StringIndexOutOfBoundsException` for an out-of-range or inverted range.
fn substring(s: &str, begin: i64, end: i64) -> Result<Value, String> {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len() as i64;
    if begin < 0 || end > len || begin > end {
        return Err(format!(
            "scalars: java.lang.StringIndexOutOfBoundsException: begin {begin}, end {end}, length {len}"
        ));
    }
    Ok(Value::str(
        chars[begin as usize..end as usize]
            .iter()
            .collect::<String>(),
    ))
}

/// `Int` methods (a faithful subset of `scala.Int` / `RichInt`).
fn int_method(n: i64, name: &str, args: &[Value]) -> Result<Value, String> {
    match (name, args.len()) {
        ("abs", 0) => Ok(Value::int(n.wrapping_abs())),
        ("toDouble" | "toFloat", 0) => Ok(Value::float(n as f64)),
        ("toInt" | "toLong", 0) => Ok(Value::int(n)),
        ("max", 1) => Ok(Value::int(n.max(args[0].to_int()))),
        ("min", 1) => Ok(Value::int(n.min(args[0].to_int()))),
        ("abs" | "toDouble" | "toFloat" | "toInt" | "toLong" | "max" | "min", _) => {
            Err(format!("scalars: Int.{name}: wrong number of arguments"))
        }
        _ => Err(no_such_method(&Value::int(n), name)),
    }
}

/// `Double` methods (a faithful subset of `scala.Double` / `RichDouble`).
fn double_method(f: f64, name: &str, args: &[Value]) -> Result<Value, String> {
    match (name, args.len()) {
        ("abs", 0) => Ok(Value::float(f.abs())),
        ("toInt" | "toLong", 0) => Ok(Value::int(f as i64)),
        ("toDouble" | "toFloat", 0) => Ok(Value::float(f)),
        ("isNaN", 0) => Ok(Value::bool(f.is_nan())),
        ("isInfinity" | "isInfinite", 0) => Ok(Value::bool(f.is_infinite())),
        ("round", 0) => Ok(Value::int(f.round() as i64)),
        (
            "abs" | "toInt" | "toLong" | "toDouble" | "toFloat" | "isNaN" | "isInfinity"
            | "isInfinite" | "round",
            _,
        ) => Err(format!("scalars: Double.{name}: wrong number of arguments")),
        _ => Err(no_such_method(&Value::float(f), name)),
    }
}

/// The Scala compile-error a bad member access resembles (a `value … is not a
/// member of …`). slice-1 resolves methods at runtime, so it surfaces here.
fn no_such_method(recv: &Value, name: &str) -> String {
    let ty = match recv {
        Value::Str(_) => "String",
        Value::Int(_) => "Int",
        Value::Float(_) => "Double",
        Value::Bool(_) => "Boolean",
        Value::Undef => "Null",
        _ => "value",
    };
    format!("scalars: value {name} is not a member of {ty}")
}

/// `FFI_COMPILE` builtin: pop the base64-encoded `rust { ... }` block body and
/// compile + register it via `fusevm::ffi`. Returns `Unit`; a compile error
/// halts the VM with the message parked for the runner.
fn b_ffi_compile(vm: &mut VM, _argc: u8) -> Value {
    let body = vm.pop();
    let b64 = body.as_str_cow().into_owned();
    if let Err(e) = fusevm::ffi::compile_and_register(&b64) {
        return fault(vm, format!("rust {{}} block: {e}"));
    }
    Value::Undef
}

/// `FFI_CALL` builtin: the stack holds `[arg0 .. arg{n-1}, name]` with the
/// function name on top and `argc == n + 1`. Pop the name, pop the args, and
/// dispatch through `fusevm::ffi::try_call`, returning its result.
fn b_ffi_call(vm: &mut VM, argc: u8) -> Value {
    let name = vm.pop().as_str_cow().into_owned();
    let n = (argc as usize).saturating_sub(1);
    let mut args = Vec::with_capacity(n);
    for _ in 0..n {
        args.push(vm.pop());
    }
    args.reverse();
    match fusevm::ffi::try_call(&name, &args) {
        Some(Ok(v)) => v,
        Some(Err(e)) => fault(vm, format!("rust FFI call `{name}`: {e}")),
        None => fault(vm, format!("scalars: not found: {name}")),
    }
}

/// Scala `/` builtin. fusevm's native `Op::Div` is *always* floating (`7 / 2`
/// would be `3.5`), but Scala's `/` truncates when both operands are `Int`
/// (`7 / 2 == 3`) and only floats when an operand is a `Double`. Because slice 1
/// carries no static types, the choice is made at runtime here: both `Int` →
/// truncating integer division (toward zero, like Scala/Java); otherwise a
/// double divide (so `7 / 2.0 == 3.5`, `1.0 / 0.0 == Infinity`).
///
/// Integer division by zero throws `java.lang.ArithmeticException: / by zero` in
/// Scala (a JVM `idiv`/`irem` trap). slice 1 has no `try`/`catch`, so an
/// uncaught throw halts the VM with that exact message parked for the runner
/// (surfaced as `scalars: java.lang.ArithmeticException: / by zero`), matching an
/// uncaught exception aborting `scala`. A `wrapping_div` avoids the
/// `i64::MIN / -1` overflow panic. Floating-point `/ 0.0` is NOT an error in
/// Scala/IEEE-754 — it yields `Infinity`/`NaN` — so it stays on the float path.
fn b_div(vm: &mut VM, _argc: u8) -> Value {
    let b = vm.stack.pop().unwrap_or(Value::Undef);
    let a = vm.stack.pop().unwrap_or(Value::Undef);
    match (&a, &b) {
        (Value::Int(x), Value::Int(y)) => {
            if *y == 0 {
                fault(vm, "java.lang.ArithmeticException: / by zero")
            } else {
                Value::int(x.wrapping_div(*y))
            }
        }
        _ => Value::float(a.to_float() / b.to_float()),
    }
}

/// Predef `println` builtin: pop `argc` values (0 or 1 in slice 1), print them
/// Scala-formatted followed by a newline, and return `Unit`/`null`.
fn b_println(vm: &mut VM, argc: u8) -> Value {
    print_args(vm, argc, true)
}

/// Predef `print` builtin: as [`b_println`] but with no trailing newline.
fn b_print(vm: &mut VM, argc: u8) -> Value {
    print_args(vm, argc, false)
}

fn print_args(vm: &mut VM, argc: u8, newline: bool) -> Value {
    use std::io::Write;
    // Pop the args (pushed left-to-right, so the last is on top) and restore
    // source order.
    let mut vals = Vec::with_capacity(argc as usize);
    for _ in 0..argc {
        vals.push(vm.stack.pop().unwrap_or(Value::Undef));
    }
    vals.reverse();
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    for v in &vals {
        let _ = write!(lock, "{}", scala_str(v));
    }
    if newline {
        let _ = writeln!(lock);
    }
    // `println`/`print` return `Unit`; the CallBuiltin result is discarded by a
    // trailing Pop in statement position.
    Value::Undef
}

/// Render a value with Scala's `String.valueOf`/`println` rules (as opposed to
/// fusevm's shell-flavoured `as_str_cow`): booleans as `true`/`false`, whole
/// doubles with a trailing `.0`, `Undef` (a `null` literal) as `null`.
pub fn scala_str(v: &Value) -> String {
    match v {
        Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        Value::Float(f) => format_double(*f),
        Value::Undef => "null".to_string(),
        // The only arrays this frontend produces are `for … yield` results, whose
        // static type over a range generator is `IndexedSeq`/`Vector`; render them
        // as Scala's `Vector(e0, e1, …)` (elements via `toString`, unquoted).
        Value::Array(items) => {
            let inner = items.iter().map(scala_str).collect::<Vec<_>>().join(", ");
            format!("Vector({inner})")
        }
        other => other.as_str_cow().into_owned(),
    }
}

/// Scala's `Double.toString` (Java's) rendering, ported faithfully.
///
/// Non-finite values print `NaN`/`Infinity`/`-Infinity`; `0.0`/`-0.0` keep their
/// sign. Otherwise Java chooses notation by magnitude: **decimal** when
/// `1e-3 <= |x| < 1e7` (`0.001`, `123.456`, `9999999.0`), **computerized
/// scientific** otherwise (`1.0E7`, `1.23456789E8`, `1.0E-4`, `1.5E300`). Both
/// forms carry the shortest round-tripping digit string and always keep at least
/// one fractional digit.
///
/// The shortest digits and the base-10 exponent come from Rust's `{:e}` (which,
/// like Java, emits the shortest representation that round-trips) normalized to a
/// single leading digit; only the *placement* (plain vs `E`-notation) is Java's.
fn format_double(f: f64) -> String {
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f.is_infinite() {
        return if f < 0.0 { "-Infinity" } else { "Infinity" }.to_string();
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0.0" } else { "0.0" }.to_string();
    }
    let neg = f < 0.0;
    let m = f.abs();
    // `{:e}` → "d[.ddd]e<exp>" with one leading digit and a clean base-10 exponent.
    let sci = format!("{m:e}");
    let (mant, exp_str) = sci.split_once('e').expect("`{:e}` always contains `e`");
    let exp: i32 = exp_str.parse().expect("`{:e}` exponent is an integer");

    let body = if (-3..=6).contains(&exp) {
        // Decimal notation. The plain shortest form matches Java's digits in this
        // range; just guarantee a fractional digit (`5` → `5.0`).
        let plain = format!("{m}");
        if plain.contains('.') {
            plain
        } else {
            format!("{plain}.0")
        }
    } else {
        // Scientific notation: `mantissa` (with a fractional digit) + `E` + exp.
        let mant = if mant.contains('.') {
            mant.to_string()
        } else {
            format!("{mant}.0")
        };
        format!("{mant}E{exp}")
    };
    if neg {
        format!("-{body}")
    } else {
        body
    }
}

/// Strict numeric hook: fusevm calls this only for an operation with a
/// non-numeric operand. In slice 1 that is Scala's `String` `+` overload plus
/// value comparisons against strings; all-numeric arithmetic never reaches here
/// (it stays on the native fast path and the JIT).
pub fn numeric_hook(op: NumOp, a: &Value, b: &Value) -> Result<Value, String> {
    match op {
        // Scala 3 `+` on a mixed operand. Unlike Scala 2, there is NO universal
        // `any2stringadd`, so `+` is defined only two ways when a non-numeric
        // operand is involved:
        //   * `String.+(Any)`   — a String *left* operand concatenates anything.
        //   * numeric `+(String)` — `Int`/`Double`/… define `+(String): String`,
        //     so a numeric left operand concatenates a String *right* operand.
        // Everything else (`Boolean`/`null`/… on the left, or numeric `+` a
        // non-String) has no `+` method and is a compile error in Scala 3; reject
        // it rather than silently concatenating. (The hook only fires when an
        // operand is non-numeric, so a numeric `a` here implies a non-numeric `b`.)
        NumOp::Add => match a {
            Value::Str(_) => Ok(Value::str(format!("{}{}", scala_str(a), scala_str(b)))),
            Value::Int(_) | Value::Float(_) if matches!(b, Value::Str(_)) => {
                Ok(Value::str(format!("{}{}", scala_str(a), scala_str(b))))
            }
            _ => Err(format!(
                "scalars: `+` is not defined between `{}` and `{}`",
                scala_str(a),
                scala_str(b)
            )),
        },
        // Value equality/ordering against a string operand (Scala's `==` is
        // structural `equals`, so string `==` compares by content).
        NumOp::Eq => Ok(Value::bool(scala_str(a) == scala_str(b))),
        NumOp::Ne => Ok(Value::bool(scala_str(a) != scala_str(b))),
        NumOp::Lt => Ok(Value::bool(scala_str(a) < scala_str(b))),
        NumOp::Gt => Ok(Value::bool(scala_str(a) > scala_str(b))),
        NumOp::Le => Ok(Value::bool(scala_str(a) <= scala_str(b))),
        NumOp::Ge => Ok(Value::bool(scala_str(a) >= scala_str(b))),
        // Arithmetic other than `+` on a non-numeric operand is a type error in
        // Scala (`"a" - 1` does not compile). Report it rather than coercing.
        NumOp::Sub | NumOp::Mul | NumOp::Div | NumOp::Mod | NumOp::Pow => Err(format!(
            "scalars: operator `{op:?}` is not defined for operands `{}` and `{}`",
            scala_str(a),
            scala_str(b)
        )),
        NumOp::Neg => Err(format!(
            "scalars: unary `-` is not defined for `{}`",
            scala_str(a)
        )),
    }
}
