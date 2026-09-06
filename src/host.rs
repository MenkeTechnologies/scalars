//! The scalars host: builtin registration, Scala value formatting, and the
//! strict numeric hook.
//!
//! The primitives ride the fusevm value model directly; everything with
//! structure — class and `case class` instances, collections, tuples, function
//! values, throwables — lives in the frontend-owned object heap behind fusevm's
//! opaque `Value::Obj` handle (see the `HeapVal` arena below). Two places also
//! need Scala semantics that fusevm's default awk/shell flavour does not
//! provide:
//!
//! 1. **Printing.** fusevm's native `PrintLn` renders values shell-style
//!    (`true`→`1`, `3.0`→`3`). Predef's `println`/`print` instead lower to a
//!    registered builtin ([`SPRINTLN`]/[`SPRINT`]) that formats through
//!    [`scala_str`] — `true`/`false`, `3.0`, `null` — matching `scala`.
//! 2. **`+` overloading.** Scala's `+` is string concatenation when either
//!    operand is a `String` (via `Predef.any2stringadd` / `String.+`). fusevm
//!    runs *strict* once a numeric hook is installed, delegating any operation
//!    with a non-numeric operand to [`numeric_hook`], where `+` concatenates via
//!    the same [`scala_str`]. Strict mode also delegates the two numeric cases
//!    an `f64` cannot answer exactly — integer overflow, and a mixed
//!    `Int`/`Float` pair whose integer is past `2^53` — which [`numeric_hook`]
//!    answers with Scala's own rules (`Long` wraps; a mixed pair promotes).

use fusevm::{Frame, NumOp, VMResult, Value, VM};
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

/// Builtin id for Predef `println` (one Scala-formatted arg + newline).
pub const SPRINTLN: u16 = 700;
/// Builtin id for Predef `print` (one Scala-formatted arg, no newline).
pub const SPRINT: u16 = 701;
/// Builtin id for Scala `/` (type-dispatching division — see `b_div`).
pub const SDIV: u16 = 702;
/// Builtin id for Scala `%` (type-dispatching remainder — see `b_mod`).
pub const SMOD: u16 = 771;
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
/// `b_method`.
pub const SMETHOD: u16 = 706;
/// Builtin id for `f"…"`-interpolator formatting. The stack holds the value
/// (deepest) and the format spec (a `Str`, on top); `argc` is 2. Formats through
/// the Java-`Formatter` subset in `format_one`.
pub const SFORMAT: u16 = 707;
/// Builtin id for a `match` typed-pattern runtime type test (`case x: String`).
/// The stack holds the value (deepest) and the type name (a `Str`, on top);
/// `argc` is 2. Returns a `Bool`.
pub const SISTYPE: u16 = 708;
/// Builtin id for a non-exhaustive `match` fall-through. Pops the unmatched
/// scrutinee and faults with `scala.MatchError`, matching an uncaught throw.
pub const SMATCHERR: u16 = 709;
/// Builtin id for constructing a host-heap object (a `class`/`case class`
/// instance, `object` singleton, or the built-in `Some`/`None`). The stack holds
/// the ordered constructor-argument values (deepest first), then the class-name
/// `Str`, the comma-separated field-name list `Str` (empty for a no-field
/// object), an `is_case` `Bool`, and an `is_object` `Bool` on top; `argc` is
/// field-count + 4. Returns a `Value::Obj` handle into the frontend heap (see
/// `ScalaObj`). `is_object` selects singleton `toString` (`None`, not `None()`).
pub const OBJ_NEW: u16 = 710;
/// Builtin id for reading a heap object's class name (for runtime method
/// dispatch and constructor-pattern class tests). Pops one value; returns its
/// class name `Str`, or `""` for a non-object (so the dispatch fallthrough can
/// route non-`Obj` receivers to [`SMETHOD`]).
pub const OBJ_CLASS: u16 = 711;
/// Builtin id for `case class` `copy(...)`. The stack holds the receiver
/// (deepest), the comma-separated update spec `Str` (each token is a field name,
/// or `#<index>` for a positional update), then the update values; `argc` is
/// update-count + 2. Clones the receiver's ordered record with the named/indexed
/// fields overwritten and returns a fresh `Value::Obj`.
pub const OBJ_COPY: u16 = 712;
/// Builtin id for mutating a heap object's field in place (a `var` field
/// assignment inside a method). The stack holds the receiver (deepest), the
/// field-name `Str`, and the new value on top; `argc` is 3. Returns `Unit`.
pub const OBJ_SET: u16 = 713;
/// Builtin id for building a first-class function value (a lambda). The stack
/// holds the closure body's name-pool index, its parameter count, and whether
/// its result is always a pair literal (three integers), then the captured
/// upvalue values (deepest first); `argc` is capture-count + 3. Registers a heap
/// `Closure` and returns its `Value::Obj` handle (invoked later via
/// `invoke_closure`).
pub const MAKE_CLOSURE: u16 = 714;
/// Builtin id for `apply` on a heap value — the universal `receiver(args)`
/// dispatch. The stack holds the receiver (deepest) and the `argc` args on top.
/// A closure is invoked; a `List` is indexed; a `Map` is keyed; a `Tuple` is
/// indexed (`t(0)`). Faults for a non-applyable receiver.
pub const APPLY: u16 = 715;
/// Builtin id for a `List(...)` literal. The stack holds the `argc` element
/// values (deepest first); returns a host-heap `List` handle.
pub const MAKE_LIST: u16 = 716;
/// Builtin id for a `Map(...)` literal. The stack holds the `argc` `Tuple2`
/// key/value pair values (deepest first); returns an insertion-ordered host-heap
/// `Map` handle (a duplicate key keeps its first position with the last value).
pub const MAKE_MAP: u16 = 717;
/// Builtin id for a tuple literal (`(a, b)`, `a -> b`). The stack holds the
/// `argc` element values (deepest first); returns a host-heap `Tuple` handle.
pub const MAKE_TUPLE: u16 = 718;
/// Builtin id for `::` cons. The stack holds the head value (deepest) and the
/// tail `List` on top; `argc` is 2. Returns a new `List` with `head` prepended.
pub const LIST_CONS: u16 = 719;
/// Builtin id for materializing an integer range as a `Vector` (a range
/// generator inside a desugared collection for-comprehension). The stack holds
/// `start`, `end`, an `inclusive` `Bool`, and the `by` step (top); `argc` is 4.
pub const RANGE_LIST: u16 = 720;

// ── exception builtins (`try`/`catch`/`finally`/`throw`) ────────────────────
//
// See the `Exception unwinding` section below for the protocol these implement.

/// Builtin id for constructing a built-in throwable (`new RuntimeException(m)`).
/// The stack holds the fully-qualified class name (deepest) and the message
/// (top; `Undef` for the no-arg constructor); `argc` is 2.
pub const EXC_NEW: u16 = 721;
/// Builtin id for `throw e`. Pops the thrown value and makes it the in-flight
/// exception; halts the run when no `try` is dynamically active.
pub const EXC_THROW: u16 = 722;
/// Builtin id for the unwind check the compiler emits after each statement:
/// takes no arguments and returns a `Bool` — `true` while an exception is in
/// flight.
pub const EXC_PENDING: u16 = 723;
/// Builtin id for a `catch` clause type test. Pops the caught type's simple name
/// and returns a `Bool` for whether the in-flight exception is an instance of
/// it (walking the JVM throwable hierarchy); `argc` is 1. Does not consume the
/// exception — the arm binds it with [`EXC_TAKE`] only after matching.
pub const EXC_MATCH: u16 = 724;
/// Builtin id for consuming the in-flight exception once a `catch` arm matched:
/// returns it and clears the in-flight slot, so the handler body runs normally.
pub const EXC_TAKE: u16 = 725;
/// Builtin id for entering a `try` region (increments the dynamic try depth, so
/// `fault` raises instead of halting).
pub const EXC_ENTER: u16 = 726;
/// Builtin id for leaving a `try` region (decrements the dynamic try depth).
pub const EXC_EXIT: u16 = 727;
/// Builtin id for parking the in-flight exception across a `finally` body, so
/// the finalizer's own statements are not immediately unwound.
pub const EXC_STASH: u16 = 728;
/// Builtin id for restoring the exception parked by [`EXC_STASH`]. An exception
/// raised *by* the finalizer wins over the parked one, matching Scala/the JVM.
pub const EXC_UNSTASH: u16 = 729;
/// Builtin id for turning an uncaught in-flight exception into the terminal
/// error that stops the run (the top-level arm of the unwind check).
pub const EXC_ABORT: u16 = 730;
/// Builtin id for putting an exception back in flight after a `catch` arm's
/// guard rejected it, so the remaining arms (and the enclosing `try`) still see
/// it. Unlike [`EXC_THROW`] this never halts — the value came from an arm that
/// had already caught it.
pub const EXC_RESTORE: u16 = 731;
/// Builtin id for registering one declared type's shape, emitted once per
/// `class`/`trait`/`object` before `main`. Pops the primary-constructor arity,
/// the comma-separated *linearized* supertype names, and the type name.
///
/// The runtime needs two things the flat record cannot carry: which supertypes
/// an instance conforms to (`case s: Shape`, `x.isInstanceOf`), and how many of
/// a `case class`'s fields came from its primary constructor — Scala's derived
/// `toString`/`equals`/`hashCode` cover exactly those, not the extra `val`s the
/// body declares.
pub const TYPE_REG: u16 = 732;
/// Builtin id for an `Array(a, b, c)` literal: pops `argc` elements and returns
/// the mutable array handle.
pub const MAKE_ARRAY: u16 = 733;
/// Builtin id for `new Array[T](n)`: pops the element-type name and the length,
/// and returns an array of `T`'s zero value (`0`, `0.0`, `false`, `null`).
pub const ARRAY_FILL: u16 = 734;
/// Builtin id for a first-class `Range`: pops the step, the `inclusive` flag,
/// the end and the start, and returns the range handle.
pub const MAKE_RANGE: u16 = 735;
/// Builtin id for a `scala.math` (`java.lang.Math`) member: pops the member
/// name and, beneath it, its arguments — the same shape as [`SMETHOD`].
pub const SMATH: u16 = 736;
/// Builtin id for a `Vector(...)` literal: pops `argc` elements and returns the
/// vector handle.
pub const MAKE_VECTOR: u16 = 737;
/// Build a `scala.collection.immutable.ArraySeq` from `argc` stacked elements.
/// This is the class a Scala varargs method sees for its repeated parameter, so
/// `def f(xs: Int*)` printing `xs` prints `ArraySeq(1, 2, 3)`.
pub const MAKE_ARRAYSEQ: u16 = 753;
/// Build a `scala.math.Ordering` — the value `sorted(ord)` / `max(ord)` /
/// `min(ord)` take. Every `Ordering.Int` / `Ordering.String` / … names the same
/// NATURAL order here, because `value_cmp` already compares each of those
/// types the way Scala's instance does; what the companion member selects in
/// Scala is the element TYPE, which a dynamically typed runtime does not need.
/// The only state an ordering carries is therefore its direction, which
/// `.reverse` flips.
pub const MAKE_ORDERING: u16 = 754;
/// Builtin id for a `Set(...)` literal: pops `argc` elements and returns the set
/// handle (duplicates dropped; five or more elements make a `HashSet`).
pub const MAKE_SET: u16 = 738;
/// Builtin id for a `PartialFunction` literal (`{ case … }`) — [`MAKE_CLOSURE`]
/// with one extra leading operand. The stack holds the `apply` body's name-pool
/// index, the parameter count, the pair-result flag, the `isDefinedAt` body's
/// name-pool index, then the captured upvalues; `argc` is capture-count + 4. Both
/// bodies share the parameter/capture layout, so one capture list serves them.
pub const MAKE_PARTIAL: u16 = 739;
/// Builtin id for a `mutable.ListBuffer(...)` literal: pops `argc` elements.
pub const MAKE_LISTBUFFER: u16 = 740;
/// Builtin id for a `mutable.ArrayBuffer(...)` literal: pops `argc` elements.
pub const MAKE_ARRAYBUFFER: u16 = 741;
/// Builtin id for a `mutable.Set(...)` literal: pops `argc` elements into a
/// `mutable.HashSet` sized as `HashSet.from` would size it for `argc` inputs.
pub const MAKE_MUTSET: u16 = 742;
/// Builtin id for a `mutable.Map(...)` literal: pops `argc` `Tuple2` pairs into
/// a `mutable.HashMap`, sized as `HashMap.from` would.
pub const MAKE_MUTMAP: u16 = 743;
/// Builtin id for a boxed-primitive / `String` companion member — pops the
/// `Module.member` name and, beneath it, its arguments (the [`SMATH`] shape).
/// The MODULE travels with the name because the two namespaces are disjoint:
/// `Int.MaxValue` is `scala.Int`'s and `Integer.parseInt` is
/// `java.lang.Integer`'s, and `Integer.toHexString(-1)` renders at 32 bits where
/// `java.lang.Long.toHexString(-1)` renders at 64.
pub const BOXED: u16 = 760;
/// Builtin id for a collection literal whose arguments are a varargs SPREAD
/// (`List(xs: _*)`, `mutable.Queue(xs: _*)`). Every collection factory is
/// varargs in Scala, so a spread is legal there too — but the element count is
/// only known at run time, which the fixed-`argc` `MAKE_*` builtins cannot
/// express. This pops the constructor name and then the source collection, and
/// builds the named collection from that collection's elements.
pub const FROM_SEQ: u16 = 762;
/// Builtin id for a `mutable.Queue(...)` literal: pops `argc` elements.
pub const MAKE_QUEUE: u16 = 763;
/// Builtin id for a `mutable.Stack(...)` literal: pops `argc` elements. The
/// factory's order is the stored order, so `Stack(1,2,3)` has `1` on top.
pub const MAKE_STACK: u16 = 764;
/// Builtin id for a `mutable.ArrayDeque(...)` literal: pops `argc` elements.
pub const MAKE_ARRAYDEQUE: u16 = 765;
/// Builtin id for a `StringBuilder` literal or `new StringBuilder(s)`: pops
/// `argc` values and appends each one's `String.valueOf` characters.
pub const MAKE_STRINGBUILDER: u16 = 766;
/// Builtin id for a `mutable.LinkedHashSet(...)` literal: pops `argc` elements
/// into an insertion-ordered set.
pub const MAKE_LINKEDSET: u16 = 767;
/// Builtin id for a `mutable.LinkedHashMap(...)` literal: pops `argc` `Tuple2`
/// pairs into an insertion-ordered map.
pub const MAKE_LINKEDMAP: u16 = 768;
/// Builtin id for a `mutable.PriorityQueue(...)` literal: pops `argc` elements,
/// appends them raw and then heapifies, which is exactly what Scala's builder
/// does and is why the stored order is neither the input's nor sorted (see
/// `heapify`).
pub const MAKE_PRIORITYQUEUE: u16 = 769;
/// Builtin id for an `Iterator(...)` literal: pops `argc` elements into a
/// consumable `SeqKind::Iterator`. `Iterator.empty` lowers to the same builtin
/// with no arguments.
pub const MAKE_ITERATOR: u16 = 772;
/// Builtin id for the run-time half of `x += e` / `x -= e`: pops one value and
/// answers whether it is a collection that mutates in place. `true` sends the
/// compiler-emitted branch to `x.+=(e)`, `false` to `x = x + e` — Scala makes
/// that choice statically from whether the receiver's type has a `+=` method.
pub const IS_GROWABLE: u16 = 744;
/// Builtin id for Scala `+` with an operand whose type the frontend could not
/// prove: pops the two operands and answers the same value `Op::Add` would,
/// except that a concatenation renders through [`scala_str_vm`] and so can run a
/// user `toString` override. `Op::Add`'s numeric hook is a plain
/// `Fn(NumOp, &Value, &Value)` with no VM to re-enter, which is the whole reason
/// this exists; see `Compiler::concat_add`.
pub const SADD: u16 = 770;
/// Builtin id for the `Option(x)` factory: pops one value and answers `None` for
/// `null` (fusevm's `Undef`), `Some(x)` for anything else.
pub const MAKE_OPTION: u16 = 745;
/// Builtin id for a NON-LOCAL return: pops the return value and parks a
/// `scala.runtime.NonLocalReturnControl` carrying it in the same in-flight slot
/// an exception uses, so the compiler-emitted unwind checks carry it out of the
/// closure (and through any enclosing `finally`) to the method that lexically
/// contains the `return`. This is Scala's own lowering — a `return` inside a
/// lambda, or one that has to run a `finally` on the way out, is a throw.
///
/// The parked value is a plain record, not a throwable, so [`EXC_MATCH`] never
/// matches it: no `catch` arm can intercept a non-local return.
pub const NLR_RAISE: u16 = 746;
/// Builtin id for the method-boundary half of [`NLR_RAISE`]: if the in-flight
/// value is a non-local return, clear it and push its payload; otherwise push
/// `Undef` and leave a real exception in flight to keep unwinding.
pub const NLR_TAKE: u16 = 747;
/// Builtin id for boxing a `var` that a closure mutates: pops the initial value
/// and answers a one-slot heap CELL holding it. A closure captures the cell
/// handle by value, so a write through it is visible in the frame that declared
/// the `var` — Scala's own lowering of a captured mutable local
/// (`scala.runtime.IntRef` and friends). Emitted only for the vars
/// `compiler::boxed_vars` finds; every other local keeps its plain frame slot.
pub const CELL_NEW: u16 = 748;
/// Builtin id for reading a boxed `var`: pops the cell handle, pushes its value.
pub const CELL_GET: u16 = 749;
/// Builtin id for writing a boxed `var`: pops the cell handle (top) and then the
/// value, stores it, and answers `Unit`.
pub const CELL_SET: u16 = 750;
/// Builtin id for `Char` construction: pops a code point and pushes the interned
/// `HeapVal::Char` handle for it (see [`make_char`]).
pub const CHAR_NEW: u16 = 751;
/// Builtin id for an extractor pattern whose name is a *value* rather than a
/// type — `case p(a, b)` where `p` is a `Regex`. Pops the expected group count,
/// the scrutinee and the extractor, and pushes either the bound values as an
/// `Array` or `Value::Undef` when the pattern does not match (see
/// `b_unapply_seq`).
pub const UNAPPLY_SEQ: u16 = 752;
/// Builtin id for reading one `@main` parameter off the command line. Pops the
/// declared type name and the argument index, and pushes the converted value.
///
/// This is Scala 3's `scala.util.CommandLineParser.parseArgument[T](args, i)`,
/// including its failure behaviour, which is unlike anything else in the
/// language: `CommandLineParser.showError` prints the diagnostic to **stdout**
/// and the process still exits **0**. Verified against Scala 3.8.4 / JDK 26.0.2
/// — `@main def go(i: Int, l: Long)` run with `zz` for `l` prints
/// `Illegal command line after first argument: java.lang.NumberFormatException:
/// For input string: "zz"` on stdout, `echo $?` answers 0.
pub const MAIN_ARG: u16 = 755;
/// Builtin id for the `args: Array[String]` parameter of a `def main` entry:
/// pushes the program arguments as an `Array` of `String`. Takes no operands.
pub const MAIN_ARGV: u16 = 756;

/// Round the top-of-stack number to 32-bit `Float` precision. `argc == 1`.
///
/// fusevm has one floating representation, so a `Float` here is a `Double`
/// *kept* at single precision — every place a value becomes one has to round.
/// This is that rounding: the `f` literal suffix, `.toFloat`, and a `Float`
/// parameter's incoming argument all go through it.
pub const SF32: u16 = 773;

/// One arithmetic operation performed at 32-bit `Float` width. Stack
/// `[lhs, rhs, op]` (`op` on top, one of the [`f32_op`] constants); `argc == 3`.
///
/// Rounding an `f64` result afterwards is a DIFFERENT computation: the double
/// rounding can land a ulp away from the single one Scala performs. `1.0f /
/// 3.0f` is `0.33333334`, and forming the quotient at 64 bits first gives
/// `0.3333333333333333`, whose nearest `f32` is the same — but
/// `16777217.0f * 0.2f` is `3355443.2` in Scala and `3355443.3` if the product
/// is formed in `f64` and narrowed after. So the operation itself is done in
/// `f32` throughout, which is why it costs a builtin rather than a native op.
pub const SF32_ARITH: u16 = 774;

/// `Float.toString` of the top-of-stack value. `argc == 1`.
///
/// A `Float` and a `Double` holding the same bits print differently: Scala's
/// shortest-round-trip decimal is computed against the TYPE's precision, so
/// `0.1f` prints `0.1` where the `Double` with those bits prints
/// `0.10000000149011612`. The value model cannot tell the two apart, so the
/// compiler emits this wherever a statically-`Float` value crosses into a
/// `String`.
pub const SF32_STR: u16 = 775;

/// `getClass` on a statically-`Float` receiver, which pops that receiver and
/// answers the `java.lang.Class` record for `float`. `argc == 1`.
///
/// The runtime value cannot tell a `Float` from the `Double` holding its bits,
/// so a `getClass` reaching the ordinary dispatch answers `double` for both.
/// Only the compiler knows which type the receiver had.
pub const SF32_CLASS: u16 = 776;

/// `hashCode` on a statically-`Float` receiver, which pops that receiver and
/// answers `java.lang.Float.floatToIntBits`. `argc == 1`.
///
/// A `Float` and the `Double` holding its bits hash differently — `3.0f` is
/// 1077936128 and `3.0` is 1074266112 — so this needs the static type for the
/// same reason [`SF32_CLASS`] and [`SF32_STR`] do.
pub const SF32_HASH: u16 = 777;

/// Widen the top-of-stack value to `Double` when it is a `Float`. `argc == 1`.
///
/// The mirror of [`SF32`]. Scala widens an argument to the declared type, and
/// `Float` to `Double` is a real conversion here because the two are distinct
/// runtime values: `val d: Double = 0.1f` holds `0.10000000149011612`, not the
/// `Float` `0.1`. Anything that is not a `Float` passes through untouched, so a
/// `Double`-declared position pays nothing for values that already are one.
pub const SF64: u16 = 778;

/// Wrap a zero-argument thunk as the UNFORCED state of a `lazy val`.
/// `argc == 1`; the result is stored in the binding's cell.
pub const LAZY_NEW: u16 = 779;

/// Read a `lazy val`'s cell, forcing it on the first read. Stack `[cell]`;
/// `argc == 1`.
///
/// A `lazy val` evaluates its initializer at the first READ and at most once,
/// and not at all if it is never read — all three of which are observable when
/// the initializer prints or throws. So the binding holds a thunk until the
/// first read replaces it, in the cell, with the value it produced.
pub const LAZY_FORCE: u16 = 780;

/// Build a `LazyList`. Stack `[arg…, kindTag]`; the tag selects which rule.
/// See `b_lazylist_new`, which reads the tag off the stack.
pub const LAZYLIST_NEW: u16 = 781;

/// `head #:: tail` — cons onto a `LazyList`, with the TAIL still a thunk.
/// Stack `[head, thunk]`; `argc == 2`.
///
/// The tail must stay unrun for a self-referential definition to work at all:
/// `val fibs = 0 #:: 1 #:: fibs.zip(fibs.tail)…` reads `fibs` inside its own
/// initializer, and only a thunk defers that read until after the binding
/// exists.
pub const LAZY_CONS: u16 = 782;

/// The [`SF32_ARITH`] operator codes, shared with the compiler.
pub mod f32_op {
    pub const ADD: i64 = 0;
    pub const SUB: i64 = 1;
    pub const MUL: i64 = 2;
    pub const DIV: i64 = 3;
    pub const REM: i64 = 4;
}

thread_local! {
    /// The program arguments after the script path, as
    /// [`crate::cli::Cli::argv`] captured them. Read by [`MAIN_ARG`] and
    /// [`MAIN_ARGV`]; empty unless the runner called [`set_argv`].
    static ARGV: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Hand the program arguments to the run. Called by the `scala` binary before
/// executing a file; a library `run_str` leaves them empty, which is what a
/// no-argument invocation sees.
pub fn set_argv(args: Vec<String>) {
    ARGV.with(|a| *a.borrow_mut() = args);
}

/// `MAIN_ARGV` builtin — see [`MAIN_ARGV`].
fn b_main_argv(_vm: &mut VM, _argc: u8) -> Value {
    let items: Vec<Value> = ARGV.with(|a| a.borrow().iter().map(Value::str).collect());
    new_seq(SeqKind::Array, items)
}

/// Scala's own wording for the position of the argument that could not be read,
/// as `CommandLineParser.showError` builds it: nothing for the first, the word
/// `first` for the second, a count for the rest.
fn arg_position(index: usize) -> String {
    match index {
        0 => String::new(),
        1 => " after first argument".to_string(),
        n => format!(" after {n} arguments"),
    }
}

/// Convert one command-line argument to a `@main` parameter's declared type,
/// answering the JDK exception text on failure. `Byte`/`Short` go through
/// `Integer.parseInt`'s range check, which reports differently from an
/// unparseable string.
fn parse_main_arg(text: &str, ty: &str) -> Result<Value, String> {
    let bad_num = || format!("java.lang.NumberFormatException: For input string: \"{text}\"");
    let out_of_range = || {
        format!("java.lang.NumberFormatException: Value out of range. Value:\"{text}\" Radix:10")
    };
    match ty {
        "String" => Ok(Value::str(text)),
        "Boolean" => match text {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err(format!(
                "java.lang.IllegalArgumentException: For input string: \"{text}\""
            )),
        },
        "Int" => text
            .parse::<i32>()
            .map(|v| Value::Int(v as i64))
            .map_err(|_| bad_num()),
        "Long" => text.parse::<i64>().map(Value::Int).map_err(|_| bad_num()),
        "Byte" | "Short" => {
            let n: i64 = text.parse().map_err(|_| bad_num())?;
            let (lo, hi) = if ty == "Byte" {
                (i64::from(i8::MIN), i64::from(i8::MAX))
            } else {
                (i64::from(i16::MIN), i64::from(i16::MAX))
            };
            if (lo..=hi).contains(&n) {
                Ok(Value::Int(n))
            } else {
                Err(out_of_range())
            }
        }
        "Double" => text.parse::<f64>().map(Value::Float).map_err(|_| bad_num()),
        other => Err(format!("scalars: no command-line reader for `{other}`")),
    }
}

/// `MAIN_ARG` builtin — see [`MAIN_ARG`].
fn b_main_arg(vm: &mut VM, _argc: u8) -> Value {
    let ty = vm.pop().as_str_cow().into_owned();
    let index = vm.pop().to_int().max(0) as usize;
    let arg = ARGV.with(|a| a.borrow().get(index).cloned());
    let outcome = match arg {
        None => Err("more arguments expected".to_string()),
        Some(text) => parse_main_arg(&text, &ty),
    };
    match outcome {
        Ok(v) => v,
        Err(cause) => {
            // `showError` writes to stdout and the exit status stays 0, so this
            // is a CLEAN halt rather than the `FFI_ERROR` path (which reports on
            // stderr and exits non-zero).
            println!("Illegal command line{}: {cause}", arg_position(index));
            vm.request_halt();
            Value::Undef
        }
    }
}

thread_local! {
    /// `type name → (linearized supertypes, primary-constructor arity)`,
    /// populated by [`TYPE_REG`] at the start of every run.
    static TYPES: RefCell<HashMap<String, TypeInfo>> = RefCell::new(HashMap::new());
}

/// One registered type's runtime shape (see [`TYPE_REG`]).
struct TypeInfo {
    /// Every supertype, nearest first, excluding the type itself.
    supers: Vec<String>,
    /// Primary-constructor field count — the prefix of `fields` that Scala's
    /// derived `case class` members operate on.
    ctor_arity: usize,
}

/// Clear the declared-type registry. Called by the runner before each program.
pub fn reset_types() {
    TYPES.with(|t| t.borrow_mut().clear());
}

/// `TYPE_REG` builtin — see [`TYPE_REG`]. Returns `Unit`.
fn b_type_reg(vm: &mut VM, _argc: u8) -> Value {
    let ctor_arity = vm.pop().to_int().max(0) as usize;
    let supers_csv = vm.pop().as_str_cow().into_owned();
    let name = vm.pop().as_str_cow().into_owned();
    let supers = if supers_csv.is_empty() {
        Vec::new()
    } else {
        supers_csv.split(',').map(|s| s.to_string()).collect()
    };
    TYPES.with(|t| t.borrow_mut().insert(name, TypeInfo { supers, ctor_arity }));
    Value::Undef
}

/// Whether `class` is `ty` or declares it as a supertype.
fn class_conforms(class: &str, ty: &str) -> bool {
    class == ty
        || TYPES.with(|t| {
            t.borrow()
                .get(class)
                .is_some_and(|i| i.supers.iter().any(|s| s == ty))
        })
}

/// How many of `class`'s fields Scala's derived `case class` members see. An
/// unregistered class (a built-in like `Some`) exposes all `total` of them.
fn ctor_arity(class: &str, total: usize) -> usize {
    TYPES
        .with(|t| t.borrow().get(class).map(|i| i.ctor_arity))
        .unwrap_or(total)
        .min(total)
}

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

/// Stop the run with `msg`, or — when a `try` is dynamically active and `msg`
/// names a JVM/Scala throwable — raise it as a catchable exception instead.
///
/// Every runtime error this host can raise flows through here, so routing the
/// throwable ones into [`raise`] is what makes `1 / 0` and `"x".toInt`
/// catchable without touching each call site. A *frontend-internal* fault (one
/// whose message is not a `java.…`/`scala.…` throwable) always halts: it is a
/// scalars bug, not a Scala exception, and swallowing it in a `catch` would hide
/// it.
fn fault(vm: &mut VM, msg: impl Into<String>) -> Value {
    let msg = msg.into();
    let bare = msg.strip_prefix("scalars: ").unwrap_or(&msg);
    if let Some(exc) = throwable_from_message(bare) {
        return raise(vm, exc);
    }
    FFI_ERROR.with(|e| *e.borrow_mut() = Some(msg));
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
    vm.register_builtin(SMOD, b_mod);
    vm.register_builtin(FFI_COMPILE, b_ffi_compile);
    vm.register_builtin(FFI_CALL, b_ffi_call);
    vm.register_builtin(SMETHOD, b_method);
    vm.register_builtin(SFORMAT, b_format);
    vm.register_builtin(SISTYPE, b_istype);
    vm.register_builtin(SMATCHERR, b_matcherr);
    vm.register_builtin(OBJ_NEW, b_obj_new);
    vm.register_builtin(OBJ_CLASS, b_obj_class);
    vm.register_builtin(OBJ_COPY, b_obj_copy);
    vm.register_builtin(OBJ_SET, b_obj_set);
    vm.register_builtin(MAKE_CLOSURE, b_make_closure);
    vm.register_builtin(MAKE_PARTIAL, b_make_partial);
    vm.register_builtin(APPLY, b_apply);
    vm.register_builtin(MAKE_LIST, b_make_list);
    vm.register_builtin(MAKE_MAP, b_make_map);
    vm.register_builtin(MAKE_TUPLE, b_make_tuple);
    vm.register_builtin(LIST_CONS, b_list_cons);
    vm.register_builtin(RANGE_LIST, b_range_list);
    vm.register_builtin(EXC_NEW, b_exc_new);
    vm.register_builtin(EXC_THROW, b_exc_throw);
    vm.register_builtin(EXC_PENDING, b_exc_pending);
    vm.register_builtin(EXC_MATCH, b_exc_match);
    vm.register_builtin(EXC_TAKE, b_exc_take);
    vm.register_builtin(EXC_ENTER, b_exc_enter);
    vm.register_builtin(EXC_EXIT, b_exc_exit);
    vm.register_builtin(EXC_STASH, b_exc_stash);
    vm.register_builtin(EXC_UNSTASH, b_exc_unstash);
    vm.register_builtin(EXC_ABORT, b_exc_abort);
    vm.register_builtin(EXC_RESTORE, b_exc_restore);
    vm.register_builtin(TYPE_REG, b_type_reg);
    vm.register_builtin(MAKE_ARRAY, b_make_array);
    vm.register_builtin(ARRAY_FILL, b_array_fill);
    vm.register_builtin(MAKE_RANGE, b_make_range);
    vm.register_builtin(SMATH, b_math);
    vm.register_builtin(MAKE_VECTOR, b_make_vector);
    vm.register_builtin(MAKE_ARRAYSEQ, b_make_arrayseq);
    vm.register_builtin(MAKE_ITERATOR, b_make_iterator);
    vm.register_builtin(MAKE_SET, b_make_set);
    vm.register_builtin(MAKE_LISTBUFFER, b_make_listbuffer);
    vm.register_builtin(MAKE_ARRAYBUFFER, b_make_arraybuffer);
    vm.register_builtin(MAKE_MUTSET, b_make_mutset);
    vm.register_builtin(MAKE_MUTMAP, b_make_mutmap);
    vm.register_builtin(BOXED, b_boxed);
    vm.register_builtin(MAKE_ORDERING, b_make_ordering);
    vm.register_builtin(FROM_SEQ, b_from_seq);
    vm.register_builtin(MAKE_QUEUE, b_make_queue);
    vm.register_builtin(MAKE_STACK, b_make_stack);
    vm.register_builtin(MAKE_ARRAYDEQUE, b_make_arraydeque);
    vm.register_builtin(MAKE_STRINGBUILDER, b_make_stringbuilder);
    vm.register_builtin(MAKE_LINKEDSET, b_make_linkedset);
    vm.register_builtin(MAKE_LINKEDMAP, b_make_linkedmap);
    vm.register_builtin(MAKE_PRIORITYQUEUE, b_make_priorityqueue);
    vm.register_builtin(IS_GROWABLE, b_is_growable);
    vm.register_builtin(SADD, b_add);
    vm.register_builtin(MAKE_OPTION, b_make_option);
    vm.register_builtin(NLR_RAISE, b_nlr_raise);
    vm.register_builtin(NLR_TAKE, b_nlr_take);
    vm.register_builtin(CELL_NEW, b_cell_new);
    vm.register_builtin(CELL_GET, b_cell_get);
    vm.register_builtin(CELL_SET, b_cell_set);
    vm.register_builtin(CHAR_NEW, b_char_new);
    vm.register_builtin(UNAPPLY_SEQ, b_unapply_seq);
    vm.register_builtin(MAIN_ARG, b_main_arg);
    vm.register_builtin(MAIN_ARGV, b_main_argv);
    vm.register_builtin(SF32, b_f32);
    vm.register_builtin(SF32_ARITH, b_f32_arith);
    vm.register_builtin(SF32_STR, b_f32_str);
    vm.register_builtin(SF32_CLASS, b_f32_class);
    vm.register_builtin(SF32_HASH, b_f32_hash);
    vm.register_builtin(SF64, b_f64);
    vm.register_builtin(LAZY_NEW, b_lazy_new);
    vm.register_builtin(LAZY_FORCE, b_lazy_force);
    vm.register_builtin(LAZYLIST_NEW, b_lazylist_new);
    vm.register_builtin(LAZY_CONS, b_lazy_cons);
}

// ── Exception unwinding ─────────────────────────────────────────────────────
//
// fusevm has no unwind opcode and scalars lowers `def`s to fusevm's *native*
// `Op::Call` frames, so a thrown exception cannot longjmp out of a frame the way
// a sub-chunk-interpreting frontend can. `try` is therefore implemented as a
// cooperative two-part protocol:
//
//   * **Runtime half (here).** A raise parks the exception in [`PENDING`]
//     instead of halting, provided a `try` is dynamically active ([`TRY_DEPTH`]
//     > 0). Every builtin with an observable side effect (printing, dispatch,
//     division) short-circuits while [`unwinding`] holds, so no output escapes
//     between the raise and its handler.
//   * **Compile-time half (`crate::compiler`).** When the program contains a
//     `try` at all, the compiler emits an `EXC_PENDING` test after every
//     statement; the innermost enclosing construct decides where a `true`
//     answer jumps — out of a loop, out of a `def` frame, into a `catch`
//     dispatch, or into the terminal abort at top level.
//
// The consequence is that unwinding is *statement-granular*: a raise mid-way
// through one statement finishes evaluating that statement's remaining operands
// (on garbage values, with side-effecting builtins suppressed) before control
// reaches the handler. A program with no `try` pays nothing — the checks are not
// emitted and a fault halts exactly as it did before.

thread_local! {
    /// The exception currently unwinding, if any.
    static PENDING: RefCell<Option<Value>> = const { RefCell::new(None) };
    /// How many `try` regions are dynamically active. Zero means a raise is
    /// uncatchable and halts the run immediately.
    static TRY_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// Exceptions parked across `finally` bodies (one entry per nested `finally`
    /// currently running).
    static STASH: RefCell<Vec<Option<Value>>> = const { RefCell::new(Vec::new()) };
}

/// Clear all exception state. Called by the runner before each program run so
/// one run's in-flight exception cannot leak into the next (the library
/// `run_str` path reuses the thread).
pub fn reset_exceptions() {
    PENDING.with(|p| *p.borrow_mut() = None);
    TRY_DEPTH.with(|d| d.set(0));
    STASH.with(|s| s.borrow_mut().clear());
}

/// True while an exception is in flight and has not yet reached its handler.
/// Side-effecting builtins check this and become no-ops so nothing is printed
/// (and nothing further faults) during the walk out to the `catch`.
fn unwinding() -> bool {
    PENDING.with(|p| p.borrow().is_some())
}

/// Make `exc` the in-flight exception. Inside a `try` this parks it for the
/// compiler-emitted unwind checks; outside one it is uncatchable and halts the
/// run with the JVM-style `class: message` text.
///
/// An exception raised *while already unwinding* is dropped: the first one wins,
/// matching the JVM (a second throw during unwinding cannot occur, since the
/// suppressed builtins never run).
fn raise(vm: &mut VM, exc: Value) -> Value {
    if unwinding() {
        return Value::Undef;
    }
    if TRY_DEPTH.with(|d| d.get()) > 0 {
        PENDING.with(|p| *p.borrow_mut() = Some(exc));
        return Value::Undef;
    }
    FFI_ERROR.with(|e| *e.borrow_mut() = Some(scala_str(&exc)));
    vm.request_halt();
    Value::Undef
}

/// Parse a `java.lang.Xxx: message` fault string into a throwable object, or
/// `None` when the message is a frontend-internal error rather than a Scala
/// exception.
///
/// The recognizer is deliberately narrow — a fully-qualified `java.`/`scala.`
/// name whose last segment ends in `Exception` or `Error` — so a scalars bug
/// ("`::` right operand is not a List") can never be silently swallowed by a
/// user's `catch`.
fn throwable_from_message(s: &str) -> Option<Value> {
    let (fqn, msg) = match s.split_once(": ") {
        Some((f, m)) => (f, Some(m)),
        None => (s, None),
    };
    let last = fqn.rsplit('.').next()?;
    let qualified = fqn.starts_with("java.") || fqn.starts_with("scala.");
    if !qualified || !(last.ends_with("Exception") || last.ends_with("Error")) {
        return None;
    }
    Some(new_throwable(fqn, msg))
}

/// Allocate a built-in throwable with the fully-qualified class name `fqn`.
fn new_throwable(fqn: &str, msg: Option<&str>) -> Value {
    heap_push(HeapVal::Exc(ExcObj {
        class: Arc::from(fqn),
        msg: msg.map(Arc::from),
    }))
}

/// The simple names of the built-in throwables scalars can construct or raise,
/// each mapped to its fully-qualified JVM name. `new X(…)` for a name in this
/// table lowers to [`EXC_NEW`] instead of a user-class constructor.
///
/// This is a table rather than a `java.lang.` prefix rule because the package
/// differs per class (`java.util.NoSuchElementException`, `scala.MatchError`)
/// and the package is observable through `toString`.
pub const BUILTIN_THROWABLES: &[(&str, &str)] = &[
    ("Throwable", "java.lang.Throwable"),
    ("Exception", "java.lang.Exception"),
    ("Error", "java.lang.Error"),
    ("RuntimeException", "java.lang.RuntimeException"),
    ("ArithmeticException", "java.lang.ArithmeticException"),
    (
        "IllegalArgumentException",
        "java.lang.IllegalArgumentException",
    ),
    ("IllegalStateException", "java.lang.IllegalStateException"),
    ("NumberFormatException", "java.lang.NumberFormatException"),
    (
        "IndexOutOfBoundsException",
        "java.lang.IndexOutOfBoundsException",
    ),
    (
        "StringIndexOutOfBoundsException",
        "java.lang.StringIndexOutOfBoundsException",
    ),
    (
        "ArrayIndexOutOfBoundsException",
        "java.lang.ArrayIndexOutOfBoundsException",
    ),
    ("NullPointerException", "java.lang.NullPointerException"),
    ("ClassCastException", "java.lang.ClassCastException"),
    (
        "UnsupportedOperationException",
        "java.lang.UnsupportedOperationException",
    ),
    ("NoSuchElementException", "java.util.NoSuchElementException"),
    ("MatchError", "scala.MatchError"),
    // Predef `assert`/`assume` raise this, and it is an `Error` rather than an
    // `Exception` — so `catch { case e: Exception }` does NOT catch one.
    ("AssertionError", "java.lang.AssertionError"),
    // `scala.util.control.Breaks`. `break()` raises a `BreakControl` and
    // `breakable { … }` catches it (see the desugar in [`crate::parser`]).
    // `ControlThrowable` is its supertype and NOT a subtype of `Exception`,
    // which is what makes a user's `catch { case e: Exception => … }` let a
    // `break` pass through to its enclosing `breakable` — the behavior a
    // `catch`-all handler would otherwise silently swallow.
    ("ControlThrowable", "scala.util.control.ControlThrowable"),
    ("BreakControl", "scala.util.control.BreakControl"),
];

/// The JVM throwable hierarchy scalars models, as `(class, superclass)` simple
/// names. `catch { case e: T }` matches when the thrown class is `T` or reaches
/// `T` by walking this chain — that is what makes `case e: Exception` catch an
/// `IllegalArgumentException`.
const THROWABLE_PARENTS: &[(&str, &str)] = &[
    ("Error", "Throwable"),
    ("Exception", "Throwable"),
    ("RuntimeException", "Exception"),
    ("InterruptedException", "Exception"),
    ("ArithmeticException", "RuntimeException"),
    ("IllegalArgumentException", "RuntimeException"),
    ("IllegalStateException", "RuntimeException"),
    ("NumberFormatException", "IllegalArgumentException"),
    ("IndexOutOfBoundsException", "RuntimeException"),
    (
        "StringIndexOutOfBoundsException",
        "IndexOutOfBoundsException",
    ),
    (
        "ArrayIndexOutOfBoundsException",
        "IndexOutOfBoundsException",
    ),
    ("NullPointerException", "RuntimeException"),
    ("ClassCastException", "RuntimeException"),
    ("UnsupportedOperationException", "RuntimeException"),
    ("NoSuchElementException", "RuntimeException"),
    ("MatchError", "RuntimeException"),
    ("AssertionError", "Error"),
    // Deliberately hangs off `Throwable`, not `Exception`: Scala's
    // `ControlThrowable` is a direct `Throwable` so control-flow signals are not
    // caught by ordinary `case e: Exception` handlers.
    ("ControlThrowable", "Throwable"),
    ("BreakControl", "ControlThrowable"),
];

/// The fully-qualified name of a built-in throwable's simple name.
pub fn throwable_fqn(simple: &str) -> Option<&'static str> {
    BUILTIN_THROWABLES
        .iter()
        .find(|(s, _)| *s == simple)
        .map(|(_, f)| *f)
}

/// Whether a thrown class (simple name) is an instance of the caught type
/// `want`, by walking [`THROWABLE_PARENTS`]. The chain is short and fixed, so a
/// linear walk beats any index.
fn throwable_is_a(thrown: &str, want: &str) -> bool {
    let mut cur = thrown;
    loop {
        if cur == want {
            return true;
        }
        match THROWABLE_PARENTS.iter().find(|(c, _)| *c == cur) {
            Some((_, parent)) => cur = parent,
            None => return false,
        }
    }
}

/// A built-in throwable instance behind a [`HeapVal::Exc`]. Kept separate from
/// [`ScalaObj`] because a throwable has no ordered field record and renders as
/// `fqn` / `fqn: message`, not `Class@hex`.
#[derive(Clone)]
struct ExcObj {
    /// The fully-qualified class name (`java.lang.RuntimeException`).
    class: Arc<str>,
    /// The constructor message, or `None` for the no-arg constructor (whose
    /// `getMessage` is Scala `null`).
    msg: Option<Arc<str>>,
}

/// Clone the throwable behind `v`, if it is one.
fn as_exc(v: &Value) -> Option<ExcObj> {
    if let Value::Obj(id) = v {
        HEAP.with(|h| match h.borrow().get(*id as usize) {
            Some(HeapVal::Exc(e)) => Some(e.clone()),
            _ => None,
        })
    } else {
        None
    }
}

/// A throwable's `toString`: `fqn` alone when the message is `null`, else
/// `fqn: message` (`java.lang.Throwable.toString`).
fn exc_to_string(e: &ExcObj) -> String {
    match &e.msg {
        Some(m) => format!("{}: {m}", e.class),
        None => e.class.to_string(),
    }
}

/// The runtime class name used for `catch` matching: a built-in throwable's
/// simple name, or a user class's own name (a user object thrown directly).
fn thrown_class(v: &Value) -> Option<String> {
    if let Some(e) = as_exc(v) {
        return Some(e.class.rsplit('.').next().unwrap_or(&e.class).to_string());
    }
    with_obj(v, |o| o.class.to_string())
}

/// `EXC_NEW` builtin — see [`EXC_NEW`].
fn b_exc_new(vm: &mut VM, _argc: u8) -> Value {
    let msg = vm.pop();
    let class = vm.pop().as_str_cow().into_owned();
    let msg = match msg {
        Value::Undef => None,
        other => Some(scala_str(&other)),
    };
    new_throwable(&class, msg.as_deref())
}

/// `EXC_THROW` builtin — see [`EXC_THROW`].
fn b_exc_throw(vm: &mut VM, _argc: u8) -> Value {
    let v = vm.pop();
    raise(vm, v)
}

/// `EXC_PENDING` builtin — see [`EXC_PENDING`].
fn b_exc_pending(_vm: &mut VM, _argc: u8) -> Value {
    Value::Bool(unwinding())
}

/// `EXC_MATCH` builtin — see [`EXC_MATCH`].
fn b_exc_match(vm: &mut VM, _argc: u8) -> Value {
    let want = vm.pop().as_str_cow().into_owned();
    let thrown = PENDING.with(|p| p.borrow().as_ref().and_then(thrown_class));
    Value::Bool(match thrown {
        // `case e: Throwable` (and a bare `case e`, which the compiler lowers to
        // the same test) catches everything, including a thrown user object that
        // is outside the modeled hierarchy.
        Some(_) if want == "Throwable" => true,
        Some(c) => throwable_is_a(&c, &want),
        None => false,
    })
}

/// `EXC_RESTORE` builtin — see [`EXC_RESTORE`].
fn b_exc_restore(vm: &mut VM, _argc: u8) -> Value {
    let v = vm.pop();
    PENDING.with(|p| *p.borrow_mut() = Some(v));
    Value::Undef
}

/// `EXC_TAKE` builtin — see [`EXC_TAKE`].
fn b_exc_take(_vm: &mut VM, _argc: u8) -> Value {
    PENDING
        .with(|p| p.borrow_mut().take())
        .unwrap_or(Value::Undef)
}

/// `EXC_ENTER` builtin — see [`EXC_ENTER`].
fn b_exc_enter(_vm: &mut VM, _argc: u8) -> Value {
    TRY_DEPTH.with(|d| d.set(d.get() + 1));
    Value::Undef
}

/// `EXC_EXIT` builtin — see [`EXC_EXIT`].
fn b_exc_exit(_vm: &mut VM, _argc: u8) -> Value {
    TRY_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    Value::Undef
}

/// `EXC_STASH` builtin — see [`EXC_STASH`].
fn b_exc_stash(_vm: &mut VM, _argc: u8) -> Value {
    let parked = PENDING.with(|p| p.borrow_mut().take());
    STASH.with(|s| s.borrow_mut().push(parked));
    Value::Undef
}

/// `EXC_UNSTASH` builtin — see [`EXC_UNSTASH`].
fn b_exc_unstash(_vm: &mut VM, _argc: u8) -> Value {
    let parked = STASH.with(|s| s.borrow_mut().pop()).flatten();
    // A `finally` body that threw leaves its own exception in flight; the JVM
    // discards the original in that case, so only restore when nothing is set.
    PENDING.with(|p| {
        let mut p = p.borrow_mut();
        if p.is_none() {
            *p = parked;
        }
    });
    Value::Undef
}

/// `EXC_ABORT` builtin — see [`EXC_ABORT`].
fn b_exc_abort(vm: &mut VM, _argc: u8) -> Value {
    if let Some(exc) = PENDING.with(|p| p.borrow_mut().take()) {
        FFI_ERROR.with(|e| *e.borrow_mut() = Some(scala_str(&exc)));
    }
    vm.request_halt();
    Value::Undef
}

// ── Host-side object heap ───────────────────────────────────────────────────
//
// fusevm's `Value::Obj(u32)` is an opaque handle into a *frontend-owned* object
// heap; fusevm carries the handle but the pointed-to object lives here. A Scala
// instance is an ordered, type-tagged record: the class name, whether it is a
// `case` type (structural `equals`/`hashCode`/`toString` vs. a plain class's
// reference identity), and its fields in *declared order* (so `toString` renders
// `Point(1,2)`). The handle is an index into the thread-local arena, which is
// reset per run (see [`reset_heap`]); every construction site bakes the class
// name + field names into the bytecode, so no runtime class registry is needed.

/// A live Scala object behind a [`Value::Obj`] handle: a class/case instance, a
/// collection, a tuple, or a first-class function value.
#[derive(Clone)]
enum HeapVal {
    /// A `class`/`case class`/`object` instance (an ordered, type-tagged record).
    Record(ScalaObj),
    /// A sequence collection. `kind` selects the `toString` prefix (`List(…)`,
    /// `Set(…)`, `Iterable(…)`) so `.keys`/`.values` render as Scala does.
    Seq(SeqKind, Vec<Value>),
    /// An immutable map. `Map1`..`Map4` (`HashRep::Small`) keep insertion order
    /// and `m + (k -> v)` semantics — a new key appends, an existing key keeps
    /// its position with the new value; a `HashMap` (`HashRep::Hashed`) is stored
    /// in CHAMP trie order (see [`champ_order`]).
    Map(HashRep, Vec<(Value, Value)>),
    /// A tuple (`(a, b)`, `a -> b`).
    Tuple(Vec<Value>),
    /// A first-class function value (lambda) — see [`Closure`].
    Closure(Closure),
    /// A function value built at run time out of other function values rather
    /// than from a compiled body — see [`DerivedFn`].
    Derived(DerivedFn),
    /// A built-in throwable (`new RuntimeException("…")`, or one raised by the
    /// runtime itself) — see [`ExcObj`].
    Exc(ExcObj),
    /// A `scala.math.Ordering` — see [`MAKE_ORDERING`]. `reversed` is the
    /// direction, flipped by `.reverse`. `key` and `lt` are `Value::Undef` for
    /// the natural ordering the companion members name, and hold the function
    /// an `Ordering.by(f)` / `ord.on(f)` extracts with, or the `<` test an
    /// `Ordering.fromLessThan(lt)` was built from.
    Ordering {
        key: Value,
        lt: Value,
        reversed: bool,
    },
    /// A `scala.util.matching.Regex` (`"…".r`). The *source* pattern is stored,
    /// which is what `toString`/`regex` answer; the compiled automaton lives in
    /// the [`REGEX_CACHE`] keyed by that source.
    Regex(Arc<str>),
    /// One `Regex.Match` — the matched text plus its capture groups (`None` for
    /// a group that did not participate). `toString` is the matched text, as
    /// `java.util.regex.Matcher.group()` is.
    Match {
        matched: Arc<str>,
        groups: Vec<Option<Arc<str>>>,
    },
    /// A boxed `var` — one mutable slot shared by the frame that declared it and
    /// every closure that captured it (see [`CELL_NEW`]). Never user-visible: the
    /// compiler emits a `CELL_GET`/`CELL_SET` around every access.
    Cell(Value),
    /// A `LazyList` — the one collection whose elements are produced on demand
    /// and remembered, so an INFINITE source is representable.
    ///
    /// `forced` is the prefix already computed, which is both the memo and what
    /// the rendering shows (`LazyList(1, 2, <not computed>)`); `src` is the
    /// rule that produces the next element. Every operation forces the minimum
    /// prefix it needs and no more.
    Lazy(LazyList),
    /// A `Char`. Scala's `Char` is neither an `Int` nor a `String`: it prints as
    /// one character but enters arithmetic as its 16-bit code point, and
    /// `'5'.toInt` (53) must differ from `"5".toInt` (5). A one-character
    /// `String` cannot encode that difference, and a bare `Value::Int` cannot
    /// print as text, so `Char` carries its own runtime tag — which also lets
    /// Char-ness survive a lambda or a collection (`"abc".toList.map(_.toInt)`),
    /// where no static type is available to recover it.
    ///
    /// Handles are interned by [`make_char`], so this costs no arena growth in a
    /// per-character loop and makes handle identity match value equality.
    Char(char),
}

/// Which representation an immutable `Set`/`Map` has. Scala's factories return
/// the fixed-arity `Set1`..`Set4` / `Map1`..`Map4` classes for up to four
/// entries — insertion-ordered, printed `Set(…)` / `Map(…)` — and a CHAMP
/// `HashSet`/`HashMap` beyond that, printed `HashSet(…)` / `HashMap(…)` in trie
/// order. An operation on a hashed receiver stays hashed however small the
/// result, so `Set(1,2,3,4,5).filter(_ > 1)` is a four-element `HashSet`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HashRep {
    Small,
    Hashed,
    /// A `scala.collection.mutable.HashSet`/`HashMap` — a flat, separately
    /// chained hash table, nothing like the immutable CHAMP trie. It prints
    /// `HashSet(…)`/`HashMap(…)` at every size, and its iteration order is
    /// bucket index ascending then, within a bucket, improved hash ascending.
    /// The table length rides along because the order depends on it and it grows
    /// with the collection (see [`mut_ordered`]).
    Mutable(u32),
    /// A `scala.collection.mutable.LinkedHashSet`/`LinkedHashMap` — a hash table
    /// whose entries are additionally threaded on a doubly-linked list in
    /// INSERTION order, which is the order it iterates and prints in. There is
    /// therefore no table order to reproduce: the stored `Vec` already is the
    /// linked list, so an add appends and a remove unlinks. It prints
    /// `LinkedHashSet(…)`/`LinkedHashMap(…)` at every size.
    Linked,
}

/// The rendered prefix of a [`HeapVal::Seq`].
#[derive(Clone, Copy, PartialEq)]
enum SeqKind {
    List,
    Vector,
    /// A `Set` — immutable `Set1`..`Set4` / CHAMP `HashSet`, or a mutable
    /// `HashSet` (see [`HashRep`]).
    Set(HashRep),
    Iterable,
    /// A `scala.collection.Iterator` — the one sequence kind that is CONSUMED by
    /// traversing it.
    ///
    /// The elements are still materialized (this frontend is strict), but the
    /// receiver is DRAINED by every terminal or transforming op, because that is
    /// the only part of an iterator's laziness that is observable: Scala's
    /// `it.toList` answers the elements once and `List()` the second time, and a
    /// `next()` past the end throws. A strict `Iterable` answered the full list
    /// both times and had no `next`/`hasNext` at all.
    ///
    /// It renders as `<iterator>` — `Iterator.toString` is a fixed string in the
    /// standard library, not the JVM identity form, so unlike an `Array` it is
    /// perfectly reproducible.
    Iterator,
    /// A `.view` — `SeqView`/`IndexedSeqView`/`ArrayView`, told apart by the
    /// receiver it was taken of (`false` here is the `Seq` case, `true` the
    /// indexed one; an `Array`'s view is [`SeqKind::ArrayView`]).
    ///
    /// Views are the one collection whose CONTENTS Scala does not show:
    /// `List(1,2,3).view` prints `SeqView(<not computed>)`. Strict here, as
    /// `Iterator` is — a downstream combinator sees the same elements a lazy
    /// view would produce — so only that rendering is reproduced.
    View(bool),
    /// An `Array`'s `.view`, which unlike the others DOES show its elements
    /// (`ArrayView(1, 2)`).
    ArrayView,
    /// `scala.collection.immutable.ArraySeq` — the `IndexedSeq` a `StringOps`
    /// combinator answers when its function's result type is not `Char`
    /// (`"abc".map(_.toInt)` is an `ArraySeq`, not a `Vector`).
    ArraySeq,
    /// A mutable `Array`: the only sequence kind that answers `update`.
    Array,
    /// `scala.collection.mutable.ListBuffer` — a growable sequence.
    ListBuffer,
    /// `scala.collection.mutable.ArrayBuffer` (also `Buffer`/`Seq` under the
    /// mutable namespace) — a growable indexed sequence.
    ArrayBuffer,
    /// `scala.collection.mutable.Queue` — a growable FIFO. `enqueue` appends and
    /// `dequeue` takes from the head, so the stored order is front-to-back.
    Queue,
    /// `scala.collection.mutable.Stack` — a growable LIFO whose HEAD is the top:
    /// `push` prepends, `pop`/`top` read the head. `+=` is still `Growable.addOne`
    /// and therefore APPENDS, which is why `Stack(1,2,3) += 8` prints
    /// `Stack(1, 2, 3, 8)` where `push(8)` would print `Stack(8, 1, 2, 3)`.
    Stack,
    /// `scala.collection.mutable.ArrayDeque` — growable at both ends
    /// (`prepend`/`append`, `removeHead`/`removeLast`).
    ArrayDeque,
    /// `scala.collection.mutable.PriorityQueue` — a binary MAX-HEAP in an array.
    /// Its stored order is the raw heap array, which is what `toString` and
    /// iteration expose: `PriorityQueue(3,1,4,1,5,9,2,6)` prints
    /// `PriorityQueue(9, 6, 4, 1, 5, 3, 2, 1)`, neither the input order nor a
    /// sorted one. Only the head is guaranteed to be the maximum.
    PriorityQueue,
    /// `scala.collection.mutable.StringBuilder` — a growable sequence of `Char`.
    /// It is a `Seq[Char]` (so `length`, `apply`, `map`, `mkString` all work off
    /// the same element vector) but its `toString` is the CONTENTS, with no
    /// `StringBuilder(…)` wrapper.
    StrBuf,
    /// An integer `Range` as a first-class value. Its elements are materialized
    /// like any other sequence; the bounds are kept because `Range.toString`
    /// prints them (`Range 1 to 10 by 3`) rather than the elements.
    Range {
        start: i64,
        end: i64,
        inclusive: bool,
        step: i64,
    },
}

/// Which empty-receiver access [`SeqKind::empty_fault`] is reporting. The four
/// share one message shape per kind but not one wording, so the op is a
/// parameter rather than four near-duplicate tables.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EmptyOp {
    Head,
    Last,
    Tail,
    Init,
}

impl EmptyOp {
    fn word(self) -> &'static str {
        match self {
            EmptyOp::Head => "head",
            EmptyOp::Last => "last",
            EmptyOp::Tail => "tail",
            EmptyOp::Init => "init",
        }
    }
}

impl SeqKind {
    fn label(self) -> &'static str {
        match self {
            SeqKind::List => "List",
            SeqKind::Vector => "Vector",
            SeqKind::Set(HashRep::Small) => "Set",
            SeqKind::Set(HashRep::Hashed | HashRep::Mutable(_)) => "HashSet",
            SeqKind::Set(HashRep::Linked) => "LinkedHashSet",
            SeqKind::Iterable => "Iterable",
            SeqKind::Iterator => "Iterator",
            SeqKind::View(false) => "SeqView",
            SeqKind::View(true) => "IndexedSeqView",
            SeqKind::ArrayView => "ArrayView",
            SeqKind::ArraySeq => "ArraySeq",
            SeqKind::Array => "Array",
            SeqKind::ListBuffer => "ListBuffer",
            SeqKind::ArrayBuffer => "ArrayBuffer",
            SeqKind::Queue => "Queue",
            SeqKind::Stack => "Stack",
            SeqKind::ArrayDeque => "ArrayDeque",
            SeqKind::PriorityQueue => "PriorityQueue",
            SeqKind::StrBuf => "StringBuilder",
            SeqKind::Range { .. } => "Range",
        }
    }

    /// The out-of-range text `apply(i)` answers, which is NOT one message: each
    /// Scala class reaches a different JDK/library check, so the exception CLASS
    /// differs too.
    ///
    /// Captured from Scala 3.8.4 on JDK 26.0.2 — `List(1,2)(5)` is
    /// `IndexOutOfBoundsException: 5` (`LinearSeqOps.apply` passes the bare
    /// index), `Vector(1,2)(5)` is `IndexOutOfBoundsException: 5 is out of
    /// bounds (min 0, max 1)` (`IndexedSeqOps` formats a span),
    /// `Array(1,2)(5)` is the JVM's own `ArrayIndexOutOfBoundsException: Index 5
    /// out of bounds for length 2`, and `new StringBuilder("ab")(5)` is
    /// `StringIndexOutOfBoundsException` with the array wording. Answering one
    /// of these for all of them was the bug this table replaces.
    fn index_fault(self, i: i64, len: usize) -> String {
        match self {
            // The JVM's array bytecode check, and `ArraySeq` which delegates to it.
            SeqKind::Array | SeqKind::ArraySeq => format!(
                "scalars: java.lang.ArrayIndexOutOfBoundsException: Index {i} out of bounds for length {len}"
            ),
            // `StringBuilder` forwards to `String.charAt`'s check.
            SeqKind::StrBuf => format!(
                "scalars: java.lang.StringIndexOutOfBoundsException: Index {i} out of bounds for length {len}"
            ),
            // Linear sequences pass the raw index through.
            // `Iterator` rides along in both fault tables for exhaustiveness only:
            // it exposes neither `apply` nor `head`/`last`/`tail`/`init`, so
            // Scala rejects those at compile time and neither message is
            // reachable from a valid program.
            SeqKind::List | SeqKind::Iterable | SeqKind::Iterator | SeqKind::ListBuffer => {
                format!("scalars: java.lang.IndexOutOfBoundsException: {i}")
            }
            // Every indexed sequence formats the legal span.
            _ => index_out_of_bounds(i, len),
        }
    }

    /// The out-of-range text an indexed WRITE (`a(i) = v`) answers.
    ///
    /// The same as [`SeqKind::index_fault`] for every kind but `ListBuffer`,
    /// which is the one whose read and write disagree: `apply` forwards to the
    /// LINEAR `LinearSeqOps` and reports the bare index, while `update` runs its
    /// own `checkIndex` and reports the legal span. Captured from Scala 3.8.4 on
    /// JDK 26.0.2 — `mutable.ListBuffer(1,2)(4)` is `IndexOutOfBoundsException: 4`
    /// but `lb(4) = 1` is `4 is out of bounds (min 0, max 1)`.
    fn index_write_fault(self, i: i64, len: usize) -> String {
        match self {
            SeqKind::ListBuffer => index_out_of_bounds(i, len),
            _ => self.index_fault(i, len),
        }
    }

    /// The empty-receiver text for `head`, `last`, `tail` and `init`.
    ///
    /// Same story as [`SeqKind::index_fault`]: `List` names itself
    /// (`head of empty list`), `Vector` answers `IndexedSeqOps`' `empty.head`,
    /// `Range` uses `on` where everything else uses `of`, a `Set`/`Map` reports
    /// the ITERATOR it failed to advance (`next on empty iterator`), and the
    /// mutable buffers throw with NO message at all. `None` is that last case —
    /// `getMessage` reads `null`, which the caller renders by omitting the
    /// `: <text>` suffix entirely.
    ///
    /// Captured from Scala 3.8.4 on JDK 26.0.2.
    fn empty_fault(self, op: EmptyOp) -> String {
        let exc = match (self, op) {
            // `Range` raises `NoSuchElementException` even for `tail`/`init`,
            // where every other kind raises `UnsupportedOperationException`.
            (SeqKind::Range { .. }, _) | (_, EmptyOp::Head | EmptyOp::Last) => {
                "java.util.NoSuchElementException"
            }
            _ => "java.lang.UnsupportedOperationException",
        };
        let text: Option<String> = match self {
            SeqKind::Range { .. } => Some(format!("{} on empty Range", op.word())),
            SeqKind::List | SeqKind::Iterable | SeqKind::Iterator => {
                Some(format!("{} of empty list", op.word()))
            }
            SeqKind::View(_) | SeqKind::ArrayView => Some(format!("{} of empty list", op.word())),
            // `Vector`/`IndexedSeq` name the operation on an `empty` receiver —
            // and `last` reports `empty.tail`, because it is implemented as one.
            SeqKind::Vector => Some(match op {
                EmptyOp::Last => "empty.tail".into(),
                _ => format!("empty.{}", op.word()),
            }),
            SeqKind::Array => Some(format!("{} of empty array", op.word())),
            SeqKind::ArraySeq => match op {
                EmptyOp::Head | EmptyOp::Last => Some(format!("{} of empty ArraySeq", op.word())),
                EmptyOp::Tail => Some("tail of empty array".into()),
                EmptyOp::Init => None,
            },
            // A `ListBuffer` iterates for `head` but indexes for `last`.
            SeqKind::ListBuffer => match op {
                EmptyOp::Head => Some("next on empty iterator".into()),
                EmptyOp::Last => Some("last of empty ListBuffer".into()),
                _ => None,
            },
            SeqKind::ArrayBuffer | SeqKind::Queue | SeqKind::Stack | SeqKind::ArrayDeque => {
                match op {
                    EmptyOp::Head | EmptyOp::Last => {
                        Some(format!("{} of empty {}", op.word(), self.label()))
                    }
                    _ => None,
                }
            }
            // A `StringBuilder` is a `Seq[Char]` whose ops report `IndexedSeq`.
            SeqKind::StrBuf => match op {
                EmptyOp::Head | EmptyOp::Last => Some(format!("{} of empty IndexedSeq", op.word())),
                _ => None,
            },
            // A `Set` has no order, so every access goes through its iterator.
            SeqKind::Set(_) => match op {
                EmptyOp::Head | EmptyOp::Last => Some("next on empty iterator".into()),
                _ => None,
            },
            // A `PriorityQueue` reads its root directly for `head` but iterates
            // for `last`, and says so.
            SeqKind::PriorityQueue => match op {
                EmptyOp::Head => Some("queue is empty".into()),
                EmptyOp::Last => Some("next on empty iterator".into()),
                _ => None,
            },
        };
        match text {
            Some(t) => format!("scalars: {exc}: {t}"),
            None => format!("scalars: {exc}"),
        }
    }

    /// Whether this kind is mutated in place (`a(i) = v`, `+=`, `clear()`).
    fn is_mutable(self) -> bool {
        matches!(
            self,
            SeqKind::Array | SeqKind::Set(HashRep::Mutable(_) | HashRep::Linked)
        ) || self.is_buffer()
    }

    /// Whether this kind is a growable buffer (`+=`, `append`, `remove`) — an
    /// `Array` is mutable but fixed-length, so it is not one.
    fn is_buffer(self) -> bool {
        matches!(
            self,
            SeqKind::ListBuffer
                | SeqKind::ArrayBuffer
                | SeqKind::Queue
                | SeqKind::Stack
                | SeqKind::ArrayDeque
                | SeqKind::StrBuf
        )
    }

    /// Whether this kind is a `Set` (any representation). A `Set` is `Iterable`
    /// but not a `Seq`, which is what the type-pattern tests turn on.
    fn is_set(self) -> bool {
        matches!(self, SeqKind::Set(_))
    }
}

/// Build the collection a transforming op (`map`, `filter`, …) produces from a
/// receiver of `kind`. A `Range` is a view over consecutive integers, so mapping
/// one yields a `Vector` — exactly as Scala's `IndexedSeq` result does; a `Set`
/// re-deduplicates and re-orders (a hashed receiver stays hashed, and a small one
/// grows into a `HashSet` past four elements).
/// Empty an `Iterator`'s backing element vector in place — how a traversal
/// CONSUMES it. The caller has already read the elements out, so this only makes
/// the exhaustion observable to the next traversal.
fn drain_iterator(recv: &Value) {
    if let Value::Obj(id) = recv {
        HEAP.with(|h| {
            if let Some(HeapVal::Seq(SeqKind::Iterator, xs)) = h.borrow_mut().get_mut(*id as usize)
            {
                xs.clear();
            }
        });
    }
}

/// Drop an `Iterator`'s first element — the single element `next()` consumes.
fn take_iterator_head(recv: &Value) {
    if let Value::Obj(id) = recv {
        HEAP.with(|h| {
            if let Some(HeapVal::Seq(SeqKind::Iterator, xs)) = h.borrow_mut().get_mut(*id as usize)
            {
                if !xs.is_empty() {
                    xs.remove(0);
                }
            }
        });
    }
}

fn derive_seq(kind: SeqKind, items: Vec<Value>) -> Value {
    match kind {
        SeqKind::Set(rep) => new_set(rep, items),
        SeqKind::Range { .. } => new_seq(SeqKind::Vector, items),
        // A transformed `PriorityQueue` is rebuilt through the same builder, so
        // the result is heapified rather than carrying the source array order.
        SeqKind::PriorityQueue => new_priority_queue(items),
        // `immutable.Iterable`'s own factory is `List`, so mapping a `Map`'s
        // `values` view answers `List(…)` where the view itself printed
        // `Iterable(…)`.
        SeqKind::Iterable => new_seq(SeqKind::List, items),
        other => new_seq(other, items),
    }
}

/// Scala's `Range.toString`, ported: the bounds and step, prefixed `empty ` when
/// no element is produced and `inexact ` when the step steps over `end`.
fn range_to_string(start: i64, end: i64, inclusive: bool, step: i64, is_empty: bool) -> String {
    let preposition = if inclusive { "to" } else { "until" };
    let stepped = if step == 1 {
        String::new()
    } else {
        format!(" by {step}")
    };
    let exact = step != 0 && (end - start) % step == 0;
    let prefix = if is_empty {
        "empty "
    } else if !exact {
        "inexact "
    } else {
        ""
    };
    format!("{prefix}Range {start} {preposition} {end}{stepped}")
}

/// Materialize the elements of `start until|to end by step`, capped so a range
/// that would exhaust memory fails loudly instead of hanging.
fn range_items(start: i64, end: i64, inclusive: bool, step: i64) -> Result<Vec<Value>, String> {
    if step == 0 {
        return Err("scalars: java.lang.IllegalArgumentException: step cannot be 0.".into());
    }
    const CAP: usize = 8_000_000;
    let mut out = Vec::new();
    let mut i = start;
    while if step > 0 {
        if inclusive {
            i <= end
        } else {
            i < end
        }
    } else if inclusive {
        i >= end
    } else {
        i > end
    } {
        out.push(Value::int(i));
        if out.len() > CAP {
            return Err("scalars: java.lang.OutOfMemoryError: Range too large".into());
        }
        match i.checked_add(step) {
            Some(n) => i = n,
            None => break,
        }
    }
    Ok(out)
}

/// A first-class function value. `name_idx` is the closure body's name-pool index
/// (resolved to a subroutine entry via `Chunk::find_sub` at call time), `params`
/// its declared parameter count, and `captures` the values read from the enclosing
/// frame at creation time (its upvalues, stored by value so a returned closure
/// still sees them after the defining frame has popped).
#[derive(Clone)]
struct Closure {
    name_idx: u16,
    params: u8,
    captures: Vec<Value>,
    /// For a `{ case … }` literal (Scala's `PartialFunction`), the name-pool
    /// index of the derived `isDefinedAt` body — the same patterns and guards
    /// answering `true`/`false` (see [`crate::compiler`]). `None` for a plain
    /// `x => e` lambda, which is total: defined at every argument.
    defined_idx: Option<u16>,
    /// Whether every value the body can answer is a pair literal, decided at
    /// compile time (see `compiler::yields_pairs`). Consulted only when a
    /// `Map.map`/`Map.collect` produced **no** results, where the run-time shape
    /// of the results cannot pick the builder.
    pair_body: bool,
    /// Whether the body's static result type is `Char`, and whether it is
    /// `String` (see `compiler::yields_chars` / `compiler::yields_strings`).
    /// Consulted only when a `String` combinator produced **no** results, where
    /// there is nothing to read the overload off of.
    char_body: bool,
    string_body: bool,
}

/// A function value composed from other function values. There is no compiled
/// body to point at, so the combination is stored and re-played on each call —
/// which is also what makes `isDefinedAt` exact for a composed partial function
/// (`(pf1 orElse pf2).isDefinedAt` is either operand's, never an arm body).
#[derive(Clone)]
enum DerivedFn {
    /// `pf.lift` — `x => if (pf.isDefinedAt(x)) Some(pf(x)) else None`. Total.
    Lift(Value),
    /// `pf1 orElse pf2` — defined where either is, `pf1` winning.
    OrElse(Value, Value),
    /// `f andThen g` — `x => g(f(x))`.
    AndThen(Value, Value),
    /// `f compose g` — `x => f(g(x))`.
    Compose(Value, Value),
}

/// A live Scala class instance behind a [`HeapVal::Record`].
#[derive(Clone)]
struct ScalaObj {
    /// The declaring class/object name (`Point`, `Some`, `None`, …).
    class: Arc<str>,
    /// `true` for a `case class`/`case object` — selects structural
    /// `equals`/`hashCode`/`toString`. A plain `class` uses reference identity.
    is_case: bool,
    /// `true` for a singleton `object`/`case object` — a `case` singleton renders
    /// as its bare name (`None`), not `None()`.
    is_object: bool,
    /// Fields in declared order: `(name, value)`. Order is load-bearing for
    /// `toString` and positional constructor-pattern binding.
    fields: Vec<(Arc<str>, Value)>,
}

thread_local! {
    /// The per-run object arena. Handles index into it; cleared by [`reset_heap`]
    /// before each program so runs in one process (the library `run_str` path)
    /// do not share instances.
    static HEAP: RefCell<Vec<HeapVal>> = const { RefCell::new(Vec::new()) };
}

/// Clear the object arena. Called by the runner before each program run.
pub fn reset_heap() {
    HEAP.with(|h| h.borrow_mut().clear());
    // The intern table holds arena indices, which the clear above invalidates.
    CHARS.with(|t| t.borrow_mut().clear());
    // Method entries are offsets into the OUTGOING chunk; the next program
    // compiles its own, so a stale hit would jump into unrelated bytecode.
    METHOD_ENTRIES.with(|t| t.borrow_mut().clear());
    USER_TOSTRING.with(|t| t.set(None));
    // The table-order ledger is keyed by arena index, which the clear above
    // invalidates; a stale id would claim a fresh collection is already sorted.
    MUT_SORTED.with(|t| t.borrow_mut().clear());
    reset_regex_cache();
}

/// Allocate `o` in the arena and return its `Value::Obj` handle.
fn heap_push(o: HeapVal) -> Value {
    HEAP.with(|h| {
        let mut h = h.borrow_mut();
        h.push(o);
        Value::Obj((h.len() - 1) as u32)
    })
}

/// Allocate a class/case record and return its handle.
fn heap_alloc(o: ScalaObj) -> Value {
    heap_push(HeapVal::Record(o))
}

thread_local! {
    /// `char → arena handle`, so every occurrence of a character shares one
    /// [`HeapVal::Char`]. The arena is append-only within a run, so without this
    /// a loop over a long string would allocate one entry per character read.
    static CHARS: RefCell<HashMap<char, u32>> = RefCell::new(HashMap::new());
}

/// The interned `Char` value for `c`.
pub fn make_char(c: char) -> Value {
    CHARS.with(|t| {
        if let Some(id) = t.borrow().get(&c) {
            return Value::Obj(*id);
        }
        let v = heap_push(HeapVal::Char(c));
        if let Value::Obj(id) = v {
            t.borrow_mut().insert(c, id);
        }
        v
    })
}

/// The characters of `v`'s `String.valueOf`, as interned `Char` values — the
/// elements a `StringBuilder` addition contributes.
fn str_chars(v: &Value) -> Vec<Value> {
    scala_str(v).chars().map(make_char).collect()
}

/// `java.util.NoSuchElementException: empty collection` — what a `Queue`/`Stack`
/// / `ArrayDeque` removal raises with nothing to remove.
fn empty_collection() -> String {
    "scalars: java.util.NoSuchElementException: empty collection".into()
}

/// `java.lang.StringBuilder`'s out-of-range message, which reports the length
/// rather than the max index.
fn string_index_len(i: i64, len: usize) -> String {
    format!("scalars: java.lang.StringIndexOutOfBoundsException: index {i}, length {len}")
}

/// The `char` behind `v`, if `v` is a `Char`. This is the exact test that the
/// old one-character-`String` model could only approximate.
fn as_char(v: &Value) -> Option<char> {
    if let Value::Obj(id) = v {
        HEAP.with(|h| match h.borrow().get(*id as usize) {
            Some(HeapVal::Char(c)) => Some(*c),
            _ => None,
        })
    } else {
        None
    }
}

/// A `Char`'s code point, for the numeric positions Scala's `Char` participates
/// in (arithmetic, comparison, `toInt`).
fn char_code(v: &Value) -> Option<i64> {
    as_char(v).map(|c| c as u32 as i64)
}

/// `Char` construction builtin ([`CHAR_NEW`]): pops a code point, pushes the
/// interned `Char`.
fn b_char_new(vm: &mut VM, _argc: u8) -> Value {
    let code = vm.pop().to_int();
    make_char(char_of_code(code))
}

/// The `Unit` value, as an empty tuple — the same thing the `()` literal lowers
/// to. It is NOT `Value::Undef`: that is `null`, and the two print differently
/// (`()` against `null`), so a `Unit`-returning method that answered `Undef`
/// made `println(xs.foreach(f))` render `null`.
fn unit_value() -> Value {
    heap_push(HeapVal::Tuple(Vec::new()))
}

/// Build an `Ordering` value. `key`/`lt` are `Value::Undef` for the natural one.
fn make_ordering(key: Value, lt: Value, reversed: bool) -> Value {
    heap_push(HeapVal::Ordering { key, lt, reversed })
}

/// [`MAKE_ORDERING`] — build an `Ordering` from a companion member. A member
/// name arrives beneath its arguments only for the two FACTORIES that take a
/// function; every other member names the natural ordering and is called with
/// no arguments at all.
fn b_make_ordering(vm: &mut VM, argc: u8) -> Value {
    if argc == 0 {
        return make_ordering(Value::Undef, Value::Undef, false);
    }
    let name = vm.pop().as_str_cow().into_owned();
    let n = (argc as usize).saturating_sub(1);
    let mut args = Vec::with_capacity(n);
    for _ in 0..n {
        args.push(vm.pop());
    }
    args.reverse();
    match (name.as_str(), args.len()) {
        ("by", 1) => make_ordering(args[0].clone(), Value::Undef, false),
        ("fromLessThan", 1) => make_ordering(Value::Undef, args[0].clone(), false),
        // Every other companion member names the natural ordering, for the
        // reason `MAKE_ORDERING` gives: the member picks the element TYPE, which
        // is erased here.
        (_, 0) => make_ordering(Value::Undef, Value::Undef, false),
        _ => fault(
            vm,
            format!("scalars: value {name} is not a member of object Ordering"),
        ),
    }
}

/// `scala.math.Ordering`'s methods. `reverse` flips the direction and `compare`
/// answers the three-way result; `lt`/`gt`/`lteq`/`gteq`/`equiv` are the
/// predicates Scala derives from `compare`, and `max`/`min` pick under it.
/// `on(f)` derives an ordering that compares by `f`'s result.
///
/// Takes the VM because a keyed or `fromLessThan` ordering runs a user closure
/// to compare.
fn ordering_method(
    vm: &mut VM,
    recv: &Value,
    name: &str,
    args: &[Value],
) -> Option<Result<Value, String>> {
    let (key, lt, reversed) = as_ordering(recv)?;
    let cmp = |vm: &mut VM| ordering_cmp(vm, recv, &args[0], &args[1]);
    Some(match (name, args.len()) {
        ("reverse", 0) => Ok(make_ordering(key, lt, !reversed)),
        // `ord.on(f)` sorts by `f`'s result under `ord`. Composing two key
        // functions is not modeled, so an `on` over an already-keyed ordering is
        // rejected rather than silently dropping one of them.
        ("on", 1) => match key {
            Value::Undef => Ok(make_ordering(args[0].clone(), lt, reversed)),
            _ => Err("scalars: Ordering.on over an already-keyed Ordering is not modeled".into()),
        },
        // `Ordering.Double.TotalOrdering` / `.IeeeOrdering` select an ordering of
        // NaN. `value_cmp` has one total order, which is `TotalOrdering`'s.
        ("TotalOrdering" | "IeeeOrdering", 0) => Ok(recv.clone()),
        ("compare", 2) => cmp(vm).map(|o| {
            Value::int(match o {
                Ordering::Less => -1,
                Ordering::Equal => 0,
                Ordering::Greater => 1,
            })
        }),
        ("lt", 2) => cmp(vm).map(|o| Value::bool(o == Ordering::Less)),
        ("gt", 2) => cmp(vm).map(|o| Value::bool(o == Ordering::Greater)),
        ("lteq", 2) => cmp(vm).map(|o| Value::bool(o != Ordering::Greater)),
        ("gteq", 2) => cmp(vm).map(|o| Value::bool(o != Ordering::Less)),
        ("equiv", 2) => cmp(vm).map(|o| Value::bool(o == Ordering::Equal)),
        // Scala's `Ordering.max`/`min` keep the FIRST argument on a tie.
        ("max", 2) => cmp(vm).map(|o| {
            if o == Ordering::Less {
                args[1].clone()
            } else {
                args[0].clone()
            }
        }),
        ("min", 2) => cmp(vm).map(|o| {
            if o == Ordering::Greater {
                args[1].clone()
            } else {
                args[0].clone()
            }
        }),
        _ => Err(format!("scalars: value {name} is not a member of Ordering")),
    })
}

/// The `(key, lt, reversed)` state of `v` when it is an `Ordering`, for the
/// collection methods that take one (`sorted`, `max`, `min`, `maxBy`, `minBy`,
/// `sortBy`).
fn as_ordering(v: &Value) -> Option<(Value, Value, bool)> {
    if let Value::Obj(id) = v {
        HEAP.with(|h| match h.borrow().get(*id as usize) {
            Some(HeapVal::Ordering { key, lt, reversed }) => {
                Some((key.clone(), lt.clone(), *reversed))
            }
            _ => None,
        })
    } else {
        None
    }
}

/// Compare `a` and `b` under `ord`: extract the sort keys, apply the ordering's
/// own comparison, then flip for a `reverse`. Only this path needs the VM,
/// because a keyed or `fromLessThan` ordering runs a user closure.
fn ordering_cmp(vm: &mut VM, ord: &Value, a: &Value, b: &Value) -> Result<Ordering, String> {
    let Some((key, lt, reversed)) = as_ordering(ord) else {
        return Err("scalars: expected an Ordering".into());
    };
    let (a, b) = match key {
        Value::Undef => (a.clone(), b.clone()),
        f => (
            invoke_closure(vm, &f, std::slice::from_ref(a))?,
            invoke_closure(vm, &f, std::slice::from_ref(b))?,
        ),
    };
    let base = match lt {
        Value::Undef => cmp_vm(vm, &a, &b)?,
        // `Ordering.fromLessThan(lt)` is defined by its `lt` alone: equal when
        // neither side is less than the other.
        f => {
            if truthy(&invoke_closure(vm, &f, &[a.clone(), b.clone()])?) {
                Ordering::Less
            } else if truthy(&invoke_closure(vm, &f, &[b, a])?) {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        }
    };
    Ok(if reversed { base.reverse() } else { base })
}

/// Scala's `toChar`: the value truncated to 16 bits. An unpaired surrogate is
/// not a Rust `char`, so it stands in as U+FFFD rather than aborting.
fn char_of_code(code: i64) -> char {
    char::from_u32(code as u32 & 0xFFFF).unwrap_or('\u{FFFD}')
}

/// [`UNAPPLY_SEQ`] — apply a value-position extractor to the scrutinee.
///
/// Only `Regex` is one today: `Regex.unapplySeq` succeeds when the pattern
/// matches the **entire** input (`"a1"` does NOT match `"""(\d+)""".r`, `"123"`
/// does) and binds one value per capture group, with a group that did not
/// participate binding `null`.
///
/// Pushes the bound values as a tuple, or `Value::Undef` for no match — the
/// compiler tests that and jumps to the next `case`. The pattern's source NAME
/// rides along so a name that is not an extractor reports the identifier the
/// user wrote rather than whatever value it held.
fn b_unapply_seq(vm: &mut VM, _argc: u8) -> Value {
    let pat_name = vm.pop();
    let want = vm.pop().to_int();
    let scrutinee = vm.pop();
    let extractor = vm.pop();
    let Some(pat) = (match &extractor {
        Value::Obj(id) => HEAP.with(|h| match h.borrow().get(*id as usize) {
            Some(HeapVal::Regex(p)) => Some(p.clone()),
            _ => None,
        }),
        _ => None,
    }) else {
        // Preserves the compile-time message for a name that is not an
        // extractor at all (an undefined one reads back as `Undef`).
        return fault(
            vm,
            format!(
                "scalars: not found: constructor pattern `{}`",
                scala_str(&pat_name)
            ),
        );
    };
    // `unapplySeq` is a whole-input match, as `Regex`'s is (the non-capturing
    // wrapper keeps the user's own group numbers intact).
    let re = match regex_compile(&format!("\\A(?:{pat})\\z")) {
        Ok(re) => re,
        Err(e) => return fault(vm, e),
    };
    let s = scala_str(&scrutinee);
    let Ok(Some(caps)) = re.captures(&s) else {
        return Value::Undef;
    };
    // Group 0 is the whole match; the bindings are the explicit groups. They
    // come back as a tuple so the compiler can read them with the same `_1`..
    // accessors a tuple pattern uses.
    let mut out = Vec::with_capacity(want as usize);
    for i in 1..=want as usize {
        out.push(match caps.get(i) {
            Some(m) => Value::str(m.as_str().to_string()),
            // A group that did not participate binds Scala's `null`.
            None => Value::Undef,
        });
    }
    heap_push(HeapVal::Tuple(out))
}

/// Run `f` against the record `v` points to, or `None` if `v` is not a live
/// record handle (a collection/tuple/closure returns `None`).
fn with_obj<R>(v: &Value, f: impl FnOnce(&ScalaObj) -> R) -> Option<R> {
    if let Value::Obj(id) = v {
        HEAP.with(|h| match h.borrow().get(*id as usize) {
            Some(HeapVal::Record(o)) => Some(f(o)),
            _ => None,
        })
    } else {
        None
    }
}

/// The element count of a `Tuple` handle, if `v` is one.
fn seq_or_tuple_len(v: &Value) -> Option<usize> {
    if let Value::Obj(id) = v {
        HEAP.with(|h| match h.borrow().get(*id as usize) {
            Some(HeapVal::Tuple(items)) => Some(items.len()),
            _ => None,
        })
    } else {
        None
    }
}

/// Clone the elements of a `Seq`/`Tuple` handle (both are element sequences), if
/// `v` is one. Used by the pure (`vm`-free) helpers.
fn as_seq(v: &Value) -> Option<Vec<Value>> {
    if let Value::Obj(id) = v {
        HEAP.with(|h| match h.borrow().get(*id as usize) {
            Some(HeapVal::Seq(_, items)) => Some(items.clone()),
            _ => None,
        })
    } else {
        None
    }
}

/// A `Seq` handle's kind, if `v` is one.
///
/// Reads the kind and NOTHING else. Routing it through [`seq_kind_items`] made
/// asking "what kind is this?" copy every element of the receiver and then drop
/// the copy — a `Vec` clone and a `Vec` drop per call, which measured as 90% of
/// an indexed loop's time once `apply` started asking.
fn seq_kind(v: &Value) -> Option<SeqKind> {
    if let Value::Obj(id) = v {
        HEAP.with(|h| match h.borrow().get(*id as usize) {
            Some(HeapVal::Seq(k, _)) => Some(*k),
            _ => None,
        })
    } else {
        None
    }
}

/// Clone a `Seq` handle's kind and elements, if `v` is one.
fn seq_kind_items(v: &Value) -> Option<(SeqKind, Vec<Value>)> {
    if let Value::Obj(id) = v {
        HEAP.with(|h| match h.borrow().get(*id as usize) {
            Some(HeapVal::Seq(k, items)) => Some((*k, items.clone())),
            _ => None,
        })
    } else {
        None
    }
}

/// Clone the entries of a `Map` handle, if `v` is one.
fn as_map(v: &Value) -> Option<Vec<(Value, Value)>> {
    map_rep_entries(v).map(|(_, m)| m)
}

/// Clone a `Map` handle's representation and entries, if `v` is one.
fn map_rep_entries(v: &Value) -> Option<(HashRep, Vec<(Value, Value)>)> {
    if let Value::Obj(id) = v {
        HEAP.with(|h| match h.borrow().get(*id as usize) {
            Some(HeapVal::Map(rep, m)) => Some((*rep, m.clone())),
            _ => None,
        })
    } else {
        None
    }
}

/// Clone a closure handle's metadata, if `v` is a closure value.
fn as_closure(v: &Value) -> Option<Closure> {
    if let Value::Obj(id) = v {
        HEAP.with(|h| match h.borrow().get(*id as usize) {
            Some(HeapVal::Closure(c)) => Some(c.clone()),
            _ => None,
        })
    } else {
        None
    }
}

/// The composed function `v` points at, if it is one (see [`DerivedFn`]).
fn as_derived(v: &Value) -> Option<DerivedFn> {
    if let Value::Obj(id) = v {
        HEAP.with(|h| match h.borrow().get(*id as usize) {
            Some(HeapVal::Derived(d)) => Some(d.clone()),
            _ => None,
        })
    } else {
        None
    }
}

/// Whether `v` is any function value — a compiled lambda or a composed one.
fn is_function(v: &Value) -> bool {
    if let Value::Obj(id) = v {
        HEAP.with(|h| {
            matches!(
                h.borrow().get(*id as usize),
                Some(HeapVal::Closure(_) | HeapVal::Derived(_))
            )
        })
    } else {
        false
    }
}

/// `OBJ_NEW` builtin — see [`OBJ_NEW`]. Pops `is_case`, the field-name CSV, the
/// class name, then the field values (deepest first), and allocates the record.
fn b_obj_new(vm: &mut VM, _argc: u8) -> Value {
    let is_object = matches!(vm.pop(), Value::Bool(true));
    let is_case = matches!(vm.pop(), Value::Bool(true));
    let csv = vm.pop().as_str_cow().into_owned();
    let class = vm.pop().as_str_cow().into_owned();
    let names: Vec<&str> = if csv.is_empty() {
        Vec::new()
    } else {
        csv.split(',').collect()
    };
    let mut vals = Vec::with_capacity(names.len());
    for _ in 0..names.len() {
        vals.push(vm.pop());
    }
    vals.reverse();
    let fields = names
        .into_iter()
        .zip(vals)
        .map(|(nm, v)| (Arc::from(nm), v))
        .collect();
    heap_alloc(ScalaObj {
        class: Arc::from(class.as_str()),
        is_case,
        is_object,
        fields,
    })
}

/// `OBJ_CLASS` builtin — pop one value; push its class name (or `""` for a
/// non-object, so the method-dispatch fallthrough routes it to [`SMETHOD`]).
fn b_obj_class(vm: &mut VM, _argc: u8) -> Value {
    let v = vm.pop();
    match with_obj(&v, |o| o.class.to_string()) {
        Some(c) => Value::str(c),
        None => Value::str(""),
    }
}

/// `OBJ_COPY` builtin — see [`OBJ_COPY`]. Clones the receiver's record with the
/// named (`field`) or positional (`#index`) updates applied.
fn b_obj_copy(vm: &mut VM, argc: u8) -> Value {
    let k = (argc as usize).saturating_sub(2);
    let mut vals = Vec::with_capacity(k);
    for _ in 0..k {
        vals.push(vm.pop());
    }
    vals.reverse();
    let spec = vm.pop().as_str_cow().into_owned();
    let recv = vm.pop();
    let base = match with_obj(&recv, |o| o.clone()) {
        Some(o) => o,
        None => return fault(vm, "scalars: copy is not a member of a non-object"),
    };
    let specs: Vec<&str> = if spec.is_empty() {
        Vec::new()
    } else {
        spec.split(',').collect()
    };
    let mut fields = base.fields.clone();
    for (s, val) in specs.into_iter().zip(vals) {
        if let Some(idx) = s.strip_prefix('#') {
            if let Ok(i) = idx.parse::<usize>() {
                if let Some(f) = fields.get_mut(i) {
                    f.1 = val;
                }
            }
        } else if let Some(f) = fields.iter_mut().find(|f| &*f.0 == s) {
            f.1 = val;
        }
    }
    heap_alloc(ScalaObj {
        class: base.class,
        is_case: base.is_case,
        is_object: base.is_object,
        fields,
    })
}

/// `OBJ_SET` builtin — see [`OBJ_SET`]. Pop the new value, the field name, and
/// the receiver; overwrite that field in place. Returns `Unit`.
fn b_obj_set(vm: &mut VM, _argc: u8) -> Value {
    let val = vm.pop();
    let name = vm.pop().as_str_cow().into_owned();
    let recv = vm.pop();
    // Suppressed while unwinding, so a raise mid-assignment cannot commit
    // garbage to a live field that a handler would then read.
    if unwinding() {
        return Value::Undef;
    }
    if let Value::Obj(id) = recv {
        HEAP.with(|h| {
            if let Some(HeapVal::Record(o)) = h.borrow_mut().get_mut(id as usize) {
                if let Some(f) = o.fields.iter_mut().find(|f| f.0.as_ref() == name.as_str()) {
                    f.1 = val;
                }
            }
        });
    }
    Value::Undef
}

// ── Closures, collections, tuples ──────────────────────────────────────────

/// `MAKE_CLOSURE` builtin — pop the capture count (`argc - 3`), then the
/// pair-result flag, the parameter count and the body name index, then the
/// captured upvalue values (deepest first). Registers a heap [`Closure`] and
/// returns its handle.
fn b_make_closure(vm: &mut VM, argc: u8) -> Value {
    make_closure(vm, argc, false)
}

/// `MAKE_PARTIAL` builtin — [`b_make_closure`] with the extra `isDefinedAt`
/// name index popped between the parameter count and the captures.
fn b_make_partial(vm: &mut VM, argc: u8) -> Value {
    make_closure(vm, argc, true)
}

/// Shared body of [`b_make_closure`] / [`b_make_partial`].
fn make_closure(vm: &mut VM, argc: u8, partial: bool) -> Value {
    let fixed = if partial { 4 } else { 3 };
    let ncap = (argc as usize).saturating_sub(fixed);
    let mut captures = Vec::with_capacity(ncap);
    for _ in 0..ncap {
        captures.push(vm.pop());
    }
    captures.reverse();
    let defined_idx = partial.then(|| vm.pop().to_int() as u16);
    // One packed operand carries every compile-time fact about the body (see
    // `compiler::body_flags`), so adding a fact does not change the closure's
    // operand layout.
    let flags = vm.pop().to_int();
    let params = vm.pop().to_int() as u8;
    let name_idx = vm.pop().to_int() as u16;
    heap_push(HeapVal::Closure(Closure {
        name_idx,
        params,
        captures,
        defined_idx,
        pair_body: flags & 1 != 0,
        char_body: flags & 2 != 0,
        string_body: flags & 4 != 0,
    }))
}

/// `MAKE_LIST` builtin — pop `argc` element values (deepest first) into a `List`.
fn b_make_list(vm: &mut VM, argc: u8) -> Value {
    let mut items = Vec::with_capacity(argc as usize);
    for _ in 0..argc {
        items.push(vm.pop());
    }
    items.reverse();
    heap_push(HeapVal::Seq(SeqKind::List, items))
}

/// `MAKE_VECTOR` builtin — pop `argc` element values (deepest first) into a
/// `Vector`.
fn b_make_vector(vm: &mut VM, argc: u8) -> Value {
    new_seq(SeqKind::Vector, pop_n(vm, argc))
}

fn b_make_arrayseq(vm: &mut VM, argc: u8) -> Value {
    new_seq(SeqKind::ArraySeq, pop_n(vm, argc))
}

/// `MAKE_ITERATOR` builtin — pop `argc` element values into an `Iterator`.
/// Backs both `Iterator(a, b, c)` and `Iterator.empty` (`argc` 0).
fn b_make_iterator(vm: &mut VM, argc: u8) -> Value {
    new_seq(SeqKind::Iterator, pop_n(vm, argc))
}

/// `MAKE_SET` builtin — pop `argc` element values (deepest first) into a `Set`.
fn b_make_set(vm: &mut VM, argc: u8) -> Value {
    new_set(HashRep::Small, pop_n(vm, argc))
}

/// `MAKE_LISTBUFFER` / `MAKE_ARRAYBUFFER` builtins — pop `argc` elements into a
/// growable buffer.
fn b_make_listbuffer(vm: &mut VM, argc: u8) -> Value {
    new_seq(SeqKind::ListBuffer, pop_n(vm, argc))
}

fn b_make_arraybuffer(vm: &mut VM, argc: u8) -> Value {
    new_seq(SeqKind::ArrayBuffer, pop_n(vm, argc))
}

/// `FROM_SEQ` builtin — pop the constructor name and the source collection, and
/// build that constructor's collection from the source's elements.
fn b_from_seq(vm: &mut VM, _argc: u8) -> Value {
    let ctor = vm.pop().as_str_cow().into_owned();
    let src = vm.pop();
    // A `Map`-shaped source spreads as its `(k, v)` pairs, matching what
    // `Map(m.toList: _*)` hands the factory.
    let items = match as_seq_or_tuple(&src) {
        Some(xs) => xs,
        None => match as_map(&src) {
            Some(m) => m.into_iter().map(new_pair_of).collect(),
            None => vec![src.clone()],
        },
    };
    let pairs = |vm: &mut VM| -> Option<Vec<(Value, Value)>> {
        let mut out = Vec::with_capacity(items.len());
        for p in &items {
            match as_seq_or_tuple(p) {
                Some(t) if t.len() == 2 => out.push((t[0].clone(), t[1].clone())),
                _ => {
                    fault(vm, "scalars: Map(...) expects `key -> value` pairs");
                    return None;
                }
            }
        }
        Some(out)
    };
    match ctor.as_str() {
        "List" => new_seq(SeqKind::List, items),
        "Vector" => new_seq(SeqKind::Vector, items),
        "ArraySeq" => new_seq(SeqKind::ArraySeq, items),
        "Array" => new_seq(SeqKind::Array, items),
        "ListBuffer" => new_seq(SeqKind::ListBuffer, items),
        "ArrayBuffer" => new_seq(SeqKind::ArrayBuffer, items),
        "Queue" => new_seq(SeqKind::Queue, items),
        "Stack" => new_seq(SeqKind::Stack, items),
        "ArrayDeque" => new_seq(SeqKind::ArrayDeque, items),
        "PriorityQueue" => new_priority_queue(items),
        "StringBuilder" => new_seq(SeqKind::StrBuf, items.iter().flat_map(str_chars).collect()),
        "Set" => new_set(HashRep::Small, items),
        "LinkedHashSet" => new_set(HashRep::Linked, items),
        "mutable.Set" => mut_set_from(mut_initial_len(items.len()), items),
        "Map" => match pairs(vm) {
            Some(e) => new_map(HashRep::Small, e),
            None => Value::Undef,
        },
        "LinkedHashMap" => match pairs(vm) {
            Some(e) => new_map(HashRep::Linked, e),
            None => Value::Undef,
        },
        "mutable.Map" => match pairs(vm) {
            Some(e) => mut_map_from(mut_initial_len(items.len()), e),
            None => Value::Undef,
        },
        other => fault(
            vm,
            format!("scalars: unknown collection constructor `{other}`"),
        ),
    }
}

/// `MAKE_QUEUE` / `MAKE_STACK` / `MAKE_ARRAYDEQUE` builtins — pop `argc`
/// elements into the matching `scala.collection.mutable` buffer.
fn b_make_queue(vm: &mut VM, argc: u8) -> Value {
    new_seq(SeqKind::Queue, pop_n(vm, argc))
}

fn b_make_stack(vm: &mut VM, argc: u8) -> Value {
    new_seq(SeqKind::Stack, pop_n(vm, argc))
}

fn b_make_arraydeque(vm: &mut VM, argc: u8) -> Value {
    new_seq(SeqKind::ArrayDeque, pop_n(vm, argc))
}

// ── mutable.PriorityQueue: the raw heap array's order ──────────────────────
//
// Scala's `PriorityQueue` is a binary max-heap in an `ArrayBuffer` whose index 0
// is unused, and its `toString`/iteration expose that array VERBATIM. So the
// printed order is an artifact of the exact sift algorithm, and only Scala's own
// `fixUp`/`fixDown`/`heapify` reproduce it — `PriorityQueue(3,1,4,1,5,9,2,6)`
// prints `PriorityQueue(9, 6, 4, 1, 5, 3, 2, 1)`, which is neither the input
// order, nor sorted, nor what repeated sift-up insertion would leave
// (`9, 6, 5, 4, 1, 3, 2, 1`).
//
// The three below are ports of `scala.collection.mutable.PriorityQueue`. Scala's
// array is 1-based; `items` here is 0-based, so heap position `k` is
// `items[k - 1]` throughout — `pos` does that one conversion.

/// The element at 1-based heap position `k`.
fn pos(items: &[Value], k: usize) -> &Value {
    &items[k - 1]
}

/// Scala's `fixUp`: sift the element at position `m` toward the root while it is
/// greater than its parent. A tie does NOT swap (the comparison is strict),
/// which is what keeps equal elements in the order they arrived.
fn fix_up(vm: &mut VM, items: &mut [Value], m: usize) -> Result<(), String> {
    let mut k = m;
    while k > 1 {
        if cmp_vm(vm, pos(items, k / 2), pos(items, k))? != Ordering::Less {
            break;
        }
        items.swap(k - 1, k / 2 - 1);
        k /= 2;
    }
    Ok(())
}

/// Scala's `fixDown`: sift the element at position `m` toward the leaves, always
/// through the GREATER child, over a heap whose last position is `n`.
fn fix_down(vm: &mut VM, items: &mut [Value], m: usize, n: usize) -> Result<(), String> {
    let mut k = m;
    while n >= 2 * k {
        let mut j = 2 * k;
        if j < n && cmp_vm(vm, pos(items, j), pos(items, j + 1))? == Ordering::Less {
            j += 1;
        }
        if cmp_vm(vm, pos(items, k), pos(items, j))? != Ordering::Less {
            return Ok(());
        }
        items.swap(k - 1, j - 1);
        k = j;
    }
    Ok(())
}

/// Scala's `heapify(start)`: restore the heap property over positions
/// `start..=n`.
///
/// The two branches are Scala's, and they are NOT interchangeable — they leave
/// different arrays, and the array is what prints. Building from empty
/// (`start == 1`) is Floyd's bottom-up `fixDown` sweep; adding a few elements to
/// an existing heap sifts each new arrival UP instead.
fn heapify(vm: &mut VM, items: &mut [Value], start: usize) -> Result<(), String> {
    let n = items.len();
    if start == 1 {
        let mut i = n / 2;
        while i >= 1 {
            fix_down(vm, items, i, n)?;
            i -= 1;
        }
    } else {
        for i in start..=n {
            fix_up(vm, items, i)?;
        }
    }
    Ok(())
}

/// A `PriorityQueue` over `items`, built as Scala's builder does: every element
/// appended raw, then one `heapify` over the whole array.
///
/// Ordering failures cannot be reported from here, so an element pair the
/// runtime cannot compare leaves the array in whatever order the partial sweep
/// reached — the same non-answer `sorted` gives such a pair.
fn new_priority_queue_vm(vm: &mut VM, mut items: Vec<Value>) -> Value {
    let _ = heapify(vm, &mut items, 1);
    new_seq(SeqKind::PriorityQueue, items)
}

/// [`new_priority_queue_vm`] for the callers with no VM in hand. A user
/// `Ordered` class cannot be consulted without one, so this orders by the
/// structural [`value_cmp`] — which is what every non-user type uses anyway.
fn new_priority_queue(items: Vec<Value>) -> Value {
    let mut items = items;
    let n = items.len();
    // The same Floyd sweep as `heapify(_, 1)`, against the infallible comparison.
    let mut i = n / 2;
    while i >= 1 {
        let mut k = i;
        while n >= 2 * k {
            let mut j = 2 * k;
            if j < n && value_cmp(&items[j - 1], &items[j]) == Ordering::Less {
                j += 1;
            }
            if value_cmp(&items[k - 1], &items[j - 1]) != Ordering::Less {
                break;
            }
            items.swap(k - 1, j - 1);
            k = j;
        }
        i -= 1;
    }
    new_seq(SeqKind::PriorityQueue, items)
}

/// `MAKE_PRIORITYQUEUE` builtin — see [`MAKE_PRIORITYQUEUE`].
fn b_make_priorityqueue(vm: &mut VM, argc: u8) -> Value {
    let items = pop_n(vm, argc);
    new_priority_queue_vm(vm, items)
}

/// The `PriorityQueue`-specific members, which the generic sequence dispatcher
/// would answer wrongly: `+=` must sift rather than append, and `dequeue`/`head`
/// read the heap's root rather than the sequence's front. `None` falls through
/// to the shared read-only paths (`size`, `isEmpty`, `toList`, `mkString`,
/// `foreach`, …), all of which see the raw array exactly as Scala's do.
fn priority_queue_method(
    vm: &mut VM,
    recv: &Value,
    items: &[Value],
    name: &str,
    args: &[Value],
) -> Option<Result<Value, String>> {
    let set = |v: Vec<Value>| set_seq_items(recv, SeqKind::PriorityQueue, v);
    Some(match (name, args.len()) {
        // `addOne`: append, then sift the new arrival up.
        ("+=" | "addOne" | "enqueue", 1) => {
            let mut v = items.to_vec();
            v.push(args[0].clone());
            let n = v.len();
            if let Err(e) = fix_up(vm, &mut v, n) {
                return Some(Err(e));
            }
            set(v);
            Ok(recv.clone())
        }
        // `addAll`: append them ALL, then one `heapify` from the first new
        // position — not a sift per element, which would leave a different array.
        ("++=" | "addAll" | "enqueue", _) if !args.is_empty() => {
            let mut v = items.to_vec();
            let start = v.len() + 1;
            for a in args {
                match as_seq_or_tuple(a) {
                    Some(xs) if name != "enqueue" => v.extend(xs.iter().cloned()),
                    _ => v.push(a.clone()),
                }
            }
            if let Err(e) = heapify(vm, &mut v, start) {
                return Some(Err(e));
            }
            set(v);
            Ok(recv.clone())
        }
        ("dequeue", 0) => {
            if items.is_empty() {
                return Some(Err(
                    "scalars: java.util.NoSuchElementException: no element to remove from heap"
                        .into(),
                ));
            }
            let mut v = items.to_vec();
            let head = v.swap_remove(0);
            let n = v.len();
            if n > 1 {
                if let Err(e) = fix_down(vm, &mut v, 1, n) {
                    return Some(Err(e));
                }
            }
            set(v);
            Ok(head)
        }
        // `dequeueAll` drains into an `ArraySeq`, so it IS the sorted order.
        ("dequeueAll", 0) => {
            let mut v = items.to_vec();
            let mut out = Vec::with_capacity(v.len());
            while !v.is_empty() {
                let head = v.swap_remove(0);
                out.push(head);
                let n = v.len();
                if n > 1 {
                    if let Err(e) = fix_down(vm, &mut v, 1, n) {
                        return Some(Err(e));
                    }
                }
            }
            set(v);
            Ok(new_seq(SeqKind::ArraySeq, out))
        }
        // Both read the heap root, but they fail differently: `head` reports the
        // queue, `max` reports the generic empty-reduction.
        ("head" | "max", 0) => match items.first() {
            Some(v) => Ok(v.clone()),
            None if name == "head" => Err(SeqKind::PriorityQueue.empty_fault(EmptyOp::Head)),
            None => Err("scalars: java.lang.UnsupportedOperationException: empty.max".into()),
        },
        ("headOption", 0) => Ok(opt(items.first().cloned())),
        ("clone", 0) => Ok(new_seq(SeqKind::PriorityQueue, items.to_vec())),
        ("clear", 0) => {
            set(Vec::new());
            Ok(unit_value())
        }
        _ => return None,
    })
}

/// `MAKE_STRINGBUILDER` builtin — pop `argc` values and seed the builder with
/// each one's characters, so `new StringBuilder("ab")` starts at `ab` and the
/// no-argument `new StringBuilder` starts empty.
fn b_make_stringbuilder(vm: &mut VM, argc: u8) -> Value {
    let seeds = pop_n(vm, argc);
    new_seq(SeqKind::StrBuf, seeds.iter().flat_map(str_chars).collect())
}

/// `MAKE_LINKEDSET` builtin — pop `argc` elements into an insertion-ordered
/// `mutable.LinkedHashSet`.
fn b_make_linkedset(vm: &mut VM, argc: u8) -> Value {
    new_set(HashRep::Linked, pop_n(vm, argc))
}

/// `MAKE_LINKEDMAP` builtin — the `mutable.LinkedHashMap` counterpart.
fn b_make_linkedmap(vm: &mut VM, argc: u8) -> Value {
    let pairs = pop_n(vm, argc);
    let mut entries: Vec<(Value, Value)> = Vec::with_capacity(pairs.len());
    for p in &pairs {
        match as_seq_or_tuple(p) {
            Some(t) if t.len() == 2 => entries.push((t[0].clone(), t[1].clone())),
            _ => return fault(vm, "scalars: Map(...) expects `key -> value` pairs"),
        }
    }
    new_map(HashRep::Linked, entries)
}

/// `MAKE_MUTSET` builtin — pop `argc` elements into a `mutable.HashSet` whose
/// table starts where `HashSet.from` would put it for `argc` inputs.
fn b_make_mutset(vm: &mut VM, argc: u8) -> Value {
    mut_set_from(mut_initial_len(argc as usize), pop_n(vm, argc))
}

/// `MAKE_MUTMAP` builtin — the `mutable.HashMap` counterpart.
fn b_make_mutmap(vm: &mut VM, argc: u8) -> Value {
    let pairs = pop_n(vm, argc);
    let mut entries: Vec<(Value, Value)> = Vec::with_capacity(pairs.len());
    for p in &pairs {
        match as_seq_or_tuple(p) {
            Some(t) if t.len() == 2 => entries.push((t[0].clone(), t[1].clone())),
            _ => return fault(vm, "scalars: Map(...) expects `key -> value` pairs"),
        }
    }
    mut_map_from(mut_initial_len(argc as usize), entries)
}

/// `IS_GROWABLE` builtin — whether the popped value mutates in place.
fn b_is_growable(vm: &mut VM, _argc: u8) -> Value {
    let v = vm.pop();
    let mutable_seq = seq_kind_items(&v).is_some_and(|(k, _)| k.is_mutable());
    let mutable_map = matches!(
        map_rep_entries(&v),
        Some((HashRep::Mutable(_) | HashRep::Linked, _))
    );
    Value::bool(mutable_seq || mutable_map)
}

/// `SADD` builtin — Scala `+`, able to run a user `toString` override.
///
/// Every answer here is the one [`numeric_hook`] gives for the same pair, which
/// is what `Op::Add` would have reached; the ONE difference is that a
/// concatenation renders its operands with [`scala_str_vm`] rather than
/// [`scala_str`], so an override is honoured. The two concatenating shapes are
/// Scala 3's only ones (there is no `any2stringadd`): a `String` LEFT operand
/// takes anything, and a numeric left takes a `String` right. Everything else —
/// `Set`/`Map` `+`, `Char` arithmetic, `Long` wrap, `Int`/`Double` promotion,
/// and the rejections — is delegated unchanged.
fn b_add(vm: &mut VM, _argc: u8) -> Value {
    let b = vm.stack.pop().unwrap_or(Value::Undef);
    let a = vm.stack.pop().unwrap_or(Value::Undef);
    // Suppressed while unwinding: an operand is the `Undef` a raise left behind,
    // and rejecting it here would displace the real exception.
    if unwinding() {
        return Value::Undef;
    }
    let concatenates = matches!(a, Value::Str(_))
        || (matches!(b, Value::Str(_)) && matches!(a, Value::Int(_) | Value::Float(_)));
    if concatenates {
        let left = scala_str_vm(vm, &a);
        let right = scala_str_vm(vm, &b);
        return Value::str(format!("{left}{right}"));
    }
    // Two `Double`s never reach the hook (the VM adds them itself), so answer
    // them here rather than falling into its rejections.
    if let (Value::Float(x), Value::Float(y)) = (&a, &b) {
        return Value::float(x + y);
    }
    match numeric_hook(NumOp::Add, &a, &b) {
        Ok(v) => v,
        Err(e) => fault(vm, e),
    }
}

/// The class tag of a parked non-local return (see [`NLR_RAISE`]).
const NLR_CLASS: &str = "scala.runtime.NonLocalReturnControl";

/// `NLR_RAISE` builtin — see [`NLR_RAISE`].
fn b_nlr_raise(vm: &mut VM, _argc: u8) -> Value {
    let v = vm.pop();
    // An exception already unwinding wins, exactly as in `raise`: the side
    // effects that would have produced this return are suppressed anyway.
    if unwinding() {
        return Value::Undef;
    }
    let ctl = heap_alloc(ScalaObj {
        class: Arc::from(NLR_CLASS),
        is_case: false,
        is_object: false,
        fields: vec![(Arc::from("value"), v)],
    });
    PENDING.with(|p| *p.borrow_mut() = Some(ctl));
    let _ = vm;
    Value::Undef
}

/// `NLR_TAKE` builtin — see [`NLR_TAKE`].
fn b_nlr_take(_vm: &mut VM, _argc: u8) -> Value {
    let is_nlr = PENDING.with(|p| {
        p.borrow()
            .as_ref()
            .and_then(|v| with_obj(v, |o| &*o.class == NLR_CLASS))
            .unwrap_or(false)
    });
    if !is_nlr {
        return Value::Undef;
    }
    let ctl = PENDING.with(|p| p.borrow_mut().take());
    ctl.and_then(|v| with_obj(&v, |o| o.fields.first().map(|(_, v)| v.clone())))
        .flatten()
        .unwrap_or(Value::Undef)
}

/// `CELL_NEW` builtin — see [`CELL_NEW`].
fn b_cell_new(vm: &mut VM, _argc: u8) -> Value {
    let v = vm.pop();
    heap_push(HeapVal::Cell(v))
}

/// `CELL_GET` builtin — see [`CELL_GET`].
fn b_cell_get(vm: &mut VM, _argc: u8) -> Value {
    let cell = vm.pop();
    let Value::Obj(id) = cell else {
        return Value::Undef;
    };
    HEAP.with(|h| match h.borrow().get(id as usize) {
        Some(HeapVal::Cell(v)) => v.clone(),
        _ => Value::Undef,
    })
}

/// `CELL_SET` builtin — see [`CELL_SET`].
fn b_cell_set(vm: &mut VM, _argc: u8) -> Value {
    let cell = vm.pop();
    let v = vm.pop();
    if let Value::Obj(id) = cell {
        HEAP.with(|h| {
            if let Some(HeapVal::Cell(slot)) = h.borrow_mut().get_mut(id as usize) {
                *slot = v;
            }
        });
    }
    Value::Undef
}

/// `MAKE_OPTION` builtin — see [`MAKE_OPTION`].
fn b_make_option(vm: &mut VM, _argc: u8) -> Value {
    let v = vm.pop();
    if matches!(v, Value::Undef) {
        make_none()
    } else {
        make_some(v)
    }
}

/// Pop `argc` stack values into source order (the deepest is the first element).
fn pop_n(vm: &mut VM, argc: u8) -> Vec<Value> {
    let mut items = Vec::with_capacity(argc as usize);
    for _ in 0..argc {
        items.push(vm.pop());
    }
    items.reverse();
    items
}

/// `MAKE_TUPLE` builtin — pop `argc` element values (deepest first) into a tuple.
fn b_make_tuple(vm: &mut VM, argc: u8) -> Value {
    let mut items = Vec::with_capacity(argc as usize);
    for _ in 0..argc {
        items.push(vm.pop());
    }
    items.reverse();
    heap_push(HeapVal::Tuple(items))
}

/// `MAKE_MAP` builtin — pop `argc` `Tuple2` pair values (deepest first) into a
/// map. A duplicate key keeps its first position with the last value (Scala's
/// `Map(a -> 1, a -> 2)` == `Map(a -> 2)`); five or more entries make a
/// `HashMap`, printed in trie order.
fn b_make_map(vm: &mut VM, argc: u8) -> Value {
    let mut pairs = Vec::with_capacity(argc as usize);
    for _ in 0..argc {
        pairs.push(vm.pop());
    }
    pairs.reverse();
    let mut entries: Vec<(Value, Value)> = Vec::with_capacity(pairs.len());
    for p in &pairs {
        let (k, v) = match as_seq_or_tuple(p) {
            Some(t) if t.len() == 2 => (t[0].clone(), t[1].clone()),
            _ => return fault(vm, "scalars: Map(...) expects `key -> value` pairs"),
        };
        map_put(&mut entries, k, v);
    }
    new_map(HashRep::Small, entries)
}

/// `LIST_CONS` builtin (`head :: tail`) — pop the tail `List` and the head, and
/// return a new `List` with `head` prepended.
fn b_list_cons(vm: &mut VM, _argc: u8) -> Value {
    let tail = vm.pop();
    let head = vm.pop();
    let mut items = match as_seq(&tail) {
        Some(t) => t,
        None => return fault(vm, "scalars: `::` right operand is not a List"),
    };
    items.insert(0, head);
    heap_push(HeapVal::Seq(SeqKind::List, items))
}

/// `RANGE_LIST` builtin — materialize `start until|to end by step` as a
/// `Vector`. The stack holds `start`, `end`, the `inclusive` `Bool`, then the
/// step (on top; `1` when the source had no `by` clause).
///
/// The walk is written as an explicit `while` rather than a Rust `step_by`
/// because Scala's step may be negative, which `Iterator::step_by` cannot
/// express. A zero step is a Scala `IllegalArgumentException`, raised here so
/// the materializing path (a range feeding a collection generator) reports it
/// identically to the counted-loop path's compile-time guard.
fn b_range_list(vm: &mut VM, _argc: u8) -> Value {
    let step = vm.pop().to_int();
    let inclusive = matches!(vm.pop(), Value::Bool(true));
    let end_v = vm.pop();
    let start_v = vm.pop();
    if let Err(e) = reject_char_endpoint(&start_v, &end_v) {
        return fault(vm, e);
    }
    let end = end_v.to_int();
    let start = start_v.to_int();
    if step == 0 {
        return fault(vm, "java.lang.IllegalArgumentException: step cannot be 0.");
    }
    let mut items = Vec::new();
    let mut i = start;
    while if step > 0 {
        if inclusive {
            i <= end
        } else {
            i < end
        }
    } else if inclusive {
        i >= end
    } else {
        i > end
    } {
        items.push(Value::int(i));
        i += step;
    }
    // A `Range`'s `map`/`flatMap` yields an `IndexedSeq` (`Vector`); model the
    // materialized range as a `Vector` so a range-led comprehension renders as
    // Scala's `Vector(...)`, not `List(...)`.
    heap_push(HeapVal::Seq(SeqKind::Vector, items))
}

/// `'a' to 'e'` is a `NumericRange[Char]`, which this frontend does not build.
/// A `Char` endpoint would read as an integer here and the range would come out
/// silently wrong, so it is refused instead. The compiler already refuses the
/// literal spelling; this catches the endpoints that arrive as values.
fn reject_char_endpoint(start: &Value, end: &Value) -> Result<(), String> {
    if as_char(start).is_some() || as_char(end).is_some() {
        return Err("scalars: a Char range (`'a' to 'z'`) is not modeled — its \
                    NumericRange[Char] has no representation here"
            .to_string());
    }
    Ok(())
}

/// Read a tuple/seq's elements (a `Tuple2` is a 2-element sequence).
fn as_seq_or_tuple(v: &Value) -> Option<Vec<Value>> {
    if let Value::Obj(id) = v {
        HEAP.with(|h| match h.borrow().get(*id as usize) {
            Some(HeapVal::Tuple(t)) | Some(HeapVal::Seq(_, t)) => Some(t.clone()),
            _ => None,
        })
    } else {
        None
    }
}

/// Insert/update `(k, v)` in an ordered entry list: update in place if `k` is
/// already present (keeping its position), else append.
fn map_put(entries: &mut Vec<(Value, Value)>, k: Value, v: Value) {
    match entries.iter_mut().find(|(ek, _)| value_eq(ek, &k)) {
        Some(slot) => slot.1 = v,
        None => entries.push((k, v)),
    }
}

/// `APPLY` builtin — the universal `receiver(args)` dispatch (Scala `apply`).
/// The stack holds the receiver (deepest) then the `argc` args. A closure is
/// invoked; a `List`/`Tuple` is indexed; a `Map` is keyed.
fn b_apply(vm: &mut VM, argc: u8) -> Value {
    let k = argc as usize;
    let mut args = Vec::with_capacity(k);
    for _ in 0..k {
        args.push(vm.pop());
    }
    args.reverse();
    let recv = vm.pop();
    // Suppressed while unwinding: `recv` is garbage, so calling it would fault
    // again and displace the in-flight exception.
    if unwinding() {
        return Value::Undef;
    }
    match apply_value(vm, &recv, &args) {
        Ok(v) => v,
        Err(e) => fault(vm, e),
    }
}

/// Dispatch `recv(args)`: closure invocation, list/tuple indexing, or map lookup.
fn apply_value(vm: &mut VM, recv: &Value, args: &[Value]) -> Result<Value, String> {
    if is_function(recv) {
        return invoke_closure(vm, recv, args);
    }
    // `s(i)` is `StringOps.apply`, i.e. `charAt` — the same method the named
    // spelling `s.apply(i)` already reaches through `string_method`. It has to be
    // here too: a string receiver that is a *binding* rather than a literal
    // arrives as a value at this universal dispatch, not as a method call.
    if let Value::Str(s) = recv {
        return string_method(s, "charAt", args);
    }
    // `xs(i)` on a `LazyList` forces exactly the prefix it needs, so it reaches
    // the lazy dispatcher rather than the strict sequence one.
    if as_lazy(recv).is_some() {
        if let Some(r) = lazy_method(vm, recv, "apply", args) {
            return r;
        }
    }
    if let Value::Obj(id) = recv {
        let kind = HEAP.with(|h| {
            h.borrow().get(*id as usize).map(|o| match o {
                HeapVal::Seq(..) => 0u8,
                HeapVal::Tuple(_) => 1,
                HeapVal::Map(..) => 2,
                _ => 3,
            })
        });
        match kind {
            Some(0) | Some(1) => {
                // A `Set`'s `apply` is `contains`, not an index: `val s =
                // Set(1,2,3); s(2)` is `true`. It has to be answered here as
                // well as in `seq_method`, because a BOUND set reaches this
                // universal dispatch rather than the named-method path.
                if matches!(seq_kind(recv), Some(SeqKind::Set(_))) {
                    let items = as_seq_or_tuple(recv).unwrap_or_default();
                    let key = args.first().cloned().unwrap_or(Value::Undef);
                    return Ok(Value::bool(items.iter().any(|x| value_eq(x, &key))));
                }
                let i = args.first().map(|a| a.to_int()).unwrap_or(0);
                // ONE element, read under a single borrow. Copying the whole
                // receiver to answer one index is what made an indexed loop
                // quadratic — `while (i < n) s += v(i)` over a 20 000-element
                // `Vector` moved 400 million values to read 20 000 of them.
                //
                // Applying a BOUND sequence (`val b = …; b(5)`) must fail the
                // same way `Seq(…)(5)` does, so it reads the receiver's kind
                // rather than defaulting to `List`'s bare-index message. A
                // `Tuple` has no kind and keeps that bare index, which is what
                // `Tuple.productElement` raises.
                return HEAP.with(|h| {
                    let heap = h.borrow();
                    let (kind, items) = match heap.get(*id as usize) {
                        Some(HeapVal::Seq(k, items)) => (Some(*k), &items[..]),
                        Some(HeapVal::Tuple(items)) => (None, &items[..]),
                        _ => (None, &[][..]),
                    };
                    match usize::try_from(i).ok().and_then(|u| items.get(u)) {
                        Some(v) => Ok(v.clone()),
                        None => Err(match kind {
                            Some(k) => k.index_fault(i, items.len()),
                            None => {
                                format!("scalars: java.lang.IndexOutOfBoundsException: {i}")
                            }
                        }),
                    }
                });
            }
            Some(2) => {
                let m = as_map(recv).unwrap_or_default();
                let key = args.first().cloned().unwrap_or(Value::Undef);
                return map_get(&m, &key).ok_or_else(|| {
                    format!(
                        "scalars: java.util.NoSuchElementException: key not found: {}",
                        scala_str(&key)
                    )
                });
            }
            _ => {}
        }
    }
    Err(format!(
        "scalars: value {} cannot be applied to arguments",
        scala_str(recv)
    ))
}

/// Read element `i` of a list/tuple, or an out-of-bounds error.
///
/// The message is the bare index, matching `scala.collection.LinearSeqOps.apply`
/// — verified against the reference toolchain, and observable now that
/// `catch { case e => e.getMessage }` can read it.
fn list_index(items: &[Value], i: i64) -> Result<Value, String> {
    if i < 0 || i as usize >= items.len() {
        Err(format!("scalars: java.lang.IndexOutOfBoundsException: {i}"))
    } else {
        Ok(items[i as usize].clone())
    }
}

/// Look up `key` in an ordered map's entries (structural key equality).
fn map_get(entries: &[(Value, Value)], key: &Value) -> Option<Value> {
    entries
        .iter()
        .find(|(k, _)| value_eq(k, key))
        .map(|(_, v)| v.clone())
}

// ── Re-entrant closure/subroutine invocation ────────────────────────────────
//
// A collection op (`map`/`filter`/`foldLeft`/…) must run a closure body mid-op.
// This drives a *nested* `VM::run`: push a call frame whose `return_ip` is past
// the chunk end so the nested run halts exactly when the body's `ReturnValue`
// pops that frame, then save/restore the interpreter IP so the enclosing dispatch
// loop resumes cleanly. This is the same host-side first-class-closure pattern
// groovyrs uses — no fusevm change, closures live entirely in the frontend heap.

/// Run a subroutine body whose prologue values are already pushed above
/// `stack_base`, returning its result value.
fn run_sub(vm: &mut VM, entry: usize, stack_base: usize) -> Result<Value, String> {
    let return_ip = vm.chunk.ops.len();
    vm.frames.push(Frame {
        return_ip,
        stack_base,
        slots: Vec::new(),
        // Same identity `Op::Call` records: this frame enters the subroutine
        // at `entry`, so `Chunk::sub_slot_names` is reachable from it.
        entry_ip: Some(entry),
    });
    let saved_ip = vm.ip;
    vm.ip = entry;
    let result = vm.run();
    vm.ip = saved_ip;
    match result {
        VMResult::Ok(v) => Ok(v),
        VMResult::Halted => Ok(vm.stack.pop().unwrap_or(Value::Undef)),
        VMResult::Error(e) => Err(e),
    }
}

/// Invoke closure `clo` with `args`: push exactly `params` arguments (padding
/// with `null`, dropping extras), then the captured upvalues in declaration
/// order, and run the body. The prologue pops params+captures into slots.
fn invoke_closure(vm: &mut VM, clo: &Value, args: &[Value]) -> Result<Value, String> {
    if let Some(d) = as_derived(clo) {
        return invoke_derived(vm, &d, args);
    }
    let meta = as_closure(clo).ok_or_else(|| "scalars: value is not a function".to_string())?;
    invoke_body(vm, &meta, meta.name_idx, args)
}

/// Apply a composed function value (see [`DerivedFn`]).
fn invoke_derived(vm: &mut VM, d: &DerivedFn, args: &[Value]) -> Result<Value, String> {
    let arg = args.first().cloned().unwrap_or(Value::Undef);
    match d {
        DerivedFn::Lift(pf) => {
            if is_defined_at(vm, pf, &arg)? {
                Ok(make_some(invoke_closure(vm, pf, args)?))
            } else {
                Ok(make_none())
            }
        }
        DerivedFn::OrElse(a, b) => {
            if is_defined_at(vm, a, &arg)? {
                invoke_closure(vm, a, args)
            } else {
                invoke_closure(vm, b, args)
            }
        }
        DerivedFn::AndThen(f, g) => {
            let mid = invoke_closure(vm, f, args)?;
            invoke_closure(vm, g, std::slice::from_ref(&mid))
        }
        DerivedFn::Compose(f, g) => {
            let mid = invoke_closure(vm, g, args)?;
            invoke_closure(vm, f, std::slice::from_ref(&mid))
        }
    }
}

/// Whether the function value `clo` is defined at `arg` — Scala's
/// `PartialFunction.isDefinedAt`. A plain lambda is total, so it answers `true`
/// without running anything; a `{ case … }` literal runs its derived pattern
/// test, which evaluates the patterns and guards but never an arm body.
fn is_defined_at(vm: &mut VM, clo: &Value, arg: &Value) -> Result<bool, String> {
    if let Some(d) = as_derived(clo) {
        return match &d {
            // `lift` is total; `andThen`/`compose` are defined wherever the
            // function they feed from is.
            DerivedFn::Lift(_) => Ok(true),
            DerivedFn::AndThen(f, _) | DerivedFn::Compose(_, f) => is_defined_at(vm, f, arg),
            DerivedFn::OrElse(a, b) => Ok(is_defined_at(vm, a, arg)? || is_defined_at(vm, b, arg)?),
        };
    }
    let meta = as_closure(clo).ok_or_else(|| "scalars: value is not a function".to_string())?;
    let Some(idx) = meta.defined_idx else {
        return Ok(true);
    };
    Ok(truthy(&invoke_body(
        vm,
        &meta,
        idx,
        std::slice::from_ref(arg),
    )?))
}

/// Run one of closure `meta`'s bodies (`name_idx`) with `args`. Both a partial
/// function's `apply` and its `isDefinedAt` share the parameter/capture layout,
/// so the same argument marshalling serves either.
fn invoke_body(
    vm: &mut VM,
    meta: &Closure,
    name_idx: u16,
    args: &[Value],
) -> Result<Value, String> {
    let entry = vm
        .chunk
        .find_sub(name_idx)
        .ok_or_else(|| "scalars: closure body not found".to_string())?;
    let want = meta.params as usize;
    // A pattern-matching anonymous function (`{ case (a, b) => … }`) is one
    // parameter matched against its argument, and Scala unifies that with the
    // `FunctionN` a two-argument caller like `foldLeft` wants by tupling the
    // arguments. Do the same when a caller passes more values than the closure
    // declares.
    let tupled;
    let args = if want == 1 && args.len() > 1 {
        tupled = [heap_push(HeapVal::Tuple(args.to_vec()))];
        &tupled[..]
    } else {
        args
    };
    let stack_base = vm.stack.len();
    for i in 0..want {
        vm.stack.push(args.get(i).cloned().unwrap_or(Value::Undef));
    }
    for cap in &meta.captures {
        vm.stack.push(cap.clone());
    }
    run_sub(vm, entry, stack_base)
}

/// Resolve a method/field access on a heap object (the `Value::Obj` arm of
/// [`dispatch_method`]). Handles `hashCode`/`equals` and paren-less field reads;
/// `toString` is handled generically by the caller via [`scala_str`].
fn obj_method(recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    let (class, is_case, fields) =
        with_obj(recv, |o| (o.class.to_string(), o.is_case, o.fields.clone()))
            .ok_or_else(|| "scalars: dangling object handle".to_string())?;
    match (name, args.len()) {
        ("hashCode", 0) => Ok(Value::int(obj_hash(&class, is_case, &fields, recv))),
        ("equals", 1) => Ok(Value::bool(obj_eq(recv, &args[0]))),
        // `scala.Product`, which every `case class`/`case object` implements.
        // The primary-constructor prefix is what Scala exposes, matching the
        // derived `unapply` and `toString`.
        ("productArity", 0) if is_case => Ok(Value::int(ctor_fields(&class, &fields).len() as i64)),
        ("productPrefix", 0) if is_case => Ok(Value::str(class.clone())),
        ("productIterator" | "productElementNames", 0) if is_case => {
            let ctor = ctor_fields(&class, &fields);
            Ok(new_list(if name == "productIterator" {
                ctor.iter().map(|(_, v)| v.clone()).collect()
            } else {
                ctor.iter()
                    .map(|(n, _)| Value::str(n.to_string()))
                    .collect()
            }))
        }
        ("productElement" | "productElementName", 1) if is_case => {
            let ctor = ctor_fields(&class, &fields);
            let i = args[0].to_int();
            match usize::try_from(i).ok().and_then(|i| ctor.get(i)) {
                Some((n, v)) => Ok(if name == "productElement" {
                    v.clone()
                } else {
                    Value::str(n.to_string())
                }),
                None => Err(format!(
                    "scalars: java.lang.IndexOutOfBoundsException: {i} is out of bounds (min 0, max {})",
                    ctor.len().saturating_sub(1)
                )),
            }
        }
        // A paren-less access naming a field reads that field.
        (_, 0) => match fields.iter().find(|(fname, _)| &**fname == name) {
            Some((_, v)) => Ok(v.clone()),
            None => Err(no_such_obj_member(&class, name)),
        },
        _ => Err(no_such_obj_member(&class, name)),
    }
}

/// The class tag of the record a `getClass` answers.
const CLASS_CLASS: &str = "java.lang.Class";

/// `xs.sortBy(f)(ord)` — sort by `f`'s result under `ord`, i.e. `ord.on(f)`.
fn make_keyed_ordering(vm: &mut VM, f: &Value, ord: &Value) -> Result<Value, String> {
    match ordering_method(vm, ord, "on", std::slice::from_ref(f)) {
        Some(r) => r,
        None => Err("scalars: expected an Ordering".into()),
    }
}

/// `xs.max(ord)` / `xs.min(ord)` and their `By` forms: the extreme element under
/// `ord`. Ties keep the FIRST element, as Scala's `reduceLeft`-based `max` does.
fn best_by_ordering(
    vm: &mut VM,
    ord: &Value,
    items: &[Value],
    name: &str,
) -> Result<Value, String> {
    let Some(first) = items.first() else {
        return Err(format!(
            "scalars: java.lang.UnsupportedOperationException: empty.{name}"
        ));
    };
    let want = if name.starts_with("max") {
        Ordering::Greater
    } else {
        Ordering::Less
    };
    let mut best = first.clone();
    for it in &items[1..] {
        if ordering_cmp(vm, ord, it, &best)? == want {
            best = it.clone();
        }
    }
    Ok(best)
}

/// Sort `items` under `ord`, stably. Goes through [`merge_sort_idx`] rather than
/// `slice::sort_by` because the comparison runs a user closure, which may raise
/// and which `sort_by` would panic on if it were inconsistent.
fn sort_by_ordering(vm: &mut VM, ord: &Value, items: &[Value]) -> Result<Vec<Value>, String> {
    let idx = merge_sort_idx(vm, items.len(), &mut |vm, i, j| {
        ordering_cmp(vm, ord, &items[i], &items[j])
    })?;
    Ok(idx.into_iter().map(|i| items[i].clone()).collect())
}

/// `x.getClass` — a `java.lang.Class` record carrying the two names a program
/// reads off it. The names are stored as FIELDS called `getName`/`getSimpleName`
/// so the ordinary paren-less field read in [`obj_method`] answers them.
///
/// Only the receivers whose JVM class this frontend can name faithfully are
/// answered. A collection's runtime class is a private implementation detail
/// (`List(1).getClass.getName` is `scala.collection.immutable.$colon$colon`, a
/// `Vector` of one is `Vector1`, and a `Tuple2[Int, Int]` carries the
/// specialization suffix `Tuple2$mcII$sp`) — reproducing those would mean
/// modelling Scala's class hierarchy and its `@specialized` naming, so they stay
/// an error rather than becoming a plausible-looking wrong answer.
fn class_of(recv: &Value) -> Result<Value, String> {
    // `(name, simple, primitive)`. A primitive's `Class.toString` is the bare
    // name (`int`); a reference type's is `class java.lang.String`.
    let (name, simple, primitive) = match recv {
        Value::Str(_) => ("java.lang.String".to_string(), "String".to_string(), false),
        Value::Int(_) => ("int".to_string(), "int".to_string(), true),
        Value::Float(_) => ("double".to_string(), "double".to_string(), true),
        Value::Status(_) => ("float".to_string(), "float".to_string(), true),
        Value::Bool(_) => ("boolean".to_string(), "boolean".to_string(), true),
        Value::Undef => ("void".to_string(), "void".to_string(), true),
        Value::Obj(_) if as_char(recv).is_some() => ("char".to_string(), "char".to_string(), true),
        // A built-in throwable knows its own fully-qualified JDK name.
        Value::Obj(_) if as_exc(recv).is_some() => {
            let n = as_exc(recv).expect("just matched").class.to_string();
            let simple = n.rsplit('.').next().unwrap_or(&n).to_string();
            (n, simple, false)
        }
        // A user `class`/`case class`/`object`. There are no packages here, so
        // the qualified and simple names coincide — except for an `object`,
        // whose JVM class carries the `$` suffix Scala appends.
        Value::Obj(_) => {
            let Some((class, is_object)) = with_obj(recv, |o| (o.class.to_string(), o.is_object))
            else {
                return Err(no_such_method(recv, "getClass"));
            };
            let n = if is_object {
                format!("{class}$")
            } else {
                class
            };
            (n.clone(), n, false)
        }
        _ => return Err(no_such_method(recv, "getClass")),
    };
    Ok(class_record(&name, &simple, primitive))
}

/// The `java.lang.Class` record `getClass` answers. The names are stored as
/// FIELDS called `getName`/`getSimpleName` so the ordinary paren-less field read
/// in [`obj_method`] answers them.
fn class_record(name: &str, simple: &str, primitive: bool) -> Value {
    heap_alloc(ScalaObj {
        class: Arc::from(CLASS_CLASS),
        is_case: false,
        is_object: false,
        fields: vec![
            (Arc::from("getName"), Value::str(name.to_string())),
            (Arc::from("getSimpleName"), Value::str(simple.to_string())),
            (Arc::from("isPrimitive"), Value::bool(primitive)),
        ],
    })
}

/// The primary-constructor prefix of a record's fields — exactly what Scala's
/// derived `Product`/`unapply`/`toString` expose (an inherited or body-declared
/// field is not a product element).
fn ctor_fields<'a>(class: &str, fields: &'a [(Arc<str>, Value)]) -> &'a [(Arc<str>, Value)] {
    &fields[..ctor_arity(class, fields.len()).min(fields.len())]
}

/// The Scala "value … is not a member of …" message for an unresolved access on
/// an instance of `class`.
fn no_such_obj_member(class: &str, name: &str) -> String {
    format!("scalars: value {name} is not a member of {class}")
}

/// Render a heap object as Scala would. A `case` instance is `Class(f0,f1,…)`
/// (fields in declared order via `toString`, comma-joined with no space); a
/// plain instance is `Class@<hex>` (the handle stands in for the JVM identity
/// hash). A collection renders `List(e0, e1)` / `Set(…)` / `Iterable(…)`; a map
/// `Map(k -> v, …)`; a tuple `(a,b)`; a function `<functionN>`.
fn obj_to_string(v: &Value) -> String {
    let id = if let Value::Obj(i) = v { *i } else { 0 };
    HEAP.with(|h| {
        let h = h.borrow();
        match h.get(id as usize) {
            Some(HeapVal::Record(o)) => {
                // `java.lang.Class.toString` is `class <name>` for a reference
                // type and the bare name for a primitive (`int`, `void`).
                if &*o.class == CLASS_CLASS {
                    let field = |n: &str| {
                        o.fields
                            .iter()
                            .find(|(f, _)| &**f == n)
                            .map(|(_, v)| v.clone())
                            .unwrap_or(Value::Undef)
                    };
                    let name = scala_str(&field("getName"));
                    return if truthy(&field("isPrimitive")) {
                        name
                    } else {
                        format!("class {name}")
                    };
                }
                if o.is_case && o.is_object {
                    // A `case object` prints as its bare name (`None`), not `None()`.
                    o.class.to_string()
                } else if o.is_case {
                    // Scala's derived `toString` renders the primary-constructor
                    // parameters only — a `val` declared in the body is not part
                    // of it.
                    let n = ctor_arity(&o.class, o.fields.len());
                    let inner = o.fields[..n]
                        .iter()
                        .map(|(_, val)| scala_str(val))
                        .collect::<Vec<_>>()
                        .join(",");
                    format!("{}({})", o.class, inner)
                } else {
                    format!("{}@{:x}", o.class, id)
                }
            }
            Some(HeapVal::Seq(
                SeqKind::Range {
                    start,
                    end,
                    inclusive,
                    step,
                },
                items,
            )) => range_to_string(*start, *end, *inclusive, *step, items.is_empty()),
            // `StringBuilder.toString` is the contents, not a rendered sequence.
            Some(HeapVal::Seq(SeqKind::StrBuf, items)) => {
                items.iter().map(scala_str).collect::<String>()
            }
            // `Iterator.toString` is a FIXED string in the standard library, not
            // the elements and not the JVM identity form — printing an iterator
            // must not consume it, so it cannot report what it holds.
            // A `LazyList` shows what it has FORCED and says the rest is not
            // computed — printing one must not force it, which is the whole
            // difference between this collection and every other.
            Some(HeapVal::Lazy(l)) => {
                let mut parts: Vec<String> = l.forced.iter().map(scala_str).collect();
                if !l.done {
                    parts.push("<not computed>".to_string());
                }
                format!("LazyList({})", parts.join(", "))
            }
            Some(HeapVal::Seq(SeqKind::Iterator, _)) => "<iterator>".to_string(),
            // A view does not show its contents: `List(1,2,3).view` is
            // `SeqView(<not computed>)`. An `Array`'s view is the exception and
            // renders its elements, so it falls through to the general arm.
            Some(HeapVal::Seq(k @ SeqKind::View(_), _)) => {
                format!("{}(<not computed>)", k.label())
            }
            Some(HeapVal::Seq(kind, items)) => {
                let inner = items.iter().map(scala_str).collect::<Vec<_>>().join(", ");
                format!("{}({inner})", kind.label())
            }
            Some(HeapVal::Map(rep, entries)) => {
                let inner = entries
                    .iter()
                    .map(|(k, val)| format!("{} -> {}", scala_str(k), scala_str(val)))
                    .collect::<Vec<_>>()
                    .join(", ");
                let label = match rep {
                    HashRep::Hashed | HashRep::Mutable(_) => "HashMap",
                    HashRep::Linked => "LinkedHashMap",
                    HashRep::Small => "Map",
                };
                format!("{label}({inner})")
            }
            Some(HeapVal::Tuple(items)) => {
                let inner = items.iter().map(scala_str).collect::<Vec<_>>().join(",");
                format!("({inner})")
            }
            Some(HeapVal::Closure(c)) => format!("<function{}>", c.params),
            // A composed function is always one-argument (`lift`/`orElse`/
            // `andThen`/`compose` all build a `Function1`).
            Some(HeapVal::Derived(_)) => "<function1>".to_string(),
            Some(HeapVal::Exc(e)) => exc_to_string(e),
            // `Regex.toString` is the source pattern; `Match.toString` is the
            // matched text (both as in Scala/`java.util.regex`).
            Some(HeapVal::Regex(p)) => p.to_string(),
            // Scala renders an `Ordering` as an anonymous-class identity string
            // (`scala.math.Ordering$$anon$1@1b6d3586`), which is unreproducible
            // and never depended on; name the class without the address.
            Some(HeapVal::Ordering { .. }) => "scala.math.Ordering".to_string(),
            Some(HeapVal::Match { matched, .. }) => matched.to_string(),
            // A `Char` renders as its one character, everywhere text conversion
            // applies: `println`, `toString`, interpolation, and as a collection
            // element (`List('a')` is `List(a)`).
            Some(HeapVal::Char(c)) => c.to_string(),
            // A boxed `var` is compiler-internal — every access goes through
            // `CELL_GET`/`CELL_SET`, so a cell handle never reaches user code.
            // Rendering the value it holds keeps a diagnostic dump readable.
            Some(HeapVal::Cell(v)) => scala_str(v),
            None => "null".to_string(),
        }
    })
}

// ── Scala hashing and CHAMP trie order ──────────────────────────────────────
//
// An immutable `Set`/`Map` of five or more entries is a CHAMP hash trie, and
// Scala prints it in *trie* order rather than insertion order — so
// `println(Set(9,3,1,2,7))` is `HashSet(1, 9, 2, 7, 3)`. Matching the reference
// compiler byte-for-byte therefore needs the three pieces the trie is built
// from, all ported here: the JVM/Scala `##` hash codes, the trie's `improve`
// scramble of them, and the depth-first traversal its iterator performs.

/// `java.lang.String.hashCode` — `h = 31*h + c` over the UTF-16 code units.
fn string_hash(s: &str) -> i32 {
    s.encode_utf16()
        .fold(0i32, |h, u| h.wrapping_mul(31).wrapping_add(u as i32))
}

/// `java.lang.Long.hashCode` — the two halves xored.
fn long_hash(v: i64) -> i32 {
    (v ^ ((v as u64) >> 32) as i64) as i32
}

/// `scala.runtime.Statics.doubleHash` — the `##` of a `Double`, which agrees
/// with `Int`/`Long`/`Float` hashes wherever the value is exactly representable
/// as one (so `2.0.## == 2` and a `Set[Any]` mixing `2` and `2.0` collides as
/// Scala's does).
fn double_hash(d: f64) -> i32 {
    let iv = d as i32;
    if f64::from(iv) == d {
        return iv;
    }
    let lv = d as i64;
    if lv as f64 == d {
        return long_hash(lv);
    }
    let fv = d as f32;
    if f64::from(fv) == d {
        return fv.to_bits() as i32;
    }
    long_hash(d.to_bits() as i64)
}

/// `scala.util.hashing.MurmurHash3.productSeed`.
const PRODUCT_SEED: i32 = 0xcafe_babeu32 as i32;

/// `MurmurHash3.mixLast`.
fn mm_mix_last(hash: i32, data: i32) -> i32 {
    let mut k = data as u32;
    k = k.wrapping_mul(0xcc9e_2d51);
    k = k.rotate_left(15);
    k = k.wrapping_mul(0x1b87_3593);
    (hash as u32 ^ k) as i32
}

/// `MurmurHash3.mix`.
fn mm_mix(hash: i32, data: i32) -> i32 {
    let h = (mm_mix_last(hash, data) as u32).rotate_left(13);
    (h.wrapping_mul(5).wrapping_add(0xe654_6b64)) as i32
}

/// `MurmurHash3.finalizeHash` — the length mixed in, then avalanched.
fn mm_finalize(hash: i32, length: i32) -> i32 {
    mm_avalanche(hash ^ length)
}

/// `MurmurHash3.avalanche`.
fn mm_avalanche(hash: i32) -> i32 {
    let mut h = hash as u32;
    h ^= h >> 16;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    h ^= h >> 16;
    h as i32
}

/// `MurmurHash3.orderedHash` — the hash every `Seq` uses, with the `Seq` seed.
///
/// Scala reaches this value through three different loops (`indexedSeqHash` for
/// an `IndexedSeq`, `listHash` for a `List`, `orderedHash` for anything else),
/// but all three compute the same number: each mixes every element in order and
/// then, when the elements form an arithmetic progression with a non-zero step,
/// substitutes `rangeHash(first, step, last)` so a `Range` and the `Vector` of
/// the same numbers agree. One implementation therefore serves them all.
fn ordered_hash(elems: &[Value], seed: i32) -> Option<i32> {
    let hs: Vec<i32> = elems.iter().map(scala_hash).collect::<Option<_>>()?;
    match hs.len() {
        0 => Some(mm_finalize(seed, 0)),
        1 => Some(mm_finalize(mm_mix(seed, hs[0]), 1)),
        _ => {
            let mut h = mm_mix(seed, hs[0]);
            let h0 = h;
            let mut prev = hs[1];
            let range_diff = prev.wrapping_sub(hs[0]);
            let mut i = 2;
            while i < hs.len() {
                h = mm_mix(h, prev);
                let hash = hs[i];
                // Not a progression (or a zero step): fall back to mixing the
                // rest element by element.
                if range_diff != hash.wrapping_sub(prev) || range_diff == 0 {
                    h = mm_mix(h, hash);
                    for &rest in &hs[i + 1..] {
                        h = mm_mix(h, rest);
                    }
                    return Some(mm_finalize(h, hs.len() as i32));
                }
                prev = hash;
                i += 1;
            }
            // `rangeHash(first, step, last, seed)`.
            Some(mm_avalanche(mm_mix(mm_mix(h0, range_diff), prev)))
        }
    }
}

/// `MurmurHash3.unorderedHash` — symmetric in its arguments, which is what a
/// `Set`'s and a `Map`'s hash need (their iteration order is an implementation
/// detail, but equal collections must hash equal).
fn unordered_hash(hashes: &[i32], seed: i32) -> i32 {
    let (mut a, mut b) = (0i32, 0i32);
    let mut c = 1i32;
    for &h in hashes {
        a = a.wrapping_add(h);
        b ^= h;
        c = c.wrapping_mul(h | 1);
    }
    let mut h = mm_mix(seed, a);
    h = mm_mix(h, b);
    h = mm_mix_last(h, c);
    mm_finalize(h, hashes.len() as i32)
}

/// `MurmurHash3.productHash` — the seed, the `productPrefix` hash, then every
/// element's `##`, finalized with the arity. A zero-arity product (a `case
/// object`) hashes as its prefix alone, matching Scala.
fn product_hash(prefix: &str, elems: &[Value]) -> Option<i32> {
    if elems.is_empty() {
        return Some(string_hash(prefix));
    }
    let mut h = mm_mix(PRODUCT_SEED, string_hash(prefix));
    for e in elems {
        h = mm_mix(h, scala_hash(e)?);
    }
    Some(mm_finalize(h, elems.len() as i32))
}

/// Scala's `##` for the value kinds whose JVM hash is reproducible here: the
/// primitives, `String`, `null`, tuples and `case` records (via
/// [`product_hash`]). `None` for the rest — a plain class hashes by JVM identity
/// and a collection by an unported `MurmurHash3` seq/set/map hash, neither of
/// which can be reproduced, so a hashed `Set`/`Map` keyed by one keeps insertion
/// order instead of guessing a trie order (documented in `BUGS.md`).
fn scala_hash(v: &Value) -> Option<i32> {
    match v {
        Value::Int(i) => Some(if let Ok(n) = i32::try_from(*i) {
            n
        } else {
            long_hash(*i)
        }),
        Value::Float(f) => Some(double_hash(*f)),
        // `Statics.anyHash` on a `Float` box folds to the SAME hash a `Double`
        // of that value gets, which is what keeps `Set(1.0f, 1.0)` a
        // one-element set and a `Float` key findable by its `Double` twin.
        Value::Status(_) => f32_of(v).map(|f| double_hash(f64::from(f))),
        Value::Str(s) => Some(string_hash(s)),
        Value::Bool(b) => Some(if *b { 1231 } else { 1237 }),
        Value::Undef => Some(0),
        Value::Obj(id) => {
            let hv = HEAP.with(|h| h.borrow().get(*id as usize).cloned())?;
            match hv {
                // `Char.hashCode` is the code point, as `java.lang.Character`'s
                // is — so a `Char` and its `Int` code point hash alike.
                HeapVal::Char(c) => Some(c as u32 as i32),
                HeapVal::Tuple(items) => product_hash(&format!("Tuple{}", items.len()), &items),
                HeapVal::Record(o) if o.is_case => {
                    let n = ctor_arity(&o.class, o.fields.len());
                    let elems: Vec<Value> =
                        o.fields[..n].iter().map(|(_, val)| val.clone()).collect();
                    product_hash(&o.class, &elems)
                }
                // A `Set` hashes symmetrically, every other sequence in order —
                // which is why `List(1,2,3)`, `Vector(1,2,3)`, `1 to 3` and
                // `ListBuffer(1,2,3)` all hash alike. An `Array` is the one
                // exception: it keeps the JVM's identity hash, which no
                // reimplementation can reproduce.
                HeapVal::Seq(SeqKind::Array | SeqKind::Iterable, _) => None,
                HeapVal::Seq(SeqKind::Set(_), items) => {
                    let hs: Vec<i32> = items.iter().map(scala_hash).collect::<Option<_>>()?;
                    Some(unordered_hash(&hs, string_hash("Set")))
                }
                HeapVal::Seq(_, items) => ordered_hash(&items, string_hash("Seq")),
                // `mapHash`: each entry hashed as the `Tuple2` it is, then
                // combined symmetrically.
                HeapVal::Map(_, entries) => {
                    let hs: Vec<i32> = entries
                        .iter()
                        .map(|(k, v)| product_hash("Tuple2", &[k.clone(), v.clone()]))
                        .collect::<Option<_>>()?;
                    Some(unordered_hash(&hs, string_hash("Map")))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// The CHAMP trie's hash scramble (`scala.collection.immutable.Node.improve`).
fn improve(hcode: i32) -> u32 {
    let mut h = hcode as u32;
    h = h.wrapping_add(!(h << 9));
    h ^= h >> 14;
    h = h.wrapping_add(h << 4);
    h ^ (h >> 10)
}

// ── mutable HashSet/HashMap: the flat table's order ────────────────────────
//
// `scala.collection.mutable.HashSet`/`HashMap` are one algorithm (their sources
// differ only in what a node carries), ported here from
// `src/library/scala/collection/mutable/HashSet.scala`:
//
//   * `improveHash(h) = h ^ (h >>> 16)`, and the bucket is `hash & (len - 1)`.
//   * Every bucket is kept sorted by **improved hash ascending**; `addElem`
//     walks past equal hashes, so equal-hash elements keep insertion order.
//   * `add` grows (doubling) when `contentSize + 1 >= threshold`, *before*
//     inserting — so an add that turns out to be a duplicate can still grow the
//     table. `threshold = (len * 0.75).toInt`.
//   * `growTable` splits each bucket into a low and a high sublist in order, so
//     growing never disturbs the relative order within a bucket.
//   * Iteration walks table indices ascending, each chain front to back.
//   * Removal never shrinks the table.
//
// The last two points are why elements can be *stored* in iteration order and
// re-sorted after every change: a stable sort by (bucket, improved hash) over
// the previous iteration order reproduces the table exactly, because equal-hash
// elements are adjacent in one bucket and keep their relative order under both
// growth and re-sorting.

/// `mutable.HashSet.defaultInitialCapacity`.
const MUT_INITIAL_CAPACITY: i64 = 16;
/// `mutable.HashSet.defaultLoadFactor`.
const MUT_LOAD_FACTOR: f64 = 0.75;

/// `mutable.HashSet.improveHash`. Unrelated to the immutable [`improve`].
fn mut_improve(hcode: i32) -> i32 {
    hcode ^ ((hcode as u32) >> 16) as i32
}

/// `mutable.HashSet.tableSizeFor`: `(highestOneBit(max(capacity - 1, 4)) * 2)`,
/// capped at `1 << 30`.
fn mut_table_size_for(capacity: i64) -> usize {
    let c = (capacity - 1).max(4) as u64;
    let highest_one_bit = 1u64 << (63 - c.leading_zeros());
    (highest_one_bit * 2).min(1 << 30) as usize
}

/// `mutable.HashSet.newThreshold`.
fn mut_threshold(len: usize) -> usize {
    (len as f64 * MUT_LOAD_FACTOR) as usize
}

/// The table length `mutable.HashSet.from`/`HashMap.from` start at for a factory
/// call of `n` arguments (`cap = ((n + 1) / 0.75).toInt`, or the default 16 when
/// the source's size is unknown).
fn mut_initial_len(n: usize) -> usize {
    let cap = if n > 0 {
        ((n as f64 + 1.0) / MUT_LOAD_FACTOR) as i64
    } else {
        MUT_INITIAL_CAPACITY
    };
    mut_table_size_for(cap)
}

/// Replay `add`'s grow-then-insert over `adds` attempted insertions into a table
/// of length `len` already holding `have` elements, `new_count` of which the
/// insertions actually add. Answers the resulting table length.
///
/// Growth is checked once per *attempted* add — a duplicate still triggers it —
/// so the caller passes both counts.
fn mut_grown(mut len: usize, mut have: usize, adds: &[bool]) -> usize {
    let mut threshold = mut_threshold(len);
    for &is_new in adds {
        if have + 1 >= threshold {
            len *= 2;
            threshold = mut_threshold(len);
        }
        if is_new {
            have += 1;
        }
    }
    len
}

/// `Growable.sizeHint`: grow to fit `n` incoming elements when `n` is known.
/// A `List` does not know its size (`knownSize` is -1 unless empty), so a
/// `++=` from one never hints — which is observable, because the hint can pick
/// a different table length than the one growth would have reached.
fn mut_size_hint(len: usize, n: Option<usize>) -> usize {
    match n {
        Some(n) => {
            let target = mut_table_size_for(((n as f64 + 1.0) / MUT_LOAD_FACTOR) as i64);
            len.max(target)
        }
        None => len,
    }
}

/// `IterableOnce.knownSize` for a heap collection: `-1` (here `None`) for a
/// `List`, whose size is not known without walking it, and the length for every
/// kind that stores one.
fn known_size(v: &Value) -> Option<usize> {
    if let Some((kind, items)) = seq_kind_items(v) {
        return match kind {
            SeqKind::List if !items.is_empty() => None,
            _ => Some(items.len()),
        };
    }
    as_map(v).map(|m| m.len())
}

thread_local! {
    /// Heap ids of mutable hash collections whose stored `Vec` is KNOWN to be in
    /// the table's iteration order.
    ///
    /// [`mut_ordered`] answers `None` when any key is unhashable, and the caller
    /// then stores insertion order instead — a documented fallback for a
    /// collection whose real order is a JVM identity hash and unreproducible
    /// anyway. The incremental insert below binary-searches, which is only valid
    /// on a sorted vector, so it needs to tell the two apart. Membership is
    /// therefore recorded at the one place the order is established, and an
    /// absent id simply takes the full-rebuild path — the ledger can only make
    /// the fast path unavailable, never wrong.
    ///
    /// Ids are arena indices, which [`reset_heap`] invalidates, so it clears
    /// this too.
    static MUT_SORTED: RefCell<std::collections::HashSet<u32>> =
        RefCell::new(std::collections::HashSet::new());
}

/// Record whether the vector just stored for `recv` is in table order.
fn mut_note_sorted(recv: &Value, sorted: bool) {
    if let Value::Obj(id) = recv {
        MUT_SORTED.with(|s| {
            let mut s = s.borrow_mut();
            if sorted {
                s.insert(*id);
            } else {
                s.remove(id);
            }
        });
    }
}

/// Whether `recv`'s stored vector is known to be in table order.
fn mut_is_sorted(recv: &Value) -> bool {
    match recv {
        Value::Obj(id) => MUT_SORTED.with(|s| s.borrow().contains(id)),
        _ => false,
    }
}

/// The position a mutable hash table's iteration order sorts a key by: its
/// bucket index, then its improved hash. `None` for an unhashable key — an
/// `Array`, a plain (non-`case`) instance, a function value — whose JVM hash is
/// an identity no reimplementation can reproduce.
fn mut_slot(v: &Value, len: usize) -> Option<(usize, i32)> {
    let h = mut_improve(scala_hash(v)?);
    Some(((h as u32 as usize) & (len - 1), h))
}

/// Where a key belongs in a vector already held in the table order for `len`,
/// as `(insert_at, found)`: the index of the first entry ordered AFTER the key,
/// and the index of an entry equal to it when the table already holds one.
///
/// This is what makes an insert cost `O(log n)` hashes instead of the `O(n)`
/// [`mut_ordered`] rebuild. The vector is sorted by `(bucket, improved hash)`
/// with ties in insertion order, so the run of entries sharing the key's slot
/// is a contiguous range found by two binary searches; equality is then tested
/// only inside that run, and a genuinely new key lands at its END, which is
/// where a stable sort by insertion index would have put it.
///
/// `None` when the key does not hash, or when a probe lands on an entry that
/// does not — the caller falls back to the full rebuild.
fn mut_find_slot<T>(
    items: &[T],
    len: usize,
    k: &Value,
    key: impl Fn(&T) -> Value,
) -> Option<(usize, Option<usize>)> {
    let slot = mut_slot(k, len)?;
    // `partition_point` needs a monotone predicate, which holds because the
    // vector is sorted by exactly this key. An unhashable entry would break
    // that, so it aborts the whole search rather than answering from it.
    let broke = std::cell::Cell::new(false);
    let probe = |x: &T| match mut_slot(&key(x), len) {
        Some(s) => s,
        None => {
            broke.set(true);
            (usize::MAX, i32::MAX)
        }
    };
    let start = items.partition_point(|x| probe(x) < slot);
    let end = start + items[start..].partition_point(|x| probe(x) == slot);
    if broke.get() {
        return None;
    }
    let found = items[start..end]
        .iter()
        .position(|x| value_eq(&key(x), k))
        .map(|i| start + i);
    Some((end, found))
}

/// The order a mutable hash table iterates `items`: bucket index ascending,
/// then improved hash ascending, ties keeping their current order. `None` when
/// any key is unhashable, which leaves the caller's insertion order alone.
fn mut_ordered<T: Clone>(items: &[T], len: usize, key: impl Fn(&T) -> Value) -> Option<Vec<T>> {
    let mut keyed: Vec<(usize, i32, usize)> = Vec::with_capacity(items.len());
    for (i, it) in items.iter().enumerate() {
        let h = mut_improve(scala_hash(&key(it))?);
        keyed.push(((h as u32 as usize) & (len - 1), h, i));
    }
    keyed.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    Some(
        keyed
            .into_iter()
            .map(|(_, _, i)| items[i].clone())
            .collect(),
    )
}

/// The order a CHAMP trie iterates `hashes`, as a permutation of their indices.
///
/// Each level consumes five bits of the improved hash, low bits first. Within a
/// node the slots holding exactly one element are the *data* payloads and are
/// emitted first in ascending slot order; the slots holding two or more are
/// sub-tries, walked afterwards in ascending slot order. The shape is canonical
/// (CHAMP compacts on removal), so it depends only on the set of hashes.
fn champ_order(hashes: &[u32]) -> Vec<usize> {
    fn walk(idxs: &[usize], hashes: &[u32], shift: u32, out: &mut Vec<usize>) {
        // Past 32 bits every hash in `idxs` is identical: a `HashCollisionNode`,
        // which keeps insertion order.
        if shift >= 32 {
            out.extend_from_slice(idxs);
            return;
        }
        // A node has exactly 32 slots, so they are a fixed array rather than a
        // `BTreeMap` — which allocated a map plus a `Vec` per slot at every
        // level of every walk, and this walk runs once per rebuild of an
        // immutable `Map`/`Set`.
        let mut counts = [0u32; 32];
        for &i in idxs {
            counts[((hashes[i] >> shift) & 31) as usize] += 1;
        }
        // The data payloads: slots holding exactly ONE element, emitted in
        // ascending SLOT order (not the order the elements arrived in) and
        // before any sub-trie. That is the shape that makes the order
        // canonical, so both halves of it are load-bearing.
        let mut single = [usize::MAX; 32];
        for &i in idxs {
            let slot = ((hashes[i] >> shift) & 31) as usize;
            if counts[slot] == 1 {
                single[slot] = i;
            }
        }
        for &i in single.iter() {
            if i != usize::MAX {
                out.push(i);
            }
        }
        // Then the sub-tries, also in ascending slot order. A node whose
        // children are all singletons allocates nothing here.
        if !counts.iter().any(|&c| c > 1) {
            return;
        }
        for slot in 0..32u32 {
            if counts[slot as usize] <= 1 {
                continue;
            }
            let mut group = Vec::with_capacity(counts[slot as usize] as usize);
            for &i in idxs {
                if (hashes[i] >> shift) & 31 == slot {
                    group.push(i);
                }
            }
            walk(&group, hashes, shift + 5, out);
        }
    }
    let mut out = Vec::with_capacity(hashes.len());
    walk(&(0..hashes.len()).collect::<Vec<_>>(), hashes, 0, &mut out);
    out
}

/// Reorder `items` into CHAMP trie order, keyed by `key`. `None` when any key's
/// JVM hash is not reproducible (see [`scala_hash`]), in which case the caller
/// keeps insertion order.
fn champ_sorted<T: Clone>(items: &[T], key: impl Fn(&T) -> Value) -> Option<Vec<T>> {
    let mut hashes = Vec::with_capacity(items.len());
    for it in items {
        hashes.push(improve(scala_hash(&key(it))?));
    }
    Some(
        champ_order(&hashes)
            .into_iter()
            .map(|i| items[i].clone())
            .collect(),
    )
}

/// A `case` instance's `hashCode`: `MurmurHash3.productHash` over the
/// primary-constructor prefix (the same fields `equals` compares, so the
/// equal-implies-equal contract holds). A plain instance hashes by handle
/// identity, standing in for the JVM identity hash.
fn obj_hash(class: &str, is_case: bool, fields: &[(Arc<str>, Value)], v: &Value) -> i64 {
    if is_case {
        let n = ctor_arity(class, fields.len());
        let elems: Vec<Value> = fields[..n].iter().map(|(_, val)| val.clone()).collect();
        if let Some(h) = product_hash(class, &elems) {
            return i64::from(h);
        }
    }
    let mut h = DefaultHasher::new();
    if let Value::Obj(id) = v {
        id.hash(&mut h);
    }
    (h.finish() & 0x7fff_ffff) as i64
}

/// Scala `==`/`equals` between two values, with object semantics: a `case`
/// instance compares structurally (class + field-by-field); a plain instance
/// compares by reference identity (the handle). An object vs. a non-object, or
/// two different classes, are unequal.
fn obj_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Obj(ia), Value::Obj(ib)) => {
            if ia == ib {
                return true; // same instance (also the plain-class identity case)
            }
            let (oa, ob) = match HEAP.with(|h| {
                let h = h.borrow();
                Some((h.get(*ia as usize)?.clone(), h.get(*ib as usize)?.clone()))
            }) {
                Some(p) => p,
                None => return false,
            };
            match (oa, ob) {
                (HeapVal::Record(oa), HeapVal::Record(ob)) => {
                    // Plain classes already failed the identity check above.
                    // Scala's derived `equals` compares the primary-constructor
                    // parameters only.
                    let n = ctor_arity(&oa.class, oa.fields.len());
                    oa.is_case
                        && ob.is_case
                        && oa.class == ob.class
                        && oa.fields.len() == ob.fields.len()
                        && oa.fields[..n]
                            .iter()
                            .zip(&ob.fields[..n])
                            .all(|((_, x), (_, y))| value_eq(x, y))
                }
                // A `Set` is UNORDERED, so `Set.equals` is "same size and every
                // element of mine is in yours" — `Set(1, 2) == Set(2, 1)`. The
                // stored `Vec` is only an iteration order, and for a
                // `mutable.HashSet` or a `LinkedHashSet` it is not even the same
                // order two equal sets would arrive at, so comparing positionally
                // answers `false` for sets Scala calls equal.
                //
                // A set is also equal only to another set: `Set` and `Seq` are
                // different branches of `Iterable`, and `scala.collection.Set`'s
                // `equals` requires a `Set` on the other side (Scala 3 rejects
                // `Set(1, 2) == List(1, 2)` at compile time; at run time
                // `.equals` answers `false`).
                (HeapVal::Seq(ka, xa), HeapVal::Seq(kb, xb))
                    if matches!(ka, SeqKind::Set(_)) || matches!(kb, SeqKind::Set(_)) =>
                {
                    matches!(ka, SeqKind::Set(_))
                        && matches!(kb, SeqKind::Set(_))
                        && xa.len() == xb.len()
                        && xa.iter().all(|x| xb.iter().any(|y| value_eq(x, y)))
                }
                // Two sequences/tuples are equal element-by-element.
                (HeapVal::Seq(_, xa), HeapVal::Seq(_, xb))
                | (HeapVal::Tuple(xa), HeapVal::Tuple(xb)) => {
                    xa.len() == xb.len() && xa.iter().zip(&xb).all(|(x, y)| value_eq(x, y))
                }
                (HeapVal::Map(_, ma), HeapVal::Map(_, mb)) => {
                    ma.len() == mb.len()
                        && ma
                            .iter()
                            .all(|(k, v)| map_get(&mb, k).is_some_and(|w| value_eq(v, &w)))
                }
                _ => false,
            }
        }
        _ => false, // object vs. non-object
    }
}

/// Structural equality of two field values (recurses into nested objects).
fn value_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Obj(_), Value::Obj(_)) => obj_eq(a, b),
        // A `Float` is a distinct VARIANT, so `a == b` would answer `false` for
        // it against the `Int` or `Double` of the same value. Scala's `==`
        // crosses the numeric widths: `1.0f == 1` and `1.0f == 1.0` are both
        // true, and `0.1f == 0.1` is false because the widening is what is
        // compared, not the rendering.
        (Value::Status(_), _) | (_, Value::Status(_)) => match (float_of(a), float_of(b)) {
            (Some(x), Some(y)) => x == y,
            _ => a == b,
        },
        _ => a == b,
    }
}

/// Scala `==` for the [`numeric_hook`] `Eq`/`Ne` arms: object operands compare
/// with [`obj_eq`]; everything else keeps the existing structural
/// (`scala_str`-based) comparison.
fn scala_eq(a: &Value, b: &Value) -> bool {
    if matches!(a, Value::Obj(_)) || matches!(b, Value::Obj(_)) {
        obj_eq(a, b)
    } else {
        scala_str(a) == scala_str(b)
    }
}

/// `SFORMAT` builtin: pop the format spec (top) and the value, and format the
/// value per the Java-`Formatter` subset. A malformed/unsupported spec halts the
/// VM with the message parked for the runner.
fn b_format(vm: &mut VM, _argc: u8) -> Value {
    let spec = vm.pop().as_str_cow().into_owned();
    let v = vm.pop();
    match format_one(&spec, &v, Some(vm)) {
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
    // A `match` whose scrutinee is the garbage left by a raise falls through
    // every arm; that is not a real `MatchError`, so stay quiet and let the
    // in-flight exception reach its handler.
    if unwinding() {
        return Value::Undef;
    }
    let class = match &v {
        Value::Int(_) => "java.lang.Integer",
        Value::Float(_) => "java.lang.Double",
        Value::Status(_) => "java.lang.Float",
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
        "Double" => matches!(v, Value::Float(_)),
        "Float" => matches!(v, Value::Status(_)),
        "Boolean" => matches!(v, Value::Bool(_)),
        "Any" | "AnyRef" | "AnyVal" | "Object" => true,
        // The sequence shapes a sequence pattern (`case List(a, b) =>`) and the
        // cons pattern (`case h :: t =>`) test against. `Seq`/`Iterable` accept
        // any sequence; the named kinds accept only their own representation,
        // matching Scala — a `Vector` does not match `case List(…)`.
        "Seq" | "Iterable" | "collection.Seq" => matches!(seq_kind(v), Some(k) if !k.is_set()),
        "List" => matches!(seq_kind(v), Some(SeqKind::List)),
        "Vector" | "IndexedSeq" => matches!(seq_kind(v), Some(SeqKind::Vector)),
        "Array" => matches!(seq_kind(v), Some(SeqKind::Array)),
        "Set" => matches!(seq_kind(v), Some(k) if k.is_set()),
        "Map" => as_map(v).is_some(),
        // `Nil` is the empty `List`; `::` is a non-empty one (the cons cell
        // class). Both are shape tests, not `==` against a singleton.
        "Nil" => matches!(seq_kind_items(v), Some((SeqKind::List, xs)) if xs.is_empty()),
        "::" => matches!(seq_kind_items(v), Some((SeqKind::List, xs)) if !xs.is_empty()),
        // `TupleN` — the type a tuple pattern (`case (a, b) =>`) tests against.
        _ if ty.starts_with("Tuple") => match ty[5..].parse::<usize>() {
            Ok(n) => matches!(seq_or_tuple_len(v), Some(len) if len == n),
            Err(_) => false,
        },
        // A THROWABLE tested outside a `catch` — `e match { case _:
        // ArithmeticException => … }`, and the `isDefinedAt` behind the partial
        // function `Try.recover` takes. `catch` has its own JVM-hierarchy test
        // ([`EXC_MATCH`]), but a plain `match` reaches here, where a throwable is
        // not a record, so EVERY typed arm used to fail. It walks the same
        // hierarchy table, off the simple class name that table is keyed by.
        _ if as_exc(v).is_some() => {
            ty == "Throwable" || thrown_class(v).is_some_and(|thrown| throwable_is_a(&thrown, ty))
        }
        // A user instance conforms to its own class and to every supertype the
        // compiler registered for it (`case c: Shape` on a `Circle`).
        _ => with_obj(v, |o| class_conforms(&o.class, ty)).unwrap_or(false),
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
/// `"…".format(args)` — walk a whole Java format string, delegating each
/// conversion to [`format_one`]. `%%` is a literal percent and `%n` a newline;
/// neither consumes an argument. A conversion with no argument left is Java's
/// `MissingFormatArgumentException`, which a Scala program can catch.
fn format_all(fmt: &str, args: &[Value], mut vm: Option<&mut VM>) -> Result<String, String> {
    let b = fmt.as_bytes();
    let mut out = String::with_capacity(fmt.len());
    let (mut i, mut next) = (0usize, 0usize);
    while i < b.len() {
        if b[i] != b'%' {
            let start = i;
            while i < b.len() && b[i] != b'%' {
                i += 1;
            }
            // `%` is ASCII, so both ends land on a char boundary.
            out.push_str(&fmt[start..i]);
            continue;
        }
        let start = i;
        i += 1;
        while i < b.len() && !b[i].is_ascii_alphabetic() && b[i] != b'%' {
            i += 1;
        }
        if i >= b.len() {
            return Err(format!(
                "scalars: java.util.UnknownFormatConversionException: Conversion = '{}'",
                &fmt[start + 1..]
            ));
        }
        let conv = b[i] as char;
        i += 1;
        match conv {
            '%' => out.push('%'),
            'n' => out.push('\n'),
            _ => {
                let Some(v) = args.get(next) else {
                    return Err(format!(
                        "scalars: java.util.MissingFormatArgumentException: Format specifier '{}'",
                        &fmt[start..i]
                    ));
                };
                next += 1;
                out.push_str(&format_one(&fmt[start..i], v, vm.as_deref_mut())?);
            }
        }
    }
    Ok(out)
}

fn format_one(spec: &str, v: &Value, vm: Option<&mut VM>) -> Result<String, String> {
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
            // `%s` is the one conversion that renders through `toString`, so it
            // is the one that can need the VM. A caller that has none renders
            // structurally, exactly as before.
            let mut s = match vm {
                Some(vm) => scala_str_vm(vm, v),
                None => scala_str(v),
            };
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
        'f' | 'F' => {
            let x = num_f64(v);
            let p = prec.unwrap_or(6);
            let digits = match nonfinite(x, conv, plus, space) {
                Some(t) => return Ok(pad_str(t, left, width)),
                None => round_half_up(x.abs(), p),
            };
            Ok(pad_num(
                digits,
                x.is_sign_negative(),
                left,
                zero,
                plus,
                space,
                width,
            ))
        }
        // Radix conversions. Java formats the two's-complement bit pattern with
        // no sign, at the WIDTH OF THE STATIC TYPE: `Int` is 32 bits
        // (`-1` is `ffffffff`) and `Long` is 64 (`ffffffffffffffff`).
        //
        // This frontend models every integer as `i64` and so has no static type
        // to consult. It picks the narrowest width that holds the value, which
        // is right for every `Int` — the overwhelmingly common case, and the one
        // a literal like `-1` has. A `Long` variable holding a small negative
        // number is the residual gap (see `BUGS.md`); it renders 32-bit.
        'x' | 'X' | 'o' => {
            let n = v.to_int();
            let bits = match i32::try_from(n) {
                Ok(narrow) => narrow as u32 as u64,
                Err(_) => n as u64,
            };
            let body = match conv {
                'x' => format!("{bits:x}"),
                'X' => format!("{bits:X}"),
                _ => format!("{bits:o}"),
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
        // Scientific notation. Java always writes a sign and AT LEAST two
        // exponent digits (`1.000000e+00`), where Rust's `{:e}` writes neither.
        // The mantissa is rounded off the value's shortest round-tripping
        // digits (see `round_half_up`) rather than off `a / 10^exp`, because
        // that division is itself inexact and moves the tie: `1234.5` at
        // `%.3e` is `1.235e+03`, not `1.234e+03`.
        'e' | 'E' => {
            let x = num_f64(v);
            let p = prec.unwrap_or(6);
            if let Some(t) = nonfinite(x, conv, plus, space) {
                return Ok(pad_str(t, left, width));
            }
            let full = format!("{:e}", x.abs());
            let (mant, exp_s) = full.split_once('e').unwrap_or((full.as_str(), "0"));
            let mut exp: i32 = exp_s.parse().unwrap_or(0);
            let mut mantissa = round_half_up_str(mant, p);
            // A carry out of the leading digit (`9.99` at `%.1e`) is one more
            // power of ten, renormalized back to a single leading digit.
            if mantissa.starts_with("10") {
                exp += 1;
                mantissa = round_half_up_str("1", p);
            }
            let body = format!(
                "{mantissa}{}{}{:02}",
                if conv == 'E' { "E" } else { "e" },
                if exp < 0 { "-" } else { "+" },
                exp.abs()
            );
            // `-0.0` keeps its sign through `%f`/`%e` (Java prints `-0.00`), so
            // the test is the sign BIT, not `x < 0.0`.
            Ok(pad_num(
                body,
                x.is_sign_negative(),
                left,
                zero,
                plus,
                space,
                width,
            ))
        }
        'c' => {
            let ch = match v {
                Value::Str(s) => s.chars().next().unwrap_or('\0'),
                _ => char::from_u32(v.to_int() as u32).unwrap_or('\u{fffd}'),
            };
            Ok(pad_str(ch.to_string(), left, width))
        }
        // `java.util.Formatter` rejects an unknown conversion character with a
        // catchable exception naming just that character — the same one the
        // truncated-specifier path above raises. This used to answer wording of
        // its own, which no `catch` could match on and no JDK ever printed.
        other => Err(format!(
            "scalars: java.util.UnknownFormatConversionException: Conversion = '{other}'"
        )),
    }
}

/// Java's rendering of a non-finite double under a float conversion, or `None`
/// for a finite one. Rust writes `inf`/`-inf` where Java writes
/// `Infinity`/`-Infinity`; an UPPERCASE conversion (`%E`, `%F`) upper-cases the
/// whole word, and the `+`/space flags apply to an infinity but not to `NaN`.
fn nonfinite(x: f64, conv: char, plus: bool, space: bool) -> Option<String> {
    let mut t = if x.is_nan() {
        "NaN".to_string()
    } else if x.is_infinite() {
        let mut t = "Infinity".to_string();
        if x < 0.0 {
            t.insert(0, '-');
        } else if plus {
            t.insert(0, '+');
        } else if space {
            t.insert(0, ' ');
        }
        t
    } else {
        return None;
    };
    if conv.is_ascii_uppercase() {
        t = t.to_uppercase();
    }
    Some(t)
}

/// `a` (non-negative, finite) rendered with exactly `p` fraction digits, rounded
/// the way `java.util.Formatter` rounds.
///
/// Two things differ from Rust's `{:.*}`, and both are observable:
///
/// 1. Java rounds HALF_UP where Rust rounds half-to-even, so they disagree on
///    every tie — `0.125` at `%.2f` is Java's `0.13` and Rust's `0.12`.
/// 2. Java rounds the SHORTEST round-tripping decimal (the digits
///    `Double.toString` would print), not the value's exact binary expansion.
///    That is why `1.005` at `%.2f` is `1.01` even though the stored double is
///    `1.00499999999999989…`, and why `0.15` at `%.1f` is `0.2`. Rounding the
///    exact expansion instead answers `1.00`/`0.1` — right by IEEE, wrong by
///    Java.
///
/// Rust's `{}` for `f64` is that same shortest round-tripping form, so it is
/// the correct input, and the rounding is then done on the digit string.
fn round_half_up(a: f64, p: usize) -> String {
    round_half_up_str(&format!("{a}"), p)
}

/// Round a non-negative decimal STRING (`"12.345"`, no sign, no exponent) to `p`
/// fraction digits, half away from zero. Digits past `p` are inspected only to
/// decide the carry, so the caller must pass an exact expansion for the result
/// to be exact.
fn round_half_up_str(s: &str, p: usize) -> String {
    let (int_part, frac) = s.split_once('.').unwrap_or((s, ""));
    let mut digits: Vec<u8> = int_part
        .bytes()
        .chain(frac.bytes())
        .take(int_part.len() + p)
        .collect();
    while digits.len() < int_part.len() + p {
        digits.push(b'0');
    }
    if frac.as_bytes().get(p).is_some_and(|d| *d >= b'5') {
        let mut i = digits.len();
        loop {
            if i == 0 {
                digits.insert(0, b'1');
                break;
            }
            i -= 1;
            if digits[i] == b'9' {
                digits[i] = b'0';
            } else {
                digits[i] += 1;
                break;
            }
        }
    }
    let out = String::from_utf8(digits).unwrap_or_default();
    if p == 0 {
        return out;
    }
    let cut = out.len() - p;
    format!("{}.{}", &out[..cut], &out[cut..])
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
    // A range `for … yield` result is a fusevm `Value::Array` (rendered as
    // `Vector`); promote it to a heap `Vector` seq so the collection methods
    // (`map`, `toList`, …) apply uniformly.
    let recv = match &recv {
        Value::Array(items) => heap_push(HeapVal::Seq(SeqKind::Vector, items.to_vec())),
        _ => recv,
    };

    // Suppressed while unwinding: the receiver and arguments are garbage left by
    // the raise, and dispatching them would fault a second time (see the
    // `Exception unwinding` section). Args are already popped, so the stack
    // stays balanced.
    if unwinding() {
        return Value::Undef;
    }

    // `java.lang.Throwable`'s observable surface. Handled before the collection
    // and pure dispatchers because a throwable is neither.
    if let Some(e) = as_exc(&recv) {
        return match name.as_str() {
            "getMessage" | "getLocalizedMessage" => match &e.msg {
                Some(m) => Value::str(m.to_string()),
                None => Value::Undef,
            },
            "toString" => Value::str(exc_to_string(&e)),
            // The usual way a program names an exception's type.
            "getClass" => match class_of(&recv) {
                Ok(v) => v,
                Err(msg) => fault(vm, msg),
            },
            _ => fault(
                vm,
                format!("scalars: value {name} is not a member of {}", e.class),
            ),
        };
    }

    // `StringOps`' closure-taking combinators. Dispatched here (not in
    // `string_method`) because running the predicate body needs the VM.
    if let Value::Str(s) = &recv {
        let s = s.to_string();
        if let Some(r) = string_fn_method(vm, &s, &name, &args) {
            return match r {
                Ok(v) => v,
                Err(e) => fault(vm, e),
            };
        }
    }

    // An `Ordering`'s surface. Dispatched here rather than in `obj_method`
    // because `compare`/`lt`/… may run the key function, which needs the VM.
    if let Some(r) = ordering_method(vm, &recv, &name, &args) {
        return match r {
            Ok(v) => v,
            Err(e) => fault(vm, e),
        };
    }

    // `Option`'s surface. Dispatched here rather than in `obj_method` because
    // `map`/`filter`/`fold`/… run a closure body and so need the VM. An
    // unrecognized name falls through to the record dispatcher below, which
    // still answers `Some(x).value`, `hashCode`, `equals`, ….
    if let Some(inner) = as_option(&recv) {
        if let Some(r) = option_method(vm, inner, &name, &args) {
            return match r {
                Ok(v) => v,
                Err(e) => fault(vm, e),
            };
        }
    }

    // A `LazyList` — answered before every strict dispatcher, because its
    // combinators must NOT force and the strict ones do.
    if as_lazy(&recv).is_some() {
        if let Some(r) = lazy_method(vm, &recv, &name, &args) {
            return match r {
                Ok(v) => v,
                Err(e) => fault(vm, e),
            };
        }
    }

    // `Either`'s surface, same reason and same fall-through.
    if let Some(outcome) = as_either(&recv) {
        if let Some(r) = either_method(vm, &recv, outcome, &name, &args) {
            return match r {
                Ok(v) => v,
                Err(e) => fault(vm, e),
            };
        }
    }

    // The left-biased view `e.left` answers, whose members mirror the ones
    // above onto the `Left`.
    if let Some(either) = as_left_projection(&recv) {
        if let Some(outcome) = as_either(&either) {
            if let Some(r) = left_projection_method(vm, &either, outcome, &name, &args) {
                return match r {
                    Ok(v) => v,
                    Err(e) => fault(vm, e),
                };
            }
        }
    }

    // `scala.util.Try`'s surface, for the same reason and with the same
    // fall-through: `map`/`recover`/`foreach` run a closure body.
    if let Some(outcome) = as_try(&recv) {
        if let Some(r) = try_method(vm, &recv, outcome, &name, &args) {
            return match r {
                Ok(v) => v,
                Err(e) => fault(vm, e),
            };
        }
    }

    // A heap collection/tuple/closure may need to run a closure body mid-method
    // (`map`, `filter`, `foldLeft`, …), so those dispatch through the vm-aware
    // path; everything else stays on the pure dispatcher.
    if is_heap_collection(&recv) {
        return match heap_method(vm, &recv, &name, &args) {
            Ok(v) => v,
            Err(e) => fault(vm, e),
        };
    }
    // The members `Ordered` DERIVES from the user's `compare`. The class only
    // writes `compare`, so the compiler's method index has no entry for these
    // and they would otherwise be rejected as "not a member". Answered here
    // rather than in the pure `obj_method` because running the user's `compare`
    // needs the VM. (The infix spelling `a < b` never reaches a method call at
    // all — it lowers to a numeric op, and is handled in the compiler.)
    if let Some(r) = ordered_derived_method(vm, &recv, &name, &args) {
        return match r {
            Ok(v) => v,
            Err(e) => fault(vm, e),
        };
    }

    // `o.f(x)` where `f` names a FIELD rather than a method is `o.f.apply(x)` —
    // indexing a `String`/`List` field, keying a `Map` field, calling a function
    // field. The compiler routes a known method to its own subroutine, so a
    // record reaching here with a matching field name has no such method.
    if !args.is_empty() {
        let field = with_obj(&recv, |o| {
            o.fields
                .iter()
                .find(|(fname, _)| **fname == *name)
                .map(|(_, v)| v.clone())
        })
        .flatten();
        if let Some(f) = field {
            return match apply_value(vm, &f, &args) {
                Ok(v) => v,
                Err(e) => fault(vm, e),
            };
        }
    }
    // `dispatch_method` deliberately holds no VM handle, so the two members it
    // answers that can run user code are resolved here instead, where the VM is
    // in hand: `toString`, defined on every value, and `"…".format(…)`, whose
    // `%s` conversions are `toString` calls on the arguments.
    if name == "toString" && args.is_empty() {
        return Value::str(scala_str_vm(vm, &recv));
    }
    if name == "format" {
        if let Value::Str(fmt) = &recv {
            let fmt = fmt.to_string();
            return match format_all(&fmt, &args, Some(vm)) {
                Ok(s) => Value::str(s),
                Err(e) => fault(vm, e),
            };
        }
    }
    match dispatch_method(&recv, &name, &args) {
        Ok(v) => v,
        Err(e) => fault(vm, e),
    }
}

/// Whether `v` is a heap `Seq`/`Map`/`Tuple`/`Closure` (dispatched vm-aware).
fn is_heap_collection(v: &Value) -> bool {
    if let Value::Obj(id) = v {
        HEAP.with(|h| {
            matches!(
                h.borrow().get(*id as usize),
                Some(
                    HeapVal::Seq(..)
                        | HeapVal::Map(..)
                        | HeapVal::Tuple(_)
                        | HeapVal::Closure(_)
                        | HeapVal::Derived(_)
                )
            )
        })
    } else {
        false
    }
}

/// The heap kind of `v`, for method routing.
fn heap_kind(v: &Value) -> Option<u8> {
    if let Value::Obj(id) = v {
        HEAP.with(|h| {
            h.borrow().get(*id as usize).map(|o| match o {
                HeapVal::Seq(..) => 0u8,
                HeapVal::Map(..) => 1,
                HeapVal::Tuple(_) => 2,
                HeapVal::Closure(_) | HeapVal::Derived(_) => 3,
                HeapVal::Record(_) => 4,
                HeapVal::Exc(_) => 5,
                // A `Regex`/`Regex.Match` is answered by `regex_method` before
                // any of the routed dispatchers, so it needs no kind of its own.
                HeapVal::Regex(_) | HeapVal::Match { .. } => 6,
                HeapVal::Cell(_) => 7,
                // A `Char` is answered by `char_method` ahead of the routed
                // dispatchers, so it needs no kind of its own.
                HeapVal::Char(_) => 8,
                // Likewise an `Ordering`, which `ordering_method` answers.
                HeapVal::Ordering { .. } => 9,
                HeapVal::Lazy(_) => 10,
            })
        })
    } else {
        None
    }
}

/// Dispatch a method on a heap collection/tuple/closure. The closure-consuming
/// operations re-enter the VM (via [`invoke_closure`]) to run their function arg.
fn heap_method(vm: &mut VM, recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    if name == "toString" && args.is_empty() {
        return Ok(Value::str(scala_str_vm(vm, recv)));
    }
    if (name == "equals" || name == "==") && args.len() == 1 {
        return Ok(Value::bool(obj_eq(recv, &args[0])));
    }
    // A collection's `hashCode` is its `MurmurHash3` seq/set/map hash; an
    // `Array` and a function value keep the JVM identity hash, which is not
    // reproducible, so the handle stands in for it (see `BUGS.md`).
    if name == "hashCode" && args.is_empty() {
        return Ok(Value::int(match scala_hash(recv) {
            Some(h) => i64::from(h),
            None => match recv {
                Value::Obj(id) => i64::from(*id),
                _ => 0,
            },
        }));
    }
    match heap_kind(recv) {
        Some(0) => seq_method(vm, recv, name, args),
        Some(1) => map_method(vm, recv, name, args),
        Some(2) => tuple_method(recv, name, args),
        Some(3) => closure_method(vm, recv, name, args),
        _ => Err(no_such_method(recv, name)),
    }
}

/// Replace the contents (and kind) of the mutable sequence `recv` points at.
fn set_seq_items(recv: &Value, kind: SeqKind, items: Vec<Value>) {
    // Any wholesale replacement forfeits the table-order claim; the one caller
    // that establishes the order re-asserts it right after. Fail-safe: a lost
    // claim costs a rebuild, a wrong one would misplace an element.
    mut_note_sorted(recv, false);
    if let Value::Obj(id) = recv {
        HEAP.with(|h| {
            if let Some(HeapVal::Seq(k, xs)) = h.borrow_mut().get_mut(*id as usize) {
                *k = kind;
                *xs = items;
            }
        });
    }
}

/// The elements a `+=`-style argument contributes: one value for the `One`
/// forms, the argument's elements for the `All` forms.
fn spread(v: &Value, all: bool) -> Vec<Value> {
    if all {
        as_seq_or_tuple(v)
            .or_else(|| as_map(v).map(|m| m.into_iter().map(new_pair_of).collect()))
            .unwrap_or_else(|| vec![v.clone()])
    } else {
        vec![v.clone()]
    }
}

/// A `(k, v)` entry as a `Tuple2` value.
fn new_pair_of((k, v): (Value, Value)) -> Value {
    heap_push(HeapVal::Tuple(vec![k, v]))
}

/// Add `adds` to the `mutable.HashSet` behind `recv`, replaying the table growth
/// `add` would have done and re-sorting into the table's iteration order.
fn mut_set_add(recv: &Value, len: usize, items: &[Value], adds: &[Value], hint: Option<usize>) {
    // The fast path: the table does not grow, its stored vector is already in
    // table order, and every added element hashes. Then each add is a binary
    // search and a splice, with no copy of the collection and no re-sort — which
    // is what keeps filling a set linear instead of quadratic.
    // `mut_grown` is asked with every add counted as NEW — the worst case for
    // growth. If the table does not grow even then, it cannot grow for the adds
    // that turn out to be repeats either, and the fast path applies.
    let worst = vec![true; adds.len()];
    if mut_grown(mut_size_hint(len, hint), items.len(), &worst) == len
        && mut_is_sorted(recv)
        && adds.iter().all(|a| mut_slot(a, len).is_some())
    {
        mut_set_splice(recv, len, adds);
        return;
    }
    let mut cur = items.to_vec();
    let mut flags = Vec::with_capacity(adds.len());
    for a in adds {
        let is_new = !cur.iter().any(|u| value_eq(u, a));
        flags.push(is_new);
        if is_new {
            cur.push(a.clone());
        }
    }
    let len = mut_grown(mut_size_hint(len, hint), items.len(), &flags);
    let ordered = mut_ordered(&cur, len, Clone::clone);
    let sorted = ordered.is_some();
    set_seq_items(
        recv,
        SeqKind::Set(HashRep::Mutable(len as u32)),
        ordered.unwrap_or(cur),
    );
    mut_note_sorted(recv, sorted);
}

/// Splice `adds` into the mutable-SET vector behind `recv`, in place and in
/// table order, without copying the collection.
///
/// The caller has already established the two preconditions — the collection's
/// stored order is the table's, and every added element hashes — so no element
/// here can fail to be placed. That is why this returns nothing to check: a
/// partial splice would leave the collection in an order neither path produces.
///
/// Positions are computed under a SHARED borrow that is released before the
/// write: hashing a `Value::Obj` key reads the arena itself, so the lookup
/// cannot run inside a `borrow_mut`, and it nests a second shared borrow inside
/// this one. Each add is located and then applied, one at a time, so a repeat
/// within one `++=` sees what the earlier ones added.
fn mut_set_splice(recv: &Value, len: usize, adds: &[Value]) {
    let Value::Obj(id) = recv else { return };
    let id = *id as usize;
    for a in adds {
        // A SHARED borrow, which the hashing inside the lookup may nest another
        // of (a `Value::Obj` key's hash reads the arena). It is dropped before
        // the write below, which needs the exclusive one.
        let Some((at, found)) = HEAP.with(|h| {
            let hb = h.borrow();
            match hb.get(id) {
                Some(HeapVal::Seq(_, xs)) => mut_find_slot(xs, len, a, Clone::clone),
                _ => None,
            }
        }) else {
            return;
        };
        if found.is_some() {
            continue;
        }
        HEAP.with(|h| {
            if let Some(HeapVal::Seq(_, xs)) = h.borrow_mut().get_mut(id) {
                xs.insert(at, a.clone());
            }
        });
    }
}

/// A `Map` handle's representation and entry COUNT, without copying it.
///
/// [`map_rep_entries`] clones the whole entry list, which every `Map` method
/// used to pay before it knew whether it needed one — including the `update`
/// that fills a map in a loop, making that loop quadratic on the copy alone.
fn map_rep_len(v: &Value) -> Option<(HashRep, usize)> {
    if let Value::Obj(id) = v {
        HEAP.with(|h| match h.borrow().get(*id as usize) {
            Some(HeapVal::Map(rep, m)) => Some((*rep, m.len())),
            _ => None,
        })
    } else {
        None
    }
}

/// The `mutable.Map` operations that can be answered in place, with no copy of
/// the map and no rebuild of its order. `None` means the name is not one of
/// them, or its preconditions do not hold, and the caller takes the ordinary
/// path — which is exactly what it did before this existed.
///
/// The preconditions are the two [`mut_map_splice`] needs (the stored order is
/// the table's, and the key hashes) plus, for a write, that the table does not
/// grow. A growth re-buckets every entry, so it goes through the full rebuild
/// and pays its `O(n)` there — amortized to `O(1)` per insert by the doubling.
fn mut_map_fast(
    recv: &Value,
    rep: HashRep,
    n: usize,
    name: &str,
    args: &[Value],
) -> Option<Result<Value, String>> {
    let HashRep::Mutable(len) = rep else {
        return None;
    };
    let len = len as usize;
    if !mut_is_sorted(recv) {
        return None;
    }
    // The key each supported name reads or writes at.
    let (k, v) = match (name, args.len()) {
        ("apply" | "get" | "contains" | "isDefinedAt", 1) => (&args[0], None),
        ("update" | "put" | "getOrElseUpdate", 2) => (&args[0], Some(&args[1])),
        _ => return None,
    };
    mut_slot(k, len)?;
    let (_, found) = HEAP.with(|h| {
        let hb = h.borrow();
        match hb.get(as_obj_id(recv)?) {
            Some(HeapVal::Map(_, m)) => mut_find_slot(m, len, k, |(ek, _)| ek.clone()),
            _ => None,
        }
    })?;
    let existing = found.and_then(|i| {
        HEAP.with(|h| match h.borrow().get(as_obj_id(recv)?) {
            Some(HeapVal::Map(_, m)) => Some(m[i].1.clone()),
            _ => None,
        })
    });
    match name {
        "apply" => {
            return Some(match existing {
                Some(v) => Ok(v),
                None => Err(format!(
                    "scalars: java.util.NoSuchElementException: key not found: {}",
                    scala_str(k)
                )),
            })
        }
        "get" => return Some(Ok(opt(existing))),
        "contains" | "isDefinedAt" => return Some(Ok(Value::bool(existing.is_some()))),
        "getOrElseUpdate" if existing.is_some() => return Some(Ok(existing.expect("just tested"))),
        _ => {}
    }
    let v = v.expect("the write names all carry a value");
    // A new key grows the table one insertion earlier than a repeat does, so the
    // growth question is asked with the answer the lookup already found.
    if mut_grown(len, n, &[found.is_none()]) != len {
        return None;
    }
    let displaced = mut_map_splice(recv, len, &[(k.clone(), v.clone())]);
    Some(Ok(match name {
        "put" => opt(displaced),
        "getOrElseUpdate" => v.clone(),
        _ => Value::Undef,
    }))
}

/// A heap handle's arena index.
fn as_obj_id(v: &Value) -> Option<usize> {
    match v {
        Value::Obj(id) => Some(*id as usize),
        _ => None,
    }
}

/// The same for a mutable MAP: a new key is spliced in at its table position, a
/// repeated one keeps its position and takes the new value (`put0`'s rule).
/// Answers the value a repeated key displaced, for `put`.
fn mut_map_splice(recv: &Value, len: usize, adds: &[(Value, Value)]) -> Option<Value> {
    let Value::Obj(id) = recv else { return None };
    let id = *id as usize;
    let mut displaced = None;
    for (k, v) in adds {
        let Some((at, found)) = HEAP.with(|h| {
            let hb = h.borrow();
            match hb.get(id) {
                Some(HeapVal::Map(_, m)) => mut_find_slot(m, len, k, |(ek, _)| ek.clone()),
                _ => None,
            }
        }) else {
            return displaced;
        };
        HEAP.with(|h| {
            if let Some(HeapVal::Map(_, m)) = h.borrow_mut().get_mut(id) {
                match found {
                    Some(i) => {
                        displaced = Some(m[i].1.clone());
                        m[i].1 = v.clone();
                    }
                    None => m.insert(at, (k.clone(), v.clone())),
                }
            }
        });
    }
    displaced
}

/// In-place mutation of a mutable sequence (`Array`, `ListBuffer`,
/// `ArrayBuffer`, `mutable.Set`). `None` means the name is not a mutation, so
/// the caller falls through to the shared read-only implementation.
fn mut_seq_method(
    recv: &Value,
    kind: SeqKind,
    items: &[Value],
    name: &str,
    args: &[Value],
) -> Option<Result<Value, String>> {
    let set_len = match kind {
        SeqKind::Set(HashRep::Mutable(n)) => Some(n as usize),
        _ => None,
    };
    // A `LinkedHashSet` is mutable but has no table order to replay: it iterates
    // its insertion list, so an add appends and a remove unlinks.
    let linked_set = matches!(kind, SeqKind::Set(HashRep::Linked));
    let is_set = set_len.is_some() || linked_set;
    let buffer = kind.is_buffer();
    let strbuf = kind == SeqKind::StrBuf;
    let me = || Ok(recv.clone());
    // Whether `args[0]` is a collection of elements (`++=`) or one element.
    let all = is_add_all_form(name);
    // What a `StringBuilder` addition contributes: the characters of a `Char`
    // sequence for the `All` forms (`sb ++= List('a','b')`), and otherwise the
    // characters of the argument's `String.valueOf` — which is why
    // `sb.append(7)` appends `'7'` and `sb.append(List(1,2))` appends
    // `List(1, 2)`.
    let chars_of = |v: &Value| -> Vec<Value> {
        if all {
            if let Some(elems) = as_seq_or_tuple(v) {
                return elems.iter().flat_map(str_chars).collect();
            }
        }
        str_chars(v)
    };

    match (name, args.len()) {
        // Additions. `+=`/`++=` answer the receiver; `add` answers whether the
        // element was absent.
        ("+=" | "++=" | "addOne" | "addAll" | "append" | "appendAll" | "add", 1)
            if is_set || buffer =>
        {
            let adds = if strbuf {
                chars_of(&args[0])
            } else {
                spread(&args[0], all)
            };
            if is_set {
                let absent = !items.iter().any(|u| value_eq(u, &args[0]));
                match set_len {
                    Some(len) => {
                        let hint = all.then(|| known_size(&args[0])).flatten();
                        mut_set_add(recv, len, items, &adds, hint);
                    }
                    None => {
                        let mut out = items.to_vec();
                        for a in adds {
                            if !out.iter().any(|u| value_eq(u, &a)) {
                                out.push(a);
                            }
                        }
                        set_seq_items(recv, kind, out);
                    }
                }
                return Some(if name == "add" {
                    Ok(Value::bool(absent))
                } else {
                    me()
                });
            }
            let mut out = items.to_vec();
            out.extend(adds);
            set_seq_items(recv, kind, out);
            Some(me())
        }
        ("+=:" | "prepend" | "prependAll", 1) if buffer => {
            let mut out = if strbuf {
                chars_of(&args[0])
            } else {
                spread(&args[0], all)
            };
            out.extend_from_slice(items);
            set_seq_items(recv, kind, out);
            Some(me())
        }
        // `Queue.enqueue` appends; `Stack.push` PREPENDS, because a `Stack`'s
        // head is its top.
        ("enqueue" | "enqueueAll", 1) if kind == SeqKind::Queue => {
            let mut out = items.to_vec();
            out.extend(spread(&args[0], name == "enqueueAll"));
            set_seq_items(recv, kind, out);
            Some(me())
        }
        ("push", 1) if kind == SeqKind::Stack => {
            let mut out = vec![args[0].clone()];
            out.extend_from_slice(items);
            set_seq_items(recv, kind, out);
            Some(me())
        }
        // The head-and-tail removals: `Queue.dequeue`, `Stack.pop` and
        // `ArrayDeque.removeHead`/`removeLast` all answer the element they took.
        ("dequeue" | "pop" | "removeHead" | "removeLast", 0) if buffer => {
            if items.is_empty() {
                return Some(Err(empty_collection()));
            }
            let mut out = items.to_vec();
            let gone = if name == "removeLast" {
                out.pop().expect("non-empty")
            } else {
                out.remove(0)
            };
            set_seq_items(recv, kind, out);
            Some(Ok(gone))
        }
        // `Stack.top` / `Queue.front` peek without removing.
        ("top" | "front", 0) if buffer => match items.first() {
            Some(v) => Some(Ok(v.clone())),
            None => Some(Err(kind.empty_fault(EmptyOp::Head))),
        },
        // `StringBuilder`'s own character-level mutators.
        ("deleteCharAt", 1) | ("setCharAt", 2) if strbuf => {
            let i = args[0].to_int();
            if i < 0 || i as usize >= items.len() {
                return Some(Err(string_index_len(i, items.len())));
            }
            let mut out = items.to_vec();
            if name == "setCharAt" {
                out[i as usize] = args[1].clone();
            } else {
                out.remove(i as usize);
            }
            set_seq_items(recv, kind, out);
            // Both answer the builder (`this.type`), so they chain.
            Some(me())
        }
        // `setLength` truncates, or pads with NUL as `java.lang.StringBuilder`
        // does.
        ("setLength", 1) if strbuf => {
            let n = args[0].to_int();
            if n < 0 {
                return Some(Err(string_index_len(n, items.len())));
            }
            let mut out = items.to_vec();
            out.resize(n as usize, make_char('\0'));
            set_seq_items(recv, kind, out);
            Some(Ok(Value::Undef))
        }
        // `result()` freezes the builder's contents into a `String`.
        ("result", 0) if strbuf => Some(Ok(Value::str(
            items.iter().map(scala_str).collect::<String>(),
        ))),
        ("insert", 2) if strbuf => {
            let i = args[0].to_int();
            if i < 0 || i as usize > items.len() {
                return Some(Err(string_index_len(i, items.len())));
            }
            let mut out = items.to_vec();
            out.splice(i as usize..i as usize, str_chars(&args[1]));
            set_seq_items(recv, kind, out);
            Some(me())
        }
        // Removals. A table never shrinks and the surviving elements keep their
        // order, so a removal is a filter over the stored order.
        ("-=" | "--=" | "subtractOne" | "subtractAll" | "remove", 1) if is_set => {
            let drop = spread(&args[0], all);
            let present = items.iter().any(|u| value_eq(u, &args[0]));
            let kept: Vec<Value> = items
                .iter()
                .filter(|it| !drop.iter().any(|d| value_eq(d, it)))
                .cloned()
                .collect();
            set_seq_items(recv, kind, kept);
            Some(if name == "remove" {
                Ok(Value::bool(present))
            } else {
                me()
            })
        }
        ("-=" | "--=" | "subtractOne" | "subtractAll", 1) if buffer => {
            // A buffer drops only the FIRST occurrence of each value.
            let mut out = items.to_vec();
            for d in spread(&args[0], all) {
                if let Some(i) = out.iter().position(|x| value_eq(x, &d)) {
                    out.remove(i);
                }
            }
            set_seq_items(recv, kind, out);
            Some(me())
        }
        // `remove(i)` answers the element it took out.
        ("remove", 1) if buffer => {
            let i = args[0].to_int();
            if i < 0 || i as usize >= items.len() {
                return Some(Err(index_out_of_bounds(i, items.len())));
            }
            let mut out = items.to_vec();
            let gone = out.remove(i as usize);
            set_seq_items(recv, kind, out);
            Some(Ok(gone))
        }
        ("insert", 2) | ("insertAll", 2) if buffer => {
            let i = args[0].to_int();
            if i < 0 || i as usize > items.len() {
                return Some(Err(index_out_of_bounds(i, items.len())));
            }
            let mut out = items.to_vec();
            let ins = spread(&args[1], name == "insertAll");
            out.splice(i as usize..i as usize, ins);
            set_seq_items(recv, kind, out);
            Some(Ok(Value::Undef))
        }
        ("clear", 0) => {
            // Clearing does not reset the table (`java.util.Arrays.fill`), so a
            // mutable set keeps the length it grew to.
            set_seq_items(recv, kind, Vec::new());
            Some(Ok(Value::Undef))
        }
        _ => None,
    }
}

/// Replace the contents (and, for a `HashMap`, the table length) of the mutable
/// map behind `recv`. A `LinkedHashMap` iterates its insertion list, so its
/// entries are stored exactly as given; a `HashMap` is re-sorted into the
/// iteration order a table of `len` buckets produces.
fn set_map_entries(recv: &Value, into: HashRep, entries: Vec<(Value, Value)>) {
    let (ordered, sorted) = match into {
        HashRep::Mutable(len) => match mut_ordered(&entries, len as usize, |(k, _)| k.clone()) {
            Some(o) => (o, true),
            None => (entries, false),
        },
        _ => (entries, false),
    };
    if let Value::Obj(id) = recv {
        HEAP.with(|h| {
            if let Some(HeapVal::Map(rep, m)) = h.borrow_mut().get_mut(*id as usize) {
                *rep = into;
                *m = ordered;
            }
        });
    }
    // Recorded AFTER the write, so the claim describes what is now stored. A
    // representation with no table order (`Small`, `Hashed`, `Linked`) never
    // claims one — the incremental insert is a mutable-table optimization.
    mut_note_sorted(recv, sorted);
}

/// Put `adds` into the mutable map behind `recv`. On a `HashMap` this replays
/// `put0`'s table growth; on a `LinkedHashMap` there is no table order, so the
/// insertion list is simply appended to. Answers the value a repeated key
/// displaced, for `put`.
fn mut_map_put(
    recv: &Value,
    rep: HashRep,
    entries: &[(Value, Value)],
    adds: &[(Value, Value)],
    hint: Option<usize>,
) -> Option<Value> {
    // The fast path, as in `mut_set_add`: the table cannot grow even with every
    // add counted as new, the stored entries are already in table order, and
    // every added key hashes. Then each add is a binary search and a splice —
    // no copy of the map, no re-sort — which is what keeps filling one linear.
    if let HashRep::Mutable(len) = rep {
        let len = len as usize;
        let worst = vec![true; adds.len()];
        if mut_grown(mut_size_hint(len, hint), entries.len(), &worst) == len
            && mut_is_sorted(recv)
            && adds.iter().all(|(k, _)| mut_slot(k, len).is_some())
        {
            return mut_map_splice(recv, len, adds);
        }
    }
    let mut cur = entries.to_vec();
    let mut flags = Vec::with_capacity(adds.len());
    let mut displaced = None;
    for (k, v) in adds {
        match cur.iter_mut().find(|(ek, _)| value_eq(ek, k)) {
            Some(slot) => {
                displaced = Some(slot.1.clone());
                slot.1 = v.clone();
                flags.push(false);
            }
            None => {
                cur.push((k.clone(), v.clone()));
                flags.push(true);
            }
        }
    }
    let into = match rep {
        HashRep::Mutable(len) => {
            HashRep::Mutable(
                mut_grown(mut_size_hint(len as usize, hint), entries.len(), &flags) as u32,
            )
        }
        other => other,
    };
    set_map_entries(recv, into, cur);
    displaced
}

/// In-place mutation of a `mutable.Map`. `None` falls through to the shared
/// read-only implementation.
fn mut_map_method(
    recv: &Value,
    rep: HashRep,
    entries: &[(Value, Value)],
    name: &str,
    args: &[Value],
) -> Option<Result<Value, String>> {
    /// A `+=`/`++=` argument as `(key, value)` entries.
    fn as_entries(v: &Value, all: bool) -> Option<Vec<(Value, Value)>> {
        if !all {
            let t = as_seq_or_tuple(v)?;
            return (t.len() == 2).then(|| vec![(t[0].clone(), t[1].clone())]);
        }
        if let Some(m) = as_map(v) {
            return Some(m);
        }
        as_seq_or_tuple(v)?
            .iter()
            .map(|p| match as_seq_or_tuple(p) {
                Some(t) if t.len() == 2 => Some((t[0].clone(), t[1].clone())),
                _ => None,
            })
            .collect()
    }
    let all = matches!(name, "++=" | "addAll" | "--=" | "subtractAll");
    let me = || Ok(recv.clone());

    match (name, args.len()) {
        ("+=" | "++=" | "addOne" | "addAll", 1) => match as_entries(&args[0], all) {
            Some(adds) => {
                mut_map_put(
                    recv,
                    rep,
                    entries,
                    &adds,
                    all.then(|| known_size(&args[0])).flatten(),
                );
                Some(me())
            }
            None => Some(Err("scalars: Map `+=` expects `key -> value` pairs".into())),
        },
        ("update", 2) => {
            mut_map_put(
                recv,
                rep,
                entries,
                &[(args[0].clone(), args[1].clone())],
                None,
            );
            Some(Ok(Value::Undef))
        }
        ("put", 2) => {
            let old = mut_map_put(
                recv,
                rep,
                entries,
                &[(args[0].clone(), args[1].clone())],
                None,
            );
            Some(Ok(opt(old)))
        }
        ("getOrElseUpdate", 2) => match map_get(entries, &args[0]) {
            Some(v) => Some(Ok(v)),
            None => {
                mut_map_put(
                    recv,
                    rep,
                    entries,
                    &[(args[0].clone(), args[1].clone())],
                    None,
                );
                Some(Ok(args[1].clone()))
            }
        },
        // Removals keep the table length and the surviving order.
        ("-=" | "--=" | "subtractOne" | "subtractAll" | "remove", 1) => {
            let drop: Vec<Value> = if all {
                as_seq_or_tuple(&args[0]).unwrap_or_else(|| vec![args[0].clone()])
            } else {
                vec![args[0].clone()]
            };
            let old = map_get(entries, &args[0]);
            let kept: Vec<(Value, Value)> = entries
                .iter()
                .filter(|(k, _)| !drop.iter().any(|d| value_eq(d, k)))
                .cloned()
                .collect();
            set_map_entries(recv, rep, kept);
            Some(if name == "remove" { Ok(opt(old)) } else { me() })
        }
        ("clear", 0) => {
            set_map_entries(recv, rep, Vec::new());
            Some(Ok(Value::Undef))
        }
        _ => None,
    }
}

/// The element WRITES that need no copy of the receiver — the single-element
/// appends (`buf += x`, `buf.append(x)`, `q.enqueue(x)`, `sb.append(x)`) and the
/// indexed `a(i) = v` — performed in place.
///
/// [`seq_method`] opens by CLONING the receiver's elements, which nearly every
/// operation needs. These do not: an append only pushes onto the end, and an
/// indexed write only needs the LENGTH to bounds-check. Paying for the copy
/// anyway, and then a second one to rebuild the vector, made both QUADRATIC in a
/// loop — filling a 16_000-element `Array` by index took 6.8s — 20_000 `StringBuilder.append`s took 11.2s and 40_000
/// took 45.1s, the clean 4x-per-doubling of an O(n²) curve, and the 200_000 a
/// realistic program does never finished.
///
/// Only the kinds whose append is a plain push are taken here; everything else
/// falls through to the general path unchanged. A `Set` must scan for membership
/// first, a `PriorityQueue` sifts rather than appends (and is not a buffer), and
/// `Stack.push` PREPENDS — which is why the NAME is checked and not just the
/// kind, since a `Stack`'s `+=` is still `Growable.addOne` and does append.
///
/// A `StringBuilder` given an object argument also falls through: that argument
/// is rendered through a user `toString` override, which needs the VM this
/// function deliberately does not take.
/// The `mutable.Set` operations that can be answered in place, with no copy of
/// the set and no rebuild of its order — the `Set` counterpart of
/// [`mut_map_fast`], and quadratic for the same reason without it.
fn mut_set_fast(recv: &Value, name: &str, args: &[Value]) -> Option<Result<Value, String>> {
    if args.len() != 1 || !matches!(name, "+=" | "add" | "addOne" | "contains" | "apply") {
        return None;
    }
    let id = as_obj_id(recv)?;
    let (len, n) = HEAP.with(|h| match h.borrow().get(id) {
        Some(HeapVal::Seq(SeqKind::Set(HashRep::Mutable(len)), xs)) => {
            Some((*len as usize, xs.len()))
        }
        _ => None,
    })?;
    if !mut_is_sorted(recv) {
        return None;
    }
    let k = &args[0];
    mut_slot(k, len)?;
    let (at, found) = HEAP.with(|h| {
        let hb = h.borrow();
        match hb.get(id) {
            Some(HeapVal::Seq(_, xs)) => mut_find_slot(xs, len, k, Clone::clone),
            _ => None,
        }
    })?;
    if matches!(name, "contains" | "apply") {
        return Some(Ok(Value::bool(found.is_some())));
    }
    // A growth re-buckets every element, so it takes the full rebuild and pays
    // its `O(n)` there — amortized to `O(1)` per add by the doubling.
    if mut_grown(len, n, &[found.is_none()]) != len {
        return None;
    }
    if found.is_none() {
        HEAP.with(|h| {
            if let Some(HeapVal::Seq(_, xs)) = h.borrow_mut().get_mut(id) {
                xs.insert(at, k.clone());
            }
        });
    }
    Some(Ok(match name {
        "add" => Value::bool(found.is_none()),
        _ => recv.clone(),
    }))
}

fn append_in_place(recv: &Value, name: &str, args: &[Value]) -> Option<Result<Value, String>> {
    let Value::Obj(id) = recv else {
        return None;
    };
    let id = *id as usize;
    // The kind and length are read under a SHORT immutable borrow: building the
    // appended values can re-enter the heap (`str_chars` renders a collection
    // argument), so no borrow may be held across that.
    let (kind, len) = HEAP.with(|h| match h.borrow().get(id) {
        Some(HeapVal::Seq(k, xs)) => Some((*k, xs.len())),
        _ => None,
    })?;

    // `a(i) = v`, the desugar target of `Array.update`. It reads no element
    // either — only the LENGTH, for the bounds check.
    if (name, args.len()) == ("update", 2) {
        if !(kind == SeqKind::Array || kind.is_buffer()) {
            return None;
        }
        let i = args[0].to_int();
        if i < 0 || i as usize >= len {
            return Some(Err(kind.index_write_fault(i, len)));
        }
        HEAP.with(|h| {
            if let Some(HeapVal::Seq(_, xs)) = h.borrow_mut().get_mut(id) {
                xs[i as usize] = args[1].clone();
            }
        });
        return Some(Ok(Value::Undef));
    }

    if args.len() != 1 || !kind.is_buffer() {
        return None;
    }
    let appends =
        matches!(name, "+=" | "addOne" | "append") || (name == "enqueue" && kind == SeqKind::Queue);
    if !appends {
        return None;
    }
    let adds = if kind == SeqKind::StrBuf {
        if matches!(args[0], Value::Obj(_)) {
            return None;
        }
        str_chars(&args[0])
    } else {
        vec![args[0].clone()]
    };
    HEAP.with(|h| {
        if let Some(HeapVal::Seq(_, xs)) = h.borrow_mut().get_mut(id) {
            xs.extend(adds);
        }
    });
    Some(Ok(recv.clone()))
}

/// Whether an addition/removal name takes a COLLECTION of elements (`++=`,
/// `addAll`, `appendAll`, …) rather than one element (`+=`, `append`, …).
///
/// Shared by the two places that must agree about it: `mut_seq_method`, which
/// spreads the argument, and its call site, which pre-renders a `StringBuilder`'s
/// argument through a user `toString` and must NOT do so for these.
fn is_add_all_form(name: &str) -> bool {
    matches!(
        name,
        "++=" | "addAll" | "appendAll" | "prependAll" | "--=" | "subtractAll"
    )
}

/// The O(1) READS, answered off the heap without copying the receiver.
///
/// [`seq_kind_items`] CLONES every element of the receiver, and `seq_method`
/// calls it once per dispatch — so `v.length` inside a loop over a 20 000-element
/// `Vector` copied 20 000 values to answer a number it already knew, and the
/// loop as a whole moved 400 million of them. That is the same shape
/// [`append_in_place`] exists to avoid on the write side, and this is its read
/// side: `length`, `size`, `isEmpty`, `nonEmpty`, `head`, `last` and a
/// positional `apply` all read ONE fact out of the vector and need no copy of
/// it at all.
///
/// `None` leaves the call on the ordinary path, which is where every kind whose
/// answer is not the plain positional one goes:
///
/// * `Iterator`, because traversing it CONSUMES it — the drain in `seq_method`
///   is part of the answer, not an optimization to skip.
/// * `PriorityQueue`, whose `head` is the heap's root rather than the backing
///   array's first slot.
/// * a `Set` asked for `apply`, which is `contains` and not an index.
///
/// The bodies below are the same expressions the corresponding `seq_method`
/// arms evaluate, including the per-kind [`SeqKind::empty_fault`] and
/// [`SeqKind::index_fault`] messages: this is a shortcut to the same answer,
/// never a second definition of it.
fn seq_read_fast(recv: &Value, name: &str, args: &[Value]) -> Option<Result<Value, String>> {
    if !matches!(
        (name, args.len()),
        ("length", 0)
            | ("size", 0)
            | ("isEmpty", 0)
            | ("nonEmpty", 0)
            | ("head", 0)
            | ("last", 0)
            | ("apply", 1)
    ) {
        return None;
    }
    let Value::Obj(id) = recv else { return None };
    HEAP.with(|h| {
        let heap = h.borrow();
        let Some(HeapVal::Seq(kind, items)) = heap.get(*id as usize) else {
            return None;
        };
        let kind = *kind;
        if matches!(kind, SeqKind::Iterator | SeqKind::PriorityQueue)
            || (name == "apply" && matches!(kind, SeqKind::Set(_)))
        {
            return None;
        }
        Some(match name {
            "length" | "size" => Ok(Value::int(items.len() as i64)),
            "isEmpty" => Ok(Value::bool(items.is_empty())),
            "nonEmpty" => Ok(Value::bool(!items.is_empty())),
            "head" => items
                .first()
                .cloned()
                .ok_or_else(|| kind.empty_fault(EmptyOp::Head)),
            "last" => items
                .last()
                .cloned()
                .ok_or_else(|| kind.empty_fault(EmptyOp::Last)),
            _ => {
                let i = args[0].to_int();
                match usize::try_from(i).ok().and_then(|u| items.get(u)) {
                    Some(v) => Ok(v.clone()),
                    None => Err(kind.index_fault(i, items.len())),
                }
            }
        })
    })
}

/// The JDK's out-of-bounds message for an indexed sequence write.
///
/// The maximum is `len - 1` computed as a SIGNED number, so an empty receiver
/// reports `max -1`. Saturating it at zero was wrong and was invisible until an
/// `updated` on an empty sequence reached this: the reference reports
/// `0 is out of bounds (min 0, max -1)` and this answered `max 0`, naming an
/// index that is not legal either.
fn index_out_of_bounds(i: i64, len: usize) -> String {
    format!(
        "scalars: java.lang.IndexOutOfBoundsException: {i} is out of bounds (min 0, max {})",
        len as i64 - 1
    )
}

/// The out-of-range text `updated(i, v)` answers, which is NOT the one `apply`
/// answers on the same receiver: `updated` runs `IndexedSeqOps`' bounds check on
/// every collection that has one, so `List(1,2).updated(9, 0)` reports the legal
/// span where `List(1,2)(9)` reports the bare index. Measured against Scala
/// 3.9.0 on JDK 26.0.2.1.
///
/// Two cases are their own. Every MUTABLE sequence — `ListBuffer`,
/// `ArrayBuffer`, `Queue`, `Stack`, `ArrayDeque` — reports the BARE index, all
/// measured; and an EMPTY `Vector` has its own sentence, because the persistent
/// vector's check names the emptiness instead of a span it cannot state.
fn updated_fault(kind: SeqKind, i: i64, len: usize) -> String {
    match kind {
        // An `Array` is mutable but is NOT one of them: `ArrayOps.updated`
        // builds a fresh array through the indexed check, and reports the span.
        SeqKind::Array => index_out_of_bounds(i, len),
        k if k.is_mutable() => format!("scalars: java.lang.IndexOutOfBoundsException: {i}"),
        SeqKind::Vector if len == 0 => format!(
            "scalars: java.lang.IndexOutOfBoundsException: {i} is out of bounds (empty vector)"
        ),
        _ => index_out_of_bounds(i, len),
    }
}

/// `Seq` (`List`/`Set`/`Iterable`) methods — a faithful subset. Closure-taking
/// ops run their function argument through [`invoke_closure`].
fn seq_method(vm: &mut VM, recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    // Appending is answered BEFORE the receiver's elements are read, because
    // reading them is what made appending quadratic — see [`append_in_place`].
    if let Some(r) = append_in_place(recv, name, args) {
        return r;
    }
    // Same reasoning for a `mutable.Set`, whose adds and lookups need neither a
    // copy nor a re-sort.
    if let Some(r) = mut_set_fast(recv, name, args) {
        return r;
    }
    // The O(1) reads, likewise before the receiver is copied — see
    // [`seq_read_fast`].
    if let Some(r) = seq_read_fast(recv, name, args) {
        return r;
    }
    let (kind, items) = seq_kind_items(recv).unwrap_or((SeqKind::List, Vec::new()));
    // A `scala.collection.Iterator` is CONSUMED by traversing it, and that is
    // the one part of its laziness a strict frontend must still reproduce: the
    // elements were read into `items` just above, so emptying the receiver now
    // makes the SECOND traversal see an exhausted iterator the way Scala's does
    // (`it.toList` answers the elements, then `List()`).
    //
    // Only the ops that ask whether it is exhausted leave it alone. `next`
    // consumes exactly one element and drains itself on its own arm below.
    if kind == SeqKind::Iterator
        && !matches!(
            name,
            "hasNext" | "next" | "isEmpty" | "nonEmpty" | "knownSize"
        )
    {
        drain_iterator(recv);
    }
    // A transforming op keeps the receiver's collection kind (`List.map` → `List`,
    // a range-derived `Vector.map` → `Vector`).
    let same = |v: Vec<Value>| derive_seq(kind, v);
    // A transforming op whose element type may change takes its builder from
    // `iterableFactory` rather than `fromSpecific`. Every kind here rebuilds
    // itself except `StringBuilder`, which is a `mutable.IndexedSeq` and so
    // falls back to that trait's factory: `sb.map(_.toUpper)` is an
    // `ArrayBuffer`, where the selecting `sb.filter(…)` is a `StringBuilder`.
    let mapped = |v: Vec<Value>| match kind {
        // A `PriorityQueue` is likewise not an `IterableFactory`: its builder
        // needs an `Ordering` for the result's element type, which `map` cannot
        // supply, so `pq.map(f)` is an `ArrayBuffer` where the selecting
        // `pq.filter(p)` is still a `PriorityQueue`.
        SeqKind::StrBuf | SeqKind::PriorityQueue => new_seq(SeqKind::ArrayBuffer, v),
        _ => derive_seq(kind, v),
    };
    // In-place mutation (`+=`, `clear()`, …) — before the pure paths, because
    // several names (`+`, `-`, `++`) mean "mutate me" on a mutable receiver and
    // "build a new one" on an immutable one.
    // The heap members, before the generic mutable ones: `+=` on a
    // `PriorityQueue` sifts rather than appends, and `head` is the heap's root.
    if kind == SeqKind::PriorityQueue {
        if let Some(r) = priority_queue_method(vm, recv, &items, name, args) {
            return r;
        }
    }
    if kind.is_mutable() {
        // What a `StringBuilder` addition contributes is the argument's
        // `String.valueOf`, so an appended instance renders through its
        // `toString`. `mut_seq_method` holds no VM, so the argument is rendered
        // here — the string it becomes then appends character by character
        // exactly as a literal would.
        //
        // Only the ONE-element forms, though. `++=`/`appendAll` take an
        // `IterableOnce[Char]` and contribute its ELEMENTS, and rendering the
        // argument first destroyed that: `sb ++= List('x', 'y')` appended the
        // eleven characters of `List(x, y)` instead of `xy`.
        let rendered: Vec<Value>;
        let args = if kind == SeqKind::StrBuf
            && !is_add_all_form(name)
            && matches!(args.first(), Some(Value::Obj(_)))
        {
            rendered = args
                .iter()
                .map(|a| match a {
                    Value::Obj(_) => Value::str(scala_str_vm(vm, a)),
                    other => other.clone(),
                })
                .collect();
            &rendered[..]
        } else {
            args
        };
        if let Some(r) = mut_seq_method(recv, kind, &items, name, args) {
            return r;
        }
    }
    // A `StringBuilder` is a `CharSequence` as well as a `Seq[Char]`, and the
    // text-shaped members it forwards to `java.lang.StringBuilder` are not
    // sequence operations at all. `indexOf`/`lastIndexOf` are overloaded on both
    // sides, so only a STRING argument takes the text one — a `Char` argument
    // still means `SeqOps.indexOf(elem)`.
    if kind == SeqKind::StrBuf {
        let text_arg = matches!(args.first(), Some(Value::Str(_)));
        if matches!(name, "substring" | "charAt")
            || (text_arg && matches!(name, "indexOf" | "lastIndexOf"))
        {
            let text: String = items.iter().map(scala_str).collect();
            return string_method(&text, name, args);
        }
    }
    // The pure slice/reorder methods first — they share one body.
    if let Some(out) = seq_slice_method(&items, name, args) {
        return Ok(same(out));
    }
    match (name, args.len()) {
        // `Range.reverse` is another `Range`, walked the other way.
        ("reverse", 0) if matches!(kind, SeqKind::Range { .. }) => {
            let SeqKind::Range { step, .. } = kind else {
                unreachable!("just matched a range")
            };
            match (items.first(), items.last()) {
                (Some(a), Some(b)) => Ok(new_seq(
                    SeqKind::Range {
                        start: b.to_int(),
                        end: a.to_int(),
                        inclusive: true,
                        step: -step,
                    },
                    items.iter().rev().cloned().collect(),
                )),
                _ => Ok(new_seq(kind, Vec::new())),
            }
        }
        // `(a to b) by s` — rebuild the range with a new step.
        ("by", 1) => {
            let SeqKind::Range {
                start,
                end,
                inclusive,
                ..
            } = kind
            else {
                return Err(no_such_method(recv, name));
            };
            let step = args[0].to_int();
            let items = range_items(start, end, inclusive, step)?;
            Ok(new_seq(
                SeqKind::Range {
                    start,
                    end,
                    inclusive,
                    step,
                },
                items,
            ))
        }
        // In-place element assignment (`a(i) = v`), the desugar target of
        // `Array.update`. Only a `mutable.Seq` answers it — an `Array` or one of
        // the growable buffers — and those are all handled by
        // [`append_in_place`] before the elements are copied, so what reaches
        // here is an immutable receiver, which has no `update` at all.
        ("update", 2) => Err(no_such_method(recv, name)),
        // `max`/`min` under the implicit ordering. The explicit-`Ordering` form
        // runs a user closure when the ordering is keyed, so it is answered on
        // the VM-aware arm below.
        ("min", 0) | ("max", 0) => {
            let want = if name == "max" {
                Ordering::Greater
            } else {
                Ordering::Less
            };
            let Some(first) = items.first() else {
                // A `Range` knows its extremes without scanning, so it answers
                // `last`/`head` on an empty receiver rather than `empty.max`.
                if matches!(kind, SeqKind::Range { .. }) {
                    let op = if name == "max" {
                        EmptyOp::Last
                    } else {
                        EmptyOp::Head
                    };
                    return Err(kind.empty_fault(op));
                }
                return Err(format!(
                    "scalars: java.lang.UnsupportedOperationException: empty.{name}"
                ));
            };
            let mut best = first.clone();
            for it in items.iter().skip(1) {
                if cmp_vm(vm, it, &best)? == want {
                    best = it.clone();
                }
            }
            Ok(best)
        }
        ("toArray", 0) => Ok(new_seq(SeqKind::Array, items)),
        ("toVector", 0) => Ok(new_seq(SeqKind::Vector, items)),
        // `toIndexedSeq` is `Vector` for every collection EXCEPT an `Array`,
        // whose `IndexedSeq` is the `ArraySeq` that wraps it in place —
        // `Array(1,2).toIndexedSeq` prints `ArraySeq(1, 2)` where
        // `List(1,2).toIndexedSeq` prints `Vector(1, 2)`.
        ("toIndexedSeq", 0) => Ok(new_seq(
            if kind == SeqKind::Array {
                SeqKind::ArraySeq
            } else {
                SeqKind::Vector
            },
            items,
        )),
        // `startsWith(that[, offset])` / `endsWith(that)` — prefix and suffix
        // tests against ANOTHER SEQUENCE (`String`'s same-named methods take a
        // string and live in [`string_method`]). An argument longer than what
        // remains is `false`, and an empty one is `true` at any legal offset.
        ("startsWith", 1) | ("startsWith", 2) | ("endsWith", 1) => {
            let that = as_seq_or_tuple(&args[0]).unwrap_or_default();
            let from = if args.len() == 2 {
                args[1].to_int()
            } else if name == "endsWith" {
                items.len() as i64 - that.len() as i64
            } else {
                0
            };
            if from < 0 || from as usize + that.len() > items.len() {
                return Ok(Value::bool(false));
            }
            let at = from as usize;
            Ok(Value::bool(
                that.iter()
                    .enumerate()
                    .all(|(i, e)| scala_eq(&items[at + i], e)),
            ))
        }
        // `iterator`/`reverseIterator` materialize their elements — this
        // frontend is strict — but answer an `Iterator`, not an `Iterable`, so
        // the two behaviors that ARE observable without laziness hold: it
        // renders as `<iterator>`, and traversing it consumes it.
        ("iterator", 0) => Ok(new_seq(SeqKind::Iterator, items)),
        ("reverseIterator", 0) => Ok(new_seq(
            SeqKind::Iterator,
            items.into_iter().rev().collect(),
        )),
        // The `Iterator` protocol itself. Guarded on the kind because Scala
        // declares neither on any other collection — `List(1).hasNext` is a
        // compile error there and stays a missing method here.
        // `.view` is STRICT here, exactly as `.iterator` is: the elements are
        // already materialized, so a downstream combinator sees what a lazy
        // view would have produced. Only the view's own rendering differs from
        // an ordinary collection's, and that is what the kind carries.
        ("view", 0) => Ok(new_seq(
            match kind {
                SeqKind::Array => SeqKind::ArrayView,
                SeqKind::Vector
                | SeqKind::ArraySeq
                | SeqKind::ArrayBuffer
                | SeqKind::Range { .. } => SeqKind::View(true),
                _ => SeqKind::View(false),
            },
            items.to_vec(),
        )),
        ("hasNext", 0) if kind == SeqKind::Iterator => Ok(Value::bool(!items.is_empty())),
        ("next", 0) if kind == SeqKind::Iterator => {
            let Some(first) = items.first().cloned() else {
                return Err(
                    "scalars: java.util.NoSuchElementException: next on empty iterator".to_string(),
                );
            };
            take_iterator_head(recv);
            Ok(first)
        }
        ("length" | "size", 0) => Ok(Value::int(items.len() as i64)),
        ("isEmpty", 0) => Ok(Value::bool(items.is_empty())),
        ("nonEmpty", 0) => Ok(Value::bool(!items.is_empty())),
        // The four empty-receiver accesses. Each kind words its own failure —
        // see [`SeqKind::empty_fault`] — so they route through that table
        // rather than answering `List`'s message for every collection.
        ("head", 0) => items
            .first()
            .cloned()
            .ok_or_else(|| kind.empty_fault(EmptyOp::Head)),
        ("last", 0) => items
            .last()
            .cloned()
            .ok_or_else(|| kind.empty_fault(EmptyOp::Last)),
        ("tail", 0) => {
            if items.is_empty() {
                Err(kind.empty_fault(EmptyOp::Tail))
            } else {
                Ok(same(items[1..].to_vec()))
            }
        }
        ("init", 0) => {
            if items.is_empty() {
                Err(kind.empty_fault(EmptyOp::Init))
            } else {
                Ok(same(items[..items.len() - 1].to_vec()))
            }
        }
        ("reverse", 0) => Ok(same(items.iter().rev().cloned().collect())),
        ("sum", 0) => Ok(seq_sum(&items)),
        ("mkString", 0) => Ok(Value::str(join_vm(vm, &items, ""))),
        ("mkString", 1) => {
            let sep = args[0].as_str_cow().into_owned();
            Ok(Value::str(join_vm(vm, &items, &sep)))
        }
        ("contains", 1) => Ok(Value::bool(items.iter().any(|x| value_eq(x, &args[0])))),
        // A `Set`'s `apply` is `contains`, not an index: `Set(1,2,3)(2)` is
        // `true` and `Set(1,2,3)(9)` is `false`. Reading it positionally
        // answered the ELEMENT (`3`) and threw an `IndexOutOfBoundsException`
        // for a member that is simply absent.
        ("apply", 1) if matches!(kind, SeqKind::Set(_)) => {
            Ok(Value::bool(items.iter().any(|x| value_eq(x, &args[0]))))
        }
        ("apply", 1) => {
            let i = args[0].to_int();
            match usize::try_from(i).ok().and_then(|u| items.get(u)) {
                Some(v) => Ok(v.clone()),
                None => Err(kind.index_fault(i, items.len())),
            }
        }
        ("toList", 0) => Ok(new_list(items)),
        ("map", 1) => {
            let mut out = Vec::with_capacity(items.len());
            for it in &items {
                out.push(invoke_closure(vm, &args[0], std::slice::from_ref(it))?);
            }
            Ok(mapped(out))
        }
        ("flatMap", 1) => {
            let mut out = Vec::new();
            for it in &items {
                let r = invoke_closure(vm, &args[0], std::slice::from_ref(it))?;
                match as_seq(&r) {
                    Some(inner) => out.extend(inner),
                    None => return Err("scalars: flatMap function must return a collection".into()),
                }
            }
            Ok(mapped(out))
        }
        ("filter" | "filterNot" | "withFilter", 1) => {
            let keep_if = name != "filterNot";
            let mut out = Vec::new();
            for it in &items {
                let r = invoke_closure(vm, &args[0], std::slice::from_ref(it))?;
                if truthy(&r) == keep_if {
                    out.push(it.clone());
                }
            }
            Ok(same(out))
        }
        ("foreach", 1) => {
            for it in &items {
                invoke_closure(vm, &args[0], std::slice::from_ref(it))?;
            }
            Ok(unit_value())
        }
        // `collect` / `collectFirst` — `filter` and `map` in one pass, driven by
        // the partial function's `isDefinedAt` so an element no arm matches is
        // skipped rather than raising `MatchError`. This is Scala's
        // `applyOrElse` protocol: the arm body runs only for a defined element.
        ("collect", 1) => {
            let mut out = Vec::new();
            for it in &items {
                if is_defined_at(vm, &args[0], it)? {
                    out.push(invoke_closure(vm, &args[0], std::slice::from_ref(it))?);
                }
            }
            Ok(mapped(out))
        }
        ("collectFirst", 1) => {
            for it in &items {
                if is_defined_at(vm, &args[0], it)? {
                    return Ok(make_some(invoke_closure(
                        vm,
                        &args[0],
                        std::slice::from_ref(it),
                    )?));
                }
            }
            Ok(make_none())
        }
        ("exists", 1) => {
            for it in &items {
                if truthy(&invoke_closure(vm, &args[0], std::slice::from_ref(it))?) {
                    return Ok(Value::bool(true));
                }
            }
            Ok(Value::bool(false))
        }
        ("forall", 1) => {
            for it in &items {
                if !truthy(&invoke_closure(vm, &args[0], std::slice::from_ref(it))?) {
                    return Ok(Value::bool(false));
                }
            }
            Ok(Value::bool(true))
        }
        ("count", 1) => {
            let mut n = 0i64;
            for it in &items {
                if truthy(&invoke_closure(vm, &args[0], std::slice::from_ref(it))?) {
                    n += 1;
                }
            }
            Ok(Value::int(n))
        }
        ("foldLeft", 2) => {
            let mut acc = args[0].clone();
            for it in &items {
                acc = invoke_closure(vm, &args[1], &[acc, it.clone()])?;
            }
            Ok(acc)
        }
        ("foldRight", 2) => {
            let mut acc = args[0].clone();
            for it in items.iter().rev() {
                acc = invoke_closure(vm, &args[1], &[it.clone(), acc])?;
            }
            Ok(acc)
        }
        // `scanLeft`/`scanRight` are the folds that keep every intermediate
        // accumulator, so the result is one longer than the receiver and always
        // starts (`scanLeft`) or ends (`scanRight`) with the seed — an empty
        // receiver still answers the one-element `List(seed)`.
        ("scanLeft", 2) => {
            let mut acc = args[0].clone();
            let mut out = vec![acc.clone()];
            for it in &items {
                acc = invoke_closure(vm, &args[1], &[acc, it.clone()])?;
                out.push(acc.clone());
            }
            Ok(new_seq(kind, out))
        }
        ("scanRight", 2) => {
            let mut acc = args[0].clone();
            let mut out = vec![acc.clone()];
            for it in items.iter().rev() {
                acc = invoke_closure(vm, &args[1], &[it.clone(), acc])?;
                out.push(acc.clone());
            }
            out.reverse();
            Ok(new_seq(kind, out))
        }
        ("reduce" | "reduceLeft", 1) => {
            if items.is_empty() {
                return Err(
                    "scalars: java.lang.UnsupportedOperationException: empty.reduceLeft".into(),
                );
            }
            let mut acc = items[0].clone();
            for it in &items[1..] {
                acc = invoke_closure(vm, &args[0], &[acc, it.clone()])?;
            }
            Ok(acc)
        }
        ("reduceRight", 1) => {
            if items.is_empty() {
                return Err(
                    "scalars: java.lang.UnsupportedOperationException: empty.reduceRight".into(),
                );
            }
            let mut acc = items[items.len() - 1].clone();
            for it in items[..items.len() - 1].iter().rev() {
                acc = invoke_closure(vm, &args[0], &[it.clone(), acc])?;
            }
            Ok(acc)
        }
        // `fold(z)(op)` — the parser flattens the two argument lists into one.
        ("fold", 2) => {
            let mut acc = args[0].clone();
            for it in &items {
                acc = invoke_closure(vm, &args[1], &[acc, it.clone()])?;
            }
            Ok(acc)
        }
        ("headOption", 0) => Ok(opt(items.first().cloned())),
        ("lastOption", 0) => Ok(opt(items.last().cloned())),
        ("product", 0) => Ok(seq_product(&items)),
        ("mkString", 3) => {
            let joined = join_vm(vm, &items, &args[1].as_str_cow());
            Ok(Value::str(format!(
                "{}{joined}{}",
                args[0].as_str_cow(),
                args[2].as_str_cow()
            )))
        }
        ("indexOf", 1) => Ok(Value::int(
            items
                .iter()
                .position(|x| value_eq(x, &args[0]))
                .map_or(-1, |i| i as i64),
        )),
        ("lastIndexOf", 1) => Ok(Value::int(
            items
                .iter()
                .rposition(|x| value_eq(x, &args[0]))
                .map_or(-1, |i| i as i64),
        )),
        // `xs.padTo(len, elem)` — `xs` extended with `elem` up to `len`. A `len`
        // at or below the current size returns the sequence unchanged.
        ("padTo", 2) => {
            let want = args[0].to_int().max(0) as usize;
            let mut out = items.clone();
            while out.len() < want {
                out.push(args[1].clone());
            }
            Ok(same(out))
        }
        ("zip", 1) => {
            let other = as_seq_or_tuple(&args[0]).unwrap_or_default();
            Ok(same(
                items
                    .iter()
                    .zip(other)
                    .map(|(a, b)| new_pair(a.clone(), b))
                    .collect(),
            ))
        }
        // `zipAll(that, thisElem, thatElem)` — the zip that runs to the LONGER
        // of the two, padding whichever ran out. `zip` stops at the shorter.
        ("zipAll", 3) => {
            let other = as_seq_or_tuple(&args[0]).unwrap_or_default();
            let n = items.len().max(other.len());
            Ok(same(
                (0..n)
                    .map(|i| {
                        new_pair(
                            items.get(i).cloned().unwrap_or_else(|| args[1].clone()),
                            other.get(i).cloned().unwrap_or_else(|| args[2].clone()),
                        )
                    })
                    .collect(),
            ))
        }
        ("zipWithIndex", 0) => Ok(same(
            items
                .iter()
                .enumerate()
                .map(|(i, a)| new_pair(a.clone(), Value::int(i as i64)))
                .collect(),
        )),
        ("unzip", 0) => {
            let mut ls = Vec::with_capacity(items.len());
            let mut rs = Vec::with_capacity(items.len());
            for it in &items {
                match as_seq_or_tuple(it) {
                    Some(t) if t.len() == 2 => {
                        ls.push(t[0].clone());
                        rs.push(t[1].clone());
                    }
                    _ => return Err("scalars: unzip expects a collection of pairs".into()),
                }
            }
            Ok(new_pair(same(ls), same(rs)))
        }
        ("splitAt", 1) => {
            let at = clamp(args[0].to_int(), items.len());
            Ok(new_pair(
                same(items[..at].to_vec()),
                same(items[at..].to_vec()),
            ))
        }
        // `grouped`/`sliding` — all three spellings are ONE walk (see
        // [`window_starts`]): `grouped(n)` is `sliding(n, n)` and `sliding(n)` is
        // `sliding(n, 1)`, which is what `GroupedIterator` does underneath. They
        // used to be three special cases — `chunks`, `windows`, and a receiver
        // shorter than the window — and the two-argument form was missing, so
        // `xs.sliding(2, 2)` was refused outright.
        ("grouped" | "sliding", 1) | ("sliding", 2) => {
            let n = args[0].to_int();
            let step = match (name, args.len()) {
                (_, 2) => args[1].to_int(),
                ("grouped", _) => n,
                _ => 1,
            };
            if n < 1 || step < 1 {
                // Both are `iterateUntilEmpty(size, step)` underneath, and its
                // `require` reports BOTH numbers — which is why `grouped(0)` and
                // `sliding(0)` print different text for the same argument.
                return Err(format!(
                    "scalars: java.lang.IllegalArgumentException: requirement failed: \
                     size={n} and step={step}, but both must be positive"
                ));
            }
            let out = window_starts(items.len(), n as usize, step as usize)
                .map(|(a, b)| same(items[a..b].to_vec()))
                .collect();
            // Both answer an `Iterator` of windows, not a `List` of them, so the
            // un-consumed result prints `<iterator>` and a second traversal
            // sees it exhausted.
            Ok(new_seq(SeqKind::Iterator, out))
        }
        // `tails`/`inits` — every suffix and every prefix, longest first, both
        // ending in the empty one. An empty receiver still answers ONE window
        // (itself), never none, which is the opposite of `sliding`'s answer for
        // the same receiver. Iterators, like the two above.
        ("tails", 0) => Ok(new_seq(
            SeqKind::Iterator,
            (0..=items.len())
                .map(|i| same(items[i..].to_vec()))
                .collect(),
        )),
        ("inits", 0) => Ok(new_seq(
            SeqKind::Iterator,
            (0..=items.len())
                .rev()
                .map(|i| same(items[..i].to_vec()))
                .collect(),
        )),
        // `updated(i, v)` — the copy with one element replaced. Bounds-checked
        // against the same message `apply` reports, because it is the same
        // `IndexOutOfBoundsException` the standard library raises.
        ("updated", 2) => {
            let i = args[0].to_int();
            if i < 0 || i as usize >= items.len() {
                return Err(updated_fault(kind, i, items.len()));
            }
            let mut out = items;
            out[i as usize] = args[1].clone();
            Ok(same(out))
        }
        // `permutations`/`combinations(n)` — the DISTINCT arrangements and
        // sub-multisets, in the order [`first_occurrence_order`] explains. Both
        // answer an `Iterator` (so an un-consumed one prints `<iterator>` and a
        // second traversal sees it exhausted) whose windows keep the receiver's
        // own kind, exactly as `grouped`/`sliding` do.
        ("permutations", 0) => Ok(new_seq(
            SeqKind::Iterator,
            permutations_of(&items).into_iter().map(same).collect(),
        )),
        ("combinations", 1) => Ok(new_seq(
            SeqKind::Iterator,
            combinations_of(&items, args[0].to_int())
                .into_iter()
                .map(same)
                .collect(),
        )),
        ("toSet", 0) => Ok(new_set(HashRep::Small, items)),
        // `toIterable` is `Iterable.toIterable`, defined as `this`: it changes
        // nothing at all. A `Vector` stays a `Vector`, a `Set` stays a `Set`, a
        // `ListBuffer` stays a `ListBuffer` and a `Range` stays a `Range`.
        // Answering `List(…)` for all of them — which is what this did — was
        // wrong for every receiver except a `List`.
        //
        // An `Array` is the exception, because it is not an `Iterable` at all:
        // `ArrayOps.toIterable` wraps it in the `immutable.ArraySeq` that is its
        // `IndexedSeq` view.
        ("toIterable", 0) if kind == SeqKind::Array => Ok(new_seq(SeqKind::ArraySeq, items)),
        ("toIterable", 0) => Ok(recv.clone()),
        // `toSeq` answers an IMMUTABLE `Seq`, so it is `this` only when the
        // receiver already is one — `Vector(1,2).toSeq` is that same `Vector`
        // and `(1 to 3).toSeq` is that same `Range`. A `Set`, a `Map`, an
        // `Iterator` and every MUTABLE sequence must be copied out, and their
        // builder is `List`'s: `ListBuffer(1,2).toSeq` is `List(1, 2)`, not a
        // `ListBuffer`. An `Array`'s is `ArraySeq`, as above.
        ("toSeq", 0) => match kind {
            SeqKind::List | SeqKind::Vector | SeqKind::ArraySeq | SeqKind::Range { .. } => {
                Ok(recv.clone())
            }
            SeqKind::Array => Ok(new_seq(SeqKind::ArraySeq, items)),
            _ => Ok(new_list(items)),
        },
        ("toMap", 0) => {
            let mut entries: Vec<(Value, Value)> = Vec::with_capacity(items.len());
            for it in &items {
                match as_seq_or_tuple(it) {
                    Some(t) if t.len() == 2 => map_put(&mut entries, t[0].clone(), t[1].clone()),
                    _ => return Err("scalars: toMap expects a collection of pairs".into()),
                }
            }
            Ok(new_map(HashRep::Small, entries))
        }
        // Scala's `groupBy` builds through a `HashMap` builder, so its result is a
        // `HashMap` however few groups there are.
        ("groupBy", 1) => {
            let ks = keys_of(vm, &args[0], &items)?;
            let mut entries: Vec<(Value, Vec<Value>)> = Vec::new();
            for (k, it) in ks.into_iter().zip(items.iter()) {
                match entries.iter_mut().find(|(ek, _)| value_eq(ek, &k)) {
                    Some(slot) => slot.1.push(it.clone()),
                    None => entries.push((k, vec![it.clone()])),
                }
            }
            Ok(new_map(
                HashRep::Hashed,
                entries
                    .into_iter()
                    .map(|(k, group)| (k, same(group)))
                    .collect(),
            ))
        }
        ("sortBy", 1) => {
            // Sorted by INDEX so `f` runs exactly once per element, as Scala's
            // `sortBy` (a `map`-then-sort over the keys) does.
            let ks = keys_of(vm, &args[0], &items)?;
            let idx = merge_sort_idx(vm, items.len(), &mut |vm, i, j| cmp_vm(vm, &ks[i], &ks[j]))?;
            Ok(same(idx.into_iter().map(|i| items[i].clone()).collect()))
        }
        // The implicit ordering. A user class that extends `Ordered` supplies it
        // through its own `compare`, which needs the VM — so this cannot be
        // answered by the pure `seq_slice_method` path above.
        ("sorted", 0) => Ok(same(sort_values(vm, &items)?)),
        // An EXPLICIT `Ordering`. Scala passes it as a second (implicit)
        // parameter list, which this frontend flattens into the same call, so
        // `xs.sorted(ord)` arrives with one argument and `xs.sortBy(f)(ord)`
        // with two.
        ("sorted", 1) if as_ordering(&args[0]).is_some() => {
            Ok(same(sort_by_ordering(vm, &args[0], &items)?))
        }
        ("sortBy", 2) if as_ordering(&args[1]).is_some() => {
            let keyed = make_keyed_ordering(vm, &args[0], &args[1])?;
            Ok(same(sort_by_ordering(vm, &keyed, &items)?))
        }
        ("max" | "min", 1) if as_ordering(&args[0]).is_some() => {
            best_by_ordering(vm, &args[0], &items, name)
        }
        ("maxBy" | "minBy", 2) if as_ordering(&args[1]).is_some() => {
            let keyed = make_keyed_ordering(vm, &args[0], &args[1])?;
            best_by_ordering(vm, &keyed, &items, name)
        }
        ("sortWith", 1) => {
            // A user `lt` may be inconsistent, and `sort_by` panics on a bad
            // comparator, so the merge is written out (stably) instead.
            let mut out: Vec<Value> = Vec::with_capacity(items.len());
            for it in &items {
                let mut at = out.len();
                while at > 0
                    && truthy(&invoke_closure(
                        vm,
                        &args[0],
                        &[it.clone(), out[at - 1].clone()],
                    )?)
                {
                    at -= 1;
                }
                out.insert(at, it.clone());
            }
            Ok(same(out))
        }
        ("maxBy" | "minBy", 1) => {
            if items.is_empty() {
                return Err(format!(
                    "scalars: java.lang.UnsupportedOperationException: empty.{name}"
                ));
            }
            let ks = keys_of(vm, &args[0], &items)?;
            let want = if name == "maxBy" {
                Ordering::Greater
            } else {
                Ordering::Less
            };
            let mut best = 0usize;
            for i in 1..items.len() {
                if cmp_vm(vm, &ks[i], &ks[best])? == want {
                    best = i;
                }
            }
            Ok(items[best].clone())
        }
        ("find", 1) => {
            for it in &items {
                if truthy(&invoke_closure(vm, &args[0], std::slice::from_ref(it))?) {
                    return Ok(make_some(it.clone()));
                }
            }
            Ok(make_none())
        }
        ("indexWhere", 1) => {
            for (i, it) in items.iter().enumerate() {
                if truthy(&invoke_closure(vm, &args[0], std::slice::from_ref(it))?) {
                    return Ok(Value::int(i as i64));
                }
            }
            Ok(Value::int(-1))
        }
        ("lastIndexWhere", 1) => {
            for (i, it) in items.iter().enumerate().rev() {
                if truthy(&invoke_closure(vm, &args[0], std::slice::from_ref(it))?) {
                    return Ok(Value::int(i as i64));
                }
            }
            Ok(Value::int(-1))
        }
        // The length of the longest PREFIX all of whose elements hold — it stops
        // at the first failure rather than counting every match.
        ("segmentLength", 1) => {
            let mut n = 0usize;
            while n < items.len()
                && truthy(&invoke_closure(
                    vm,
                    &args[0],
                    std::slice::from_ref(&items[n]),
                )?)
            {
                n += 1;
            }
            Ok(Value::int(n as i64))
        }
        ("takeWhile" | "dropWhile", 1) => {
            let mut n = 0usize;
            while n < items.len()
                && truthy(&invoke_closure(
                    vm,
                    &args[0],
                    std::slice::from_ref(&items[n]),
                )?)
            {
                n += 1;
            }
            Ok(same(if name == "takeWhile" {
                items[..n].to_vec()
            } else {
                items[n..].to_vec()
            }))
        }
        ("partition" | "span", 1) => {
            let mut yes = Vec::new();
            let mut no = Vec::new();
            let mut split = false;
            for it in &items {
                let hit = truthy(&invoke_closure(vm, &args[0], std::slice::from_ref(it))?);
                // `span` stops testing at the first failure; `partition` sorts
                // every element into one side or the other.
                if name == "span" && !hit {
                    split = true;
                }
                if if name == "span" { !split } else { hit } {
                    yes.push(it.clone());
                } else {
                    no.push(it.clone());
                }
            }
            Ok(new_pair(same(yes), same(no)))
        }
        // Set algebra. `+`/`-` also reach here through the numeric hook.
        ("union" | "++" | "concat" | "|", 1) => {
            let mut out = items.clone();
            out.extend(as_seq_or_tuple(&args[0]).unwrap_or_default());
            Ok(same(out))
        }
        ("intersect" | "&", 1) => {
            let other = as_seq_or_tuple(&args[0]).unwrap_or_default();
            Ok(same(
                items
                    .iter()
                    .filter(|x| other.iter().any(|y| value_eq(x, y)))
                    .cloned()
                    .collect(),
            ))
        }
        ("diff" | "--" | "removedAll" | "&~", 1) => {
            let other = as_seq_or_tuple(&args[0]).unwrap_or_default();
            Ok(same(
                items
                    .iter()
                    .filter(|x| !other.iter().any(|y| value_eq(x, y)))
                    .cloned()
                    .collect(),
            ))
        }
        ("subsetOf", 1) => {
            let other = as_seq_or_tuple(&args[0]).unwrap_or_default();
            Ok(Value::bool(
                items.iter().all(|x| other.iter().any(|y| value_eq(x, y))),
            ))
        }
        ("incl" | "+", 1) if matches!(kind, SeqKind::Set(_)) => {
            let mut out = items.clone();
            out.push(args[0].clone());
            Ok(same(out))
        }
        ("excl" | "-", 1) if matches!(kind, SeqKind::Set(_)) => Ok(same(
            items
                .iter()
                .filter(|x| !value_eq(x, &args[0]))
                .cloned()
                .collect(),
        )),
        (":+" | "appended", 1) => {
            let mut out = items.clone();
            out.push(args[0].clone());
            Ok(same(out))
        }
        ("+:" | "prepended", 1) => {
            let mut out = vec![args[0].clone()];
            out.extend(items);
            Ok(same(out))
        }
        _ => Err(no_such_method(recv, name)),
    }
}

/// Multiply a numeric sequence (`Int` result when all `Int`, else `Double`).
///
/// The integral accumulator wraps, for the reason given on [`seq_sum`].
fn seq_product(items: &[Value]) -> Value {
    if items.iter().all(|v| matches!(v, Value::Int(_))) {
        Value::int(
            items
                .iter()
                .map(Value::to_int)
                .fold(1i64, i64::wrapping_mul),
        )
    } else {
        Value::float(items.iter().map(Value::to_float).product())
    }
}

/// `Map` methods — a faithful subset. A method that answers another map keeps
/// the receiver's representation (a `HashMap` stays hashed however few entries
/// survive); one that answers a bare sequence answers a `List`, as Scala's
/// `Map.to*` do.
fn map_method(vm: &mut VM, recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    // Asked BEFORE the entries are copied: the in-place reads and writes need
    // neither a copy nor a rebuild, and paying for one on every `m(k) = v` is
    // what made filling a map quadratic.
    if let Some((rep, n)) = map_rep_len(recv) {
        if let Some(r) = mut_map_fast(recv, rep, n, name, args) {
            return r;
        }
    }
    let (rep, entries) = map_rep_entries(recv).unwrap_or((HashRep::Small, Vec::new()));
    // Every closure-taking `Map` method passes one `Tuple2` argument. Built on
    // DEMAND, because materializing it allocates a fresh heap tuple PER ENTRY:
    // the methods that never traverse the map — `apply`, `get`, `size`, and
    // above all the `update` that fills one in a loop — were paying n
    // allocations on every call.
    let pairs = || -> Vec<Value> {
        entries
            .iter()
            .map(|(k, v)| new_pair(k.clone(), v.clone()))
            .collect()
    };
    // In-place mutation, before the pure paths: on a `mutable.Map` the names
    // `+`/`-`/`++` still build a new map, but `+=`/`-=`/`update`/`put` mutate.
    if matches!(rep, HashRep::Mutable(_) | HashRep::Linked) {
        if let Some(r) = mut_map_method(recv, rep, &entries, name, args) {
            return r;
        }
    }
    match (name, args.len()) {
        // `Iterable.toIterable` is `this`: a `Map` stays a `Map`.
        ("toIterable", 0) => Ok(recv.clone()),
        ("updated", 2) | ("+", 1) => {
            let mut out = entries.clone();
            match (name, as_seq_or_tuple(&args[0])) {
                ("updated", _) => map_put(&mut out, args[0].clone(), args[1].clone()),
                (_, Some(t)) if t.len() == 2 => map_put(&mut out, t[0].clone(), t[1].clone()),
                _ => return Err("scalars: Map `+` expects a `key -> value` pair".into()),
            }
            Ok(new_map(rep, out))
        }
        ("removed" | "-", 1) => Ok(new_map(
            rep,
            entries
                .iter()
                .filter(|(k, _)| !value_eq(k, &args[0]))
                .cloned()
                .collect(),
        )),
        ("++" | "concat", 1) => {
            let mut out = entries.clone();
            for p in as_seq_or_tuple(&args[0]).unwrap_or_default() {
                match as_seq_or_tuple(&p) {
                    Some(t) if t.len() == 2 => map_put(&mut out, t[0].clone(), t[1].clone()),
                    _ => return Err("scalars: Map `++` expects `key -> value` pairs".into()),
                }
            }
            for (k, v) in as_map(&args[0]).unwrap_or_default() {
                map_put(&mut out, k, v);
            }
            Ok(new_map(rep, out))
        }
        // `m.map(f)` answers a `Map` when `f` answers pairs (Scala picks the
        // `Map` builder from the element type) and an `Iterable` otherwise.
        // `m.collect(pf)` is the same, over the entries `pf` is defined at.
        ("map" | "flatMap" | "collect", 1) => {
            let ps = pairs();
            let mut out = Vec::with_capacity(ps.len());
            for p in &ps {
                if name == "collect" && !is_defined_at(vm, &args[0], p)? {
                    continue;
                }
                let r = invoke_closure(vm, &args[0], std::slice::from_ref(p))?;
                if name == "flatMap" {
                    out.extend(as_seq_or_tuple(&r).unwrap_or_else(|| vec![r]));
                } else {
                    out.push(r);
                }
            }
            // With results in hand their own shape picks the builder; with none
            // (an empty map, or a `collect` that matched nothing) only the
            // function's compile-time result shape can.
            let all_pairs = if out.is_empty() {
                as_closure(&args[0]).map_or(true, |c| c.pair_body)
            } else {
                out.iter()
                    .all(|r| as_seq_or_tuple(r).is_some_and(|t| t.len() == 2))
            };
            // A non-pair result leaves `Map`'s builder for `immutable.Iterable`'s,
            // which is `List` — so `m.map { case (k, v) => v }` prints `List(…)`,
            // in the map's own (representation) order.
            if !all_pairs {
                return Ok(new_seq(SeqKind::List, out));
            }
            let mut mapped: Vec<(Value, Value)> = Vec::with_capacity(out.len());
            for r in &out {
                let t = as_seq_or_tuple(r).unwrap_or_default();
                map_put(&mut mapped, t[0].clone(), t[1].clone());
            }
            Ok(new_map(rep, mapped))
        }
        ("filter" | "filterNot" | "withFilter" | "takeWhile" | "dropWhile", 1) => {
            let keep_if = name != "filterNot";
            let mut kept = Vec::new();
            let mut dropping = false;
            let ps = pairs();
            for (p, e) in ps.iter().zip(entries.iter()) {
                let hit = truthy(&invoke_closure(vm, &args[0], std::slice::from_ref(p))?);
                match name {
                    "takeWhile" | "dropWhile" => {
                        dropping |= !hit;
                        if dropping == (name == "dropWhile") {
                            kept.push(e.clone());
                        }
                    }
                    _ if hit == keep_if => kept.push(e.clone()),
                    _ => {}
                }
            }
            Ok(new_map(rep, kept))
        }
        ("partition", 1) => {
            let mut yes = Vec::new();
            let mut no = Vec::new();
            let ps = pairs();
            for (p, e) in ps.iter().zip(entries.iter()) {
                if truthy(&invoke_closure(vm, &args[0], std::slice::from_ref(p))?) {
                    yes.push(e.clone());
                } else {
                    no.push(e.clone());
                }
            }
            Ok(new_pair(new_map(rep, yes), new_map(rep, no)))
        }
        ("head", 0) | ("last", 0) | ("headOption", 0) | ("lastOption", 0) => {
            let ps = pairs();
            let pick = if name.starts_with("head") {
                ps.first()
            } else {
                ps.last()
            };
            if name.ends_with("Option") {
                return Ok(opt(pick.cloned()));
            }
            // A `Map` is unordered, so `head`/`last` both advance its iterator
            // and both report that iterator when there is nothing to advance —
            // NOT a `head of empty map`, which no Scala version prints.
            pick.cloned().ok_or_else(|| {
                "scalars: java.util.NoSuchElementException: next on empty iterator".into()
            })
        }
        // `Map.groupBy` builds sub-MAPS, not sub-sequences: `MapOps` rebuilds
        // each group through its own `fromSpecific`, so
        // `Map("a"->1,"b"->2,"c"->3).groupBy(_._2 % 2)` is
        // `HashMap(0 -> Map(b -> 2), 1 -> Map(a -> 1, c -> 3))`. Delegating to
        // the sequence implementation answered `0 -> List((b,2))` instead. The
        // OUTER map is a `HashMap` at any size, as the sequence `groupBy`'s is.
        ("groupBy", 1) => {
            let mut groups: Vec<(Value, Vec<(Value, Value)>)> = Vec::new();
            for (p, e) in pairs().iter().zip(entries.iter()) {
                let k = invoke_closure(vm, &args[0], std::slice::from_ref(p))?;
                match groups.iter_mut().find(|(gk, _)| value_eq(gk, &k)) {
                    Some(slot) => slot.1.push(e.clone()),
                    None => groups.push((k, vec![e.clone()])),
                }
            }
            Ok(new_map(
                HashRep::Hashed,
                groups
                    .into_iter()
                    .map(|(k, group)| (k, new_map(rep, group)))
                    .collect(),
            ))
        }
        // `Map.mkString` renders each entry `k -> v`, not as the `Tuple2` the
        // sequence implementation prints: `MapOps.addString` supplies its own
        // rendering, which is the same one behind `Map.toString`'s arrows. So
        // `Map("a"->1).mkString(";")` is `a -> 1`, not `(a,1)`.
        ("mkString", _) => {
            let rendered: Vec<Value> = entries
                .iter()
                .map(|(k, v)| Value::str(format!("{} -> {}", scala_str(k), scala_str(v))))
                .collect();
            let seq = new_list(rendered);
            seq_method(vm, &seq, name, args)
        }
        // Everything else that only reads the entries as a pair sequence is the
        // sequence implementation over `Map`'s `Tuple2` elements.
        (
            "exists" | "forall" | "count" | "find" | "collectFirst" | "foldLeft" | "foldRight"
            | "fold" | "reduce" | "maxBy" | "minBy" | "toList" | "toSeq" | "toVector" | "toArray"
            | "toSet" | "sortBy" | "unzip" | "zipWithIndex" | "iterator",
            _,
        ) => {
            let seq = new_list(pairs());
            seq_method(vm, &seq, name, args)
        }
        ("toMap", 0) => Ok(recv.clone()),
        ("foreach", 1) => {
            for p in &pairs() {
                invoke_closure(vm, &args[0], std::slice::from_ref(p))?;
            }
            Ok(unit_value())
        }
        _ => map_read_method(&entries, recv, name, args),
    }
}

/// The read-only `Map` lookups (`size`, `get`, `keys`, …) — split out of
/// [`map_method`] so neither arm grows past a screen.
fn map_read_method(
    entries: &[(Value, Value)],
    recv: &Value,
    name: &str,
    args: &[Value],
) -> Result<Value, String> {
    match (name, args.len()) {
        ("size", 0) => Ok(Value::int(entries.len() as i64)),
        ("isEmpty", 0) => Ok(Value::bool(entries.is_empty())),
        ("nonEmpty", 0) => Ok(Value::bool(!entries.is_empty())),
        ("contains", 1) => Ok(Value::bool(map_get(entries, &args[0]).is_some())),
        ("apply", 1) => map_get(entries, &args[0]).ok_or_else(|| {
            format!(
                "scalars: java.util.NoSuchElementException: key not found: {}",
                scala_str(&args[0])
            )
        }),
        ("get", 1) => Ok(match map_get(entries, &args[0]) {
            Some(v) => make_some(v),
            None => make_none(),
        }),
        ("getOrElse", 2) => Ok(map_get(entries, &args[0]).unwrap_or_else(|| args[1].clone())),
        // Scala prints both key views as `Set(…)` whatever the map size (they are
        // `HashMap.HashKeySet`, not a `HashSet`), and in the map's own order — so
        // the view is built directly rather than through `new_set`.
        ("keys" | "keySet", 0) => Ok(new_seq(
            SeqKind::Set(HashRep::Small),
            entries.iter().map(|(k, _)| k.clone()).collect(),
        )),
        ("values", 0) => Ok(new_seq(
            SeqKind::Iterable,
            entries.iter().map(|(_, v)| v.clone()).collect(),
        )),
        _ => Err(no_such_method(recv, name)),
    }
}

/// `Tuple` methods — element accessors `_1`/`_2`/… and indexing.
fn tuple_method(recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    let items = as_seq_or_tuple(recv).unwrap_or_default();
    // A tuple is a `Product`, so its `hashCode` is the same `MurmurHash3`
    // product hash a `case class` gets — and the one the CHAMP order reads.
    if name == "hashCode" && args.is_empty() {
        if let Some(h) = product_hash(&format!("Tuple{}", items.len()), &items) {
            return Ok(Value::int(i64::from(h)));
        }
    }
    if name == "equals" && args.len() == 1 {
        return Ok(Value::bool(value_eq(recv, &args[0])));
    }
    if name == "productArity" && args.is_empty() {
        return Ok(Value::int(items.len() as i64));
    }
    if name == "productPrefix" && args.is_empty() {
        return Ok(Value::str(format!("Tuple{}", items.len())));
    }
    if name == "productIterator" && args.is_empty() {
        return Ok(new_list(items));
    }
    if name == "productElement" && args.len() == 1 {
        return list_index(&items, args[0].to_int());
    }
    // `Tuple2.swap` — the only arity that has it.
    if name == "swap" && args.is_empty() && items.len() == 2 {
        return Ok(heap_push(HeapVal::Tuple(vec![
            items[1].clone(),
            items[0].clone(),
        ])));
    }
    if args.is_empty() {
        if let Some(n) = name.strip_prefix('_').and_then(|d| d.parse::<usize>().ok()) {
            if n >= 1 && n <= items.len() {
                return Ok(items[n - 1].clone());
            }
        }
    }
    if name == "apply" && args.len() == 1 {
        return list_index(&items, args[0].to_int());
    }
    Err(no_such_method(recv, name))
}

/// Closure methods — `apply`/`call`.
fn closure_method(vm: &mut VM, recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    match (name, args.len()) {
        ("apply" | "call", _) => invoke_closure(vm, recv, args),
        // `PartialFunction`'s own surface. A plain lambda answers `true` from
        // `isDefinedAt` (it is total), which is what Scala's implicit
        // `Function1 => PartialFunction` lift does too.
        ("isDefinedAt", 1) => Ok(Value::bool(is_defined_at(vm, recv, &args[0])?)),
        // `applyOrElse(x, default)` — one `isDefinedAt` test, then exactly one
        // body runs. The default is a function of the argument, as in Scala.
        ("applyOrElse", 2) => {
            if is_defined_at(vm, recv, &args[0])? {
                invoke_closure(vm, recv, &args[..1])
            } else {
                invoke_closure(vm, &args[1], &args[..1])
            }
        }
        // The combinators, which answer a new function value. The parser folds a
        // trailing application into the same argument list (`pf.lift(x)` and
        // `(f andThen g)(x)` arrive as one call), so an extra argument beyond
        // the combinator's own means "build it, then apply it to that".
        ("lift", 0..=1) => derived(vm, DerivedFn::Lift(recv.clone()), &args[0..]),
        ("orElse", 1..=2) => derived(
            vm,
            DerivedFn::OrElse(recv.clone(), args[0].clone()),
            &args[1..],
        ),
        ("andThen", 1..=2) => derived(
            vm,
            DerivedFn::AndThen(recv.clone(), args[0].clone()),
            &args[1..],
        ),
        ("compose", 1..=2) => derived(
            vm,
            DerivedFn::Compose(recv.clone(), args[0].clone()),
            &args[1..],
        ),
        _ => Err(no_such_method(recv, name)),
    }
}

/// Allocate composed function `d`, or — when the call site folded a trailing
/// application into the same argument list — apply it to `rest` right away.
fn derived(vm: &mut VM, d: DerivedFn, rest: &[Value]) -> Result<Value, String> {
    let f = heap_push(HeapVal::Derived(d));
    if rest.is_empty() {
        Ok(f)
    } else {
        invoke_closure(vm, &f, rest)
    }
}

/// Scala truthiness of a closure result used as a predicate (`Boolean`).
fn truthy(v: &Value) -> bool {
    matches!(v, Value::Bool(true))
}

/// The `Ordering` `sorted`/`sortBy`/`min`/`max`/`maxBy` use. Numbers compare
/// numerically (an `Int` against a `Double` promotes, as Scala's numeric
/// `Ordering` does), strings lexicographically by code unit (Java's
/// `String.compareTo`), `false < true`, and tuples element-by-element
/// (`Ordering.Tuple2`). Any other pairing compares equal, which leaves the sort
/// — stable in both languages — holding the input order.
fn value_cmp(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Str(x), Value::Str(y)) => java_str_cmp(x, y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (
            Value::Int(_) | Value::Float(_) | Value::Status(_),
            Value::Int(_) | Value::Float(_) | Value::Status(_),
        ) => double_total_cmp(
            float_of(a).unwrap_or(f64::NAN),
            float_of(b).unwrap_or(f64::NAN),
        ),
        // `Char` orders by code point, so a `Seq[Char]` sorts like the `String`
        // of the same characters does.
        (Value::Obj(_), Value::Obj(_)) if as_char(a).is_some() && as_char(b).is_some() => {
            as_char(a).cmp(&as_char(b))
        }
        (Value::Obj(_), Value::Obj(_)) => match (as_seq_or_tuple(a), as_seq_or_tuple(b)) {
            (Some(xs), Some(ys)) => xs
                .iter()
                .zip(ys.iter())
                .map(|(x, y)| value_cmp(x, y))
                .find(|o| *o != Ordering::Equal)
                .unwrap_or_else(|| xs.len().cmp(&ys.len())),
            _ => Ordering::Equal,
        },
        _ => Ordering::Equal,
    }
}

/// `java.lang.Double.compare`, which is what BOTH `Ordering.Double.TotalOrdering`
/// (the implicit `Ordering[Double]`, so what `sorted`/`max`/`min`/`sortBy` use)
/// and `Ordering.Double.IeeeOrdering.compare` answer.
///
/// It is a TOTAL order, and that is the whole point: `-0.0` sorts BELOW `0.0`
/// and `NaN` sorts above everything including `Infinity`, neither of which
/// `partial_cmp` can express — it answers `None` for NaN, and the old code read
/// that as `Equal`. Under `Equal`, `List(1.0, NaN, 2.0).max` answered `2.0`
/// where Scala answers `NaN`, and `List(NaN, 1.0).sorted` left `NaN` in front.
///
/// Verified against Scala 3.8.4 on JDK 26.0.2:
/// `List(Double.NaN, 1.0, -0.0, 0.0, 2.0).sorted` is
/// `List(-0.0, 0.0, 1.0, 2.0, NaN)`.
///
/// The IEEE comparisons stay IEEE: `==`, `<` and friends are VM opcodes and do
/// not come through here, so `Double.NaN == Double.NaN` is still `false` and
/// `-0.0 == 0.0` is still `true`.
fn double_total_cmp(x: f64, y: f64) -> Ordering {
    // `f64::total_cmp` is `Double.compare`'s bit ordering, but it splits the two
    // NaN sign bits, and `Double.compare` treats every NaN as one value.
    match (x.is_nan(), y.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => x.total_cmp(&y),
    }
}

/// `java.lang.Math.max`/`min` for `Double`, which are NOT `f64::max`/`f64::min`.
///
/// Rust's IGNORE a NaN operand and answer the other one; Java's PROPAGATE it.
/// Java also specifies the `±0.0` tie (`max(-0.0, 0.0)` is `0.0`), where Rust
/// leaves it unspecified. Verified against Scala 3.8.4 on JDK 26.0.2:
/// `math.max(1.0, Double.NaN)` is `NaN`.
fn java_double_max(x: f64, y: f64) -> f64 {
    if x.is_nan() || y.is_nan() {
        return f64::NAN;
    }
    match double_total_cmp(x, y) {
        Ordering::Less => y,
        _ => x,
    }
}

fn java_double_min(x: f64, y: f64) -> f64 {
    if x.is_nan() || y.is_nan() {
        return f64::NAN;
    }
    match double_total_cmp(x, y) {
        Ordering::Greater => y,
        _ => x,
    }
}

/// `java.lang.Math.round(double)` — the real JDK body, not `floor(x + 0.5)` and
/// not Rust's `f64::round`.
///
/// Both shortcuts are wrong, in opposite places. `floor(x + 0.5)` answers `1`
/// for `0.49999999999999994` because adding `0.5` rounds UP to exactly `1.0`
/// before the floor sees it — the bug JDK-6430675 fixed in Java 7, and the JDK
/// has not used that formula since. Rust's `f64::round` is half-AWAY-from-zero,
/// so it answers `-3` for `-2.5` where the JVM answers `-2`; `Math.round` is
/// half-UP, toward positive infinity.
///
/// This is a direct port of `java.lang.Math.round`: read the biased exponent,
/// derive the shift that puts the rounding bit at position 0, then add one and
/// drop it. Values too large to have a fractional part take the plain `d2l`
/// cast, which saturates — so `NaN` is `0` and `Infinity` is `Long.MaxValue`.
///
/// Verified against Scala 3.8.4 on JDK 26.0.2: `(-2.5).round` is `-2`,
/// `(0.49999999999999994).round` is `0`, `math.round(1e300)` is
/// `9223372036854775807`.
fn java_round(a: f64) -> i64 {
    /// `DoubleConsts.SIGNIFICAND_WIDTH`.
    const SIGNIFICAND_WIDTH: i64 = 53;
    /// `DoubleConsts.EXP_BIAS`.
    const EXP_BIAS: i64 = 1023;
    /// `DoubleConsts.EXP_BIT_MASK`.
    const EXP_BIT_MASK: i64 = 0x7FF0_0000_0000_0000u64 as i64;
    /// `DoubleConsts.SIGNIF_BIT_MASK`.
    const SIGNIF_BIT_MASK: i64 = 0x000F_FFFF_FFFF_FFFF;

    let long_bits = a.to_bits() as i64;
    let biased_exp = (long_bits & EXP_BIT_MASK) >> (SIGNIFICAND_WIDTH - 1);
    let shift = (SIGNIFICAND_WIDTH - 2 + EXP_BIAS) - biased_exp;
    // `shift >= 0 && shift < 64`, written as the JDK writes it.
    if (shift & -64) == 0 {
        let mut r = (long_bits & SIGNIF_BIT_MASK) | (SIGNIF_BIT_MASK + 1);
        if long_bits < 0 {
            r = -r;
        }
        ((r >> shift) + 1) >> 1
    } else {
        // The `d2l` cast, which saturates at both ends and answers 0 for NaN.
        a as i64
    }
}

/// `java.lang.String.trim`, which is NOT Rust's `str::trim`.
///
/// Java's `trim` predates Unicode-aware trimming and cuts every character with a
/// code point at or below `U+0020` — including the control characters Rust
/// leaves alone — and cuts NOTHING above it, including the no-break space Rust
/// removes. Verified against Scala 3.8.4 on JDK 26.0.2: `"a".trim` is
/// `"a"` and `" a".trim` is `" a"`.
fn java_trim(s: &str) -> String {
    s.trim_matches(|c| c <= '\u{20}').to_string()
}

/// `java.lang.Character.isWhitespace`, the predicate `String.strip` uses.
///
/// It is Unicode-aware where `trim` is not, but it still excludes the three
/// NO-BREAK space separators — `U+00A0`, `U+2007` and `U+202F` — which Rust's
/// `char::is_whitespace` counts. It adds the file/group/record/unit separators
/// `U+001C`..`U+001F`, which Rust does not.
fn java_is_whitespace(c: char) -> bool {
    matches!(c,
        // Zs, minus the three non-breaking ones.
        '\u{20}' | '\u{1680}' | '\u{2000}'..='\u{200A}' | '\u{205F}' | '\u{3000}'
        // Zl and Zp.
        | '\u{2028}' | '\u{2029}'
        // The ASCII control whitespace, and the four information separators.
        | '\u{09}'..='\u{0D}' | '\u{1C}'..='\u{1F}')
}

/// `java.lang.String.strip` (Java 11) — `trim`'s Unicode-aware replacement.
fn java_strip(s: &str) -> String {
    s.trim_matches(java_is_whitespace).to_string()
}

/// `scala.collection.StringOps.stripMargin` — drop each line's leading
/// whitespace up to and including the first `margin` character, leaving lines
/// without one untouched.
fn strip_margin(s: &str, margin: char) -> String {
    let mut out = String::with_capacity(s.len());
    for (i, line) in s.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let rest = line.trim_start_matches(char::is_whitespace);
        match rest.strip_prefix(margin) {
            Some(after) => out.push_str(after),
            None => out.push_str(line),
        }
    }
    out
}

/// `java.lang.String.compareTo` — by UTF-16 code unit, then by length.
fn java_str_cmp(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// Key of [`METHOD_ENTRIES`]: the class name, the method name and the arity,
/// because an overloaded method is registered once per arity.
type MethodKey = (Arc<str>, &'static str, usize);

thread_local! {
    /// `(class, method, argc) → subroutine entry`, memoized because resolving one
    /// is a LINEAR scan of the chunk's name table and a `sorted` over a
    /// user-`Ordered` class asks the same question once per comparison. A MISS is
    /// cached too: sorting ordinary `case class`es asks about a `compare` that is
    /// not there, once per comparison, and that scan is the one worth skipping.
    /// Cleared by [`reset_heap`], which is also when a new program's chunk
    /// arrives.
    ///
    /// `argc` is part of the key because an OVERLOADED method is registered
    /// per-arity (`Class$m$1`), so `Class.m` is two different entries.
    static METHOD_ENTRIES: RefCell<HashMap<MethodKey, Option<usize>>> =
        RefCell::new(HashMap::new());
}

/// The entry point of the user-defined `argc`-argument method `Class.name`, when
/// the program compiled one. `compiler::Compiler::sub_name` registers every class
/// method under `Class$name` — or `Class$name$argc` when the class overloads the
/// name — so the host can re-enter one exactly as [`invoke_body`] re-enters a
/// closure body: no fusevm change, the name table is already public.
fn user_method_entry(vm: &VM, class: &Arc<str>, name: &'static str, argc: usize) -> Option<usize> {
    let key = (Arc::clone(class), name, argc);
    if let Some(hit) = METHOD_ENTRIES.with(|t| t.borrow().get(&key).copied()) {
        return hit;
    }
    let find = |want: &str| {
        vm.chunk
            .names
            .iter()
            .position(|n| n == want)
            .and_then(|idx| vm.chunk.find_sub(idx as u16))
    };
    // The class itself, then its linearization: a method the class INHERITS is
    // registered under the supertype that implements it (`Named$toString`), so
    // looking only at the receiver's own tag misses every trait-provided member.
    // `TYPES` holds that linearization, nearest first, which is Scala's own
    // resolution order.
    let mut owners = vec![class.to_string()];
    owners.extend(TYPES.with(|t| {
        t.borrow()
            .get(&**class)
            .map(|i| i.supers.clone())
            .unwrap_or_default()
    }));
    let found = owners.iter().find_map(|owner| {
        let plain = format!("{owner}${name}");
        // The unoverloaded name first: it is what all but a handful of programs
        // register, so the overload probe costs a second scan only when it is
        // the one that can succeed.
        find(&plain).or_else(|| find(&format!("{plain}${argc}")))
    });
    METHOD_ENTRIES.with(|t| t.borrow_mut().insert(key, found));
    found
}

/// Call the user-defined method `recv.name(args)` when `recv` is an instance of
/// a class that defines it. `None` means "no such method on that class", which
/// lets a caller fall back to the built-in behaviour.
///
/// A method subroutine takes `this` first, then its declared parameters — the
/// same shape [`compiler::dispatch_instance_method`] pushes.
fn call_user_method(
    vm: &mut VM,
    recv: &Value,
    // `'static` so the memo in `user_method_entry` can key on it without
    // allocating; every caller passes a literal method name.
    name: &'static str,
    args: &[Value],
) -> Option<Result<Value, String>> {
    // `with_obj` is what restricts this to a class instance: a `Char`, tuple or
    // collection handle is also a `Value::Obj` but is not a record.
    let class = with_obj(recv, |o| Arc::clone(&o.class))?;
    let entry = user_method_entry(vm, &class, name, args.len())?;
    let stack_base = vm.stack.len();
    vm.stack.push(recv.clone());
    for a in args {
        vm.stack.push(a.clone());
    }
    Some(run_sub(vm, entry, stack_base))
}

/// `a compare b` when `a`'s class defines it — `class V extends Ordered[V]` or
/// `implements Comparable[V]`. Scala's `Ordered`/`Comparable` are ordinary
/// traits whose single abstract member is the user's, and the implicit
/// `Ordering.ordered` makes every ordering-taking collection method use it, so
/// this is what `sorted`/`min`/`max`/`sortBy` must consult before falling back
/// to the structural order in [`value_cmp`].
///
/// `Comparable` spells the method `compareTo`; both names are tried, `compare`
/// first, since a class extending `Ordered` inherits a `compareTo` that merely
/// forwards to `compare`.
fn user_cmp(vm: &mut VM, a: &Value, b: &Value) -> Option<Result<Ordering, String>> {
    let r = call_user_method(vm, a, "compare", std::slice::from_ref(b))
        .or_else(|| call_user_method(vm, a, "compareTo", std::slice::from_ref(b)))?;
    // `compare` answers a negative/zero/positive `Int`, not `-1`/`0`/`1`.
    Some(r.map(|v| v.to_int().cmp(&0)))
}

/// [`value_cmp`], plus the `compare` a user class defines. Takes the VM because
/// running that `compare` re-enters the interpreter.
///
/// The element-wise arm is repeated from `value_cmp` rather than delegating to
/// it, because a tuple or nested sequence of user-ordered values has to keep
/// consulting the user's `compare` all the way down: `List((1, V(2)))` sorts by
/// `V`'s order in its second position.
fn cmp_vm(vm: &mut VM, a: &Value, b: &Value) -> Result<Ordering, String> {
    if let Some(r) = user_cmp(vm, a, b) {
        return r;
    }
    if matches!((a, b), (Value::Obj(_), Value::Obj(_))) && as_char(a).is_none() {
        if let (Some(xs), Some(ys)) = (as_seq_or_tuple(a), as_seq_or_tuple(b)) {
            for (x, y) in xs.iter().zip(ys.iter()) {
                let o = cmp_vm(vm, x, y)?;
                if o != Ordering::Equal {
                    return Ok(o);
                }
            }
            return Ok(xs.len().cmp(&ys.len()));
        }
    }
    Ok(value_cmp(a, b))
}

/// The members `scala.math.Ordered` derives from the single `compare` the user
/// writes: the four relational operators and `compareTo`, the `Comparable`
/// name. `None` when the receiver's class defines no `compare`, or the name is
/// not one of them, so the caller goes on to its usual dispatch.
///
/// `Ordered` in Scala 3 does NOT give a class `min`/`max` — those are extension
/// methods that need `import scala.math.Ordering.Implicits.infixOrderingOps`,
/// and the reference rejects `V(1).min(V(2))` without it. They are deliberately
/// absent here so the same program is rejected rather than silently answered.
fn ordered_derived_method(
    vm: &mut VM,
    recv: &Value,
    name: &str,
    args: &[Value],
) -> Option<Result<Value, String>> {
    if args.len() != 1 || !matches!(name, "<" | ">" | "<=" | ">=" | "compareTo") {
        return None;
    }
    let that = std::slice::from_ref(&args[0]);
    let raw = match call_user_method(vm, recv, "compare", that)
        .or_else(|| call_user_method(vm, recv, "compareTo", that))?
    {
        Ok(v) => v.to_int(),
        Err(e) => return Some(Err(e)),
    };
    // `Ordered.compareTo` is `def compareTo(that: A) = compare(that)` — it
    // answers the user's result VERBATIM. A `compare` of `(n - that.n) * 100`
    // makes `V(1).compareTo(V(3))` answer -200, not -1 (checked against the
    // reference). Only the relational operators reduce it to a sign.
    if name == "compareTo" {
        return Some(Ok(Value::int(raw)));
    }
    let o = raw.cmp(&0);
    Some(Ok(Value::bool(match name {
        "<" => o == Ordering::Less,
        ">" => o == Ordering::Greater,
        "<=" => o != Ordering::Greater,
        _ => o != Ordering::Less,
    })))
}

/// A STABLE sort of `0..len` under a comparator that may re-enter the VM and
/// fail, returning the sorted index permutation.
///
/// `slice::sort_by` cannot be used: its comparator answers a bare `Ordering`,
/// with nowhere to report a user `compare` that raised. The obvious alternative
/// — inserting each element into a growing sorted run — is O(n²), which a
/// reverse-ordered 4000-element `sorted` feels immediately (measured at 57s
/// before this became a merge). This is a bottom-up merge instead: O(n log n)
/// comparisons, and stable because a tie never takes from the right run.
fn merge_sort_idx(
    vm: &mut VM,
    len: usize,
    cmp: &mut dyn FnMut(&mut VM, usize, usize) -> Result<Ordering, String>,
) -> Result<Vec<usize>, String> {
    let mut cur: Vec<usize> = (0..len).collect();
    let mut buf: Vec<usize> = vec![0; len];
    let mut width = 1;
    while width < len {
        let mut lo = 0;
        while lo < len {
            let mid = (lo + width).min(len);
            let hi = (lo + 2 * width).min(len);
            let (mut i, mut j, mut k) = (lo, mid, lo);
            while i < mid && j < hi {
                // Strictly-less is what makes this stable: on a tie the left run
                // goes first, so equal elements keep their input order.
                if cmp(vm, cur[j], cur[i])? == Ordering::Less {
                    buf[k] = cur[j];
                    j += 1;
                } else {
                    buf[k] = cur[i];
                    i += 1;
                }
                k += 1;
            }
            buf[k..k + (mid - i)].copy_from_slice(&cur[i..mid]);
            k += mid - i;
            buf[k..k + (hi - j)].copy_from_slice(&cur[j..hi]);
            lo = hi;
        }
        std::mem::swap(&mut cur, &mut buf);
        width *= 2;
    }
    Ok(cur)
}

/// Sort `items` stably under [`cmp_vm`] — the implicit ordering, including a
/// user class's own `compare`.
fn sort_values(vm: &mut VM, items: &[Value]) -> Result<Vec<Value>, String> {
    let idx = merge_sort_idx(vm, items.len(), &mut |vm, i, j| {
        cmp_vm(vm, &items[i], &items[j])
    })?;
    Ok(idx.into_iter().map(|i| items[i].clone()).collect())
}

/// The `Some(v)`/`None` a `find`/`headOption`-style method returns.
fn opt(v: Option<Value>) -> Value {
    match v {
        Some(v) => make_some(v),
        None => make_none(),
    }
}

/// Whether `v` is an immutable `Set` handle.
fn is_set(v: &Value) -> bool {
    matches!(seq_kind_items(v), Some((SeqKind::Set(_), _)))
}

/// `set + e` (`incl`) / `set - e` (`excl`), preserving the set.s representation.
fn set_incl(set: &Value, e: Value, add: bool) -> Value {
    let (kind, mut items) =
        seq_kind_items(set).unwrap_or((SeqKind::Set(HashRep::Small), Vec::new()));
    let rep = match kind {
        SeqKind::Set(rep) => rep,
        _ => HashRep::Small,
    };
    if add {
        items.push(e);
    } else {
        items.retain(|x| !value_eq(x, &e));
    }
    new_set(rep, items)
}

/// A `Tuple2` heap value — the result shape of `partition`, `span`, `splitAt`
/// and `unzip`, and the element shape of `zip`.
fn new_pair(a: Value, b: Value) -> Value {
    heap_push(HeapVal::Tuple(vec![a, b]))
}

/// Clamp a Scala `take`/`drop` count into a slice bound (a negative count is a
/// no-op, an over-long one saturates — Scala never throws for either).
fn clamp(n: i64, len: usize) -> usize {
    n.clamp(0, len as i64) as usize
}

/// `sortBy`/`maxBy`/`minBy` support: the key `f` computes for every element.
fn keys_of(vm: &mut VM, f: &Value, items: &[Value]) -> Result<Vec<Value>, String> {
    let mut out = Vec::with_capacity(items.len());
    for it in items {
        out.push(invoke_closure(vm, f, std::slice::from_ref(it))?);
    }
    Ok(out)
}

/// The sequence methods that need no closure and produce a plain slice of the
/// receiver: `take`/`drop`/`slice`/`init`/… Returns `None` for an unknown name so
/// the caller can go on to the closure-taking methods.
fn seq_slice_method(items: &[Value], name: &str, args: &[Value]) -> Option<Vec<Value>> {
    let len = items.len();
    Some(match (name, args.len()) {
        ("take", 1) => items[..clamp(args[0].to_int(), len)].to_vec(),
        ("drop", 1) => items[clamp(args[0].to_int(), len)..].to_vec(),
        ("takeRight", 1) => items[len - clamp(args[0].to_int(), len)..].to_vec(),
        ("dropRight", 1) => items[..len - clamp(args[0].to_int(), len)].to_vec(),
        ("slice", 2) => {
            let from = clamp(args[0].to_int(), len);
            let until = clamp(args[1].to_int(), len).max(from);
            items[from..until].to_vec()
        }
        // `init` is NOT here: on an empty receiver it raises, and the wording is
        // per-kind, so `seq_method` answers it from `SeqKind::empty_fault`.
        ("distinct", 0) => {
            let mut out: Vec<Value> = Vec::with_capacity(len);
            for it in items {
                if !out.iter().any(|u| value_eq(u, it)) {
                    out.push(it.clone());
                }
            }
            out
        }
        ("flatten", 0) => {
            let mut out = Vec::new();
            for it in items {
                // `List[Option[A]].flatten` drops the empties and unwraps the
                // rest — an `Option` is an `IterableOnce` in Scala, so it
                // flattens exactly like a one-or-zero element collection.
                match as_option(it) {
                    Some(inner) => out.extend(inner),
                    None => out.extend(as_seq_or_tuple(it)?),
                }
            }
            out
        }
        _ => return None,
    })
}

/// Sum a numeric sequence (`Int` result when all `Int`, else `Double`).
///
/// The integral accumulator wraps. Scala's `sum` is the element type's `+`, so
/// `List(Long.MaxValue, 1L).sum` answers `Long.MinValue` rather than raising --
/// `Iterator::sum` would instead panic here in debug and wrap only in release,
/// making the answer depend on the build profile. The 64-bit wrap is the `Long`
/// case; a `List[Int]` is narrowed to 32 bits by `member_width` on the compiler
/// side, so both widths overflow where Scala does.
fn seq_sum(items: &[Value]) -> Value {
    if items.iter().all(|v| matches!(v, Value::Int(_))) {
        Value::int(
            items
                .iter()
                .map(Value::to_int)
                .fold(0i64, i64::wrapping_add),
        )
    } else {
        Value::float(items.iter().map(num_f64).sum())
    }
}

/// `MAKE_ARRAY` builtin — pop `argc` elements (deepest first) into a mutable
/// `Array`.
fn b_make_array(vm: &mut VM, argc: u8) -> Value {
    let mut items = Vec::with_capacity(argc as usize);
    for _ in 0..argc {
        items.push(vm.pop());
    }
    items.reverse();
    new_seq(SeqKind::Array, items)
}

/// `ARRAY_FILL` builtin — pop the element-type name and the length; return an
/// array of that type's Scala zero value.
fn b_array_fill(vm: &mut VM, _argc: u8) -> Value {
    let ty = vm.pop().as_str_cow().into_owned();
    let n = vm.pop().to_int();
    if n < 0 {
        return fault(
            vm,
            format!("scalars: java.lang.NegativeArraySizeException: {n}"),
        );
    }
    let zero = match ty.as_str() {
        "Int" | "Long" | "Short" | "Byte" | "Char" => Value::int(0),
        "Double" | "Float" => Value::float(0.0),
        "Boolean" => Value::bool(false),
        _ => Value::Undef,
    };
    new_seq(SeqKind::Array, vec![zero; n as usize])
}

/// `MAKE_RANGE` builtin — pop the step, `inclusive`, end and start; return the
/// materialized range.
fn b_make_range(vm: &mut VM, _argc: u8) -> Value {
    let step = vm.pop().to_int();
    let inclusive = matches!(vm.pop(), Value::Bool(true));
    let end_v = vm.pop();
    let start_v = vm.pop();
    if let Err(e) = reject_char_endpoint(&start_v, &end_v) {
        return fault(vm, e);
    }
    let end = end_v.to_int();
    let start = start_v.to_int();
    match range_items(start, end, inclusive, step) {
        Ok(items) => new_seq(
            SeqKind::Range {
                start,
                end,
                inclusive,
                step,
            },
            items,
        ),
        Err(e) => fault(vm, e),
    }
}

/// `SMATH` builtin — pop the member name, then its arguments; evaluate the
/// `scala.math` member.
fn b_math(vm: &mut VM, argc: u8) -> Value {
    let name = vm.pop().as_str_cow().into_owned();
    let n = (argc as usize).saturating_sub(1);
    let mut args = Vec::with_capacity(n);
    for _ in 0..n {
        args.push(vm.pop());
    }
    args.reverse();
    match math_member(&name, &args) {
        Ok(v) => v,
        Err(e) => fault(vm, e),
    }
}

/// `BOXED` builtin — pop the `Module.member` name, then its arguments.
fn b_boxed(vm: &mut VM, argc: u8) -> Value {
    let qualified = vm.pop().as_str_cow().into_owned();
    let n = (argc as usize).saturating_sub(1);
    let mut args = Vec::with_capacity(n);
    for _ in 0..n {
        args.push(vm.pop());
    }
    args.reverse();
    let (module, member) = qualified
        .rsplit_once('.')
        .expect("the compiler always emits `Module.member`");
    // `String.valueOf(x)` IS `x.toString`, so a user override has to run — and
    // `boxed_member` has no VM to run it with.
    if (module, member, args.len()) == ("java.String", "valueOf", 1) {
        if let Value::Obj(_) = &args[0] {
            return Value::str(scala_str_vm(vm, &args[0]));
        }
    }
    match boxed_member(module, member, &args) {
        Ok(v) => v,
        Err(e) => fault(vm, e),
    }
}

/// `java.lang.NumberFormatException`'s message, which quotes the input.
fn number_format(s: &str) -> String {
    format!("scalars: java.lang.NumberFormatException: For input string: \"{s}\"")
}

/// `java.lang.Integer.toString(i, radix)` — the digits of `i` in `radix`, with a
/// leading `-` for a negative value (the JDK renders the SIGN, not the two's
/// complement; only the fixed-width `toBinaryString`/`toHexString`/
/// `toOctalString` render the bit pattern).
fn to_radix(mut v: i64, radix: u32) -> String {
    if !(2..=36).contains(&radix) {
        // The JDK silently falls back to base 10 for an out-of-range radix.
        return v.to_string();
    }
    if v == 0 {
        return "0".to_string();
    }
    let neg = v < 0;
    let mut digits = Vec::new();
    while v != 0 {
        let d = (v % radix as i64).unsigned_abs() as u32;
        digits.push(std::char::from_digit(d, radix).expect("digit below radix"));
        v /= radix as i64;
    }
    if neg {
        digits.push('-');
    }
    digits.iter().rev().collect()
}

/// The digits of an UNSIGNED 64-bit value in `radix` — what
/// `Integer.toUnsignedString(i, radix)` and `Long.toUnsignedString(l, radix)`
/// render. It differs from [`to_radix`] in exactly the way the JDK's two
/// families differ: there is no sign to emit, because the bit pattern IS the
/// number. The out-of-range radix falls back to ten, as every JDK renderer does.
fn to_radix_unsigned(mut v: u64, radix: u32) -> String {
    if !(2..=36).contains(&radix) {
        return v.to_string();
    }
    if v == 0 {
        return "0".to_string();
    }
    let mut digits = Vec::new();
    while v != 0 {
        let d = (v % u64::from(radix)) as u32;
        digits.push(std::char::from_digit(d, radix).expect("digit below radix"));
        v /= u64::from(radix);
    }
    digits.iter().rev().collect()
}

/// The JVM's `i2b` narrowing conversion: the low eight bits of the two's
/// complement, sign-extended. It TRUNCATES rather than clamps, so `300.toByte`
/// is 44 and `128.toByte` is -128 — the one place a Scala numeric conversion
/// silently changes a value's sign.
fn to_byte(v: i64) -> i64 {
    i64::from(v as i8)
}

/// The JVM's `i2s` — [`to_byte`] one width up (`70000.toShort` is 4464).
fn to_short(v: i64) -> i64 {
    i64::from(v as i16)
}

/// `java.lang.NumberFormatException.forInputString` — the JDK names the radix in
/// the message for every base BUT ten, which is how `Integer.decode("08")`
/// reports `"8" under radix 8` while `"zz".toInt` reports only the string.
fn number_format_radix(s: &str, radix: i64) -> String {
    if radix == 10 {
        number_format(s)
    } else {
        format!(
            "scalars: java.lang.NumberFormatException: \
             For input string: \"{s}\" under radix {radix}"
        )
    }
}

/// `Byte.parseByte`/`Short.parseShort`'s out-of-range message. A string whose
/// digits parse but whose VALUE does not fit reports differently from one that
/// does not parse at all, and the message quotes the radix it was read in.
fn value_out_of_range(s: &str, radix: i64) -> String {
    format!(
        "scalars: java.lang.NumberFormatException: \
         Value out of range. Value:\"{s}\" Radix:{radix}"
    )
}

/// `Character.MIN_RADIX`/`MAX_RADIX`, checked BEFORE any digit is read — the
/// order matters because each bound has its own message and neither mentions the
/// input. This check is also what keeps a bad radix from reaching Rust's
/// `from_str_radix`, which PANICS outside `2..=36` rather than returning an
/// error.
fn check_radix(radix: i64) -> Result<u32, String> {
    if radix < 2 {
        Err(format!(
            "scalars: java.lang.NumberFormatException: \
             radix {radix} less than Character.MIN_RADIX"
        ))
    } else if radix > 36 {
        Err(format!(
            "scalars: java.lang.NumberFormatException: \
             radix {radix} greater than Character.MAX_RADIX"
        ))
    } else {
        Ok(radix as u32)
    }
}

/// `java.lang.Long.parseLong(s, radix)`. No trimming: the JDK rejects the
/// padding that `String.trim` would have removed, which is why `" 42".toLong`
/// throws where `" 42".trim.toLong` does not.
fn java_parse_long(s: &str, radix: i64) -> Result<i64, String> {
    let r = check_radix(radix)?;
    i64::from_str_radix(s, r).map_err(|_| number_format_radix(s, radix))
}

/// `java.lang.Integer.parseInt(s, radix)` — [`java_parse_long`] plus the 32-bit
/// range, which the JDK enforces DURING accumulation and reports as an ordinary
/// format error: `"2147483648".toInt` is a `NumberFormatException`, not a wrap.
fn java_parse_int(s: &str, radix: i64) -> Result<i64, String> {
    let v = java_parse_long(s, radix)?;
    if v < i64::from(i32::MIN) || v > i64::from(i32::MAX) {
        return Err(number_format_radix(s, radix));
    }
    Ok(v)
}

/// `Byte.parseByte`/`Short.parseShort`: `parseInt` first, then the box's own
/// range — so `"7f".toByte` fails on the DIGITS (radix ten has no `f`) while
/// `"128".toByte` fails on the VALUE, with the two different messages.
fn java_parse_narrow(s: &str, radix: i64, lo: i64, hi: i64) -> Result<i64, String> {
    let v = java_parse_int(s, radix)?;
    if v < lo || v > hi {
        return Err(value_out_of_range(s, radix));
    }
    Ok(v)
}

/// `Integer.decode`/`Long.decode` — a sign, then an optional base prefix
/// (`0x`, `0X`, `#`, or a leading `0` with digits after it), then the digits.
///
/// Its two failures are its own, not `parseInt`'s: an empty string is
/// `Zero length string`, and a sign AFTER the prefix is `Sign character in wrong
/// position`. Everything else is reported by re-parsing the SUBJECT — the text
/// past the prefix with the sign put back — under the radix the prefix chose,
/// which is why `decode("08")` blames `"8" under radix 8` rather than `"08"`.
fn java_decode(s: &str, long: bool) -> Result<i64, String> {
    if s.is_empty() {
        return Err("scalars: java.lang.NumberFormatException: Zero length string".to_string());
    }
    let mut i = 0;
    let negative = s.starts_with('-');
    if negative || s.starts_with('+') {
        i = 1;
    }
    let rest = &s[i..];
    let radix = if rest.starts_with("0x") || rest.starts_with("0X") {
        i += 2;
        16
    } else if rest.starts_with('#') {
        i += 1;
        16
    } else if rest.starts_with('0') && s.len() > i + 1 {
        i += 1;
        8
    } else {
        10
    };
    let body = &s[i..];
    if body.starts_with('-') || body.starts_with('+') {
        return Err(
            "scalars: java.lang.NumberFormatException: Sign character in wrong position"
                .to_string(),
        );
    }
    let subject = if negative {
        format!("-{body}")
    } else {
        body.to_string()
    };
    if long {
        java_parse_long(&subject, radix)
    } else {
        java_parse_int(&subject, radix)
    }
}

/// `Integer.parseUnsignedInt` — the 32-bit UNSIGNED range, answered as the
/// signed bit pattern (`"4294967295"` decodes to -1).
///
/// It separates the two ways a string can fail in a way no other parse here
/// does: BAD DIGITS report `For input string:`, while legal digits whose value
/// is too large report `exceeds range of unsigned int.` — and that stays true
/// however large the value is, so `"99999999999999999999999999"` is a range
/// error rather than the format error a 64-bit parse of it would raise.
fn java_parse_unsigned_int(s: &str, radix: i64) -> Result<i64, String> {
    let r = check_radix(radix)?;
    if s.starts_with('-') {
        return Err(format!(
            "scalars: java.lang.NumberFormatException: \
             Illegal leading minus sign on unsigned string {s}."
        ));
    }
    let body = s.strip_prefix('+').unwrap_or(s);
    if body.is_empty() || body.chars().any(|c| c.to_digit(r).is_none()) {
        return Err(number_format_radix(s, radix));
    }
    let too_big = || {
        format!(
            "scalars: java.lang.NumberFormatException: \
             String value {s} exceeds range of unsigned int."
        )
    };
    let mut v: u64 = 0;
    for c in body.chars() {
        let d = u64::from(c.to_digit(r).expect("digit checked above"));
        // Every digit only grows the value, so leaving the range once is final.
        v = v
            .checked_mul(u64::from(r))
            .and_then(|x| x.checked_add(d))
            .filter(|x| *x <= u64::from(u32::MAX))
            .ok_or_else(too_big)?;
    }
    Ok(i64::from(v as u32 as i32))
}

/// A boxed-primitive or `String` companion member. The Scala companions
/// (`Int`, `Double`, …) carry only the `MaxValue`-family constants; the
/// `java.lang` boxes (`java.Integer`, `java.Character`, …) carry the JDK
/// statics. The split is Scala's own — `Double.parseDouble` really is "not a
/// member of object Double" there — so an unknown pairing is an error here too
/// rather than a quietly accepted superset.
fn boxed_member(module: &str, name: &str, args: &[Value]) -> Result<Value, String> {
    let unknown = || Err(format!("scalars: value {name} is not a member of {module}"));
    let i = |k: usize| args[k].to_int();
    // The JDK's fixed-width renderings show the two's-complement bit pattern at
    // the box's own width, so `Integer.toHexString(-1)` is 8 digits and
    // `Long.toHexString(-1)` is 16.
    let unsigned = |v: i64| -> u64 {
        if module == "java.Long" {
            v as u64
        } else {
            v as i32 as u32 as u64
        }
    };
    match (module, name, args.len()) {
        // ── The Scala companions' constants ──────────────────────────────────
        ("Int", "MaxValue", 0) => Ok(Value::int(i32::MAX as i64)),
        ("Int", "MinValue", 0) => Ok(Value::int(i32::MIN as i64)),
        ("Long", "MaxValue", 0) => Ok(Value::int(i64::MAX)),
        ("Long", "MinValue", 0) => Ok(Value::int(i64::MIN)),
        ("Short", "MaxValue", 0) => Ok(Value::int(i16::MAX as i64)),
        ("Short", "MinValue", 0) => Ok(Value::int(i16::MIN as i64)),
        ("Byte", "MaxValue", 0) => Ok(Value::int(i8::MAX as i64)),
        ("Byte", "MinValue", 0) => Ok(Value::int(i8::MIN as i64)),
        ("Char", "MaxValue", 0) => Ok(make_char('\u{ffff}')),
        ("Char", "MinValue", 0) => Ok(make_char('\0')),
        ("Double", "MaxValue", 0) => Ok(Value::float(f64::MAX)),
        ("Double", "MinValue", 0) => Ok(Value::float(f64::MIN)),
        ("Double", "MinPositiveValue", 0) => Ok(Value::float(f64::from_bits(1))),
        ("Double", "PositiveInfinity", 0) => Ok(Value::float(f64::INFINITY)),
        ("Double", "NegativeInfinity", 0) => Ok(Value::float(f64::NEG_INFINITY)),
        ("Double", "NaN", 0) => Ok(Value::float(f64::NAN)),
        // `Float`'s constants are the `f32` value widened, which is exact — an
        // `f32` is an `f64`. What makes them print as Scala's `3.4028235E38`
        // rather than as `3.4028234663852886E38` is not the value but the
        // RENDERING, and the compiler types these accesses `Float` so they
        // render through `Float.toString` like any other one.
        ("Float", "MaxValue", 0) => Ok(Value::float(f64::from(f32::MAX))),
        ("Float", "MinValue", 0) => Ok(Value::float(f64::from(f32::MIN))),
        ("Float", "MinPositiveValue", 0) => Ok(Value::float(f64::from(f32::from_bits(1)))),
        ("Float", "NaN", 0) => Ok(Value::float(f64::NAN)),
        ("Float", "PositiveInfinity", 0) => Ok(Value::float(f64::INFINITY)),
        ("Float", "NegativeInfinity", 0) => Ok(Value::float(f64::NEG_INFINITY)),
        // ── The `java.lang` boxes' constants ─────────────────────────────────
        ("java.Integer", "MAX_VALUE", 0) => Ok(Value::int(i32::MAX as i64)),
        ("java.Integer", "MIN_VALUE", 0) => Ok(Value::int(i32::MIN as i64)),
        ("java.Long", "MAX_VALUE", 0) => Ok(Value::int(i64::MAX)),
        ("java.Long", "MIN_VALUE", 0) => Ok(Value::int(i64::MIN)),
        ("java.Short", "MAX_VALUE", 0) => Ok(Value::int(i16::MAX as i64)),
        ("java.Short", "MIN_VALUE", 0) => Ok(Value::int(i16::MIN as i64)),
        ("java.Byte", "MAX_VALUE", 0) => Ok(Value::int(i8::MAX as i64)),
        ("java.Byte", "MIN_VALUE", 0) => Ok(Value::int(i8::MIN as i64)),
        ("java.Double", "MAX_VALUE", 0) => Ok(Value::float(f64::MAX)),
        ("java.Double", "MIN_VALUE", 0) => Ok(Value::float(f64::from_bits(1))),
        ("java.Double", "NaN", 0) => Ok(Value::float(f64::NAN)),
        ("java.Double", "POSITIVE_INFINITY", 0) => Ok(Value::float(f64::INFINITY)),
        ("java.Double", "NEGATIVE_INFINITY", 0) => Ok(Value::float(f64::NEG_INFINITY)),
        ("java.Float", "MAX_VALUE", 0) => Ok(Value::float(f64::from(f32::MAX))),
        // `java.lang.Float.MIN_VALUE` is the smallest POSITIVE `float`, which is
        // `scala.Float.MinPositiveValue`; the JDK boxes have no name for the
        // most negative one. That is the same asymmetry `java.Double` carries
        // above.
        ("java.Float", "MIN_VALUE", 0) => Ok(Value::float(f64::from(f32::from_bits(1)))),
        ("java.Float", "NaN", 0) => Ok(Value::float(f64::NAN)),
        ("java.Float", "POSITIVE_INFINITY", 0) => Ok(Value::float(f64::INFINITY)),
        ("java.Float", "NEGATIVE_INFINITY", 0) => Ok(Value::float(f64::NEG_INFINITY)),
        // ── Parsing ──────────────────────────────────────────────────────────
        // The JDK does NOT trim, so `Integer.parseInt(" 1")` throws where
        // `" 1".trim.toInt` does not.
        //
        // Which WIDTH the digits are checked against is the method's, not the
        // module's: every one of these reads at 64 bits and then applies its own
        // range, and the range is where the three distinct JDK messages come
        // from — `For input string:` when the digits (or an `Int`'s 32-bit
        // range) reject the text, `Value out of range.` when a `Byte`/`Short`
        // does, and the two `Character.*_RADIX` texts when the base itself is
        // impossible. That last one used to reach Rust's `from_str_radix`, which
        // PANICS outside `2..=36`: `Integer.parseInt("12", 40)` aborted the
        // process with a Rust backtrace instead of throwing.
        (
            "java.Integer" | "java.Long" | "java.Short" | "java.Byte",
            "parseInt" | "parseLong" | "parseShort" | "parseByte" | "valueOf" | "decode",
            1 | 2,
        ) if matches!(args[0], Value::Str(_)) => {
            let s = args[0].as_str_cow();
            let radix = if args.len() == 2 { i(1) } else { 10 };
            let byte = || java_parse_narrow(&s, radix, i64::from(i8::MIN), i64::from(i8::MAX));
            let short = || java_parse_narrow(&s, radix, i64::from(i16::MIN), i64::from(i16::MAX));
            let v = match name {
                "decode" => java_decode(&s, module == "java.Long"),
                "parseLong" => java_parse_long(&s, radix),
                "parseByte" => byte(),
                "parseShort" => short(),
                // `valueOf` is the box's OWN parse, so it is the one member here
                // whose width comes from the module rather than the name.
                "valueOf" => match module {
                    "java.Long" => java_parse_long(&s, radix),
                    "java.Byte" => byte(),
                    "java.Short" => short(),
                    _ => java_parse_int(&s, radix),
                },
                _ => java_parse_int(&s, radix),
            }?;
            Ok(Value::int(v))
        }
        // A `null` argument is its own message — the JDK checks for it before it
        // looks at any character, so it never becomes `For input string: "null"`.
        (
            "java.Integer" | "java.Long" | "java.Short" | "java.Byte",
            "parseInt" | "parseLong" | "parseShort" | "parseByte" | "parseUnsignedInt",
            1 | 2,
        ) if matches!(args[0], Value::Undef) => {
            Err("scalars: java.lang.NumberFormatException: Cannot parse null string".to_string())
        }
        ("java.Integer", "parseUnsignedInt", 1 | 2) => {
            let s = args[0].as_str_cow();
            let radix = if args.len() == 2 { i(1) } else { 10 };
            java_parse_unsigned_int(&s, radix).map(Value::int)
        }
        ("java.Integer" | "java.Long" | "java.Short" | "java.Byte", "valueOf", 1) => {
            Ok(Value::int(i(0)))
        }
        ("java.Double" | "java.Float", "parseDouble" | "parseFloat" | "valueOf", 1) => {
            let s = args[0].as_str_cow();
            match &args[0] {
                Value::Str(_) => s
                    .trim()
                    .parse::<f64>()
                    .map(Value::float)
                    .map_err(|_| number_format(&s)),
                other => Ok(Value::float(num_f64(other))),
            }
        }
        ("java.Boolean", "parseBoolean" | "valueOf", 1) => Ok(Value::bool(
            args[0].as_str_cow().eq_ignore_ascii_case("true"),
        )),
        ("java.Double" | "java.Float", "isNaN", 1) => Ok(Value::bool(num_f64(&args[0]).is_nan())),
        ("java.Double" | "java.Float", "isInfinite", 1) => {
            Ok(Value::bool(num_f64(&args[0]).is_infinite()))
        }
        // ── Rendering ────────────────────────────────────────────────────────
        ("java.Integer" | "java.Long", "toBinaryString", 1) => {
            Ok(Value::str(format!("{:b}", unsigned(i(0)))))
        }
        ("java.Integer" | "java.Long", "toHexString", 1) => {
            Ok(Value::str(format!("{:x}", unsigned(i(0)))))
        }
        ("java.Integer" | "java.Long", "toOctalString", 1) => {
            Ok(Value::str(format!("{:o}", unsigned(i(0)))))
        }
        ("java.Integer" | "java.Long" | "java.Short" | "java.Byte", "toString", 1) => {
            Ok(Value::str(i(0).to_string()))
        }
        ("java.Integer" | "java.Long", "toString", 2) => {
            Ok(Value::str(to_radix(i(0), i(1) as u32)))
        }
        // `toUnsignedString` reads the same bits as the number's OWN width but
        // prints them without a sign, so `Integer.toUnsignedString(-1)` is
        // 4294967295 and the `Long` spelling of the same bits is
        // 18446744073709551615.
        ("java.Integer" | "java.Long", "toUnsignedString", 1) => {
            Ok(Value::str(unsigned(i(0)).to_string()))
        }
        ("java.Integer" | "java.Long", "toUnsignedString", 2) => {
            Ok(Value::str(to_radix_unsigned(unsigned(i(0)), i(1) as u32)))
        }
        // `Integer.toUnsignedLong` is the widening that makes that bit pattern a
        // number again — the only way to see an `Int`'s unsigned value as one.
        ("java.Integer", "toUnsignedLong", 1) => Ok(Value::int(i64::from(i(0) as i32 as u32))),
        // `Double.toString(d)` is the same shortest-round-trip rendering the
        // implicit `"" + d` uses, so `-0.0` keeps its sign.
        ("java.Double" | "java.Float", "toString", 1) => {
            Ok(Value::str(format_double(num_f64(&args[0]))))
        }
        ("java.String", "valueOf", 1) => Ok(Value::str(scala_str(&args[0]))),
        // ── Arithmetic statics ───────────────────────────────────────────────
        ("java.Integer" | "java.Long", "compare", 2) => Ok(Value::int(match i(0).cmp(&i(1)) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        })),
        ("java.Integer" | "java.Long", "max", 2) => Ok(Value::int(i(0).max(i(1)))),
        ("java.Integer" | "java.Long", "min", 2) => Ok(Value::int(i(0).min(i(1)))),
        ("java.Integer" | "java.Long", "sum", 2) => Ok(Value::int(i(0) + i(1))),
        ("java.Integer" | "java.Long", "signum", 1) => Ok(Value::int(i(0).signum())),
        ("java.Integer", "bitCount", 1) => Ok(Value::int(i64::from((i(0) as i32).count_ones()))),
        ("java.Long", "bitCount", 1) => Ok(Value::int(i64::from(i(0).count_ones()))),
        // `hashCode` of a box: an `Integer`'s is the value, a `Long`'s folds the
        // two halves together so that `5L` and `5` hash alike.
        ("java.Integer", "hashCode", 1) => Ok(Value::int(i64::from(i(0) as i32))),
        ("java.Long", "hashCode", 1) => {
            let n = i(0);
            Ok(Value::int(i64::from(
                (n ^ ((n as u64) >> 32) as i64) as i32,
            )))
        }
        // ── Bit twiddling ────────────────────────────────────────────────────
        // Every one of these is defined on the box's OWN width, which the value
        // model cannot recover from an `i64`: `Integer.reverse(1)` is
        // -2147483648 and `Long.reverse(1L)` is -9223372036854775808. The module
        // is the only place that width survives, so each is written twice rather
        // than once over a widened value.
        ("java.Integer", "reverse", 1) => Ok(Value::int(i64::from((i(0) as i32).reverse_bits()))),
        ("java.Long", "reverse", 1) => Ok(Value::int(i(0).reverse_bits())),
        ("java.Integer", "reverseBytes", 1) => {
            Ok(Value::int(i64::from((i(0) as i32).swap_bytes())))
        }
        ("java.Long", "reverseBytes", 1) => Ok(Value::int(i(0).swap_bytes())),
        ("java.Integer", "highestOneBit", 1) => {
            let n = i(0) as i32;
            Ok(Value::int(i64::from(if n == 0 {
                0
            } else {
                1i32 << (31 - n.leading_zeros())
            })))
        }
        ("java.Long", "highestOneBit", 1) => {
            let n = i(0);
            Ok(Value::int(if n == 0 {
                0
            } else {
                1i64 << (63 - n.leading_zeros())
            }))
        }
        ("java.Integer", "lowestOneBit", 1) => {
            let n = i(0) as i32;
            Ok(Value::int(i64::from(n & n.wrapping_neg())))
        }
        ("java.Long", "lowestOneBit", 1) => {
            let n = i(0);
            Ok(Value::int(n & n.wrapping_neg()))
        }
        // The two counts answer the FULL width for zero (32 and 64), which is
        // the one input where a naive "index of the first set bit" disagrees.
        ("java.Integer", "numberOfLeadingZeros", 1) => {
            Ok(Value::int(i64::from((i(0) as i32).leading_zeros())))
        }
        ("java.Long", "numberOfLeadingZeros", 1) => Ok(Value::int(i64::from(i(0).leading_zeros()))),
        ("java.Integer", "numberOfTrailingZeros", 1) => {
            Ok(Value::int(i64::from((i(0) as i32).trailing_zeros())))
        }
        ("java.Long", "numberOfTrailingZeros", 1) => {
            Ok(Value::int(i64::from(i(0).trailing_zeros())))
        }
        // The JDK masks the distance to the width's own bit count, so a negative
        // or oversized rotation is well defined rather than an error.
        ("java.Integer", "rotateLeft" | "rotateRight", 2) => {
            let (n, d) = (i(0) as i32, i(1) as u32 & 31);
            Ok(Value::int(i64::from(if name == "rotateLeft" {
                n.rotate_left(d)
            } else {
                n.rotate_right(d)
            })))
        }
        ("java.Long", "rotateLeft" | "rotateRight", 2) => {
            let (n, d) = (i(0), i(1) as u32 & 63);
            Ok(Value::int(if name == "rotateLeft" {
                n.rotate_left(d)
            } else {
                n.rotate_right(d)
            }))
        }
        // ── Unsigned arithmetic ──────────────────────────────────────────────
        ("java.Integer" | "java.Long", "compareUnsigned", 2) => {
            Ok(Value::int(match unsigned(i(0)).cmp(&unsigned(i(1))) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            }))
        }
        ("java.Integer" | "java.Long", "divideUnsigned" | "remainderUnsigned", 2) => {
            let (a, b) = (unsigned(i(0)), unsigned(i(1)));
            if b == 0 {
                return Err("scalars: java.lang.ArithmeticException: / by zero".to_string());
            }
            let r = if name == "divideUnsigned" {
                a / b
            } else {
                a % b
            };
            // The quotient is a bit pattern at the box's own width again: an
            // `Int` division whose result has the high bit set comes back
            // negative.
            Ok(Value::int(if module == "java.Long" {
                r as i64
            } else {
                i64::from(r as u32 as i32)
            }))
        }
        // ── `java.lang.Character` ────────────────────────────────────────────
        ("java.Character", _, 1) => {
            let c = as_char(&args[0])
                .or_else(|| char::from_u32(args[0].to_int() as u32))
                .ok_or_else(|| format!("scalars: {name} expects a Char"))?;
            match name {
                "isDigit" => Ok(Value::bool(c.is_numeric())),
                "isLetter" => Ok(Value::bool(c.is_alphabetic())),
                "isLetterOrDigit" => Ok(Value::bool(c.is_alphanumeric())),
                "isWhitespace" | "isSpaceChar" => Ok(Value::bool(c.is_whitespace())),
                "isUpperCase" => Ok(Value::bool(c.is_uppercase())),
                "isLowerCase" => Ok(Value::bool(c.is_lowercase())),
                "toUpperCase" => Ok(make_char(c.to_ascii_uppercase())),
                "toLowerCase" => Ok(make_char(c.to_ascii_lowercase())),
                // `getNumericValue` answers -1 for a non-alphanumeric character
                // and 10..35 for `a`..`z`, as the JDK does.
                "getNumericValue" => Ok(Value::int(match c.to_digit(36) {
                    Some(d) => d as i64,
                    None => -1,
                })),
                _ => unknown(),
            }
        }
        _ => unknown(),
    }
}

/// `scala.math` / `java.lang.Math` members. Integer-preserving where Scala's
/// overloads are (`abs`/`min`/`max`/`signum` on two `Int`s stay `Int`); every
/// other member is the `Double` overload, as the JDK defines it.
fn math_member(name: &str, args: &[Value]) -> Result<Value, String> {
    // `java.lang.Math` has no `signum(int)` overload, so an integral argument
    // widens there (`Math.signum(5)` is `1.0`) but not in `scala.math`.
    let (java, name) = match name.strip_prefix("java.") {
        Some(n) => (true, n),
        None => (false, name),
    };
    let ints = args.iter().all(|a| matches!(a, Value::Int(_))) && !(java && name == "signum");
    let f = |i: usize| num_f64(&args[i]);
    match (name, args.len()) {
        ("Pi" | "PI", 0) => Ok(Value::float(std::f64::consts::PI)),
        ("E", 0) => Ok(Value::float(std::f64::consts::E)),
        ("abs", 1) if ints => Ok(Value::int(args[0].to_int().abs())),
        ("abs", 1) => Ok(Value::float(f(0).abs())),
        ("signum", 1) if ints => Ok(Value::int(args[0].to_int().signum())),
        ("signum", 1) => Ok(Value::float(if f(0) == 0.0 { f(0) } else { f(0).signum() })),
        ("max", 2) if ints => Ok(Value::int(args[0].to_int().max(args[1].to_int()))),
        ("max", 2) => Ok(Value::float(java_double_max(f(0), f(1)))),
        ("min", 2) if ints => Ok(Value::int(args[0].to_int().min(args[1].to_int()))),
        ("min", 2) => Ok(Value::float(java_double_min(f(0), f(1)))),
        // `Math.round` is neither `floor(x + 0.5)` nor Rust's `round` — see
        // `java_round`, which is the JDK body.
        ("round", 1) => Ok(Value::int(java_round(f(0)))),
        ("floor", 1) => Ok(Value::float(f(0).floor())),
        ("ceil", 1) => Ok(Value::float(f(0).ceil())),
        ("rint", 1) => Ok(Value::float(round_half_even(f(0)))),
        ("sqrt", 1) => Ok(Value::float(f(0).sqrt())),
        ("cbrt", 1) => Ok(Value::float(f(0).cbrt())),
        ("exp", 1) => Ok(Value::float(f(0).exp())),
        ("log", 1) => Ok(Value::float(f(0).ln())),
        ("log10", 1) => Ok(Value::float(f(0).log10())),
        ("pow", 2) => Ok(Value::float(f(0).powf(f(1)))),
        ("hypot", 2) => Ok(Value::float(f(0).hypot(f(1)))),
        ("sin", 1) => Ok(Value::float(f(0).sin())),
        ("cos", 1) => Ok(Value::float(f(0).cos())),
        ("tan", 1) => Ok(Value::float(f(0).tan())),
        ("asin", 1) => Ok(Value::float(f(0).asin())),
        ("acos", 1) => Ok(Value::float(f(0).acos())),
        ("atan", 1) => Ok(Value::float(f(0).atan())),
        ("atan2", 2) => Ok(Value::float(f(0).atan2(f(1)))),
        ("toRadians", 1) => Ok(Value::float(f(0).to_radians())),
        ("toDegrees", 1) => Ok(Value::float(f(0).to_degrees())),
        _ => Err(format!(
            "scalars: value {name} is not a member of scala.math"
        )),
    }
}

/// IEEE round-half-to-even, which is what `Math.rint` does (and `f64::round`
/// does not).
fn round_half_even(x: f64) -> f64 {
    let r = x.round();
    if (x - x.trunc()).abs() == 0.5 && r % 2.0 != 0.0 {
        r - x.signum()
    } else {
        r
    }
}

/// Build a `List` heap value.
fn new_list(items: Vec<Value>) -> Value {
    heap_push(HeapVal::Seq(SeqKind::List, items))
}

/// Build a labelled `Seq` heap value.
fn new_seq(kind: SeqKind, items: Vec<Value>) -> Value {
    heap_push(HeapVal::Seq(kind, items))
}

/// Build an immutable `Set`: duplicates dropped (the first occurrence wins), the
/// representation upgraded to a `HashSet` when `rep` already was one or the
/// result exceeds four elements, and a `HashSet`'s elements put in trie order.
fn new_set(rep: HashRep, items: Vec<Value>) -> Value {
    // A mutable receiver derives through a fresh `HashSet.newBuilder`, which
    // starts at the default capacity however large the receiver was.
    if matches!(rep, HashRep::Mutable(_)) {
        return mut_set_from(mut_table_size_for(MUT_INITIAL_CAPACITY), items);
    }
    let mut uniq: Vec<Value> = Vec::with_capacity(items.len());
    for it in items {
        if !uniq.iter().any(|u| value_eq(u, &it)) {
            uniq.push(it);
        }
    }
    // A `LinkedHashSet` iterates its insertion list, so the deduplicated order
    // IS the order — nothing to reorder and no size threshold to cross.
    if rep == HashRep::Linked {
        return heap_push(HeapVal::Seq(SeqKind::Set(HashRep::Linked), uniq));
    }
    let rep = hash_rep(rep, uniq.len());
    if rep == HashRep::Hashed {
        if let Some(ordered) = champ_sorted(&uniq, Clone::clone) {
            uniq = ordered;
        }
    }
    heap_push(HeapVal::Seq(SeqKind::Set(rep), uniq))
}

/// Build a `mutable.HashSet` by inserting `items` into a table of length
/// `len`, replaying the growth the real `add` would have done.
fn mut_set_from(len: usize, items: Vec<Value>) -> Value {
    let mut uniq: Vec<Value> = Vec::with_capacity(items.len());
    let mut adds = Vec::with_capacity(items.len());
    for it in items {
        let is_new = !uniq.iter().any(|u| value_eq(u, &it));
        adds.push(is_new);
        if is_new {
            uniq.push(it);
        }
    }
    let len = mut_grown(len, 0, &adds);
    let ordered = mut_ordered(&uniq, len, Clone::clone);
    let sorted = ordered.is_some();
    let v = heap_push(HeapVal::Seq(
        SeqKind::Set(HashRep::Mutable(len as u32)),
        ordered.unwrap_or(uniq),
    ));
    mut_note_sorted(&v, sorted);
    v
}

/// Build a `mutable.HashMap` the same way, keyed by each entry's key. A repeated
/// key keeps its first position and takes the last value, as `put0` does.
fn mut_map_from(len: usize, entries: Vec<(Value, Value)>) -> Value {
    let mut uniq: Vec<(Value, Value)> = Vec::with_capacity(entries.len());
    let mut adds = Vec::with_capacity(entries.len());
    for (k, v) in entries {
        let existing = uniq.iter_mut().find(|(ek, _)| value_eq(ek, &k));
        adds.push(existing.is_none());
        match existing {
            Some(slot) => slot.1 = v,
            None => uniq.push((k, v)),
        }
    }
    let len = mut_grown(len, 0, &adds);
    let ordered = mut_ordered(&uniq, len, |(k, _)| k.clone());
    let sorted = ordered.is_some();
    let v = heap_push(HeapVal::Map(
        HashRep::Mutable(len as u32),
        ordered.unwrap_or(uniq),
    ));
    mut_note_sorted(&v, sorted);
    v
}

/// Build an immutable `Map` from already-deduplicated `entries` — the `Set`
/// treatment of [`new_set`], keyed by the entry key.
fn new_map(rep: HashRep, entries: Vec<(Value, Value)>) -> Value {
    if matches!(rep, HashRep::Mutable(_)) {
        return mut_map_from(mut_table_size_for(MUT_INITIAL_CAPACITY), entries);
    }
    let mut entries = entries;
    // A `LinkedHashMap` keeps its insertion list: a repeated key holds its
    // original position and takes the later value, exactly as `put` does.
    if rep == HashRep::Linked {
        let mut uniq: Vec<(Value, Value)> = Vec::with_capacity(entries.len());
        for (k, v) in entries {
            match uniq.iter_mut().find(|(ek, _)| value_eq(ek, &k)) {
                Some(slot) => slot.1 = v,
                None => uniq.push((k, v)),
            }
        }
        return heap_push(HeapVal::Map(HashRep::Linked, uniq));
    }
    let rep = hash_rep(rep, entries.len());
    if rep == HashRep::Hashed {
        if let Some(ordered) = champ_sorted(&entries, |(k, _)| k.clone()) {
            entries = ordered;
        }
    }
    heap_push(HeapVal::Map(rep, entries))
}

/// The representation an immutable `Set`/`Map` of `len` entries derived from a
/// `rep` receiver has: hashed once it was hashed or has outgrown `Set4`/`Map4`.
/// A mutable receiver stays mutable — [`new_set`]/[`new_map`] intercept it
/// before this is reached.
fn hash_rep(rep: HashRep, len: usize) -> HashRep {
    if rep == HashRep::Linked {
        return HashRep::Linked;
    }
    if rep == HashRep::Hashed || len > 4 {
        HashRep::Hashed
    } else {
        HashRep::Small
    }
}

/// Build a built-in `Some(v)` case-class record.
/// Build the built-in `Success(v)` record — `scala.util.Try`'s success case.
fn make_try_success(v: Value) -> Value {
    heap_alloc(ScalaObj {
        class: Arc::from("Success"),
        is_case: true,
        is_object: false,
        fields: vec![(Arc::from("value"), v)],
    })
}

/// Build the built-in `Failure(e)` record — `scala.util.Try`'s failure case.
fn make_try_failure(e: Value) -> Value {
    heap_alloc(ScalaObj {
        class: Arc::from("Failure"),
        is_case: true,
        is_object: false,
        fields: vec![(Arc::from("exception"), e)],
    })
}

fn make_some(v: Value) -> Value {
    heap_alloc(ScalaObj {
        class: Arc::from("Some"),
        is_case: true,
        is_object: false,
        fields: vec![(Arc::from("value"), v)],
    })
}

/// Build the built-in `None` case-object record.
fn make_none() -> Value {
    heap_alloc(ScalaObj {
        class: Arc::from("None"),
        is_case: true,
        is_object: true,
        fields: Vec::new(),
    })
}

/// Build a built-in `Left(v)` / `Right(v)` case-class record (`Either`'s cases,
/// which `Option.toRight`/`toLeft` answer).
fn make_either(right: bool, v: Value) -> Value {
    heap_alloc(ScalaObj {
        class: Arc::from(if right { "Right" } else { "Left" }),
        is_case: true,
        is_object: false,
        fields: vec![(Arc::from("value"), v)],
    })
}

/// View a value as an `Option`: `Some(Some(inner))` for a `Some` record,
/// `Some(None)` for the `None` singleton, `None` for anything else. The class
/// tag is the whole test — the compiler injects these two built-ins unless the
/// user shadows them, in which case the user's own record dispatches here too
/// and gets the same (correct) `Option` surface.
fn as_option(v: &Value) -> Option<Option<Value>> {
    with_obj(v, |o| match &*o.class {
        "Some" => o.fields.first().map(|(_, v)| v.clone()).map(Some),
        "None" => Some(None),
        _ => None,
    })
    .flatten()
}

/// View a value as an `Either`: `Some(Ok(v))` for a `Right` record, `Some(Err(v))`
/// for a `Left`, `None` for anything else. The class tag is the whole test, as
/// in [`as_option`].
fn as_either(v: &Value) -> Option<Result<Value, Value>> {
    with_obj(v, |o| {
        let field = o.fields.first().map(|(_, v)| v.clone())?;
        match &*o.class {
            "Right" => Some(Ok(field)),
            "Left" => Some(Err(field)),
            _ => None,
        }
    })
    .flatten()
}

/// `scala.util.Either`'s method surface. Returns `None` when `name` is not an
/// `Either` method, so the caller falls through to the record dispatcher (which
/// still answers `Right(x).value`, `hashCode`, `equals`, …).
///
/// Scala 2.13 made `Either` RIGHT-BIASED: `map`/`flatMap`/`getOrElse`/`exists`
/// and friends all operate on the `Right` and pass a `Left` through unchanged,
/// which is why `outcome` is a `Result` with `Right` as the `Ok` side.
fn either_method(
    vm: &mut VM,
    recv: &Value,
    outcome: Result<Value, Value>,
    name: &str,
    args: &[Value],
) -> Option<Result<Value, String>> {
    Some(match (name, args.len(), &outcome) {
        ("isRight", 0, _) => Ok(Value::bool(outcome.is_ok())),
        ("isLeft", 0, _) => Ok(Value::bool(outcome.is_err())),
        ("getOrElse", 1, Ok(v)) => Ok(v.clone()),
        ("getOrElse", 1, Err(_)) => Ok(args[0].clone()),
        ("orElse", 1, Ok(_)) => Ok(recv.clone()),
        ("orElse", 1, Err(_)) => Ok(args[0].clone()),
        ("toOption", 0, _) => Ok(opt(outcome.clone().ok())),
        ("toSeq" | "toList", 0, _) => Ok(new_list(outcome.clone().ok().into_iter().collect())),
        // `swap` exchanges the two sides.
        ("swap", 0, Ok(v)) => Ok(make_either(false, v.clone())),
        ("swap", 0, Err(v)) => Ok(make_either(true, v.clone())),
        ("contains", 1, Ok(v)) => Ok(Value::bool(value_eq(v, &args[0]))),
        ("contains", 1, Err(_)) => Ok(Value::bool(false)),
        // The right-biased combinators. A `Left` is returned UNCHANGED by
        // `map`/`flatMap`/`foreach`, and answers the empty result for the
        // predicates — `forall` on a `Left` is vacuously true, `exists` false.
        ("map", 1, Ok(v)) => match invoke_closure(vm, &args[0], std::slice::from_ref(v)) {
            Ok(r) => Ok(make_either(true, r)),
            Err(e) => Err(e),
        },
        ("flatMap", 1, Ok(v)) => invoke_closure(vm, &args[0], std::slice::from_ref(v)),
        ("foreach", 1, Ok(v)) => {
            invoke_closure(vm, &args[0], std::slice::from_ref(v)).map(|_| unit_value())
        }
        ("map" | "flatMap", 1, Err(_)) => Ok(recv.clone()),
        ("foreach", 1, Err(_)) => Ok(unit_value()),
        ("exists" | "forall", 1, Ok(v)) => invoke_closure(vm, &args[0], std::slice::from_ref(v))
            .map(|hit| Value::bool(truthy(&hit))),
        ("exists", 1, Err(_)) => Ok(Value::bool(false)),
        ("forall", 1, Err(_)) => Ok(Value::bool(true)),
        // `fold(fa, fb)` applies the FIRST function to a `Left` and the second
        // to a `Right`, so it is the one member that runs on both sides.
        ("fold", 2, _) => {
            let (f, v) = match &outcome {
                Ok(v) => (&args[1], v),
                Err(v) => (&args[0], v),
            };
            invoke_closure(vm, f, std::slice::from_ref(v))
        }
        // A `Right` failing the predicate becomes `Left(zero)`; a `Left` is
        // already left and passes through.
        ("filterOrElse", 2, Ok(v)) => match invoke_closure(vm, &args[0], std::slice::from_ref(v)) {
            Ok(hit) if truthy(&hit) => Ok(recv.clone()),
            Ok(_) => Ok(make_either(false, args[1].clone())),
            Err(e) => Err(e),
        },
        ("filterOrElse", 2, Err(_)) => Ok(recv.clone()),
        // `e.left` — the LEFT-biased view of the same `Either`, which is how a
        // right-biased `Either` still reaches its `Left`. It is a `case class`
        // in Scala (`Either.LeftProjection`), so it is one here: the record
        // renders `LeftProjection(Left(bad))` and compares structurally through
        // the ordinary case-record machinery, with no rule of its own.
        ("left", 0, _) => Ok(heap_alloc(ScalaObj {
            class: Arc::from(LEFT_PROJECTION),
            is_case: true,
            is_object: false,
            fields: vec![(Arc::from("e"), recv.clone())],
        })),
        _ => return None,
    })
}

/// The class tag of the record `Either.left` answers.
const LEFT_PROJECTION: &str = "LeftProjection";

/// View a value as a `LeftProjection`, answering the `Either` it projects.
fn as_left_projection(v: &Value) -> Option<Value> {
    with_obj(v, |o| {
        (&*o.class == LEFT_PROJECTION).then(|| o.fields.first().map(|(_, v)| v.clone()))
    })
    .flatten()
    .flatten()
}

/// `Either.LeftProjection`'s method surface — the mirror image of
/// [`either_method`]'s right-biased half, operating on the `Left` and passing a
/// `Right` through.
///
/// `outcome` is the projected `Either` viewed as success-or-failure, so its
/// `Err` side is the `Left` this projection is about; `either` is that `Either`
/// itself, which the members that answer it unchanged return (`map` and
/// `flatMap` on a `Right` give back the SAME instance, as Scala's do).
fn left_projection_method(
    vm: &mut VM,
    either: &Value,
    outcome: Result<Value, Value>,
    name: &str,
    args: &[Value],
) -> Option<Result<Value, String>> {
    Some(match (name, args.len(), &outcome) {
        ("get", 0, Err(v)) => Ok(v.clone()),
        // Scala's message names the projection, not the `Either`.
        ("get", 0, Ok(_)) => {
            Err("scalars: java.util.NoSuchElementException: Either.left.get on Right".to_string())
        }
        ("getOrElse", 1, Err(v)) => Ok(v.clone()),
        ("getOrElse", 1, Ok(_)) => Ok(args[0].clone()),
        ("toOption", 0, _) => Ok(opt(outcome.clone().err())),
        ("toSeq" | "toList", 0, _) => Ok(new_list(outcome.clone().err().into_iter().collect())),
        ("map", 1, Err(v)) => match invoke_closure(vm, &args[0], std::slice::from_ref(v)) {
            Ok(r) => Ok(make_either(false, r)),
            Err(e) => Err(e),
        },
        ("flatMap", 1, Err(v)) => invoke_closure(vm, &args[0], std::slice::from_ref(v)),
        ("foreach", 1, Err(v)) => {
            invoke_closure(vm, &args[0], std::slice::from_ref(v)).map(|_| unit_value())
        }
        ("map" | "flatMap", 1, Ok(_)) => Ok(either.clone()),
        ("foreach", 1, Ok(_)) => Ok(unit_value()),
        ("exists" | "forall", 1, Err(v)) => invoke_closure(vm, &args[0], std::slice::from_ref(v))
            .map(|hit| Value::bool(truthy(&hit))),
        ("exists", 1, Ok(_)) => Ok(Value::bool(false)),
        ("forall", 1, Ok(_)) => Ok(Value::bool(true)),
        // `filterToOption` answers `Some(the Either)` when the `Left` passes the
        // predicate, and `None` both when it fails and when there is no `Left`.
        ("filterToOption", 1, Err(v)) => {
            match invoke_closure(vm, &args[0], std::slice::from_ref(v)) {
                Ok(hit) if truthy(&hit) => Ok(opt(Some(either.clone()))),
                Ok(_) => Ok(opt(None)),
                Err(e) => Err(e),
            }
        }
        ("filterToOption", 1, Ok(_)) => Ok(opt(None)),
        _ => return None,
    })
}

/// View a value as a `scala.util.Try`: `Some(Ok(v))` for a `Success` record,
/// `Some(Err(e))` for a `Failure`, `None` for anything else.
///
/// As with [`as_option`], the class tag is the whole test — the compiler injects
/// these two case classes unless the user shadows them, in which case the user's
/// own record dispatches here and gets the same surface.
fn as_try(v: &Value) -> Option<Result<Value, Value>> {
    with_obj(v, |o| {
        let field = o.fields.first().map(|(_, v)| v.clone())?;
        match &*o.class {
            "Success" => Some(Ok(field)),
            "Failure" => Some(Err(field)),
            _ => None,
        }
    })
    .flatten()
}

/// `scala.util.Try`'s method surface. Returns `None` when `name` is not a `Try`
/// method, so the caller falls through to the record dispatcher (which still
/// answers `Success(x).value`, `Failure(e).exception`, `hashCode`, `equals`, …).
///
/// `outcome` is the receiver already viewed as success-or-failure; `recv` is the
/// receiver itself, which several members answer unchanged (`recover` on a
/// `Success`, `map` on a `Failure`) — Scala returns the SAME instance there, and
/// so does this.
fn try_method(
    vm: &mut VM,
    recv: &Value,
    outcome: Result<Value, Value>,
    name: &str,
    args: &[Value],
) -> Option<Result<Value, String>> {
    // Re-raise a `Failure`'s exception as this frontend's own fault text, which
    // is how `get` reports. `scala_str` already renders a throwable as
    // `<fqcn>: <message>`, which is exactly the shape a fault carries.
    let throw = |e: &Value| Err(format!("scalars: {}", scala_str(e)));
    Some(match (name, args.len(), &outcome) {
        ("isSuccess", 0, _) => Ok(Value::bool(outcome.is_ok())),
        ("isFailure", 0, _) => Ok(Value::bool(outcome.is_err())),
        // `get` on a `Failure` RETHROWS the exception it holds.
        ("get", 0, Ok(v)) => Ok(v.clone()),
        ("get", 0, Err(e)) => throw(e),
        ("getOrElse", 1, Ok(v)) => Ok(v.clone()),
        ("getOrElse", 1, Err(_)) => Ok(args[0].clone()),
        ("orElse", 1, Ok(_)) => Ok(recv.clone()),
        ("orElse", 1, Err(_)) => Ok(args[0].clone()),
        ("toOption", 0, _) => Ok(opt(outcome.ok())),
        ("toEither", 0, Ok(v)) => Ok(make_either(true, v.clone())),
        ("toEither", 0, Err(e)) => Ok(make_either(false, e.clone())),
        ("toSeq" | "toList", 0, _) => Ok(new_list(outcome.ok().into_iter().collect())),
        // `map`/`flatMap`/`filter`/`foreach` are all no-ops on a `Failure`,
        // which is what makes a chain short-circuit at the first throw.
        ("map", 1, Ok(v)) => match invoke_closure(vm, &args[0], std::slice::from_ref(v)) {
            Ok(r) => Ok(make_try_success(r)),
            Err(e) => Err(e),
        },
        ("flatMap", 1, Ok(v)) => invoke_closure(vm, &args[0], std::slice::from_ref(v)),
        ("foreach", 1, Ok(v)) => {
            invoke_closure(vm, &args[0], std::slice::from_ref(v)).map(|_| unit_value())
        }
        ("foreach", 1, Err(_)) => Ok(unit_value()),
        // A `Success` whose value fails the predicate becomes a `Failure`, with
        // the message `Try.filter` builds.
        ("filter" | "withFilter", 1, Ok(v)) => {
            match invoke_closure(vm, &args[0], std::slice::from_ref(v)) {
                Ok(hit) if truthy(&hit) => Ok(recv.clone()),
                Ok(_) => Ok(make_try_failure(new_throwable(
                    "java.util.NoSuchElementException",
                    Some(&format!("Predicate does not hold for {}", scala_str(v))),
                ))),
                Err(e) => Err(e),
            }
        }
        ("map" | "flatMap" | "filter" | "withFilter", 1, Err(_)) => Ok(recv.clone()),
        // `recover` takes a PartialFunction and only applies where it is
        // defined, so an arm-less exception stays a `Failure`.
        ("recover", 1, Err(e)) => match is_defined_at(vm, &args[0], e) {
            Ok(true) => match invoke_closure(vm, &args[0], std::slice::from_ref(e)) {
                Ok(r) => Ok(make_try_success(r)),
                Err(err) => Err(err),
            },
            Ok(false) => Ok(recv.clone()),
            Err(err) => Err(err),
        },
        ("recoverWith", 1, Err(e)) => match is_defined_at(vm, &args[0], e) {
            Ok(true) => invoke_closure(vm, &args[0], std::slice::from_ref(e)),
            Ok(false) => Ok(recv.clone()),
            Err(err) => Err(err),
        },
        ("recover" | "recoverWith", 1, Ok(_)) => Ok(recv.clone()),
        // `failed` INVERTS the two cases: a `Failure`'s exception becomes a
        // `Success`, and a `Success` becomes a `Failure` saying so.
        ("failed", 0, Err(e)) => Ok(make_try_success(e.clone())),
        ("failed", 0, Ok(_)) => Ok(make_try_failure(new_throwable(
            "java.lang.UnsupportedOperationException",
            Some("Success.failed"),
        ))),
        _ => return None,
    })
}

/// `scala.Option`'s method surface. Returns `None` when `name` is not an
/// `Option` method, so the caller falls through to the record dispatcher (which
/// still answers `Some(x).value`, `hashCode`, `equals`, …).
///
/// `getOrElse`/`orElse`/`fold`'s default is by-name in Scala but is already
/// evaluated by the time it reaches here; that is observable only for a default
/// with side effects, which the parity corpus never generates.
fn option_method(
    vm: &mut VM,
    inner: Option<Value>,
    name: &str,
    args: &[Value],
) -> Option<Result<Value, String>> {
    let some = |v: Value| Ok(make_some(v));
    // A closure call can fault; `?`-style early return is spelled out because
    // this function answers `Option<Result<…>>`.
    macro_rules! call {
        ($f:expr, $x:expr) => {
            match invoke_closure(vm, $f, std::slice::from_ref($x)) {
                Ok(v) => v,
                Err(e) => return Some(Err(e)),
            }
        };
    }
    Some(match (name, args.len()) {
        ("get", 0) => match inner {
            Some(v) => Ok(v),
            None => Err("scalars: java.util.NoSuchElementException: None.get".into()),
        },
        ("getOrElse", 1) => Ok(inner.unwrap_or_else(|| args[0].clone())),
        ("orNull", 0) => Ok(inner.unwrap_or(Value::Undef)),
        ("isEmpty", 0) => Ok(Value::bool(inner.is_none())),
        ("isDefined" | "nonEmpty", 0) => Ok(Value::bool(inner.is_some())),
        ("size" | "knownSize", 0) => Ok(Value::int(i64::from(inner.is_some()))),
        ("contains", 1) => Ok(Value::bool(inner.is_some_and(|v| value_eq(&v, &args[0])))),
        ("toList" | "toSeq", 0) => Ok(new_list(inner.into_iter().collect())),
        ("toVector", 0) => Ok(new_seq(SeqKind::Vector, inner.into_iter().collect())),
        // An `Option` iterates as zero or one element, and its `iterator` is a
        // real consumable `Iterator` like every other collection's.
        ("iterator", 0) => Ok(new_seq(SeqKind::Iterator, inner.into_iter().collect())),
        ("toRight", 1) => Ok(match inner {
            Some(v) => make_either(true, v),
            None => make_either(false, args[0].clone()),
        }),
        ("toLeft", 1) => Ok(match inner {
            Some(v) => make_either(false, v),
            None => make_either(true, args[0].clone()),
        }),
        ("orElse", 1) => Ok(match inner {
            Some(v) => make_some(v),
            None => args[0].clone(),
        }),
        // `Option[Option[A]].flatten` — the inner option, or `None`.
        ("flatten", 0) => Ok(match inner {
            Some(v) if as_option(&v).is_some() => v,
            Some(_) => return Some(Err("scalars: flatten needs an Option of Option".into())),
            None => make_none(),
        }),
        ("map", 1) => match inner {
            Some(v) => some(call!(&args[0], &v)),
            None => Ok(make_none()),
        },
        ("flatMap", 1) => match inner {
            Some(v) => Ok(call!(&args[0], &v)),
            None => Ok(make_none()),
        },
        ("filter" | "filterNot" | "withFilter", 1) => match inner {
            Some(v) => {
                let keep = truthy(&call!(&args[0], &v)) == (name != "filterNot");
                Ok(if keep { make_some(v) } else { make_none() })
            }
            None => Ok(make_none()),
        },
        ("exists", 1) => match inner {
            Some(v) => Ok(Value::bool(truthy(&call!(&args[0], &v)))),
            None => Ok(Value::bool(false)),
        },
        ("forall", 1) => match inner {
            Some(v) => Ok(Value::bool(truthy(&call!(&args[0], &v)))),
            None => Ok(Value::bool(true)),
        },
        ("count", 1) => match inner {
            Some(v) => Ok(Value::int(i64::from(truthy(&call!(&args[0], &v))))),
            None => Ok(Value::int(0)),
        },
        ("foreach", 1) => {
            if let Some(v) = inner {
                call!(&args[0], &v);
            }
            Ok(unit_value())
        }
        // `fold(ifEmpty)(f)` — the parser folds the second argument list into
        // one call, so both arrive together.
        ("fold", 2) => match inner {
            Some(v) => Ok(call!(&args[1], &v)),
            None => Ok(args[0].clone()),
        },
        // `collect`/`collectFirst` over an `Option` follow the same
        // `isDefinedAt`-then-apply protocol the collections use.
        ("collect", 1) => match inner {
            Some(v) => match is_defined_at(vm, &args[0], &v) {
                Ok(true) => some(call!(&args[0], &v)),
                Ok(false) => Ok(make_none()),
                Err(e) => Err(e),
            },
            None => Ok(make_none()),
        },
        ("head", 0) => match inner {
            Some(v) => Ok(v),
            None => Err("scalars: java.util.NoSuchElementException: head of empty list".into()),
        },
        ("headOption" | "lastOption", 0) => Ok(match inner {
            Some(v) => make_some(v),
            None => make_none(),
        }),
        _ => return None,
    })
}

/// Resolve `recv.name(args)` against the wired stdlib, or return the Scala-style
/// error message for an unresolved call. Kept host-only (no VM handle) so it is
/// straightforward to unit-test.
fn dispatch_method(recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    // `toString` is defined on every value (Scala's `Any.toString`).
    if name == "toString" && args.is_empty() {
        return Ok(Value::str(scala_str(recv)));
    }
    // `getClass` is likewise on every value — for the receivers whose JVM class
    // this frontend can name faithfully (see [`class_of`]).
    if name == "getClass" && args.is_empty() {
        return class_of(recv);
    }
    // A `Regex`/`Regex.Match` handle is not a record, so it is answered before
    // the record dispatcher below.
    if let Some(r) = regex_method(recv, name, args) {
        return r;
    }
    // Likewise a `Char`: its handle is not a record, and its methods must win
    // over the `String` ones it would otherwise fall back to (`'5'.toInt` is the
    // code point 53, where `"5".toInt` parses to 5).
    if let Some(c) = as_char(recv) {
        return char_method(c, name, args);
    }
    // An `Ordering` handle is likewise not a record; `.reverse` on one must not
    // reach the sequence dispatcher, which would try to reverse a collection.
    // Its members can run a user closure, so they are answered on the VM-aware
    // path in `b_smethod`; reaching this PURE one means the name is not one of
    // them, and it stops here rather than falling through.
    if as_ordering(recv).is_some() {
        return Err(format!("scalars: value {name} is not a member of Ordering"));
    }
    // Scala's operators ARE methods, so the dotted spelling (`n.+(1)`, `"a".*(3)`)
    // is legal wherever the infix one is. Only the primitive receivers route here:
    // `+`/`-` on a `Set`/`Map` mean set inclusion/removal and stay with the
    // collection dispatcher below.
    if matches!(
        recv,
        Value::Str(_) | Value::Int(_) | Value::Float(_) | Value::Status(_)
    ) {
        if let Some(r) = operator_method(recv, name, args) {
            return r;
        }
    }
    match recv {
        Value::Str(s) => string_method(s, name, args),
        Value::Int(n) => int_method(*n, name, args),
        Value::Float(f) => double_method(*f, name, args),
        Value::Status(_) => float_method(f32_of(recv).unwrap_or(0.0), name, args),
        Value::Bool(b) => bool_method(*b, name, args),
        Value::Obj(_) => obj_method(recv, name, args),
        _ => Err(no_such_method(recv, name)),
    }
}

/// `Char`'s methods.
///
/// `Char` is a numeric type wearing a text face: every conversion answers the
/// *code point* (`'a'.toInt == 97`), the classification and case methods come
/// from `java.lang.Character`, and only `toString` renders the character. The
/// case methods answer a `Char` again, so `"abc".map(_.toUpper)` stays a
/// `String`.
///
/// Anything unrecognized falls through to [`string_method`] on the one-character
/// string, which is where the shared `Any`/comparison methods live.
fn char_method(c: char, name: &str, args: &[Value]) -> Result<Value, String> {
    let code = c as u32 as i64;
    // `Character` classification is defined over the whole code point, but
    // `isDigit` is Java's ASCII-and-Unicode-decimal test, not `is_numeric`.
    let cls = |b: bool| Ok(Value::bool(b));
    match (name, args.len()) {
        // Numeric conversions — the code point, never a parse of the text. A
        // `Char` is UNSIGNED and sixteen bits wide, so the two narrowing
        // conversions can turn a high code point negative: `Char.MaxValue.toByte`
        // and `.toShort` are both -1.
        ("toInt" | "toLong", 0) => Ok(Value::int(code)),
        ("toShort", 0) => Ok(Value::int(to_short(code))),
        ("toByte", 0) => Ok(Value::int(to_byte(code))),
        ("toDouble", 0) => Ok(Value::float(code as f64)),
        ("toFloat", 0) => Ok(Value::float(f64::from(code as f32))),
        ("toChar", 0) => Ok(make_char(c)),
        // `Char.hashCode` is the code point (as `java.lang.Character`'s is).
        ("hashCode", 0) => Ok(Value::int(code)),
        ("equals", 1) => Ok(Value::bool(as_char(&args[0]) == Some(c))),
        // `asDigit` is the *numeric value* of a digit character, so '5' is 5;
        // it also accepts the hex letters, as `Character.digit(c, 36)` does.
        ("asDigit", 0) => Ok(Value::int(c.to_digit(36).map(|d| d as i64).unwrap_or(-1))),
        ("toUpper", 0) => Ok(make_char(c.to_ascii_uppercase())),
        ("toLower", 0) => Ok(make_char(c.to_ascii_lowercase())),
        ("isDigit", 0) => cls(c.is_ascii_digit()),
        ("isLetter", 0) => cls(c.is_alphabetic()),
        ("isLetterOrDigit", 0) => cls(c.is_alphanumeric()),
        ("isUpper", 0) => cls(c.is_uppercase()),
        ("isLower", 0) => cls(c.is_lowercase()),
        ("isWhitespace" | "isSpaceChar", 0) => cls(c.is_whitespace()),
        // Ordering is by code point. Scala's `Char.compare` answers the
        // difference, exactly as `Character.compare` does.
        ("compare" | "compareTo", 1) => match char_code(&args[0]) {
            Some(o) => Ok(Value::int(code - o)),
            None => Err(no_such_method(&make_char(c), name)),
        },
        // `max`/`min` on two `Char`s answer a `Char` (`'a'.max('b')` is `b`).
        ("max" | "min", 1) if as_char(&args[0]).is_some() => {
            let o = as_char(&args[0]).unwrap();
            Ok(make_char(if (name == "max") == (c > o) { c } else { o }))
        }
        // The operators, in their dotted spelling (`'a'.+(1)`). The infix form
        // reaches `numeric_hook` instead; both share `char_binop`.
        (_, 1) if CHAR_OPS.contains(&name) => char_binop(name, &make_char(c), &args[0]),
        _ => string_method(&c.to_string(), name, args),
    }
}

/// The binary operators `Char` participates in, for both the dotted spelling
/// ([`char_method`]) and the infix one ([`numeric_hook`]).
const CHAR_OPS: &[&str] = &["+", "-", "*", "/", "%", "<", ">", "<=", ">=", "==", "!="];

/// One binary operation with at least one `Char` operand.
///
/// A `Char` enters arithmetic and comparison as its code point — `'a' + 1` is
/// the `Int` 98 and `'a' < 'b'` compares code points — with one exception:
/// `Char` defines `+(String): String`, so a `String` operand makes `+`
/// concatenation (`'a' + "b" == "ab"`), which is why this is not a plain
/// numeric coercion.
fn char_binop(op: &str, a: &Value, b: &Value) -> Result<Value, String> {
    if op == "+" && (matches!(a, Value::Str(_)) || matches!(b, Value::Str(_))) {
        return Ok(Value::str(format!("{}{}", scala_str(a), scala_str(b))));
    }
    // A `Double` operand promotes the whole operation, as it does for any other
    // integral type (`'a' + 1.5 == 98.5`).
    if matches!(a, Value::Float(_)) || matches!(b, Value::Float(_)) {
        if let (Some(x), Some(y)) = (float_of(a), float_of(b)) {
            return Ok(match op {
                "+" => Value::float(x + y),
                "-" => Value::float(x - y),
                "*" => Value::float(x * y),
                "/" => Value::float(x / y),
                "%" => Value::float(x % y),
                "<" => Value::bool(x < y),
                ">" => Value::bool(x > y),
                "<=" => Value::bool(x <= y),
                ">=" => Value::bool(x >= y),
                "==" => Value::bool(x == y),
                _ => Value::bool(x != y),
            });
        }
    }
    // Equality against a non-numeric, non-Char operand is `false` rather than an
    // error: Scala's `==` is universal even where arithmetic is not.
    let (Some(x), Some(y)) = (num_of(a), num_of(b)) else {
        return match op {
            "==" => Ok(Value::bool(scala_eq(a, b))),
            "!=" => Ok(Value::bool(!scala_eq(a, b))),
            _ => Err(format!(
                "scalars: `{op}` is not defined between `{}` and `{}`",
                scala_str(a),
                scala_str(b)
            )),
        };
    };
    Ok(match op {
        "+" => Value::int(x + y),
        "-" => Value::int(x - y),
        "*" => Value::int(x * y),
        "/" if y == 0 => return Err("scalars: java.lang.ArithmeticException: / by zero".into()),
        "/" => Value::int(x / y),
        "%" if y == 0 => return Err("scalars: java.lang.ArithmeticException: / by zero".into()),
        "%" => Value::int(x.wrapping_rem(y)),
        "<" => Value::bool(x < y),
        ">" => Value::bool(x > y),
        "<=" => Value::bool(x <= y),
        ">=" => Value::bool(x >= y),
        "==" => Value::bool(x == y),
        _ => Value::bool(x != y),
    })
}

/// The integer an operand contributes to `Char` arithmetic: a `Char`'s code
/// point or an `Int`'s value. `None` for everything else (including a `Double`,
/// which would make the result a `Double` — see [`char_binop`]'s callers, which
/// only reach it when no `Double` is involved).
fn num_of(v: &Value) -> Option<i64> {
    match v {
        Value::Int(n) => Some(*n),
        _ => char_code(v),
    }
}

/// The same operand as an `f64`, for the `Double`-promoted path.
fn float_of(v: &Value) -> Option<f64> {
    match v {
        Value::Float(f) => Some(*f),
        // A `Float` widens exactly — every `f32` is an `f64` — so the `Double`
        // view of one is its own value, which is what makes `0.1f + 1.0` the
        // `Double` 1.1 and `0.1f.toDouble` 0.10000000149011612.
        Value::Status(_) => f32_of(v).map(f64::from),
        _ => num_of(v).map(|n| n as f64),
    }
}

/// A numeric operand as an `f64`, `NaN` when it is not numeric at all.
///
/// The difference from `Value::to_float` is a `Float`: that one reads
/// `Value::Status`'s payload as the integer it is declared to be (a shell exit
/// code), which for a `Float` is its BIT PATTERN — `1.0f` came out as
/// 1065353216.0. Every arithmetic path that can see a `Float` uses this.
fn num_f64(v: &Value) -> f64 {
    float_of(v).unwrap_or_else(|| v.to_float())
}

/// The dotted spelling of a binary operator on a primitive receiver — `n.+(1)`,
/// `x./(2)`, `"a".*(3)`, `a.<(b)`. `None` when `name` is not one (so ordinary
/// method dispatch continues), which is also the answer for a recognized
/// operator at the wrong arity.
///
/// The semantics are the infix ones exactly: `/` truncates for two `Int`s and
/// throws on an integer zero divisor (see [`b_div`]), `+` concatenates when
/// either side is a `String` (Scala 3 has no universal `any2stringadd`, and one
/// side here is always primitive), and `*` on a `String` repeats it.
fn operator_method(recv: &Value, name: &str, args: &[Value]) -> Option<Result<Value, String>> {
    const OPS: &[&str] = &["+", "-", "*", "/", "%", "<", ">", "<=", ">=", "==", "!="];
    if !OPS.contains(&name) || args.len() != 1 {
        return None;
    }
    let b = &args[0];
    let both_int = matches!((recv, b), (Value::Int(_), Value::Int(_)));
    Some(match name {
        "==" => Ok(Value::bool(scala_eq(recv, b))),
        "!=" => Ok(Value::bool(!scala_eq(recv, b))),
        // A `String` receiver compares lexicographically; anything else compares
        // as a number, where an IEEE NaN operand correctly makes all four false.
        "<" | ">" | "<=" | ">=" => Ok(Value::bool(match recv {
            Value::Str(x) => {
                let (x, y) = (x.as_str(), b.as_str_cow());
                match name {
                    "<" => x < &*y,
                    ">" => x > &*y,
                    "<=" => x <= &*y,
                    _ => x >= &*y,
                }
            }
            _ => {
                let (x, y) = (num_f64(recv), num_f64(b));
                match name {
                    "<" => x < y,
                    ">" => x > y,
                    "<=" => x <= y,
                    _ => x >= y,
                }
            }
        })),
        "*" if matches!(recv, Value::Str(_)) => string_method(&recv.as_str_cow(), "*", args),
        "+" if matches!(recv, Value::Str(_)) || matches!(b, Value::Str(_)) => {
            Ok(Value::str(format!("{}{}", scala_str(recv), scala_str(b))))
        }
        _ if matches!(recv, Value::Str(_)) => Err(no_such_method(recv, name)),
        "+" if both_int => Ok(Value::int(recv.to_int().wrapping_add(b.to_int()))),
        "-" if both_int => Ok(Value::int(recv.to_int().wrapping_sub(b.to_int()))),
        "*" if both_int => Ok(Value::int(recv.to_int().wrapping_mul(b.to_int()))),
        "/" if both_int => {
            if b.to_int() == 0 {
                Err("scalars: java.lang.ArithmeticException: / by zero".to_string())
            } else {
                Ok(Value::int(recv.to_int().wrapping_div(b.to_int())))
            }
        }
        "%" if both_int => {
            if b.to_int() == 0 {
                Err("scalars: java.lang.ArithmeticException: / by zero".to_string())
            } else {
                Ok(Value::int(recv.to_int().wrapping_rem(b.to_int())))
            }
        }
        "+" => Ok(Value::float(num_f64(recv) + num_f64(b))),
        "-" => Ok(Value::float(num_f64(recv) - num_f64(b))),
        "*" => Ok(Value::float(num_f64(recv) * num_f64(b))),
        "/" => Ok(Value::float(num_f64(recv) / num_f64(b))),
        _ => Ok(Value::float(num_f64(recv) % num_f64(b))),
    })
}

/// `String` methods (a faithful subset of `java.lang.String` / Scala
/// `StringOps`). Lengths/indices are in `char`s — matching Scala for the BMP
/// text this frontend handles.
/// The `[start, end)` windows `sliding(size, step)` yields over `len` elements,
/// which is also what `grouped(n)` (step `n`) and `sliding(n)` (step 1) yield.
///
/// The rule is not "every `step`-th window" and not "full windows only" —
/// `GroupedIterator` refills from the UNDERLYING iterator between windows, so it
/// stops as soon as the window it just produced reached the end of the input,
/// and a short final window is emitted only when it began past where the previous
/// one ended. That single condition is `start + size >= len`, and it is what
/// makes these disagree, all measured against reference Scala 3.9.0:
///
/// ```text
///   List(1,2,3).sliding(2)        List(List(1, 2), List(2, 3))
///   List(1,2,3).sliding(2, 2)     List(List(1, 2), List(3))
///   List(1,2,3,4).sliding(3, 2)   List(List(1, 2, 3), List(3, 4))
///   List(1,2,3).sliding(5, 2)     List(List(1, 2, 3))
/// ```
///
/// The first drops its trailing `List(3)` and the second keeps it, for the same
/// receiver and the same window size. An EMPTY receiver yields nothing at all —
/// the old code answered one empty window instead, which printed identically for
/// a `String` (`List("")` and `List()` are both `List()` on the page) and
/// differed only in `size`.
fn window_starts(len: usize, size: usize, step: usize) -> impl Iterator<Item = (usize, usize)> {
    let mut start = 0usize;
    let mut done = len == 0;
    std::iter::from_fn(move || {
        if done {
            return None;
        }
        let end = (start + size).min(len);
        done = start + size >= len;
        let w = (start, end);
        start += step;
        Some(w)
    })
}

/// The receiver reordered the way `SeqOps.permutations` and
/// `SeqOps.combinations` both start from, as `(ranks, elements)`.
///
/// Scala's `PermutationsItr`/`CombinationsItr` do not sort the receiver and do
/// not take it as written. Both begin by numbering each element with the
/// position at which its VALUE first appeared and then stably sorting by that
/// number, which groups duplicates together while leaving distinct elements in
/// first-appearance order. That is why the enumeration is not lexicographic in
/// the elements — `List(3,2,1).permutations` leads with `List(3, 2, 1)` and
/// `List(2,1,1).combinations(2)` answers `List(2, 1)` before `List(1, 1)` —
/// and why duplicates are never repeated: with equal elements adjacent, the
/// enumerations below skip a repeat by advancing past a run.
///
/// The ranks are returned alongside because they, not the values, are what the
/// two walks compare: `Value` has no total order here, but the ranks do.
fn first_occurrence_order(items: &[Value]) -> (Vec<usize>, Vec<Value>) {
    let mut distinct: Vec<&Value> = Vec::new();
    let mut ranked: Vec<(usize, &Value)> = Vec::with_capacity(items.len());
    for it in items {
        let rank = match distinct.iter().position(|d| scala_eq(d, it)) {
            Some(r) => r,
            None => {
                distinct.push(it);
                distinct.len() - 1
            }
        };
        ranked.push((rank, it));
    }
    ranked.sort_by_key(|(r, _)| *r);
    ranked
        .into_iter()
        .map(|(r, v)| (r, v.clone()))
        .unzip::<usize, Value, Vec<usize>, Vec<Value>>()
}

/// Advance `ranks` (and `elems` in lockstep) to the next permutation in
/// ascending rank order — the textbook next-permutation step, which is exactly
/// what `PermutationsItr` performs. `false` once the last one has been reached.
fn next_permutation(ranks: &mut [usize], elems: &mut [Value]) -> bool {
    let n = ranks.len();
    if n < 2 {
        return false;
    }
    let Some(i) = (0..n - 1).rev().find(|&i| ranks[i] < ranks[i + 1]) else {
        return false;
    };
    let j = (i + 1..n)
        .rev()
        .find(|&j| ranks[j] > ranks[i])
        .expect("ranks[i] < ranks[i+1] guarantees one");
    ranks.swap(i, j);
    elems.swap(i, j);
    ranks[i + 1..].reverse();
    elems[i + 1..].reverse();
    true
}

/// Every DISTINCT permutation of `items`, in `SeqOps.permutations` order.
fn permutations_of(items: &[Value]) -> Vec<Vec<Value>> {
    let (mut ranks, mut elems) = first_occurrence_order(items);
    let mut out = vec![elems.clone()];
    while next_permutation(&mut ranks, &mut elems) {
        out.push(elems.clone());
    }
    out
}

/// Every DISTINCT `n`-element sub-multiset of `items`, in
/// `SeqOps.combinations(n)` order. Empty for a negative `n` or one larger than
/// the receiver; a single empty combination for `n == 0`, whatever the receiver.
fn combinations_of(items: &[Value], n: i64) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    if n < 0 || n as usize > items.len() {
        return out;
    }
    let (ranks, elems) = first_occurrence_order(items);
    let n = n as usize;
    let mut cur: Vec<Value> = Vec::with_capacity(n);
    // Iterative depth-first walk over start positions. `stack[d]` is the index
    // the depth-`d` element was taken from, so backtracking resumes past the
    // whole RUN of that element's rank — which is what makes each sub-multiset
    // appear once rather than once per arrangement of its equal elements.
    let mut stack: Vec<usize> = Vec::with_capacity(n);
    let mut i = 0usize;
    loop {
        if cur.len() == n {
            out.push(cur.clone());
        } else if i < elems.len() && elems.len() - i >= n - cur.len() {
            cur.push(elems[i].clone());
            stack.push(i);
            i += 1;
            continue;
        }
        // Backtrack: undo the last take and resume past its run of equals.
        let Some(taken) = stack.pop() else {
            return out;
        };
        cur.pop();
        i = taken + 1;
        while i < ranks.len() && ranks[i] == ranks[taken] {
            i += 1;
        }
    }
}

fn string_method(s: &str, name: &str, args: &[Value]) -> Result<Value, String> {
    // Every `String` method that accepts a `Char` (`indexOf`, `contains`,
    // `split`, `replace`, …) uses it as text, and Scala overloads them for both,
    // so a `Char` argument becomes its one-character `String` once here rather
    // than at each call site.
    let coerced: Vec<Value>;
    let args = if args.iter().any(|a| as_char(a).is_some()) {
        coerced = args
            .iter()
            .map(|a| match as_char(a) {
                Some(c) => Value::str(c.to_string()),
                None => a.clone(),
            })
            .collect();
        &coerced[..]
    } else {
        args
    };
    let arity_err = || format!("scalars: String.{name}: wrong number of arguments");
    match (name, args.len()) {
        ("length" | "size", 0) => Ok(Value::int(s.chars().count() as i64)),
        ("isEmpty", 0) => Ok(Value::bool(s.is_empty())),
        ("nonEmpty", 0) => Ok(Value::bool(!s.is_empty())),
        ("toUpperCase", 0) => Ok(Value::str(s.to_uppercase())),
        ("toLowerCase", 0) => Ok(Value::str(s.to_lowercase())),
        ("trim", 0) => Ok(Value::str(java_trim(s))),
        ("strip", 0) => Ok(Value::str(java_strip(s))),
        ("stripLeading", 0) => Ok(Value::str(
            s.trim_start_matches(java_is_whitespace).to_string(),
        )),
        ("stripTrailing", 0) => Ok(Value::str(
            s.trim_end_matches(java_is_whitespace).to_string(),
        )),
        ("stripMargin", 0) => Ok(Value::str(strip_margin(s, '|'))),
        // The margin may arrive as a `Char` value or, from a one-character
        // string literal, as a `Str`.
        ("stripMargin", 1) => Ok(Value::str(strip_margin(
            s,
            as_char(&args[0])
                .or_else(|| one_char(&args[0].as_str_cow()))
                .unwrap_or('|'),
        ))),
        ("concat", 1) => Ok(Value::str(format!("{s}{}", args[0].as_str_cow()))),
        // `String.split` answers an `Array[String]`, and its separator is a
        // REGEX (`java.lang.String.split`), not a literal — see [`java_split`].
        ("split", 1) => java_split(s, &args[0].as_str_cow())
            .map(|parts| new_seq(SeqKind::Array, parts.into_iter().map(Value::str).collect())),
        // The `java.util.regex` surface of `String`.
        ("matches", 1) => regex_full_match(&args[0].as_str_cow(), s),
        ("replaceAll", 2) => regex_replace(s, &args[0].as_str_cow(), &args[1].as_str_cow(), false),
        ("replaceFirst", 2) => regex_replace(s, &args[0].as_str_cow(), &args[1].as_str_cow(), true),
        // `"…".r` — `StringOps.r`, the `scala.util.matching.Regex` builder.
        ("r", 0) => Ok(heap_push(HeapVal::Regex(Arc::from(s)))),
        // The elements of a `String` are `Char`s, so every accessor that hands
        // one out hands out a `Char` — that is what makes `"abc".toList.map(_.toInt)`
        // the code points rather than a parse of each character.
        ("toList", 0) => Ok(new_list(s.chars().map(make_char).collect())),
        // `StringOps.iterator` — the characters as a CONSUMABLE `Iterator`,
        // not a `List`, so it renders as `<iterator>` and a second traversal
        // sees it exhausted (see `SeqKind::Iterator`).
        ("iterator", 0) => Ok(new_seq(
            SeqKind::Iterator,
            s.chars().map(make_char).collect(),
        )),
        // `StringOps.updated(i, c)` — the string with one character replaced.
        // Its bounds fault is `String`'s, not a collection's: the JDK's
        // `charAt` wording, which names the length rather than the max index.
        ("updated", 2) => {
            let i = args[0].to_int();
            let chars: Vec<char> = s.chars().collect();
            if i < 0 || i as usize >= chars.len() {
                return Err(format!(
                    "scalars: java.lang.StringIndexOutOfBoundsException: \
                     Index {i} out of bounds for length {}",
                    chars.len()
                ));
            }
            let c = as_char(&args[1])
                .or_else(|| one_char(&args[1].as_str_cow()))
                .unwrap_or('\u{0}');
            Ok(Value::str(
                chars
                    .iter()
                    .enumerate()
                    .map(|(j, &ch)| if j == i as usize { c } else { ch })
                    .collect::<String>(),
            ))
        }
        // `StringOps.permutations`/`combinations` — same enumeration as a
        // sequence's, but rebuilt through `StringOps`'s own builder, so the
        // windows are `String`s (`"abc".combinations(2).toList` is
        // `List(ab, ac, bc)`, not lists of `Char`).
        ("permutations", 0) | ("combinations", 1) => {
            let chars: Vec<Value> = s.chars().map(make_char).collect();
            let groups = if name == "permutations" {
                permutations_of(&chars)
            } else {
                combinations_of(&chars, args[0].to_int())
            };
            Ok(new_seq(
                SeqKind::Iterator,
                groups
                    .into_iter()
                    .map(|g| Value::str(g.iter().filter_map(as_char).collect::<String>()))
                    .collect(),
            ))
        }
        // `StringOps.grouped`/`sliding` — an `Iterator` whose windows are
        // `String`s, not `List`s of `Char` (`"abcd".grouped(2).toList` is
        // `List(ab, cd)`), because `StringOps` rebuilds through its own builder.
        ("grouped" | "sliding", 1) | ("sliding", 2) => {
            let n = args[0].to_int();
            let step = match (name, args.len()) {
                (_, 2) => args[1].to_int(),
                ("grouped", _) => n,
                _ => 1,
            };
            if n < 1 || step < 1 {
                return Err(format!(
                    "scalars: java.lang.IllegalArgumentException: requirement failed: \
                     size={n} and step={step}, but both must be positive"
                ));
            }
            let chars: Vec<char> = s.chars().collect();
            let out: Vec<Value> = window_starts(chars.len(), n as usize, step as usize)
                .map(|(a, b)| Value::str(chars[a..b].iter().collect::<String>()))
                .collect();
            Ok(new_seq(SeqKind::Iterator, out))
        }
        // `StringOps.padTo` answers a STRING (not a `Seq[Char]`), and a length at
        // or below the current one leaves it untouched.
        ("padTo", 2) => {
            let want = args[0].to_int().max(0) as usize;
            let fill = as_char(&args[1])
                .or_else(|| one_char(&args[1].as_str_cow()))
                .unwrap_or(' ');
            let mut out = s.to_string();
            for _ in s.chars().count()..want {
                out.push(fill);
            }
            Ok(Value::str(out))
        }
        // `"abc".toSeq` is a `WrappedString` — a VIEW of the same characters,
        // which prints as the string itself (`abc`, not `List(a, b, c)`) and
        // answers every `Seq` operation through `StringOps`. The string is
        // exactly that view here.
        // `toIterable` is the same view under its `Iterable` name.
        ("toSeq" | "toIterable", 0) => Ok(Value::str(s)),
        ("reverse", 0) => Ok(Value::str(s.chars().rev().collect::<String>())),
        // `StringOps.toInt` IS `Integer.parseInt`, and the JDK's integer parses
        // do not trim: `" 42".toInt` throws where `" 42".trim.toInt` answers 42,
        // and `"2147483648".toInt` is out of an `Int`'s range even though every
        // digit is legal. `toLong` is the same parse one width up, and the two
        // narrowing conversions add the box's own range check on top.
        ("toInt", 0) => java_parse_int(s, 10).map(Value::int),
        ("toLong", 0) => java_parse_long(s, 10).map(Value::int),
        // The `…Option` forms are the same parses with the throw turned into a
        // `None`, which is exactly what makes them worth having separately: they
        // reject what a "parse and swallow the error" version would accept.
        // Measured against reference Scala 3.9.0, `" 12 ".toIntOption` is `None`
        // (the JDK integer parses do not trim, as the comment above says) and
        // `"99999999999999999999".toIntOption` is `None` for being out of an
        // `Int`'s range rather than for being unparseable.
        ("toIntOption", 0) => {
            Ok(java_parse_int(s, 10).map_or_else(|_| make_none(), |n| make_some(Value::int(n))))
        }
        ("toLongOption", 0) => {
            Ok(java_parse_long(s, 10).map_or_else(|_| make_none(), |n| make_some(Value::int(n))))
        }
        ("toByte", 0) => {
            java_parse_narrow(s, 10, i64::from(i8::MIN), i64::from(i8::MAX)).map(Value::int)
        }
        ("toShort", 0) => {
            java_parse_narrow(s, 10, i64::from(i16::MIN), i64::from(i16::MAX)).map(Value::int)
        }
        // `Double.parseDouble` DOES accept surrounding whitespace, which is the
        // asymmetry above: the same padding that makes `toInt` throw is fine
        // here.
        ("toDouble" | "toFloat", 0) => s
            .trim()
            .parse::<f64>()
            .map(|v| {
                // `"0.1".toFloat` parses AT single precision, so the parse and
                // the rounding are one step in Scala and two here.
                Value::float(if name == "toFloat" {
                    f64::from(v as f32)
                } else {
                    v
                })
            })
            .map_err(|_| {
                format!("scalars: java.lang.NumberFormatException: For input string: \"{s}\"")
            }),
        ("toDoubleOption" | "toFloatOption", 0) => Ok(s.trim().parse::<f64>().map_or_else(
            |_| make_none(),
            |v| {
                make_some(Value::float(if name == "toFloatOption" {
                    f64::from(v as f32)
                } else {
                    v
                }))
            },
        )),
        // `StringOps.toBoolean` is case-insensitive but NOT trimming, and its
        // failure is an `IllegalArgumentException` rather than the
        // `NumberFormatException` every other conversion here raises.
        ("toBoolean", 0) => {
            if s.eq_ignore_ascii_case("true") {
                Ok(Value::Bool(true))
            } else if s.eq_ignore_ascii_case("false") {
                Ok(Value::Bool(false))
            } else {
                Err(format!(
                    "scalars: java.lang.IllegalArgumentException: For input string: \"{s}\""
                ))
            }
        }
        ("charAt", 1) => {
            let i = args[0].to_int();
            let chars: Vec<char> = s.chars().collect();
            if i < 0 || i as usize >= chars.len() {
                // `java.lang.String.charAt`'s exact JDK message.
                Err(format!(
                    "scalars: java.lang.StringIndexOutOfBoundsException: Index {i} out of bounds for length {}",
                    chars.len()
                ))
            } else {
                Ok(make_char(chars[i as usize]))
            }
        }
        ("contains", 1) => Ok(Value::bool(s.contains(&*args[0].as_str_cow()))),
        ("startsWith", 1) => Ok(Value::bool(s.starts_with(&*args[0].as_str_cow()))),
        ("endsWith", 1) => Ok(Value::bool(s.ends_with(&*args[0].as_str_cow()))),
        ("substring", 1) => substring(s, args[0].to_int(), s.chars().count() as i64),
        ("substring", 2) => substring(s, args[0].to_int(), args[1].to_int()),
        // `indexOf`/`lastIndexOf` answer a CHAR index, so the byte offset
        // `str::find` returns has to be converted (`café`.indexOf("é") is 3).
        ("indexOf", 1) => Ok(Value::int(char_index(s, s.find(&*args[0].as_str_cow())))),
        ("indexOf", 2) => {
            let from = char_offset(s, args[1].to_int());
            Ok(Value::int(match s.get(from..) {
                Some(rest) => char_index(s, rest.find(&*args[0].as_str_cow()).map(|i| i + from)),
                None => -1,
            }))
        }
        ("lastIndexOf", 1) => Ok(Value::int(char_index(s, s.rfind(&*args[0].as_str_cow())))),
        ("replace" | "replaceAllLiterally", 2) => Ok(Value::str(
            s.replace(&*args[0].as_str_cow(), &args[1].as_str_cow()),
        )),
        ("stripPrefix", 1) => Ok(Value::str(
            s.strip_prefix(&*args[0].as_str_cow()).unwrap_or(s),
        )),
        // `"…".format(args)` — Java's `Formatter` over a whole format string.
        ("format", _) => Ok(Value::str(format_all(s, args, None)?)),
        // `x.formatted(spec)` is the mirror image: the RECEIVER is the value and
        // the argument is the format string.
        ("formatted", 1) => Ok(Value::str(format_all(
            &args[0].as_str_cow(),
            std::slice::from_ref(&Value::str(s.to_string())),
            None,
        )?)),
        ("stripSuffix", 1) => Ok(Value::str(
            s.strip_suffix(&*args[0].as_str_cow()).unwrap_or(s),
        )),
        ("capitalize", 0) => Ok(Value::str(match s.chars().next() {
            Some(c) => c.to_uppercase().collect::<String>() + &s[c.len_utf8()..],
            None => String::new(),
        })),
        // `String.compareTo` answers the char difference at the first differing
        // position, or the length difference — `java.lang.String`'s exact rule,
        // not a normalized -1/0/1.
        ("compareTo" | "compare", 1) => Ok(Value::int(str_compare(s, &args[0].as_str_cow()))),
        ("compareToIgnoreCase", 1) => Ok(Value::int(str_compare(
            &s.to_lowercase(),
            &args[0].as_str_cow().to_lowercase(),
        ))),
        ("equalsIgnoreCase", 1) => Ok(Value::bool(
            s.to_lowercase() == args[0].as_str_cow().to_lowercase(),
        )),
        ("*", 1) => Ok(Value::str(s.repeat(args[0].to_int().max(0) as usize))),
        ("take", 1) => Ok(Value::str(char_slice(s, 0, args[0].to_int()))),
        ("drop", 1) => Ok(Value::str(char_slice(
            s,
            args[0].to_int(),
            s.chars().count() as i64,
        ))),
        ("takeRight", 1) => {
            let n = s.chars().count() as i64;
            Ok(Value::str(char_slice(s, n - args[0].to_int(), n)))
        }
        ("dropRight", 1) => Ok(Value::str(char_slice(
            s,
            0,
            s.chars().count() as i64 - args[0].to_int(),
        ))),
        ("slice", 2) => Ok(Value::str(char_slice(
            s,
            args[0].to_int(),
            args[1].to_int(),
        ))),
        ("splitAt", 1) => {
            let n = args[0].to_int();
            Ok(heap_push(HeapVal::Tuple(vec![
                Value::str(char_slice(s, 0, n)),
                Value::str(char_slice(s, n, s.chars().count() as i64)),
            ])))
        }
        ("head", 0) => s.chars().next().map(make_char).ok_or_else(|| {
            "scalars: java.util.NoSuchElementException: head of empty String".to_string()
        }),
        ("last", 0) => s.chars().next_back().map(make_char).ok_or_else(|| {
            // `StringOps.last` reads through the iterator, so it is a
            // `NoSuchElementException` — NOT the `UnsupportedOperationException`
            // its `tail`/`init` siblings raise.
            "scalars: java.util.NoSuchElementException: last of empty String".to_string()
        }),
        ("headOption", 0) => Ok(match s.chars().next() {
            Some(c) => make_some(make_char(c)),
            None => make_none(),
        }),
        ("lastOption", 0) => Ok(match s.chars().next_back() {
            Some(c) => make_some(make_char(c)),
            None => make_none(),
        }),
        // `min`/`max` over the characters — by code point, answering a `Char`.
        ("min" | "max", 0) => {
            let pick = if name == "max" {
                s.chars().max()
            } else {
                s.chars().min()
            };
            pick.map(make_char).ok_or_else(|| {
                format!("scalars: java.lang.UnsupportedOperationException: empty.{name}")
            })
        }
        // Both raise on an empty receiver — `StringOps` inherits `SeqOps`'
        // checks, so `"".tail` is NOT `""`.
        ("init", 0) if s.is_empty() => Err(
            "scalars: java.lang.UnsupportedOperationException: init of empty String".to_string(),
        ),
        ("tail", 0) if s.is_empty() => Err(
            "scalars: java.lang.UnsupportedOperationException: tail of empty String".to_string(),
        ),
        ("init", 0) => Ok(Value::str(char_slice(s, 0, s.chars().count() as i64 - 1))),
        ("tail", 0) => Ok(Value::str(char_slice(s, 1, s.chars().count() as i64))),
        ("apply", 1) => string_method(s, "charAt", args),
        ("distinct", 0) => {
            let mut seen = String::new();
            for c in s.chars() {
                if !seen.contains(c) {
                    seen.push(c);
                }
            }
            Ok(Value::str(seen))
        }
        ("sorted", 0) => {
            let mut cs: Vec<char> = s.chars().collect();
            cs.sort_unstable();
            Ok(Value::str(cs.into_iter().collect::<String>()))
        }
        ("mkString", 0) => Ok(Value::str(s)),
        ("mkString", 1) => Ok(Value::str(
            s.chars()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(&args[0].as_str_cow()),
        )),
        ("mkString", 3) => Ok(Value::str(format!(
            "{}{}{}",
            args[0].as_str_cow(),
            s.chars()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(&args[1].as_str_cow()),
            args[2].as_str_cow()
        ))),
        // `StringOps.toIndexedSeq` is a `WrappedString`, which renders as the
        // string itself rather than as a sequence of `Char`s.
        ("toIndexedSeq", 0) => Ok(Value::str(s)),
        ("toCharArray", 0) => Ok(new_seq(SeqKind::Array, s.chars().map(make_char).collect())),
        // `StringOps.zipWithIndex` answers an `IndexedSeq`, printed `Vector(…)`.
        ("zipWithIndex", 0) => Ok(new_seq(
            SeqKind::Vector,
            s.chars()
                .enumerate()
                .map(|(i, c)| heap_push(HeapVal::Tuple(vec![make_char(c), Value::int(i as i64)])))
                .collect(),
        )),
        // `Char`'s predicates. A `Char` answers these from [`char_method`], which
        // wins before any `String` dispatch, so these arms are reached only by
        // calling one on an actual `String` — which Scala rejects outright
        // (`"ab".isDigit` does not compile). Keeping them is over-acceptance of
        // the same kind as the unchecked types, never a different answer.
        ("toUpper", 0) => Ok(Value::str(s.to_uppercase())),
        ("toLower", 0) => Ok(Value::str(s.to_lowercase())),
        ("isLetter", 0) => Ok(Value::bool(one_char(s).is_some_and(char::is_alphabetic))),
        ("isDigit", 0) => Ok(Value::bool(one_char(s).is_some_and(|c| c.is_ascii_digit()))),
        ("isLetterOrDigit", 0) => Ok(Value::bool(one_char(s).is_some_and(char::is_alphanumeric))),
        ("isUpper", 0) => Ok(Value::bool(one_char(s).is_some_and(char::is_uppercase))),
        ("isLower", 0) => Ok(Value::bool(one_char(s).is_some_and(char::is_lowercase))),
        ("isWhitespace", 0) => Ok(Value::bool(one_char(s).is_some_and(char::is_whitespace))),
        // A recognized name with the wrong arity is an arity error; an
        // unrecognized name is "no such method".
        (
            "length" | "size" | "isEmpty" | "nonEmpty" | "toUpperCase" | "toLowerCase" | "trim"
            | "reverse" | "toInt" | "toLong" | "toByte" | "toShort" | "toDouble" | "toBoolean"
            | "charAt" | "contains" | "startsWith" | "endsWith" | "substring",
            _,
        ) => Err(arity_err()),
        _ => Err(no_such_method(&Value::str(s), name)),
    }
}

// ── regular expressions (`java.util.regex` / `scala.util.matching`) ─────────

thread_local! {
    /// Compiled patterns, keyed by source. `String.matches` inside a loop
    /// recompiles nothing, matching the JDK's own `Pattern` caching in
    /// `String.matches`/`split` well enough to keep the cost off the hot path.
    static REGEX_CACHE: RefCell<HashMap<String, Arc<fancy_regex::Regex>>> =
        RefCell::new(HashMap::new());
}

/// Compile (or fetch) `pat`. An invalid pattern answers Java's
/// `PatternSyntaxException`, which a Scala program can catch.
fn regex_compile(pat: &str) -> Result<Arc<fancy_regex::Regex>, String> {
    REGEX_CACHE.with(|c| {
        if let Some(r) = c.borrow().get(pat) {
            return Ok(r.clone());
        }
        match fancy_regex::Regex::new(pat) {
            Ok(r) => {
                let r = Arc::new(r);
                c.borrow_mut().insert(pat.to_string(), r.clone());
                Ok(r)
            }
            Err(e) => Err(format!(
                "scalars: java.util.regex.PatternSyntaxException: {e}"
            )),
        }
    })
}

/// Clear the compiled-pattern cache. Called by [`reset_heap`] so a process
/// running several programs (the library `run_str` path) does not accumulate
/// every pattern every program ever compiled.
fn reset_regex_cache() {
    REGEX_CACHE.with(|c| c.borrow_mut().clear());
}

/// Every match of `re` in `s`, as `(start, end)` byte ranges, iterated by
/// `java.util.regex.Matcher.find`'s rule rather than the `regex` crate's.
///
/// The two differ after a non-empty match: Java resumes the search AT the
/// previous match's end, so an empty match is allowed there (`"xx9".split("x*")`
/// is `["", "", "9"]` because `x*` matches `xx`, then the empty string at index
/// 2), while Rust's iterator skips an empty match adjacent to the previous one.
/// Java only skips forward a character when the previous match was ITSELF empty,
/// which is what stops the scan spinning.
///
/// A failed step ends the scan (`fancy_regex`'s backtracking engine can hit a
/// step limit), which is the same observable answer as "no further match".
fn regex_matches(re: &fancy_regex::Regex, s: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos <= s.len() {
        let Ok(Some(m)) = re.find_from_pos(s, pos) else {
            break;
        };
        let (a, b) = (m.start(), m.end());
        out.push((a, b));
        pos = if a == b {
            // Empty match: step one whole character so the next search cannot
            // return the same position (and stays on a UTF-8 boundary).
            b + s[b..].chars().next().map_or(1, char::len_utf8)
        } else {
            b
        };
    }
    out
}

/// Expand a `java.util.regex.Matcher` replacement string against one match:
/// `$N` splices capture group `N` (Java takes the longest group number that
/// exists), `\x` is a literal `x`, and everything else is copied through. A
/// reference to a group the pattern does not have throws, as Java's does; a
/// group that exists but did not participate splices nothing.
fn expand_replacement(repl: &str, caps: &fancy_regex::Captures) -> Result<String, String> {
    let mut out = String::new();
    let mut it = repl.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\\' => {
                if let Some(esc) = it.next() {
                    out.push(esc);
                }
            }
            '$' if it.peek().is_some_and(char::is_ascii_digit) => {
                // Java grows the group number while the longer number is still a
                // real group, so `$12` is group 12 when it exists and group 1
                // followed by `2` when it does not.
                let mut n = 0usize;
                while let Some(d) = it.peek().and_then(|c| c.to_digit(10)) {
                    let wider = n * 10 + d as usize;
                    if n > 0 && wider >= caps.len() {
                        break;
                    }
                    n = wider;
                    it.next();
                }
                if n >= caps.len() {
                    return Err(format!(
                        "scalars: java.lang.IndexOutOfBoundsException: No group {n}"
                    ));
                }
                if let Some(g) = caps.get(n) {
                    out.push_str(g.as_str());
                }
            }
            // `${name}` — a named-group reference. Not modeled (`BUGS.md`), and
            // copying it through emitted the six characters `${d}` where Java
            // splices the group: `"a1b2".replaceAll("(?<d>[0-9])", "<${d}>")`
            // answered `a<${d}>b<${d}>` against Java's `a<1>b<2>`. A silent
            // wrong answer becomes the rejection the gap is documented as.
            '$' if it.peek() == Some(&'{') => {
                let name: String = it.by_ref().skip(1).take_while(|&c| c != '}').collect();
                return Err(format!(
                    "scalars: a named regex group (`${{{name}}}` in a replacement) is not \
                     modeled — use the group's number"
                ));
            }
            _ => out.push(c),
        }
    }
    Ok(out)
}

/// `Matcher.replaceAll` / `replaceFirst`: substitute `repl` (with its `$N` group
/// splices) for each match, or only the first.
fn regex_replace(s: &str, pat: &str, repl: &str, first_only: bool) -> Result<Value, String> {
    let re = regex_compile(pat)?;
    let mut out = String::new();
    let mut last = 0;
    for (start, end) in regex_matches(&re, s) {
        // Re-run the match at its own offset to recover the capture groups the
        // replacement may splice in.
        let caps = match re.captures_from_pos(s, start) {
            Ok(Some(c)) => c,
            _ => continue,
        };
        out.push_str(&s[last..start]);
        out.push_str(&expand_replacement(repl, &caps)?);
        last = end;
        if first_only {
            break;
        }
    }
    out.push_str(&s[last..]);
    Ok(Value::str(out))
}

/// `java.util.regex.Pattern.split` with the default limit of 0, ported: split on
/// every match, drop the empty leading substring a zero-width match at position
/// 0 would produce, and then drop every TRAILING empty substring.
///
/// `String.split` is regex-based in Java (and so in Scala), which is why
/// `"a.b".split(".")` answers an empty array rather than `[a, b]`.
fn java_split(s: &str, pat: &str) -> Result<Vec<String>, String> {
    let re = regex_compile(pat)?;
    let mut parts: Vec<String> = Vec::new();
    let mut index = 0;
    for (start, end) in regex_matches(&re, s) {
        if index == 0 && start == 0 && end == 0 {
            continue;
        }
        parts.push(s[index..start].to_string());
        index = end;
    }
    // No match consumed anything: the input is the single field.
    if index == 0 {
        return Ok(vec![s.to_string()]);
    }
    parts.push(s[index..].to_string());
    while parts.last().is_some_and(String::is_empty) {
        parts.pop();
    }
    Ok(parts)
}

/// Build the `Regex.Match` handle for the match of `re` starting at `start`.
fn make_match(re: &fancy_regex::Regex, s: &str, start: usize) -> Value {
    let Ok(Some(caps)) = re.captures_from_pos(s, start) else {
        return Value::Undef;
    };
    let matched: Arc<str> = Arc::from(caps.get(0).map_or("", |m| m.as_str()));
    let groups = (1..caps.len())
        .map(|i| caps.get(i).map(|g| Arc::from(g.as_str())))
        .collect();
    heap_push(HeapVal::Match { matched, groups })
}

/// The `scala.util.matching.Regex` / `Regex.Match` surface, dispatched off a
/// heap handle. `None` when `recv` is neither (so ordinary object dispatch
/// continues).
fn regex_method(recv: &Value, name: &str, args: &[Value]) -> Option<Result<Value, String>> {
    let Value::Obj(id) = recv else { return None };
    let held = HEAP.with(|h| match h.borrow().get(*id as usize) {
        Some(HeapVal::Regex(p)) => Some(Ok(p.clone())),
        Some(HeapVal::Match { matched, groups }) => Some(Err((matched.clone(), groups.clone()))),
        _ => None,
    })?;
    let pat = match held {
        Ok(p) => p,
        // A `Match`: its groups are 1-based, and group 0 is the whole match.
        Err((matched, groups)) => {
            return Some(match (name, args.len()) {
                ("matched" | "toString", 0) => Ok(Value::str(matched.to_string())),
                ("groupCount", 0) => Ok(Value::int(groups.len() as i64)),
                ("group", 1) => {
                    // `m.group("name")`. Named groups are not modeled (`BUGS.md`),
                    // and `to_int` on a `String` is 0 — which is group 0, the
                    // whole match. `"(?<y>[0-9]{4})-(?<m>[0-9]{2})".r` on
                    // `2026-08` therefore answered `2026-08` for BOTH
                    // `group("y")` and `group("m")` instead of `2026` and `08`.
                    // An honest rejection is what the documented gap says.
                    if let Value::Str(g) = &args[0] {
                        return Some(Err(format!(
                            "scalars: a named regex group (`group(\"{g}\")`) is not modeled — \
                             use the group's number"
                        )));
                    }
                    let i = args[0].to_int();
                    if i == 0 {
                        Ok(Value::str(matched.to_string()))
                    } else {
                        match usize::try_from(i).ok().and_then(|i| groups.get(i - 1)) {
                            Some(Some(g)) => Ok(Value::str(g.to_string())),
                            Some(None) => Ok(Value::Undef),
                            // `Regex.Match.group(i)` indexes its own start/end
                            // ARRAYS rather than calling `Matcher.group`, so an
                            // out-of-range group is the JVM's array fault — not
                            // the `IndexOutOfBoundsException: No group i` that
                            // `Matcher` (and hence `replaceAll`'s `$n`) raises.
                            // The array holds group 0 too, so its length is one
                            // more than the capture count.
                            None => Err(format!(
                                "scalars: java.lang.ArrayIndexOutOfBoundsException: \
                                 Index {i} out of bounds for length {}",
                                groups.len() + 1
                            )),
                        }
                    }
                }
                ("subgroups", 0) => Ok(new_list(
                    groups
                        .iter()
                        .map(|g| match g {
                            Some(t) => Value::str(t.to_string()),
                            None => Value::Undef,
                        })
                        .collect(),
                )),
                _ => Err(no_such_obj_member("Regex.Match", name)),
            });
        }
    };
    let re = match regex_compile(&pat) {
        Ok(r) => r,
        Err(e) => return Some(Err(e)),
    };
    let target = || args[0].as_str_cow().into_owned();
    Some(match (name, args.len()) {
        ("regex" | "toString", 0) => Ok(Value::str(pat.to_string())),
        // `Regex.matches` (2.13+) is the anchored test, like `String.matches`.
        ("matches", 1) => regex_full_match(&pat, &target()),
        ("findFirstIn", 1) => {
            let s = target();
            Ok(match regex_matches(&re, &s).first() {
                Some(&(a, b)) => make_some(Value::str(&s[a..b])),
                None => make_none(),
            })
        }
        ("findFirstMatchIn", 1) => {
            let s = target();
            Ok(match regex_matches(&re, &s).first() {
                Some(&(a, _)) => make_some(make_match(&re, &s, a)),
                None => make_none(),
            })
        }
        // `findAllIn`/`findAllMatchIn` answer a `MatchIterator` in Scala; as
        // everywhere else in this frontend an iterator is modeled STRICTLY (see
        // `BUGS.md`), so every downstream consumption matches and only printing
        // the un-consumed result differs.
        ("findAllIn", 1) => {
            let s = target();
            Ok(new_seq(
                SeqKind::Iterable,
                regex_matches(&re, &s)
                    .into_iter()
                    .map(|(a, b)| Value::str(&s[a..b]))
                    .collect(),
            ))
        }
        ("findAllMatchIn", 1) => {
            let s = target();
            Ok(new_seq(
                SeqKind::Iterable,
                regex_matches(&re, &s)
                    .into_iter()
                    .map(|(a, _)| make_match(&re, &s, a))
                    .collect(),
            ))
        }
        ("replaceAllIn", 2) => regex_replace(&target(), &pat, &args[1].as_str_cow(), false),
        ("replaceFirstIn", 2) => regex_replace(&target(), &pat, &args[1].as_str_cow(), true),
        ("split", 1) => java_split(&target(), &pat)
            .map(|parts| new_seq(SeqKind::Array, parts.into_iter().map(Value::str).collect())),
        ("unanchored", 0) => Ok(recv.clone()),
        _ => Err(no_such_obj_member("Regex", name)),
    })
}

/// `String.matches` / `Regex.matches` — a whole-input match. Java anchors the
/// pattern to the entire region rather than searching, so the source is wrapped
/// in `\A(?:…)\z` (a non-capturing group, so the user's own group numbers are
/// untouched).
fn regex_full_match(pat: &str, s: &str) -> Result<Value, String> {
    // Validate the user's own pattern first, so a syntax error reports the
    // pattern they wrote rather than the anchored rewrite.
    regex_compile(pat)?;
    let re = regex_compile(&format!("\\A(?:{pat})\\z"))?;
    Ok(Value::bool(re.is_match(s).unwrap_or(false)))
}

/// Scala/Java `String.substring(begin, end)` — a half-open `char` slice that
/// throws `StringIndexOutOfBoundsException` for an out-of-range or inverted range.
fn substring(s: &str, begin: i64, end: i64) -> Result<Value, String> {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len() as i64;
    if begin < 0 || end > len || begin > end {
        // `java.lang.String.substring`'s exact JDK message (a half-open range).
        return Err(format!(
            "scalars: java.lang.StringIndexOutOfBoundsException: Range [{begin}, {end}) out of bounds for length {len}"
        ));
    }
    Ok(Value::str(
        chars[begin as usize..end as usize]
            .iter()
            .collect::<String>(),
    ))
}

/// `StringOps`' combinators that take a function. Returns `None` when `name` is
/// not one of them, so the caller falls through to the pure [`string_method`].
///
/// The element type is [`HeapVal::Char`], so `map` decides its result type by
/// asking whether every result *is* a `Char`: if so it rebuilds a `String`
/// (Scala's `Char => Char` overload), otherwise it answers an `IndexedSeq`,
/// printed `ArraySeq(…)`. That test is exact, so `s.map(_.toString)` — whose
/// results are one-character `String`s, not `Char`s — now correctly answers a
/// sequence rather than a `String`.
fn string_fn_method(
    vm: &mut VM,
    s: &str,
    name: &str,
    args: &[Value],
) -> Option<Result<Value, String>> {
    let chars: Vec<Value> = s.chars().map(make_char).collect();
    macro_rules! call {
        ($x:expr) => {
            match invoke_closure(vm, &args[0], std::slice::from_ref($x)) {
                Ok(v) => v,
                Err(e) => return Some(Err(e)),
            }
        };
    }
    // Re-join a char run into the `String` the receiver's type implies.
    let join = |xs: &[Value]| Value::str(xs.iter().map(scala_str).collect::<String>());

    // `flatMap` picks its result type from the function's, not from the
    // elements': `Char => String` concatenates into a `String`
    // (`"abc".flatMap(c => c.toString * 2)` is `aabbcc`), while
    // `Char => IterableOnce[B]` builds an `IndexedSeq[B]` — so
    // `"abc".flatMap(c => List(c, c))` is a `Vector` of `Char`, NOT a `String`.
    if name == "flatMap" && args.len() == 1 {
        let mut parts = Vec::with_capacity(chars.len());
        for c in &chars {
            parts.push(call!(c));
        }
        let all_str = if parts.is_empty() {
            as_closure(&args[0]).is_some_and(|c| c.string_body)
        } else {
            parts.iter().all(|v| matches!(v, Value::Str(_)))
        };
        return Some(Ok(if all_str {
            join(&parts)
        } else {
            let mut out = Vec::new();
            for p in &parts {
                match as_seq_or_tuple(p) {
                    Some(items) => out.extend(items),
                    None => out.push(p.clone()),
                }
            }
            new_seq(SeqKind::Vector, out)
        }));
    }

    // The remaining `StringOps` combinators are the sequence ones over the
    // receiver's characters, so they run on a `Vector[Char]` rather than being
    // reimplemented here (the same delegation `Map` uses for its pair methods).
    //
    // Scala's builder then decides the result type. `sortWith`/`sortBy` only
    // REORDER, so their elements are still `Char`s and the result is always a
    // `String` (`"abc".sortWith(_ > _)` is `cba`, and `""` sorted is `""` — note
    // their function is a comparator/key, so its own result type says nothing
    // about the elements). `collect` is the one that can change the element type,
    // so it answers a `String` only when every result is still a `Char`
    // (`"abc".collect { … => c.toInt }` is `Vector(97, 99)`). The rest keep the
    // sequence they build even though their elements ARE `Char`s — `toVector`,
    // `zip` and `scanLeft` are `Vector`s, not `String`s.
    const ALWAYS_STRING: &[&str] = &["sortWith", "sortBy"];
    const REBUILDS_STRING: &[&str] = &["collect"];
    const PASSES_THROUGH: &[&str] = &[
        "foldLeft",
        "foldRight",
        "fold",
        "reduce",
        "reduceLeft",
        "reduceRight",
        "scanLeft",
        "maxBy",
        "minBy",
        "zip",
        "toVector",
        "toArray",
        "toSet",
        "collectFirst",
        "flatten",
        "lastIndexWhere",
        "segmentLength",
    ];
    if ALWAYS_STRING.contains(&name)
        || REBUILDS_STRING.contains(&name)
        || PASSES_THROUGH.contains(&name)
    {
        let seq = new_seq(SeqKind::Vector, chars.clone());
        // `zip`'s operand is itself iterated, so a `String` there is its
        // characters (`"abc".zip("xy")`). Everywhere else a `String` argument is
        // an ordinary value — `foldRight("")` seeds the fold with the empty
        // string — so this coercion is confined to the collection-taking ops.
        let zipped;
        let args = match (name, args.first()) {
            ("zip", Some(Value::Str(t))) => {
                zipped = [new_seq(SeqKind::Vector, t.chars().map(make_char).collect())];
                &zipped[..]
            }
            _ => args,
        };
        let out = match seq_method(vm, &seq, name, args) {
            Ok(v) => v,
            Err(e) => return Some(Err(e)),
        };
        if PASSES_THROUGH.contains(&name) {
            return Some(Ok(out));
        }
        let always = ALWAYS_STRING.contains(&name);
        // `Char`-valued result → back to a `String`; anything else keeps the
        // sequence the builder produced. An empty `collect` has nothing to read,
        // so it falls back to the body's static type.
        return Some(Ok(match as_seq_or_tuple(&out) {
            Some(items) if always => join(&items),
            Some(items) if items.is_empty() => {
                if args
                    .first()
                    .and_then(as_closure)
                    .is_some_and(|c| c.char_body)
                {
                    join(&items)
                } else {
                    out
                }
            }
            Some(items) if items.iter().all(|v| as_char(v).is_some()) => join(&items),
            _ => out,
        }));
    }

    // `groupBy` keys the characters by the function, and each group is itself a
    // `String` (`"aabbc".groupBy(identity)` maps `a` to `"aa"`).
    //
    // Unlike `List.groupBy` — which is a `HashMap` at every size — this one is
    // an ordinary immutable `Map`, so up to four groups it is an
    // insertion-ordered `Map1`..`Map4` and only beyond that a CHAMP `HashMap`.
    // Building the groups in first-appearance order and handing them to
    // [`new_map`] applies exactly that rule.
    if name == "groupBy" && args.len() == 1 {
        let mut entries: Vec<(Value, Vec<Value>)> = Vec::new();
        for c in &chars {
            let k = call!(c);
            match entries.iter_mut().find(|(ek, _)| value_eq(ek, &k)) {
                Some((_, group)) => group.push(c.clone()),
                None => entries.push((k, vec![c.clone()])),
            }
        }
        return Some(Ok(new_map(
            HashRep::Small,
            entries.into_iter().map(|(k, g)| (k, join(&g))).collect(),
        )));
    }

    if args.len() != 1 {
        return None;
    }
    Some(match name {
        // `StringOps.map` has two overloads: `Char => Char` rebuilds a `String`,
        // `Char => B` builds an `IndexedSeq[B]`. Scala reads that off the
        // function's static result type; since `Char` is its own runtime type
        // here, the results themselves answer it exactly.
        "map" => {
            let mut out = Vec::with_capacity(chars.len());
            for c in &chars {
                out.push(call!(c));
            }
            // With no results there is nothing to read the overload off of, so
            // an empty receiver falls back to the body's static type.
            let chars_out = if out.is_empty() {
                as_closure(&args[0]).is_some_and(|c| c.char_body)
            } else {
                out.iter().all(|v| as_char(v).is_some())
            };
            Ok(if chars_out {
                join(&out)
            } else {
                new_seq(SeqKind::ArraySeq, out)
            })
        }
        "filter" | "filterNot" | "withFilter" => {
            let keep = name != "filterNot";
            let mut out = Vec::new();
            for c in &chars {
                if truthy(&call!(c)) == keep {
                    out.push(c.clone());
                }
            }
            Ok(join(&out))
        }
        "takeWhile" | "dropWhile" => {
            let mut n = 0;
            while n < chars.len() && truthy(&call!(&chars[n])) {
                n += 1;
            }
            Ok(join(if name == "takeWhile" {
                &chars[..n]
            } else {
                &chars[n..]
            }))
        }
        "count" => {
            let mut n = 0i64;
            for c in &chars {
                n += i64::from(truthy(&call!(c)));
            }
            Ok(Value::int(n))
        }
        "exists" | "forall" => {
            let want = name == "exists";
            for c in &chars {
                if truthy(&call!(c)) == want {
                    return Some(Ok(Value::bool(want)));
                }
            }
            Ok(Value::bool(!want))
        }
        "foreach" => {
            for c in &chars {
                call!(c);
            }
            Ok(Value::Undef)
        }
        "find" => {
            for c in &chars {
                if truthy(&call!(c)) {
                    return Some(Ok(make_some(c.clone())));
                }
            }
            Ok(make_none())
        }
        "indexWhere" => {
            for (i, c) in chars.iter().enumerate() {
                if truthy(&call!(c)) {
                    return Some(Ok(Value::int(i as i64)));
                }
            }
            Ok(Value::int(-1))
        }
        // `span` stops testing at the FIRST failure and hands the whole
        // remainder to the second component; `partition` tests every element
        // and sorts it into one side or the other.
        "span" => {
            let mut n = 0;
            while n < chars.len() && truthy(&call!(&chars[n])) {
                n += 1;
            }
            Ok(heap_push(HeapVal::Tuple(vec![
                join(&chars[..n]),
                join(&chars[n..]),
            ])))
        }
        "partition" => {
            let (mut yes, mut no) = (Vec::new(), Vec::new());
            for c in &chars {
                if truthy(&call!(c)) {
                    yes.push(c.clone());
                } else {
                    no.push(c.clone());
                }
            }
            Ok(heap_push(HeapVal::Tuple(vec![join(&yes), join(&no)])))
        }
        _ => return None,
    })
}

/// The single `char` of a one-char string, if that is what `s` is. A `Char`
/// value is modeled as a one-char `String` (the lexer gives `'a'` that shape).
fn one_char(s: &str) -> Option<char> {
    let mut it = s.chars();
    it.next().filter(|_| it.next().is_none())
}

/// Convert a BYTE offset from `str::find`/`rfind` into the CHAR index Scala's
/// `indexOf` answers; `None` (not found) is `-1`.
fn char_index(s: &str, byte: Option<usize>) -> i64 {
    match byte {
        Some(b) => s[..b].chars().count() as i64,
        None => -1,
    }
}

/// The byte offset of char index `i`, clamped to the string's bounds.
fn char_offset(s: &str, i: i64) -> usize {
    if i <= 0 {
        return 0;
    }
    s.char_indices().nth(i as usize).map_or(s.len(), |(b, _)| b)
}

/// `s`'s chars in `[from, until)`, with both ends clamped — the total,
/// non-throwing slicing `StringOps.take`/`drop`/`slice` do (unlike `substring`,
/// which throws out of range).
fn char_slice(s: &str, from: i64, until: i64) -> String {
    let len = s.chars().count() as i64;
    let from = from.clamp(0, len) as usize;
    let until = until.clamp(0, len) as usize;
    if until <= from {
        return String::new();
    }
    s.chars().skip(from).take(until - from).collect()
}

/// `java.lang.String.compareTo`: the char difference at the first differing
/// position, else the length difference. Not normalized to -1/0/1.
fn str_compare(a: &str, b: &str) -> i64 {
    for (x, y) in a.chars().zip(b.chars()) {
        if x != y {
            return x as i64 - y as i64;
        }
    }
    a.chars().count() as i64 - b.chars().count() as i64
}

/// `Int` methods (a faithful subset of `scala.Int` / `RichInt`).
fn int_method(n: i64, name: &str, args: &[Value]) -> Result<Value, String> {
    match (name, args.len()) {
        ("abs", 0) => Ok(Value::int(n.wrapping_abs())),
        ("toDouble", 0) => Ok(Value::float(n as f64)),
        ("toFloat", 0) => Ok(Value::float(f64::from(n as f32))),
        ("toInt" | "toLong", 0) => Ok(Value::int(n)),
        // The narrowing conversions. `toInt` above is a no-op here because the
        // compiler already wraps an `Int`-typed result to 32 bits; these two have
        // no such lowering, so the truncation happens at the call.
        ("toByte", 0) => Ok(Value::int(to_byte(n))),
        ("toShort", 0) => Ok(Value::int(to_short(n))),
        // `('a' + 1).toChar` — the round trip back from code point to `Char`.
        ("toChar", 0) => Ok(make_char(char_of_code(n))),
        // `RichInt`'s radix renderings, which show the two's-complement BIT
        // PATTERN rather than a sign: `(-1).toHexString` is eight `f`s. The
        // receiver's width decides how many digits that is, and the value model
        // cannot tell an `Int` from a `Long`, so a `Long` receiver is dispatched
        // under the `#long` name the compiler picks from the static type — the
        // same mechanism the shift operators use.
        ("toHexString", 0) => Ok(Value::str(format!("{:x}", n as i32 as u32))),
        ("toBinaryString", 0) => Ok(Value::str(format!("{:b}", n as i32 as u32))),
        ("toOctalString", 0) => Ok(Value::str(format!("{:o}", n as i32 as u32))),
        ("toHexString#long", 0) => Ok(Value::str(format!("{:x}", n as u64))),
        ("toBinaryString#long", 0) => Ok(Value::str(format!("{:b}", n as u64))),
        ("toOctalString#long", 0) => Ok(Value::str(format!("{:o}", n as u64))),
        ("max", 1) => Ok(Value::int(n.max(args[0].to_int()))),
        ("min", 1) => Ok(Value::int(n.min(args[0].to_int()))),
        // `RichInt.signum` and `Integer.compare`, both answering an `Int`.
        ("signum", 0) => Ok(Value::int(n.signum())),
        ("compareTo" | "compare", 1) => Ok(Value::int(n.cmp(&args[0].to_int()) as i64)),
        // `Integer.hashCode` is the value and `Long.hashCode` folds the two
        // halves together. The value model cannot tell the two apart, and does
        // not need to: an `Int` always fits in 32 bits, where the fold is the
        // identity, so the `Long` rule answers both.
        ("hashCode", 0) => Ok(Value::int(i64::from(
            (n ^ ((n as u64) >> 32) as i64) as i32,
        ))),
        (
            "abs" | "toDouble" | "toFloat" | "toInt" | "toLong" | "toByte" | "toShort" | "max"
            | "min" | "signum" | "compareTo" | "compare" | "toHexString" | "toBinaryString"
            | "toOctalString",
            _,
        ) => Err(format!("scalars: Int.{name}: wrong number of arguments")),
        // The bitwise and shift operators. Scala evaluates these at `Int` width
        // — `1 << 33` is `2`, because the shift distance is masked to five bits
        // and the result wraps at 32 bits — so they are computed on `i32` and
        // widened back, unlike `+`/`*` (see the 64-bit note in `BUGS.md`).
        ("&", 1) => Ok(Value::int(n & args[0].to_int())),
        ("|", 1) => Ok(Value::int(n | args[0].to_int())),
        ("^", 1) => Ok(Value::int(n ^ args[0].to_int())),
        ("unary_~", 0) => Ok(Value::int(!(n as i32) as i64)),
        ("<<", 1) => Ok(Value::int(i64::from(
            (n as i32).wrapping_shl(args[0].to_int() as u32 & 31),
        ))),
        (">>", 1) => Ok(Value::int(i64::from(
            (n as i32).wrapping_shr(args[0].to_int() as u32 & 31),
        ))),
        (">>>", 1) => Ok(Value::int(i64::from(
            (n as u32).wrapping_shr(args[0].to_int() as u32 & 31) as i32,
        ))),
        // The same three at `Long` width, which the compiler selects by name
        // when it can prove the receiver is a `Long`: the distance masks to six
        // bits instead of five, and the result is not truncated to 32.
        ("<<#long", 1) => Ok(Value::int(n.wrapping_shl(args[0].to_int() as u32 & 63))),
        (">>#long", 1) => Ok(Value::int(n.wrapping_shr(args[0].to_int() as u32 & 63))),
        (">>>#long", 1) => Ok(Value::int(
            (n as u64).wrapping_shr(args[0].to_int() as u32 & 63) as i64,
        )),
        ("formatted", 1) => Ok(Value::str(format_all(
            &args[0].as_str_cow(),
            std::slice::from_ref(&Value::int(n)),
            None,
        )?)),
        _ => Err(no_such_method(&Value::int(n), name)),
    }
}

/// `Boolean`'s non-short-circuiting operators, which Scala spells with the
/// single-character names (`&`, `|`, `^`) alongside `&&`/`||`.
fn bool_method(b: bool, name: &str, args: &[Value]) -> Result<Value, String> {
    let rhs = || matches!(args.first(), Some(Value::Bool(true)));
    match (name, args.len()) {
        ("&", 1) => Ok(Value::bool(b & rhs())),
        ("|", 1) => Ok(Value::bool(b | rhs())),
        ("^", 1) => Ok(Value::bool(b ^ rhs())),
        ("unary_!", 0) => Ok(Value::bool(!b)),
        // `java.lang.Boolean.hashCode` — the two constants the JDK specifies.
        ("hashCode", 0) => Ok(Value::int(if b { 1231 } else { 1237 })),
        _ => Err(no_such_method(&Value::bool(b), name)),
    }
}

/// `Float` methods (a faithful subset of `scala.Float` / `RichFloat`).
///
/// Mostly `double_method`'s surface one width down, but the differences are the
/// point: a member that answers "the same number" answers a `Float`
/// (`abs`, `max`, `min`), `toFloat` is the identity where `toDouble` widens,
/// `round` answers an `Int` (the `Double` overload answers a `Long`), and
/// `hashCode` is the 32-bit pattern rather than the folded 64-bit one.
fn float_method(f: f32, name: &str, args: &[Value]) -> Result<Value, String> {
    let arg = || args.first().and_then(as_f32).unwrap_or(f32::NAN);
    match (name, args.len()) {
        ("abs", 0) => Ok(make_f32(f.abs())),
        ("toFloat", 0) => Ok(make_f32(f)),
        // Widening to `Double` is exact and is how the single-precision
        // rounding becomes visible: `0.1f.toDouble` is `0.10000000149011612`.
        ("toDouble", 0) => Ok(Value::float(f64::from(f))),
        // `f2i`/`f2l` saturate exactly as `d2i`/`d2l` do.
        ("toInt", 0) => Ok(Value::int(i64::from(f as i32))),
        ("toLong", 0) => Ok(Value::int(f as i64)),
        ("toByte", 0) => Ok(Value::int(to_byte(i64::from(f as i32)))),
        ("toShort", 0) => Ok(Value::int(to_short(i64::from(f as i32)))),
        ("toChar", 0) => Ok(make_char(char_of_code(f as i64))),
        ("isNaN", 0) => Ok(Value::bool(f.is_nan())),
        ("isInfinity" | "isInfinite", 0) => Ok(Value::bool(f.is_infinite())),
        // `Math.round(float)` answers an `int`, where the `double` overload
        // answers a `long` — the two disagree at the extremes, and only the
        // receiver's width says which one a call selected.
        ("round", 0) => Ok(Value::int(i64::from(round_f32(f)))),
        ("max", 1) => Ok(make_f32(java_float_max(f, arg()))),
        ("min", 1) => Ok(make_f32(java_float_min(f, arg()))),
        ("signum", 0) => Ok(Value::int(if f.is_nan() || f == 0.0 {
            0
        } else if f < 0.0 {
            -1
        } else {
            1
        })),
        ("compareTo" | "compare", 1) => Ok(Value::int(float_total_cmp(f, arg()) as i64)),
        ("hashCode", 0) => Ok(Value::int(i64::from(float_to_int_bits(f)))),
        ("toString", 0) => Ok(Value::str(format_float32(f))),
        (
            "abs" | "toInt" | "toLong" | "toByte" | "toShort" | "toDouble" | "toFloat" | "isNaN"
            | "isInfinity" | "isInfinite" | "round" | "max" | "min" | "signum" | "compareTo"
            | "compare",
            _,
        ) => Err(format!("scalars: Float.{name}: wrong number of arguments")),
        // `x.formatted(spec)` on `Any`. The numeric conversions receive the
        // WIDENED value, exactly as `String.format` does — Scala promotes a
        // `Float` argument to a `Double` for them, which is why `%.9f` of
        // `1.0f/3.0f` is `0.333333343`.
        ("formatted", 1) => Ok(Value::str(format_all(
            &args[0].as_str_cow(),
            std::slice::from_ref(&Value::float(f64::from(f))),
            None,
        )?)),
        _ => Err(no_such_method(&make_f32(f), name)),
    }
}

/// `Math.round(float)` — the `int` overload, which saturates at the `Int` range
/// rather than the `Long` one. `Math.round(1.0e20f)` is `Integer.MAX_VALUE`,
/// where the `double` overload's answer is `Long.MAX_VALUE`; that difference is
/// the reason the two overloads have to be told apart at all.
///
/// The JDK adds a half and takes the floor, which is not the same as rounding
/// to nearest: `0.49999997f + 0.5f` is exactly `1.0f` at single precision, so
/// `Math.round(0.49999997f)` is 1.
fn round_f32(f: f32) -> i32 {
    if f.is_nan() {
        return 0;
    }
    (f + 0.5f32).floor() as i32
}

/// `math.max` at 32-bit width: a `NaN` operand PROPAGATES rather than being
/// ignored, and `-0.0` is below `0.0`.
fn java_float_max(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() {
        return f32::NAN;
    }
    if a == 0.0 && b == 0.0 {
        return if a.is_sign_negative() { b } else { a };
    }
    if a > b {
        a
    } else {
        b
    }
}

/// The same for `math.min`.
fn java_float_min(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() {
        return f32::NAN;
    }
    if a == 0.0 && b == 0.0 {
        return if a.is_sign_negative() { a } else { b };
    }
    if a < b {
        a
    } else {
        b
    }
}

/// `java.lang.Float.compare` — the TOTAL order, so `NaN` is above everything
/// and `-0.0` below `0.0`. Read off the bit patterns the way the JDK does.
fn float_total_cmp(a: f32, b: f32) -> i32 {
    if a < b {
        return -1;
    }
    if a > b {
        return 1;
    }
    let (ab, bb) = (float_to_int_bits(a), float_to_int_bits(b));
    match ab.cmp(&bb) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// `Double` methods (a faithful subset of `scala.Double` / `RichDouble`).
fn double_method(f: f64, name: &str, args: &[Value]) -> Result<Value, String> {
    match (name, args.len()) {
        ("abs", 0) => Ok(Value::float(f.abs())),
        // `Double.toInt` is the JVM's `d2i`, which SATURATES rather than wraps:
        // NaN answers 0 and anything past the range clamps to `Int.MaxValue` /
        // `Int.MinValue`, so `3000000000.0.toInt` is 2147483647, not a truncated
        // bit pattern. Rust's `as` cast has had exactly these semantics since
        // 1.45, at both widths, so `d2l` is the same cast one size up.
        ("toInt", 0) => Ok(Value::int(i64::from(f as i32))),
        ("toLong", 0) => Ok(Value::int(f as i64)),
        // The JVM has no `d2b`/`d2s`: `d.toByte` is `d2i` followed by `i2b`, so
        // the SATURATING cast happens first and the truncation second. That is
        // why `1e20.toByte` is -1 (Int.MaxValue's low byte) rather than 0.
        ("toByte", 0) => Ok(Value::int(to_byte(i64::from(f as i32)))),
        ("toShort", 0) => Ok(Value::int(to_short(i64::from(f as i32)))),
        ("toChar", 0) => Ok(make_char(char_of_code(f as i64))),
        ("toDouble", 0) => Ok(Value::float(f)),
        ("toFloat", 0) => Ok(Value::float(f64::from(f as f32))),
        ("isNaN", 0) => Ok(Value::bool(f.is_nan())),
        ("isInfinity" | "isInfinite", 0) => Ok(Value::bool(f.is_infinite())),
        ("round", 0) => Ok(Value::int(java_round(f))),
        // `RichDouble`'s `max`/`min` are `math.max`/`math.min`, so they PROPAGATE
        // a NaN operand rather than ignoring it.
        ("max", 1) => Ok(Value::float(java_double_max(f, num_f64(&args[0])))),
        ("min", 1) => Ok(Value::float(java_double_min(f, num_f64(&args[0])))),
        // `RichDouble.signum` answers an `Int`, and `-0.0`'s is `0`.
        ("signum", 0) => Ok(Value::int(if f.is_nan() || f == 0.0 {
            0
        } else if f < 0.0 {
            -1
        } else {
            1
        })),
        // `java.lang.Double.compare` — the total order, so `NaN` is above
        // everything and `-0.0` is below `0.0`.
        ("compareTo" | "compare", 1) => {
            Ok(Value::int(double_total_cmp(f, num_f64(&args[0])) as i64))
        }
        (
            "abs" | "toInt" | "toLong" | "toByte" | "toShort" | "toDouble" | "toFloat" | "isNaN"
            | "isInfinity" | "isInfinite" | "round" | "max" | "min" | "signum" | "compareTo"
            | "compare",
            _,
        ) => Err(format!("scalars: Double.{name}: wrong number of arguments")),
        // `java.lang.Double.hashCode` — the two halves of the bit pattern folded
        // together. A `Float` receiver does NOT come here: its hash is the
        // 32-bit pattern, which is a different number for the same value
        // (`3.0f` is 1077936128 and `3.0` is 1074266112), so the compiler routes
        // a statically-`Float` receiver to [`SF32_HASH`] instead.
        ("hashCode", 0) => {
            let bits = double_to_long_bits(f);
            Ok(Value::int(i64::from((bits ^ (bits >> 32)) as i32)))
        }
        // `x.formatted(spec)` on `Any` — the argument is the format string.
        ("formatted", 1) => Ok(Value::str(format_all(
            &args[0].as_str_cow(),
            std::slice::from_ref(&Value::float(f)),
            None,
        )?)),
        _ => Err(no_such_method(&Value::float(f), name)),
    }
}

/// `java.lang.Double.doubleToLongBits` — the raw bits, with every `NaN`
/// collapsed to the one canonical pattern the JVM reports.
fn double_to_long_bits(f: f64) -> i64 {
    if f.is_nan() {
        return 0x7ff8_0000_0000_0000u64 as i64;
    }
    f.to_bits() as i64
}

/// `java.lang.Float.floatToIntBits` — the same canonicalization one width down.
fn float_to_int_bits(f: f32) -> i32 {
    if f.is_nan() {
        return 0x7fc0_0000u32 as i32;
    }
    f.to_bits() as i32
}

/// The Scala compile-error a bad member access resembles (a `value … is not a
/// member of …`). slice-1 resolves methods at runtime, so it surfaces here.
fn no_such_method(recv: &Value, name: &str) -> String {
    let ty = match recv {
        Value::Str(_) => "String",
        Value::Int(_) => "Int",
        Value::Float(_) => "Double",
        Value::Status(_) => "Float",
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
/// (`7 / 2 == 3`) and only floats when an operand is a `Double`. Because scalars
/// carries no static types, the choice is made at runtime here: both `Int` →
/// truncating integer division (toward zero, like Scala/Java); otherwise a
/// double divide (so `7 / 2.0 == 3.5`, `1.0 / 0.0 == Infinity`).
///
/// Integer division by zero throws `java.lang.ArithmeticException: / by zero` in
/// Scala (a JVM `idiv`/`irem` trap). It is catchable; an *uncaught* one halts
/// the VM with that exact message parked for the runner
/// (surfaced as `scalars: java.lang.ArithmeticException: / by zero`), matching an
/// uncaught exception aborting `scala`. A `wrapping_div` avoids the
/// `i64::MIN / -1` overflow panic. Floating-point `/ 0.0` is NOT an error in
/// Scala/IEEE-754 — it yields `Infinity`/`NaN` — so it stays on the float path.
fn b_div(vm: &mut VM, _argc: u8) -> Value {
    let b = vm.stack.pop().unwrap_or(Value::Undef);
    let a = vm.stack.pop().unwrap_or(Value::Undef);
    // Suppressed while unwinding, so a `0` left by a raise cannot masquerade as
    // a second division-by-zero and displace the real exception.
    if unwinding() {
        return Value::Undef;
    }
    // A `Char` divides as its code point, so `'a' / 2` is the `Int` 48 — integer
    // division, not the float fallback below.
    let (a, b) = match (num_of(&a), num_of(&b)) {
        (Some(x), Some(y)) if as_char(&a).is_some() || as_char(&b).is_some() => {
            (Value::int(x), Value::int(y))
        }
        _ => (a, b),
    };
    match (&a, &b) {
        (Value::Int(x), Value::Int(y)) => {
            if *y == 0 {
                fault(vm, "java.lang.ArithmeticException: / by zero")
            } else {
                Value::int(x.wrapping_div(*y))
            }
        }
        // A `Float` on either side and no `Double` opposite it divides at 32
        // bits, exactly as `SF32_ARITH` does for the statically-typed path.
        // This is the same operation reached without the static type — from a
        // lambda parameter, an erased element — and it must answer the same
        // number: `1.0f / 3.0f` is `0.33333334`, not `0.3333333333333333`.
        _ if f32_pair(&a, &b) => {
            make_f32(as_f32(&a).unwrap_or(f32::NAN) / as_f32(&b).unwrap_or(f32::NAN))
        }
        _ => Value::float(num_f64(&a) / num_f64(&b)),
    }
}

/// Scala `%`, which shares `/`'s zero-divisor trap.
///
/// The JVM's `irem` raises the SAME `java.lang.ArithmeticException: / by zero`
/// as `idiv` — the message names `/` even though the operator was `%`. Verified
/// against Scala 3.8.4 on JDK 26.0.2: `1 % 0` answers
/// `java.lang.ArithmeticException: / by zero`. fusevm's native `Op::Mod` has no
/// such trap and quietly answers `0`, so `%` routes here exactly as `/` routes
/// through [`b_div`]. Float `%` is IEEE remainder-toward-zero and never throws,
/// so it stays on the float path.
fn b_mod(vm: &mut VM, _argc: u8) -> Value {
    let b = vm.stack.pop().unwrap_or(Value::Undef);
    let a = vm.stack.pop().unwrap_or(Value::Undef);
    if unwinding() {
        return Value::Undef;
    }
    // A `Char` takes its remainder as a code point, matching `b_div`.
    let (a, b) = match (num_of(&a), num_of(&b)) {
        (Some(x), Some(y)) if as_char(&a).is_some() || as_char(&b).is_some() => {
            (Value::int(x), Value::int(y))
        }
        _ => (a, b),
    };
    match (&a, &b) {
        (Value::Int(x), Value::Int(y)) => {
            if *y == 0 {
                fault(vm, "java.lang.ArithmeticException: / by zero")
            } else {
                // `Int.MinValue % -1` is 0 on the JVM; `wrapping_rem` agrees
                // where the plain operator would panic on overflow.
                Value::int(x.wrapping_rem(*y))
            }
        }
        _ if f32_pair(&a, &b) => {
            make_f32(as_f32(&a).unwrap_or(f32::NAN) % as_f32(&b).unwrap_or(f32::NAN))
        }
        _ => Value::float(num_f64(&a) % num_f64(&b)),
    }
}

/// Predef `println` builtin: pop `argc` values (0 or 1), print them
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
    // Suppressed while unwinding: a `println` between a raise and its `catch`
    // must not reach the terminal, and its argument is garbage anyway. The pops
    // above already ran, so the stack is balanced (see `Exception unwinding`).
    if unwinding() {
        return Value::Undef;
    }
    // Rendered BEFORE the lock is taken: a user `toString` may itself `println`,
    // and holding stdout across that would deadlock.
    let rendered: Vec<String> = vals.iter().map(|v| scala_str_vm(vm, v)).collect();
    // A user `toString` that raised leaves the exception in flight, and Scala
    // never reaches the `println` it was being rendered for. Re-checked here
    // because the check above ran before any of that code did.
    if unwinding() {
        return Value::Undef;
    }
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    for s in &rendered {
        let _ = write!(lock, "{s}");
    }
    if newline {
        let _ = writeln!(lock);
    }
    // `println`/`print` return `Unit`. In statement position a trailing `Pop`
    // discards this, but it is observable through a `Unit`-returning `def` whose
    // body is a `println` — `println(side())` prints `()`.
    unit_value()
}

/// Render a value with Scala's `String.valueOf`/`println` rules (as opposed to
/// fusevm's shell-flavoured `as_str_cow`): booleans as `true`/`false`, whole
/// doubles with a trailing `.0`, `Undef` (a `null` literal) as `null`.
pub fn scala_str(v: &Value) -> String {
    match v {
        Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        Value::Float(f) => format_double(*f),
        // A `Float` renders at its OWN precision, wherever it is reached from —
        // which is the whole point of it being a distinct runtime value:
        // `println(List(0.1f))` is `List(0.1)` because the element says so, not
        // because the compiler told the call site.
        Value::Status(_) => f32_of(v).map(format_float32).unwrap_or_default(),
        Value::Undef => "null".to_string(),
        // The only arrays this frontend produces are `for … yield` results, whose
        // static type over a range generator is `IndexedSeq`/`Vector`; render them
        // as Scala's `Vector(e0, e1, …)` (elements via `toString`, unquoted).
        Value::Array(items) => {
            let inner = items.iter().map(scala_str).collect::<Vec<_>>().join(", ");
            format!("Vector({inner})")
        }
        // A host-heap instance: `Class(f0,f1)` for a `case` type, `Class@hex`
        // for a plain class (see [`obj_to_string`]).
        Value::Obj(_) => obj_to_string(v),
        other => other.as_str_cow().into_owned(),
    }
}

thread_local! {
    /// Whether the running program defines any `toString` of its own, resolved
    /// once per chunk. `None` until asked; cleared by [`reset_heap`].
    static USER_TOSTRING: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Whether the compiled program defines a `toString` on any of its types.
///
/// `compiler::Compiler::sub_name` registers a class method as `Class$toString`,
/// so one scan of the chunk's name table answers it for the whole run. This is
/// what keeps [`scala_str_vm`] free: a program with no override takes exactly
/// the [`scala_str`] path it always did, with no per-value method lookup and no
/// heap snapshotting.
fn user_tostring_present(vm: &VM) -> bool {
    USER_TOSTRING.with(|c| match c.get() {
        Some(known) => known,
        None => {
            let found = vm.chunk.names.iter().any(|n| n.ends_with("$toString"));
            c.set(Some(found));
            found
        }
    })
}

/// [`scala_str`], able to run a user `toString` override.
///
/// Scala renders every value through its `toString`, so an override has to be
/// honoured by `println`, by interpolation, and at every depth of a collection —
/// not only by an explicit `p.toString`, which the compiler resolves statically
/// and never routes here. Running one means re-entering the VM, so this is the
/// entry point for the call sites that hold a `&mut VM`; [`scala_str`] stays as
/// the rendering for those that do not (see its callers).
pub fn scala_str_vm(vm: &mut VM, v: &Value) -> String {
    if !user_tostring_present(vm) {
        return scala_str(v);
    }
    match v {
        Value::Array(items) => {
            let items = items.clone();
            let inner = join_vm(vm, &items, ", ");
            format!("Vector({inner})")
        }
        Value::Obj(_) => obj_to_string_vm(vm, v),
        other => scala_str(other),
    }
}

/// `sep`-join the vm-aware rendering of each of `items`.
fn join_vm(vm: &mut VM, items: &[Value], sep: &str) -> String {
    let parts: Vec<String> = items.iter().map(|e| scala_str_vm(vm, e)).collect();
    parts.join(sep)
}

/// The parts of a heap value that have to be re-rendered element by element,
/// snapshotted so the heap borrow can be released before recursing.
enum Renderable {
    /// A `case` instance: its class name and its constructor fields.
    Case(Arc<str>, Vec<Value>),
    /// A sequence with a label — `List(…)`, `Vector(…)`, `Set(…)`.
    Seq(&'static str, Vec<Value>),
    /// A map with a label, as `k -> v` pairs.
    Map(&'static str, Vec<(Value, Value)>),
    /// A tuple: `(a,b)`, no spaces.
    Tuple(Vec<Value>),
    /// A `by-name`/lazy cell, rendered as its contents.
    Cell(Value),
    /// Everything whose rendering contains no nested value — a plain
    /// `Class@hex`, a `case object`, a `Range`, a closure, a throwable, a
    /// `Regex`, a `StringBuilder`. [`obj_to_string`] already answers these and
    /// no override can be reached through them.
    Leaf,
}

/// [`obj_to_string`], able to run a user `toString` override.
///
/// Two things make this a separate function rather than a flag on the original.
/// A user override runs USER BYTECODE, which needs the VM. And `obj_to_string`
/// holds `HEAP.borrow()` across its whole body, recursing into `scala_str` with
/// it live — re-entering the VM there would panic the moment the override read
/// one of its own fields, since that takes the heap again. So the shape is
/// snapshotted, the borrow dropped, and only then is anything run.
fn obj_to_string_vm(vm: &mut VM, v: &Value) -> String {
    // The override wins outright: Scala's derived `case class` rendering is
    // itself just a `toString`, and overriding it replaces it.
    if let Some(r) = call_user_method(vm, v, "toString", &[]) {
        match r {
            // A `toString` that answers a non-`String` cannot be re-entered
            // again — render its result structurally and stop.
            Ok(s) => return scala_str(&s),
            // A raise inside `toString` leaves the exception pending for the
            // statement boundary to pick up; the value still has to render.
            Err(_) => return obj_to_string(v),
        }
    }
    let id = if let Value::Obj(i) = v { *i } else { 0 };
    let shape = HEAP.with(|h| {
        let h = h.borrow();
        match h.get(id as usize) {
            Some(HeapVal::Record(o)) if o.is_case && !o.is_object && &*o.class != CLASS_CLASS => {
                let n = ctor_arity(&o.class, o.fields.len());
                let fields = o.fields[..n].iter().map(|(_, val)| val.clone()).collect();
                Renderable::Case(Arc::clone(&o.class), fields)
            }
            // A `Range` renders from its bounds and a `StringBuilder` from its
            // characters; neither can hold a record.
            Some(HeapVal::Seq(SeqKind::Range { .. } | SeqKind::StrBuf, _)) => Renderable::Leaf,
            Some(HeapVal::Seq(kind, items)) => Renderable::Seq(kind.label(), items.clone()),
            Some(HeapVal::Map(rep, entries)) => {
                let label = match rep {
                    HashRep::Hashed | HashRep::Mutable(_) => "HashMap",
                    HashRep::Linked => "LinkedHashMap",
                    HashRep::Small => "Map",
                };
                Renderable::Map(label, entries.clone())
            }
            Some(HeapVal::Tuple(items)) => Renderable::Tuple(items.clone()),
            Some(HeapVal::Cell(c)) => Renderable::Cell(c.clone()),
            _ => Renderable::Leaf,
        }
    });
    match shape {
        Renderable::Leaf => obj_to_string(v),
        Renderable::Case(class, fields) => format!("{class}({})", join_vm(vm, &fields, ",")),
        Renderable::Seq(label, items) => format!("{label}({})", join_vm(vm, &items, ", ")),
        Renderable::Map(label, entries) => {
            let parts: Vec<String> = entries
                .iter()
                .map(|(k, val)| {
                    let k = scala_str_vm(vm, k);
                    format!("{k} -> {}", scala_str_vm(vm, val))
                })
                .collect();
            format!("{label}({})", parts.join(", "))
        }
        Renderable::Tuple(items) => format!("({})", join_vm(vm, &items, ",")),
        Renderable::Cell(c) => scala_str_vm(vm, &c),
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
///
/// Java picks a decimal VALUE first ([`java_decimal`]) and only then lays it out,
/// so both notations render the same chosen digits.
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
    let (digits, exp) = java_decimal(f.abs());
    let body = if (-3..=6).contains(&exp) {
        render_plain(&digits, exp)
    } else {
        render_scientific(&digits, exp)
    };
    if neg {
        format!("-{body}")
    } else {
        body
    }
}

// ── scala.Float ─────────────────────────────────────────────────────────────
//
// A Scala `Float` is a type of its own, and the whole difficulty is that
// `fusevm::Value` has no variant for one: `Float(f64)` is the `Double`, and a
// `Float` sharing it is indistinguishable from the `Double` holding the same
// bits. That is what made `println(0.1f)` right (the compiler knew the static
// type and rendered it) while `println(List(0.1f))` was wrong (the container
// renders its elements at run time, from values that no longer carried one).
//
// The representation is `Value::Status`, whose payload is exactly the 32 bits
// an `f32` needs. Three properties fall out of that choice, and together they
// are what make a real `Float` affordable:
//
// - **No allocation and no interning.** The payload IS the float, so a `Float`
//   costs a `Value` and nothing else. Boxing one on the object arena was the
//   alternative, and this runtime's arena never frees — a `Float` accumulation
//   loop would have grown the heap by one entry per iteration.
// - **Disjoint from `Double`.** `Value::Status` is not `Value::Float`, so no
//   `Double` can be mistaken for a `Float`; `0.1f == 0.1` is `false`, the two
//   print differently, and `getClass` answers `float` versus `double` from the
//   value alone.
// - **Rejected, not coerced, by native arithmetic.** fusevm's `is_native_num`
//   admits only `Int` and `Float`, so an operand of this shape sends `Op::Add`
//   and every comparison to the [`numeric_hook`] instead of silently reading
//   the bit pattern as a number — which is where the single-precision rules
//   are applied. It also keeps a `Float` out of the JIT's typed registers.
//
// `scala.Float` is otherwise an ordinary type here: the static [`NumTy`]
// analysis in the compiler still drives literals and the arithmetic builtins,
// but it is no longer what makes rendering correct.

/// The `Value` carrying the `Float` `f` — its IEEE-754 bit pattern.
///
/// `to_bits` canonicalizes nothing, so a `NaN` keeps whichever payload the
/// operation produced; every place that observes one (`float_to_int_bits`,
/// `format_float32`) collapses it the way the JVM does.
pub fn make_f32(f: f32) -> Value {
    Value::Status(f.to_bits() as i32)
}

/// `Some(the float)` when `v` is a `Float`.
pub fn f32_of(v: &Value) -> Option<f32> {
    match v {
        Value::Status(bits) => Some(f32::from_bits(*bits as u32)),
        _ => None,
    }
}

/// Whether an operand pair is `Float` arithmetic — at least one `Float` and no
/// `Double` opposite it, which is Scala's promotion rule (`Float` sits above
/// both integer widths and below `Double`).
fn f32_pair(a: &Value, b: &Value) -> bool {
    (is_f32(a) || is_f32(b)) && !matches!(a, Value::Float(_)) && !matches!(b, Value::Float(_))
}

/// Whether `v` is a `Float`.
fn is_f32(v: &Value) -> bool {
    matches!(v, Value::Status(_))
}

/// `v` as an `f32` when it is a number of any width — a `Float` exactly, an
/// `Int`/`Char`/`Double` narrowed. `None` for everything that is not numeric.
fn as_f32(v: &Value) -> Option<f32> {
    f32_of(v).or_else(|| float_of(v).map(|d| d as f32))
}

/// [`SF32`] — round one value to 32-bit `Float` precision.
///
/// A non-numeric value passes through untouched, so the builtin is safe on a
/// statically-`Float` expression whose value turned out to be `null` or a
/// `Char`: the narrowing is about precision, not about coercing a type.
fn b_f32(vm: &mut VM, _argc: u8) -> Value {
    let v = vm.stack.pop().unwrap_or(Value::Undef);
    conv_elementwise(v, |x| as_f32(x).map(make_f32))
}

/// Apply a width conversion to a value, or ELEMENT-WISE to a collection's
/// contents when it is one.
///
/// The compiler emits a conversion for a declared collection type as readily as
/// for a scalar (`val xs: List[Double] = List(0.1f)`), and the elements are
/// what carry the width there. A value the conversion does not apply to — and
/// an element of one — passes through unchanged.
fn conv_elementwise(v: Value, f: impl Fn(&Value) -> Option<Value> + Copy) -> Value {
    if let Some(out) = f(&v) {
        return out;
    }
    let Some((kind, items)) = seq_kind_items(&v) else {
        return v;
    };
    // Rebuilt rather than mutated in place: the receiver is a value here, and a
    // conversion is not a mutation of the collection it was read from.
    let converted: Vec<Value> = items
        .iter()
        .map(|x| f(x).unwrap_or_else(|| x.clone()))
        .collect();
    heap_push(HeapVal::Seq(kind, converted))
}

/// [`SF32_ARITH`] — one arithmetic operation at 32-bit `Float` width.
///
/// Both operands are narrowed to `f32` before the operation and the result is
/// widened back to the one `Value::Float` representation, so the rounding
/// happens exactly once, where Scala's does.
fn b_f32_arith(vm: &mut VM, _argc: u8) -> Value {
    let code = vm.stack.pop().unwrap_or(Value::Undef);
    let b = vm.stack.pop().unwrap_or(Value::Undef);
    let a = vm.stack.pop().unwrap_or(Value::Undef);
    // Suppressed while unwinding, exactly as `b_div` is: the operands are the
    // `Undef` a raise left behind, and answering `NaN` for them would displace
    // the real exception's value with a plausible number.
    if unwinding() {
        return Value::Undef;
    }
    let (Some(x), Some(y)) = (as_f32(&a), as_f32(&b)) else {
        // Not a numeric pair after all — the compiler's static type was wrong
        // about one side (an erased element, a `null`). Fall back to the
        // untyped `+`, which is what this expression would have emitted
        // without the `Float` analysis.
        return match numeric_hook(NumOp::Add, &a, &b) {
            Ok(v) => v,
            Err(e) => fault(vm, &e),
        };
    };
    let r = match code.to_int() {
        f32_op::SUB => x - y,
        f32_op::MUL => x * y,
        f32_op::DIV => x / y,
        f32_op::REM => x % y,
        _ => x + y,
    };
    make_f32(r)
}

/// Read a boxed `var`'s cell directly (the value half of [`CELL_GET`]).
fn cell_get(cell: &Value) -> Value {
    let Value::Obj(id) = cell else {
        return Value::Undef;
    };
    HEAP.with(|h| match h.borrow().get(*id as usize) {
        Some(HeapVal::Cell(v)) => v.clone(),
        _ => Value::Undef,
    })
}

/// Write a boxed `var`'s cell directly (the value half of [`CELL_SET`]).
fn cell_set(cell: &Value, v: Value) {
    if let Value::Obj(id) = cell {
        HEAP.with(|h| {
            if let Some(HeapVal::Cell(slot)) = h.borrow_mut().get_mut(*id as usize) {
                *slot = v;
            }
        });
    }
}

/// [`LAZYLIST_NEW`] — build a `LazyList` from a factory tag and its arguments.
fn b_lazylist_new(vm: &mut VM, argc: u8) -> Value {
    let mut args = Vec::with_capacity(argc as usize);
    for _ in 0..argc {
        args.push(vm.stack.pop().unwrap_or(Value::Undef));
    }
    args.reverse();
    let Some(tag) = args.pop() else {
        return new_lazy(Vec::new(), LazySrc::End);
    };
    match &*tag.as_str_cow() {
        // `LazyList.from(n)` — the integers upward.
        "from" => new_lazy(
            Vec::new(),
            LazySrc::Ints {
                next: args.first().map(Value::to_int).unwrap_or(0),
            },
        ),
        // `LazyList.iterate(seed)(f)`.
        "iterate" => new_lazy(
            Vec::new(),
            LazySrc::Iter {
                next: args.first().cloned().unwrap_or(Value::Undef),
                f: args.get(1).cloned().unwrap_or(Value::Undef),
            },
        ),
        "continually" => new_lazy(
            Vec::new(),
            LazySrc::Rep {
                v: args.first().cloned().unwrap_or(Value::Undef),
            },
        ),
        // `LazyList(1, 2, 3)` and `LazyList.empty`. The elements are known,
        // but a fresh literal is still UNFORCED — the reference prints
        // `LazyList(<not computed>)` for one — so they are a rule rather than
        // a prefix.
        _ => new_lazy(Vec::new(), LazySrc::Elems { items: args, at: 0 }),
    }
}

/// [`LAZY_CONS`] — `head #:: tail`, the tail left as a thunk.
fn b_lazy_cons(vm: &mut VM, _argc: u8) -> Value {
    let thunk = vm.stack.pop().unwrap_or(Value::Undef);
    let head = vm.stack.pop().unwrap_or(Value::Undef);
    new_lazy(vec![head], LazySrc::Thunk { thunk, base: 1 })
}

/// `LazyList`'s method surface.
///
/// Every member forces the minimum prefix it needs: `take(n).toList` forces
/// `n`, `head` forces one, and `map`/`filter`/`zip` force NOTHING — they build
/// a new rule over this one, which is what keeps them usable on an infinite
/// source.
fn lazy_method(
    vm: &mut VM,
    recv: &Value,
    name: &str,
    args: &[Value],
) -> Option<Result<Value, String>> {
    // The lazy combinators, answered before anything forces.
    match (name, args.len()) {
        ("map", 1) => {
            return Some(Ok(new_lazy(
                Vec::new(),
                LazySrc::Map {
                    src: recv.clone(),
                    f: args[0].clone(),
                },
            )))
        }
        ("filter" | "withFilter", 1) => {
            return Some(Ok(new_lazy(
                Vec::new(),
                LazySrc::Filter {
                    src: recv.clone(),
                    p: args[0].clone(),
                    at: 0,
                },
            )))
        }
        ("zip", 1) => {
            return Some(Ok(new_lazy(
                Vec::new(),
                LazySrc::Zip {
                    a: recv.clone(),
                    b: args[0].clone(),
                },
            )))
        }
        // `tail` is the same list one element along, and is itself lazy.
        ("tail", 0) => {
            return Some(Ok(new_lazy(
                Vec::new(),
                LazySrc::Drop {
                    src: recv.clone(),
                    n: 1,
                },
            )))
        }
        ("drop", 1) => {
            let n = args[0].to_int().max(0) as usize;
            return Some(Ok(new_lazy(
                Vec::new(),
                LazySrc::Drop {
                    src: recv.clone(),
                    n,
                },
            )));
        }
        _ => {}
    }
    // The rest force, each only as far as it must.
    let r = (|| -> Result<Option<Value>, String> {
        Ok(match (name, args.len()) {
            ("take", 1) => {
                let n = args[0].to_int().max(0) as usize;
                let got = lazy_force(vm, recv, n.saturating_sub(1))?;
                let head: Vec<Value> = got.into_iter().take(n).collect();
                Some(new_lazy(head, LazySrc::End))
            }
            ("head", 0) => match lazy_force(vm, recv, 0)?.first() {
                Some(v) => Some(v.clone()),
                None => {
                    return Err(
                        "scalars: java.util.NoSuchElementException: head of empty lazy list"
                            .to_string(),
                    )
                }
            },
            ("headOption", 0) => Some(opt(lazy_force(vm, recv, 0)?.first().cloned())),
            ("isEmpty", 0) => Some(Value::bool(lazy_force(vm, recv, 0)?.is_empty())),
            ("nonEmpty", 0) => Some(Value::bool(!lazy_force(vm, recv, 0)?.is_empty())),
            ("apply", 1) => {
                let i = args[0].to_int().max(0) as usize;
                match lazy_force(vm, recv, i)?.get(i) {
                    Some(v) => Some(v.clone()),
                    None => {
                        return Err(format!("scalars: java.lang.IndexOutOfBoundsException: {i}"))
                    }
                }
            }
            // These need the WHOLE list, so they terminate only on a finite one.
            ("toList", 0) => Some(new_list(lazy_all(vm, recv)?)),
            ("toVector" | "toIndexedSeq" | "toSeq", 0) => {
                Some(new_seq(SeqKind::Vector, lazy_all(vm, recv)?))
            }
            ("toArray", 0) => Some(new_seq(SeqKind::Array, lazy_all(vm, recv)?)),
            ("length" | "size", 0) => Some(Value::int(lazy_all(vm, recv)?.len() as i64)),
            ("sum", 0) => Some(seq_sum(&lazy_all(vm, recv)?)),
            ("mkString", 0..=1) => {
                let items = lazy_all(vm, recv)?;
                let sep = args
                    .first()
                    .map(|a| a.as_str_cow().into_owned())
                    .unwrap_or_default();
                Some(Value::str(join_vm(vm, &items, &sep)))
            }
            ("foreach", 1) => {
                for v in lazy_all(vm, recv)? {
                    invoke_closure(vm, &args[0], std::slice::from_ref(&v))?;
                }
                Some(unit_value())
            }
            ("force", 0) => {
                lazy_all(vm, recv)?;
                Some(recv.clone())
            }
            _ => None,
        })
    })();
    match r {
        Ok(Some(v)) => Some(Ok(v)),
        Ok(None) => None,
        Err(e) => Some(Err(e)),
    }
}

/// Force a `LazyList` to the end and answer every element. Only meaningful for
/// a finite one; an infinite source runs out of the fuel `lazy_force` bounds it
/// with and reports rather than hanging.
fn lazy_all(vm: &mut VM, list: &Value) -> Result<Vec<Value>, String> {
    let mut k = 0usize;
    loop {
        let got = lazy_force(vm, list, k)?;
        if got.len() <= k {
            return Ok(got);
        }
        k = got.len();
    }
}

/// Read a `LazyList` handle's state, if `v` is one.
fn as_lazy(v: &Value) -> Option<LazyList> {
    if let Value::Obj(id) = v {
        HEAP.with(|h| match h.borrow().get(*id as usize) {
            Some(HeapVal::Lazy(l)) => Some(l.clone()),
            _ => None,
        })
    } else {
        None
    }
}

/// Allocate a `LazyList` with the given prefix and rule.
fn new_lazy(forced: Vec<Value>, src: LazySrc) -> Value {
    let done = matches!(src, LazySrc::End);
    heap_push(HeapVal::Lazy(LazyList { forced, src, done }))
}

/// Write a `LazyList` handle's state back — how memoisation is recorded.
fn set_lazy(v: &Value, l: LazyList) {
    if let Value::Obj(id) = v {
        HEAP.with(|h| {
            if let Some(HeapVal::Lazy(slot)) = h.borrow_mut().get_mut(*id as usize) {
                *slot = l;
            }
        });
    }
}

/// Force `list` until it has more than `k` elements, or is known to be
/// finished. Answers the elements forced so far.
///
/// Every forced element is written back into the handle, so a second traversal
/// recomputes nothing — which is observable by counting a `map`'s calls.
///
/// `fuel` bounds the scan a `filter` may do between hits, so a predicate no
/// element satisfies stops instead of hanging: an infinite source with no
/// matching element cannot be distinguished from a slow one, and a bound that
/// reports is better than a loop that does not.
fn lazy_force(vm: &mut VM, list: &Value, k: usize) -> Result<Vec<Value>, String> {
    const FUEL: usize = 1_000_000;
    let mut steps = 0usize;
    loop {
        let Some(mut l) = as_lazy(list) else {
            return Err("scalars: value is not a LazyList".to_string());
        };
        if l.done || l.forced.len() > k {
            return Ok(l.forced);
        }
        steps += 1;
        if steps > FUEL {
            return Err(format!(
                "scalars: LazyList did not produce element {k} within {FUEL} steps"
            ));
        }
        match l.src.clone() {
            LazySrc::End => {
                l.done = true;
                set_lazy(list, l);
            }
            LazySrc::Ints { next } => {
                l.forced.push(Value::int(next));
                l.src = LazySrc::Ints {
                    next: next.wrapping_add(1),
                };
                set_lazy(list, l);
            }
            LazySrc::Rep { v } => {
                l.forced.push(v.clone());
                set_lazy(list, l);
            }
            LazySrc::Iter { next, f } => {
                l.forced.push(next.clone());
                let after = invoke_closure(vm, &f, std::slice::from_ref(&next))?;
                l.src = LazySrc::Iter { next: after, f };
                set_lazy(list, l);
            }
            // Run the thunk ONCE, then continue from what it produced.
            LazySrc::Thunk { thunk, base } => {
                let cont = invoke_closure(vm, &thunk, &[])?;
                let mut l2 = as_lazy(list).unwrap_or(l);
                l2.src = LazySrc::Cont { list: cont, base };
                set_lazy(list, l2);
            }
            LazySrc::Cont { list: inner, base } => {
                let want = l.forced.len() - base;
                let got = lazy_force(vm, &inner, want)?;
                let mut l2 = as_lazy(list).unwrap_or(l);
                match got.get(want) {
                    Some(v) => l2.forced.push(v.clone()),
                    None => l2.done = true,
                }
                set_lazy(list, l2);
            }
            LazySrc::Map { src, f } => {
                let i = l.forced.len();
                let got = lazy_force(vm, &src, i)?;
                let mut l2 = as_lazy(list).unwrap_or(l);
                match got.get(i) {
                    Some(v) => {
                        let mapped = invoke_closure(vm, &f, std::slice::from_ref(v))?;
                        let mut l3 = as_lazy(list).unwrap_or(l2);
                        l3.forced.push(mapped);
                        set_lazy(list, l3);
                    }
                    None => {
                        l2.done = true;
                        set_lazy(list, l2);
                    }
                }
            }
            LazySrc::Filter { src, p, at } => {
                let got = lazy_force(vm, &src, at)?;
                let mut l2 = as_lazy(list).unwrap_or(l);
                match got.get(at) {
                    Some(v) => {
                        let hit = invoke_closure(vm, &p, std::slice::from_ref(v))?;
                        let keep = truthy(&hit);
                        let mut l3 = as_lazy(list).unwrap_or(l2);
                        if keep {
                            l3.forced.push(v.clone());
                        }
                        l3.src = LazySrc::Filter { src, p, at: at + 1 };
                        set_lazy(list, l3);
                    }
                    None => {
                        l2.done = true;
                        set_lazy(list, l2);
                    }
                }
            }
            LazySrc::Elems { items, at } => {
                let mut l2 = l;
                match items.get(at) {
                    Some(v) => {
                        l2.forced.push(v.clone());
                        l2.src = LazySrc::Elems { items, at: at + 1 };
                    }
                    None => l2.done = true,
                }
                set_lazy(list, l2);
            }
            LazySrc::Drop { src, n } => {
                let i = l.forced.len() + n;
                let got = lazy_force(vm, &src, i)?;
                let mut l2 = as_lazy(list).unwrap_or(l);
                match got.get(i) {
                    Some(v) => l2.forced.push(v.clone()),
                    None => l2.done = true,
                }
                set_lazy(list, l2);
            }
            LazySrc::Zip { a, b } => {
                let i = l.forced.len();
                let ga = lazy_force(vm, &a, i)?;
                let gb = lazy_force(vm, &b, i)?;
                let mut l2 = as_lazy(list).unwrap_or(l);
                match (ga.get(i), gb.get(i)) {
                    (Some(x), Some(y)) => l2.forced.push(new_pair(x.clone(), y.clone())),
                    _ => l2.done = true,
                }
                set_lazy(list, l2);
            }
        }
    }
}

/// A `LazyList`'s memoised prefix and the rule producing the rest.
#[derive(Clone)]
pub struct LazyList {
    /// Elements already computed. Never recomputed — `LazyList.from(1).map(f)`
    /// traversed twice calls `f` once per element, which a program can count.
    forced: Vec<Value>,
    /// How the next element is produced.
    src: LazySrc,
    /// Set once the source is known to be finished, so a finite list stops
    /// rather than re-asking a spent rule.
    done: bool,
}

/// How a `LazyList` produces the element after its forced prefix.
#[derive(Clone)]
enum LazySrc {
    /// No more elements.
    End,
    /// The integers from `next` upward — `LazyList.from(n)`.
    Ints { next: i64 },
    /// `LazyList.iterate(seed)(f)`: `next`, then `f(next)`, and so on.
    Iter { next: Value, f: Value },
    /// `LazyList.continually(v)` — the same element forever.
    Rep { v: Value },
    /// The continuation is behind a zero-argument thunk not yet run. This is
    /// what makes `a #:: rest` lazy in `rest`, and so what lets a `LazyList`
    /// refer to ITSELF (`val fibs = 0 #:: 1 #:: fibs.zip(fibs.tail)…`): by the
    /// time the thunk runs, the binding it reads has been assigned.
    Thunk { thunk: Value, base: usize },
    /// The continuation, already forced to a `LazyList`. Element `k >= base`
    /// of this list is element `k - base` of that one.
    Cont { list: Value, base: usize },
    /// `src.map(f)` — element `k` is `f` of the source's element `k`.
    Map { src: Value, f: Value },
    /// `src.filter(p)` — the source is scanned from `at` for the next hit.
    Filter { src: Value, p: Value, at: usize },
    /// `a.zip(b)` — element `k` is the pair of their `k`th elements.
    Zip { a: Value, b: Value },
    /// `src.drop(n)` — element `k` is the source's element `k + n`. `tail` is
    /// this with `n = 1`.
    Drop { src: Value, n: usize },
    /// A literal `LazyList(1, 2, 3)`. The elements are known, but they are
    /// still produced one at a time: Scala prints `LazyList(<not computed>)`
    /// for a fresh one, so even a literal starts unforced.
    Elems { items: Vec<Value>, at: usize },
}

/// The class tag of an unforced `lazy val`. Not spellable as a Scala class
/// name, so no user record can collide with it, and it never escapes: the only
/// value that holds one is the binding's own cell, and every read of that
/// binding goes through [`LAZY_FORCE`].
const LAZY_CLASS: &str = "<lazy>";

/// [`LAZY_NEW`] — park a thunk as the unforced state of a `lazy val`.
fn b_lazy_new(vm: &mut VM, _argc: u8) -> Value {
    let thunk = vm.stack.pop().unwrap_or(Value::Undef);
    heap_alloc(ScalaObj {
        class: Arc::from(LAZY_CLASS),
        is_case: false,
        is_object: false,
        fields: vec![(Arc::from("t"), thunk)],
    })
}

/// [`LAZY_FORCE`] — read a `lazy val`'s cell, forcing it once.
fn b_lazy_force(vm: &mut VM, _argc: u8) -> Value {
    let cell = vm.stack.pop().unwrap_or(Value::Undef);
    let current = cell_get(&cell);
    let Some(thunk) = with_obj(&current, |o| {
        (&*o.class == LAZY_CLASS)
            .then(|| o.fields.first().map(|(_, v)| v.clone()))
            .flatten()
    })
    .flatten() else {
        // Already forced — every read after the first lands here.
        return current;
    };
    let forced = match invoke_closure(vm, &thunk, &[]) {
        Ok(v) => v,
        Err(e) => return fault(vm, e),
    };
    // An initializer that RAISED produced no value to memoize: Scala leaves the
    // binding uninitialized and RE-RUNS the initializer on the next read, so a
    // `lazy val v = 1 / 0` read twice inside two `try`s runs its side effects
    // twice and throws twice. The raise here is a pending exception rather than
    // an `Err` — an arithmetic fault is not checked until the next statement
    // boundary — so `Ok` is not on its own evidence that the thunk succeeded,
    // and writing back regardless memoized whatever the faulting expression had
    // left on the stack. The second read then answered that value silently.
    if unwinding() {
        return forced;
    }
    // Written back BEFORE the value is answered, so a second read finds the
    // value rather than the thunk even when the initializer read the binding
    // itself (Scala answers the partially-initialized value there too).
    cell_set(&cell, forced.clone());
    forced
}

/// [`SF64`] — widen a `Float` to `Double`, leaving everything else alone.
fn b_f64(vm: &mut VM, _argc: u8) -> Value {
    let v = vm.stack.pop().unwrap_or(Value::Undef);
    conv_elementwise(v, |x| f32_of(x).map(|f| Value::float(f64::from(f))))
}

/// [`SF32_STR`] — `Float.toString` of one value.
///
/// A non-floating value passes through unchanged rather than being rendered,
/// so a statically-`Float` expression that turned out to hold something else
/// still stringifies the way it otherwise would.
fn b_f32_str(vm: &mut VM, _argc: u8) -> Value {
    let v = vm.stack.pop().unwrap_or(Value::Undef);
    match f32_of(&v) {
        Some(f) => Value::str(format_float32(f)),
        // A `Double` reaching a statically-`Float` site is the compiler having
        // been right about the type and the value having come from somewhere
        // that did not narrow it; render it at single precision anyway.
        None => match &v {
            Value::Float(d) => Value::str(format_float32(*d as f32)),
            _ => v,
        },
    }
}

/// [`SF32_CLASS`] — the `java.lang.Class` record for `float`.
fn b_f32_class(vm: &mut VM, _argc: u8) -> Value {
    vm.stack.pop();
    class_record("float", "float", true)
}

/// [`SF32_HASH`] — `Float.hashCode`, the 32-bit bit pattern.
fn b_f32_hash(vm: &mut VM, _argc: u8) -> Value {
    let v = vm.stack.pop().unwrap_or(Value::Undef);
    Value::int(i64::from(float_to_int_bits(as_f32(&v).unwrap_or(0.0))))
}

/// `Float.toString` — the same layout rules as [`format_double`], with the
/// shortest decimal computed against **32-bit** precision.
///
/// That is the whole difference between the two. The `f64` nearest `0.1f`
/// prints `0.10000000149011612` as a `Double` and `0.1` as a `Float`, because
/// only 24 bits of significand have to round-trip. The plain-vs-`E` threshold
/// (`1e-3 <= |x| < 1e7`) and the mandatory fractional digit are identical, so
/// only the digit selection is duplicated.
fn format_float32(f: f32) -> String {
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
    let (digits, exp) = java_decimal_f32(f.abs());
    let body = if (-3..=6).contains(&exp) {
        render_plain(&digits, exp)
    } else {
        render_scientific(&digits, exp)
    };
    if neg {
        format!("-{body}")
    } else {
        body
    }
}

/// [`java_decimal`]'s three steps, applied at `f32` precision.
///
/// The specification `Float.toString` follows is `Double.toString`'s with the
/// type changed, so the steps are the same: the shortest round-tripping form,
/// a two-digit floor when that form is one digit, and an exact tie broken
/// toward the even last digit. What differs is the precision every one of them
/// is measured at — `Float.MinPositiveValue` is `1.4E-45` for the same reason
/// `Double.MinPositiveValue` is `4.9E-324`, and only an `f32` round-trip test
/// can tell that.
fn java_decimal_f32(m: f32) -> (String, i32) {
    let sci = format!("{m:e}");
    let (mant, exp_str) = sci.split_once('e').expect("`{:e}` always contains `e`");
    let mut exp: i32 = exp_str.parse().expect("`{:e}` exponent is an integer");
    let mut digits: String = mant.chars().filter(|c| *c != '.').collect();
    if digits.len() == 1 {
        let two = format!("{m:.1e}");
        let (m2, e2) = two.split_once('e').expect("`{:e}` always contains `e`");
        digits = m2.chars().filter(|c| *c != '.').collect();
        exp = e2.parse().expect("`{:e}` exponent is an integer");
    }
    if let Some((d, e)) = break_tie_to_even_f32(m, &digits, exp) {
        digits = d;
        exp = e;
    }
    while digits.len() > 1 && digits.ends_with('0') {
        digits.pop();
    }
    (digits, exp)
}

/// [`break_tie_to_even`] at `f32` precision.
///
/// The exact expansion is read off the `f64` widening of `m`, which is exact —
/// every `f32` is an `f64` — so the tie test itself is the same arithmetic.
/// Only the round-trip test changes: a candidate counts when it parses back to
/// the same **`f32`**, which is what keeps the tie-break ranging over the
/// decimals that actually name this `Float`.
fn break_tie_to_even_f32(m: f32, digits: &str, exp: i32) -> Option<(String, i32)> {
    let n = digits.len();
    let wide = f64::from(m);
    let probe = format!("{wide:.*e}", n);
    let (pm, pe) = probe.split_once('e')?;
    if pe.parse::<i32>().ok()? != exp || !pm.ends_with('5') {
        return None;
    }
    let exact = format!("{wide:.1099e}");
    let (em, ee) = exact.split_once('e')?;
    if ee.parse::<i32>().ok()? != exp {
        return None;
    }
    let ed: Vec<u8> = em.bytes().filter(|b| *b != b'.').collect();
    if ed.len() <= n || ed[n] != b'5' || ed[n + 1..].iter().any(|b| *b != b'0') {
        return None;
    }
    let low = String::from_utf8(ed[..n].to_vec()).ok()?;
    let (high, high_exp) = increment_last_digit(&low, exp);
    if !round_trips_f32(&low, exp, m) || !round_trips_f32(&high, high_exp, m) {
        return None;
    }
    if (low.as_bytes()[n - 1] - b'0') % 2 == 0 {
        Some((low, exp))
    } else {
        Some((high, high_exp))
    }
}

/// Whether the decimal `d.ddd × 10^exp` rounds back to exactly the `f32` `m`.
fn round_trips_f32(digits: &str, exp: i32, m: f32) -> bool {
    let (head, rest) = digits.split_at(1);
    let rest = if rest.is_empty() { "0" } else { rest };
    format!("{head}.{rest}e{exp}")
        .parse::<f32>()
        .is_ok_and(|v| v == m)
}

/// The decimal Java renders for a positive finite `m`, as `(digits, exp)` — the
/// significant digits with no radix point, and the base-10 exponent of the
/// first one, so the value is `d.ddd × 10^exp`.
///
/// `Double.toString` is specified over decimal VALUES, not strings. Let R be the
/// decimals that round back to `m` and `p` the minimal length in R. If `p >= 2`
/// the answer is the length-`p` member; otherwise it is the closest member of
/// length 1 **or** 2. Ties go to the candidate whose last digit is even. Three
/// steps, in that order:
///
/// 1. **Shortest.** Rust's `{:e}` emits the shortest round-tripping form, which
///    is Java's length-`p` member.
/// 2. **The two-digit floor is a rounding rule, not padding.** When `p` is 1 the
///    closest two-digit decimal can differ from the one-digit answer with a zero
///    stuck on: `Double.MinPositiveValue` is `4.9E-324`, not `5.0E-324`. It
///    takes an ULP within about 1% of the value's own magnitude to move that
///    second digit off zero, so only the deep subnormals are affected — every
///    normal double has relative ULP at most 2^-52.
/// 3. **An exact tie goes to the even digit.** A double whose exact decimal
///    expansion ends in a lone `5` one place past the chosen length is
///    equidistant between two candidates, and Rust does not break that tie the
///    way Java does: `2^-25` is exactly `2.98023223876953125E-8`, which Java
///    renders `…312E-8` and `{:e}` renders `…313E-8`.
///
/// Trailing zeros are dropped last, because they carry no value: the two-digit
/// step turns `0.001` into digits `10`, and `0.0010` is the same decimal as
/// `0.001`. Java's "at least one fractional digit" is a LAYOUT rule, applied by
/// the two renderers rather than by padding the digits.
fn java_decimal(m: f64) -> (String, i32) {
    let sci = format!("{m:e}");
    let (mant, exp_str) = sci.split_once('e').expect("`{:e}` always contains `e`");
    let mut exp: i32 = exp_str.parse().expect("`{:e}` exponent is an integer");
    let mut digits: String = mant.chars().filter(|c| *c != '.').collect();
    // Step 2. `{:.1e}` renormalizes, so the exponent can move with it: the value
    // whose shortest form is `1e-322` is `9.9e-323` at two digits.
    if digits.len() == 1 {
        let two = format!("{m:.1e}");
        let (m2, e2) = two.split_once('e').expect("`{:e}` always contains `e`");
        digits = m2.chars().filter(|c| *c != '.').collect();
        exp = e2.parse().expect("`{:e}` exponent is an integer");
    }
    // Step 3.
    if let Some((d, e)) = break_tie_to_even(m, &digits, exp) {
        digits = d;
        exp = e;
    }
    while digits.len() > 1 && digits.ends_with('0') {
        digits.pop();
    }
    (digits, exp)
}

/// The even-digit answer when `m` is EXACTLY equidistant between two decimals of
/// length `digits.len()` that BOTH round back to it, else `None`.
///
/// Two conditions, and both are load-bearing.
///
/// *Exactly* equidistant has to be read off the exact value, not a rounding of
/// it: an expansion continuing `…5004` also rounds to a trailing `5` and is no
/// tie at all. A double's decimal expansion always terminates (it is a binary
/// fraction) and Rust prints it in full at a high enough precision — 751
/// significant digits is the worst case, for the smallest subnormal.
///
/// *Both round back* is what separates `2^-24` from `5 * 2^-23`. Their exact
/// expansions carry the SAME digits (`5.9604644775390625`), yet Java renders one
/// `5.960464477539063E-8` and the other `5.960464477539062E-7`. Java's tie-break
/// only ranges over decimals that round to the value, and at an exact power of
/// two the gap below is half the gap above — so `2^-24`'s lower candidate falls
/// outside the rounding interval and is not a candidate at all, leaving one
/// answer and no tie. Parsing each candidate back is exact (Rust's `f64`
/// `FromStr` is correctly rounded), so it decides this directly rather than by
/// reasoning about the exponent.
fn break_tie_to_even(m: f64, digits: &str, exp: i32) -> Option<(String, i32)> {
    let n = digits.len();
    // Cheap gate first: one more digit, and only a trailing `5` can be a tie.
    let probe = format!("{m:.*e}", n);
    let (pm, pe) = probe.split_once('e')?;
    if pe.parse::<i32>().ok()? != exp || !pm.ends_with('5') {
        return None;
    }
    let exact = format!("{m:.1099e}");
    let (em, ee) = exact.split_once('e')?;
    if ee.parse::<i32>().ok()? != exp {
        return None;
    }
    let ed: Vec<u8> = em.bytes().filter(|b| *b != b'.').collect();
    if ed.len() <= n || ed[n] != b'5' || ed[n + 1..].iter().any(|b| *b != b'0') {
        return None;
    }
    // The two decimals `m` sits exactly between: the truncation, and the
    // truncation plus one in its last place.
    let low = String::from_utf8(ed[..n].to_vec()).ok()?;
    let (high, high_exp) = increment_last_digit(&low, exp);
    if !round_trips(&low, exp, m) || !round_trips(&high, high_exp, m) {
        return None;
    }
    // A genuine tie. The two differ in the parity of their last digit, so
    // exactly one is the even significand Java takes. Neither can end in `0`
    // here: that decimal would have a SHORTER significand, and a shorter one
    // that round-trips would have been the shortest form to begin with.
    if (low.as_bytes()[n - 1] - b'0') % 2 == 0 {
        Some((low, exp))
    } else {
        Some((high, high_exp))
    }
}

/// `digits` plus one in its last place, carrying — `("19", 3)` is `("20", 3)`
/// and `("99", 3)` is `("10", 4)`, keeping the digit count.
fn increment_last_digit(digits: &str, exp: i32) -> (String, i32) {
    let mut d: Vec<u8> = digits.as_bytes().to_vec();
    for i in (0..d.len()).rev() {
        if d[i] == b'9' {
            d[i] = b'0';
        } else {
            d[i] += 1;
            return (String::from_utf8(d).expect("ASCII digits"), exp);
        }
    }
    // Every digit carried (`99…9` + 1 is `10…0`), one exponent up.
    d.pop();
    let mut up = vec![b'1'];
    up.append(&mut d);
    (String::from_utf8(up).expect("ASCII digits"), exp + 1)
}

/// Whether the decimal `d.ddd × 10^exp` rounds back to exactly `m`.
fn round_trips(digits: &str, exp: i32, m: f64) -> bool {
    let (head, rest) = digits.split_at(1);
    let rest = if rest.is_empty() { "0" } else { rest };
    format!("{head}.{rest}e{exp}")
        .parse::<f64>()
        .is_ok_and(|v| v == m)
}

/// `d.dddEexp` — Java's computerized scientific notation, with the mandatory
/// fractional digit.
fn render_scientific(digits: &str, exp: i32) -> String {
    let (head, rest) = digits.split_at(1);
    let rest = if rest.is_empty() { "0" } else { rest };
    format!("{head}.{rest}E{exp}")
}

/// `ddd.ddd` — Java's plain notation, used for `1e-3 <= |x| < 1e7`, with the
/// mandatory fractional digit.
fn render_plain(digits: &str, exp: i32) -> String {
    if exp < 0 {
        let zeros = (-exp - 1) as usize;
        return format!("0.{}{digits}", "0".repeat(zeros));
    }
    let int_len = exp as usize + 1;
    if digits.len() > int_len {
        let (head, rest) = digits.split_at(int_len);
        format!("{head}.{rest}")
    } else {
        format!("{digits}{}.0", "0".repeat(int_len - digits.len()))
    }
}

/// Strict numeric hook. fusevm calls this for
///
/// * an operation with a non-numeric operand — Scala's `String` `+` overload
///   plus value comparisons against strings, and the `Char` handle, which is a
///   heap object here so every operation on one arrives;
/// * integer `+`/`-`/`*` whose exact result left `i64` (answered wrapped, since
///   Scala's `Long` wraps);
/// * a mixed `Int`/`Float` pair whose integer is outside the range an `f64`
///   holds exactly (`|x| > 2^53`), where converting it would round.
///
/// All-`Float` arithmetic, and mixed arithmetic with a small integer, stay on
/// the native fast path and the JIT and never reach here.
pub fn numeric_hook(op: NumOp, a: &Value, b: &Value) -> Result<Value, String> {
    // Suppressed while unwinding: an operand is the `Undef` a raise left behind,
    // and Scala 3's strict `+`/comparison rules would reject it as a type error,
    // displacing the real exception (see `Exception unwinding`).
    if unwinding() {
        return Ok(Value::Undef);
    }
    // A `Char` operand. `Char` is a heap handle, so every operation on one
    // reaches this hook; it is numeric in all of them (`'a' + 1 == 98`) except
    // `+` against a `String`. Answered before the arms below, which would
    // otherwise treat the handle as a collection or reject it.
    if as_char(a).is_some() || as_char(b).is_some() {
        if op == NumOp::Neg {
            // Unary minus on a `Char` is `-(code point)` as an `Int`.
            return Ok(Value::int(-char_code(a).unwrap_or(0)));
        }
        let name = match op {
            NumOp::Add => "+",
            NumOp::Sub => "-",
            NumOp::Mul => "*",
            NumOp::Div => "/",
            NumOp::Mod => "%",
            NumOp::Lt => "<",
            NumOp::Gt => ">",
            NumOp::Le => "<=",
            NumOp::Ge => ">=",
            NumOp::Eq => "==",
            NumOp::Ne => "!=",
            // `Char` has no `pow`; fall through to the general arms. `Neg`
            // already returned above.
            NumOp::Pow | NumOp::Neg => "",
        };
        if !name.is_empty() {
            return char_binop(name, a, b);
        }
    }
    // A `Float` operand. `Value::Status` is not one of fusevm's native numeric
    // shapes, so EVERY operation touching a `Float` arrives here — which is
    // exactly what makes single precision the runtime's business rather than
    // the compiler's, and what closes `println(List(0.1f))`.
    //
    // Scala's binary numeric promotion puts `Float` above both integer widths
    // and below `Double`: `0.1f + 1` and `0.1f + 1L` are `Float`, `0.1f + 1.0`
    // is `Double`. So a `Double` on either side widens the pair and falls
    // through to the arms below; anything else is computed at 32 bits.
    if (is_f32(a) || is_f32(b)) && !matches!(a, Value::Float(_)) && !matches!(b, Value::Float(_)) {
        if op == NumOp::Neg {
            return Ok(make_f32(-f32_of(a).unwrap_or(0.0)));
        }
        // A `String` on either side is a concatenation, not arithmetic; it is
        // answered by the general arms below, which render each side through
        // `scala_str` — and a `Float` renders as one.
        if !matches!(a, Value::Str(_)) && !matches!(b, Value::Str(_)) {
            if let (Some(x), Some(y)) = (as_f32(a), as_f32(b)) {
                return Ok(match op {
                    NumOp::Add => make_f32(x + y),
                    NumOp::Sub => make_f32(x - y),
                    NumOp::Mul => make_f32(x * y),
                    NumOp::Div => make_f32(x / y),
                    NumOp::Mod => make_f32(x % y),
                    NumOp::Pow => make_f32(x.powf(y)),
                    NumOp::Lt => Value::bool(x < y),
                    NumOp::Gt => Value::bool(x > y),
                    NumOp::Le => Value::bool(x <= y),
                    NumOp::Ge => Value::bool(x >= y),
                    NumOp::Eq => Value::bool(x == y),
                    NumOp::Ne => Value::bool(x != y),
                    // Handled above.
                    NumOp::Neg => make_f32(-x),
                });
            }
        }
    }
    // A `Float` against a `Double`. Promotion widens the `Float` FIRST and the
    // operation is then a `Double` one — which is what makes `0.1f == 0.1`
    // FALSE (`0.1f` widens to 0.10000000149011612, not to 0.1) while
    // `1.0f == 1.0` is true. Comparing the two renderings instead, as the
    // general string-structural arm below would, gets both of those wrong.
    if (is_f32(a) && matches!(b, Value::Float(_))) || (matches!(a, Value::Float(_)) && is_f32(b)) {
        let (x, y) = (
            float_of(a).unwrap_or(f64::NAN),
            float_of(b).unwrap_or(f64::NAN),
        );
        return Ok(match op {
            NumOp::Add => Value::float(x + y),
            NumOp::Sub => Value::float(x - y),
            NumOp::Mul => Value::float(x * y),
            NumOp::Div => Value::float(x / y),
            NumOp::Mod => Value::float(x % y),
            NumOp::Pow => Value::float(x.powf(y)),
            NumOp::Lt => Value::bool(x < y),
            NumOp::Gt => Value::bool(x > y),
            NumOp::Le => Value::bool(x <= y),
            NumOp::Ge => Value::bool(x >= y),
            NumOp::Eq => Value::bool(x == y),
            NumOp::Ne => Value::bool(x != y),
            NumOp::Neg => Value::float(-x),
        });
    }
    // Two integers reaching this hook means the exact result left `i64` — the VM
    // delegates an overflow here so a host with bignums can widen. Scala has no
    // bignum in its primitive tower: `Long` arithmetic WRAPS, so
    // `4294967296L * 4294967296L` is 0. Answer the wrapped result rather than
    // rejecting the operation.
    if let (Value::Int(x), Value::Int(y)) = (a, b) {
        let wrapped = match op {
            NumOp::Add => Some(x.wrapping_add(*y)),
            NumOp::Sub => Some(x.wrapping_sub(*y)),
            NumOp::Mul => Some(x.wrapping_mul(*y)),
            _ => None,
        };
        if let Some(v) = wrapped {
            return Ok(Value::int(v));
        }
    }
    // A mixed `Int`/`Float` pair. fusevm hands one over when the integer is past
    // `2^53`, where converting it to `f64` rounds (`16677181699666569` collapses
    // onto its neighbour `16677181699666568`), so a host with an exact integer
    // type can answer exactly instead of about the neighbour.
    //
    // Scala has no such answer to give. Binary numeric promotion widens the
    // `Long` to `Double` FIRST and the operation is then a `Double` one, its
    // rounding included: reference `scala` 3.8.4 answers
    // `16677181699666569L == 1.6677181699666568E16` with `true`, and
    // `9007199254740993L == 9.007199254740992E15` (that is, `2^53+1 == 2^53`)
    // with `true` as well. So the promoted result IS the correct Scala answer —
    // return it deliberately rather than falling into the arms below, which
    // rejected the arithmetic outright and answered the comparisons by
    // LEXICOGRAPHIC order of the two rendered operands
    // (`"16677181699666569" < "1.6677181699666568E16"`).
    //
    // `Div` is included for completeness; Scala's `/` is type-dispatching and
    // lowers to the [`SDIV`] builtin (`compiler.rs`), so `Op::Div` never reaches
    // the hook from Scala source. `Pow` is not: Scala has no `**` operator, so
    // there is no reference behaviour to match and it stays with the rejections.
    if matches!(
        (a, b),
        (Value::Int(_), Value::Float(_)) | (Value::Float(_), Value::Int(_))
    ) {
        let (x, y) = (num_f64(a), num_f64(b));
        let promoted = match op {
            NumOp::Add => Some(Value::float(x + y)),
            NumOp::Sub => Some(Value::float(x - y)),
            NumOp::Mul => Some(Value::float(x * y)),
            // Scala's `%` on `Double` is the truncated remainder (`-7 % 2.5` is
            // `-2.0`, `7 % 0.0` is `NaN`), which is Rust's `f64` `%`.
            NumOp::Mod => Some(Value::float(x % y)),
            NumOp::Div => Some(Value::float(x / y)),
            // IEEE comparison on the promoted values: every one of the six is
            // false against a `NaN` operand except `!=`, which `partial_cmp`
            // gives for free.
            NumOp::Lt => Some(Value::bool(x < y)),
            NumOp::Gt => Some(Value::bool(x > y)),
            NumOp::Le => Some(Value::bool(x <= y)),
            NumOp::Ge => Some(Value::bool(x >= y)),
            NumOp::Eq => Some(Value::bool(x == y)),
            NumOp::Ne => Some(Value::bool(x != y)),
            NumOp::Pow | NumOp::Neg => None,
        };
        if let Some(v) = promoted {
            return Ok(v);
        }
    }
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
            // `set + e` / `map + (k -> v)` — the immutable `+` of `Set`/`Map`.
            Value::Obj(_) if is_set(a) => Ok(set_incl(a, b.clone(), true)),
            Value::Obj(_) if map_rep_entries(a).is_some() => {
                let (rep, mut entries) = map_rep_entries(a).unwrap();
                match as_seq_or_tuple(b) {
                    Some(t) if t.len() == 2 => {
                        map_put(&mut entries, t[0].clone(), t[1].clone());
                        Ok(new_map(rep, entries))
                    }
                    _ => Err("scalars: Map `+` expects a `key -> value` pair".to_string()),
                }
            }
            _ => Err(format!(
                "scalars: `+` is not defined between `{}` and `{}`",
                scala_str(a),
                scala_str(b)
            )),
        },
        // Value equality against a non-numeric operand. Scala's `==` is
        // structural `equals`: string `==` compares by content, and an object
        // operand compares by [`obj_eq`] (structural for a `case` instance,
        // reference identity for a plain class).
        NumOp::Eq => Ok(Value::bool(scala_eq(a, b))),
        NumOp::Ne => Ok(Value::bool(!scala_eq(a, b))),
        NumOp::Lt => Ok(Value::bool(scala_str(a) < scala_str(b))),
        NumOp::Gt => Ok(Value::bool(scala_str(a) > scala_str(b))),
        NumOp::Le => Ok(Value::bool(scala_str(a) <= scala_str(b))),
        NumOp::Ge => Ok(Value::bool(scala_str(a) >= scala_str(b))),
        // Arithmetic other than `+` on a non-numeric operand is a type error in
        // Scala (`"a" - 1` does not compile). Report it rather than coercing.
        // `set - e` / `map - k` — the immutable `-` of `Set`/`Map`.
        NumOp::Sub if is_set(a) => Ok(set_incl(a, b.clone(), false)),
        NumOp::Sub if map_rep_entries(a).is_some() => {
            let (rep, entries) = map_rep_entries(a).unwrap();
            Ok(new_map(
                rep,
                entries
                    .into_iter()
                    .filter(|(k, _)| !value_eq(k, b))
                    .collect(),
            ))
        }
        // `s * n` — `StringOps.*`, the only non-numeric `*` Scala defines. The
        // infix form reaches the arithmetic hook (the method form `s.*(n)` goes
        // straight to `string_method`), so it is answered here with the same
        // semantics: a count of zero or less answers the empty string.
        NumOp::Mul if matches!(a, Value::Str(_)) => {
            string_method(&a.as_str_cow(), "*", std::slice::from_ref(b))
        }
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
