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
//!    the same [`scala_str`].

use fusevm::{Frame, NumOp, VMResult, Value, VM};
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

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
/// Builtin id for the run-time half of `x += e` / `x -= e`: pops one value and
/// answers whether it is a collection that mutates in place. `true` sends the
/// compiler-emitted branch to `x.+=(e)`, `false` to `x = x + e` — Scala makes
/// that choice statically from whether the receiver's type has a `+=` method.
pub const IS_GROWABLE: u16 = 744;
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
    vm.register_builtin(MAKE_SET, b_make_set);
    vm.register_builtin(MAKE_LISTBUFFER, b_make_listbuffer);
    vm.register_builtin(MAKE_ARRAYBUFFER, b_make_arraybuffer);
    vm.register_builtin(MAKE_MUTSET, b_make_mutset);
    vm.register_builtin(MAKE_MUTMAP, b_make_mutmap);
    vm.register_builtin(IS_GROWABLE, b_is_growable);
    vm.register_builtin(MAKE_OPTION, b_make_option);
    vm.register_builtin(NLR_RAISE, b_nlr_raise);
    vm.register_builtin(NLR_TAKE, b_nlr_take);
    vm.register_builtin(CELL_NEW, b_cell_new);
    vm.register_builtin(CELL_GET, b_cell_get);
    vm.register_builtin(CELL_SET, b_cell_set);
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

impl SeqKind {
    fn label(self) -> &'static str {
        match self {
            SeqKind::List => "List",
            SeqKind::Vector => "Vector",
            SeqKind::Set(HashRep::Small) => "Set",
            SeqKind::Set(HashRep::Hashed | HashRep::Mutable(_)) => "HashSet",
            SeqKind::Iterable => "Iterable",
            SeqKind::ArraySeq => "ArraySeq",
            SeqKind::Array => "Array",
            SeqKind::ListBuffer => "ListBuffer",
            SeqKind::ArrayBuffer => "ArrayBuffer",
            SeqKind::Range { .. } => "Range",
        }
    }

    /// Whether this kind is mutated in place (`a(i) = v`, `+=`, `clear()`).
    fn is_mutable(self) -> bool {
        matches!(
            self,
            SeqKind::Array
                | SeqKind::ListBuffer
                | SeqKind::ArrayBuffer
                | SeqKind::Set(HashRep::Mutable(_))
        )
    }

    /// Whether this kind is a growable buffer (`+=`, `append`, `remove`) — an
    /// `Array` is mutable but fixed-length, so it is not one.
    fn is_buffer(self) -> bool {
        matches!(self, SeqKind::ListBuffer | SeqKind::ArrayBuffer)
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
fn derive_seq(kind: SeqKind, items: Vec<Value>) -> Value {
    match kind {
        SeqKind::Set(rep) => new_set(rep, items),
        SeqKind::Range { .. } => new_seq(SeqKind::Vector, items),
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
    /// Whether every value the body can answer is statically a `String` rather
    /// than a `Char` (see `compiler::yields_strings`). `String.map` picks its
    /// result type from Scala's `Char => Char` vs `Char => B` overload split,
    /// which the run-time results cannot distinguish for a body like
    /// `_.toString` — a one-char `String` either way.
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
fn seq_kind(v: &Value) -> Option<SeqKind> {
    seq_kind_items(v).map(|(k, _)| k)
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
        string_body: flags & 2 != 0,
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
    let mutable_map = matches!(map_rep_entries(&v), Some((HashRep::Mutable(_), _)));
    Value::bool(mutable_seq || mutable_map)
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
    let end = vm.pop().to_int();
    let start = vm.pop().to_int();
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
                let items = as_seq_or_tuple(recv).unwrap_or_default();
                let i = args.first().map(|a| a.to_int()).unwrap_or(0);
                return list_index(&items, i);
            }
            Some(2) => {
                let m = as_map(recv).unwrap_or_default();
                let key = args.first().cloned().unwrap_or(Value::Undef);
                return map_get(&m, &key)
                    .ok_or_else(|| format!("scalars: key not found: {}", scala_str(&key)));
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
            Some(HeapVal::Match { matched, .. }) => matched.to_string(),
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
        Value::Str(s) => Some(string_hash(s)),
        Value::Bool(b) => Some(if *b { 1231 } else { 1237 }),
        Value::Undef => Some(0),
        Value::Obj(id) => {
            let hv = HEAP.with(|h| h.borrow().get(*id as usize).cloned())?;
            match hv {
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
        let mut slots: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
        for &i in idxs {
            slots.entry((hashes[i] >> shift) & 31).or_default().push(i);
        }
        for group in slots.values() {
            if group.len() == 1 {
                out.push(group[0]);
            }
        }
        for group in slots.values() {
            if group.len() > 1 {
                walk(group, hashes, shift + 5, out);
            }
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
                // Two sequences/tuples are equal element-by-element (a `List`
                // equals a `Set` only if both order and elements match — good
                // enough for the collections this frontend builds).
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
    // A `match` whose scrutinee is the garbage left by a raise falls through
    // every arm; that is not a real `MatchError`, so stay quiet and let the
    // in-flight exception reach its handler.
    if unwinding() {
        return Value::Undef;
    }
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
    // A range `for … yield` result is a fusevm `Value::Array` (rendered as
    // `Vector`); promote it to a heap `Vector` seq so the collection methods
    // (`map`, `toList`, …) apply uniformly.
    let recv = match &recv {
        Value::Array(items) => heap_push(HeapVal::Seq(SeqKind::Vector, items.clone())),
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

    // A heap collection/tuple/closure may need to run a closure body mid-method
    // (`map`, `filter`, `foldLeft`, …), so those dispatch through the vm-aware
    // path; everything else stays on the pure dispatcher.
    if is_heap_collection(&recv) {
        return match heap_method(vm, &recv, &name, &args) {
            Ok(v) => v,
            Err(e) => fault(vm, e),
        };
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
        return Ok(Value::str(scala_str(recv)));
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
    let ordered = mut_ordered(&cur, len, Clone::clone).unwrap_or(cur);
    set_seq_items(recv, SeqKind::Set(HashRep::Mutable(len as u32)), ordered);
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
    let is_set = set_len.is_some();
    let buffer = kind.is_buffer();
    let me = || Ok(recv.clone());
    // Whether `args[0]` is a collection of elements (`++=`) or one element.
    let all = matches!(
        name,
        "++=" | "addAll" | "appendAll" | "prependAll" | "--=" | "subtractAll"
    );

    match (name, args.len()) {
        // Additions. `+=`/`++=` answer the receiver; `add` answers whether the
        // element was absent.
        ("+=" | "++=" | "addOne" | "addAll" | "append" | "appendAll" | "add", 1)
            if is_set || buffer =>
        {
            let adds = spread(&args[0], all);
            if let Some(len) = set_len {
                let absent = !items.iter().any(|u| value_eq(u, &args[0]));
                let hint = all.then(|| known_size(&args[0])).flatten();
                mut_set_add(recv, len, items, &adds, hint);
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
            let mut out = spread(&args[0], all);
            out.extend_from_slice(items);
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

/// Replace the contents (and table length) of the `mutable.Map` behind `recv`.
fn set_map_entries(recv: &Value, len: usize, entries: Vec<(Value, Value)>) {
    let ordered = mut_ordered(&entries, len, |(k, _)| k.clone()).unwrap_or(entries);
    if let Value::Obj(id) = recv {
        HEAP.with(|h| {
            if let Some(HeapVal::Map(rep, m)) = h.borrow_mut().get_mut(*id as usize) {
                *rep = HashRep::Mutable(len as u32);
                *m = ordered;
            }
        });
    }
}

/// Put `adds` into the `mutable.HashMap` behind `recv`, replaying `put0`'s
/// growth. Answers the value a repeated key displaced, for `put`.
fn mut_map_put(
    recv: &Value,
    len: usize,
    entries: &[(Value, Value)],
    adds: &[(Value, Value)],
    hint: Option<usize>,
) -> Option<Value> {
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
    let len = mut_grown(mut_size_hint(len, hint), entries.len(), &flags);
    set_map_entries(recv, len, cur);
    displaced
}

/// In-place mutation of a `mutable.Map`. `None` falls through to the shared
/// read-only implementation.
fn mut_map_method(
    recv: &Value,
    len: usize,
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
                    len,
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
                len,
                entries,
                &[(args[0].clone(), args[1].clone())],
                None,
            );
            Some(Ok(Value::Undef))
        }
        ("put", 2) => {
            let old = mut_map_put(
                recv,
                len,
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
                    len,
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
            set_map_entries(recv, len, kept);
            Some(if name == "remove" { Ok(opt(old)) } else { me() })
        }
        ("clear", 0) => {
            set_map_entries(recv, len, Vec::new());
            Some(Ok(Value::Undef))
        }
        _ => None,
    }
}

/// The JDK's out-of-bounds message for an indexed sequence write.
fn index_out_of_bounds(i: i64, len: usize) -> String {
    format!(
        "scalars: java.lang.IndexOutOfBoundsException: {i} is out of bounds (min 0, max {})",
        len.saturating_sub(1)
    )
}

/// `Seq` (`List`/`Set`/`Iterable`) methods — a faithful subset. Closure-taking
/// ops run their function argument through [`invoke_closure`].
fn seq_method(vm: &mut VM, recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    let (kind, items) = seq_kind_items(recv).unwrap_or((SeqKind::List, Vec::new()));
    // A transforming op keeps the receiver's collection kind (`List.map` → `List`,
    // a range-derived `Vector.map` → `Vector`).
    let same = |v: Vec<Value>| derive_seq(kind, v);
    // In-place mutation (`+=`, `clear()`, …) — before the pure paths, because
    // several names (`+`, `-`, `++`) mean "mutate me" on a mutable receiver and
    // "build a new one" on an immutable one.
    if kind.is_mutable() {
        if let Some(r) = mut_seq_method(recv, kind, &items, name, args) {
            return r;
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
        // `Array.update`. Only an `Array` is mutable in Scala.
        ("update", 2) => {
            if !matches!(
                kind,
                SeqKind::Array | SeqKind::ListBuffer | SeqKind::ArrayBuffer
            ) {
                return Err(no_such_method(recv, name));
            }
            let i = args[0].to_int();
            if i < 0 || i as usize >= items.len() {
                return Err(format!(
                    "scalars: java.lang.ArrayIndexOutOfBoundsException: Index {i} out of bounds for length {}",
                    items.len()
                ));
            }
            if let Value::Obj(id) = recv {
                HEAP.with(|h| {
                    if let Some(HeapVal::Seq(_, xs)) = h.borrow_mut().get_mut(*id as usize) {
                        xs[i as usize] = args[1].clone();
                    }
                });
            }
            Ok(Value::Undef)
        }
        ("min", 0) | ("max", 0) => {
            let want = if name == "max" {
                Ordering::Greater
            } else {
                Ordering::Less
            };
            items
                .iter()
                .skip(1)
                .fold(items.first().cloned(), |best, it| {
                    best.map(|b| {
                        if value_cmp(it, &b) == want {
                            it.clone()
                        } else {
                            b
                        }
                    })
                })
                .ok_or_else(|| {
                    format!("scalars: java.lang.UnsupportedOperationException: empty.{name}")
                })
        }
        ("toArray", 0) => Ok(new_seq(SeqKind::Array, items)),
        ("toVector", 0) => Ok(new_seq(SeqKind::Vector, items)),
        // `iterator`/`reverseIterator` are strict here: the result is an
        // `Iterable` carrying the same elements, so the downstream combinators
        // (`.toList`, `.map`, `.size`, …) answer exactly what Scala's lazy
        // iterator would. Only printing the iterator itself would differ, and
        // Scala's own `Iterator.toString` is unreproducible anyway.
        ("iterator", 0) => Ok(new_seq(SeqKind::Iterable, items)),
        ("reverseIterator", 0) => Ok(new_seq(
            SeqKind::Iterable,
            items.into_iter().rev().collect(),
        )),
        ("length" | "size", 0) => Ok(Value::int(items.len() as i64)),
        ("isEmpty", 0) => Ok(Value::bool(items.is_empty())),
        ("nonEmpty", 0) => Ok(Value::bool(!items.is_empty())),
        ("head", 0) => items
            .first()
            .cloned()
            .ok_or_else(|| "scalars: java.util.NoSuchElementException: head of empty list".into()),
        ("last", 0) => items
            .last()
            .cloned()
            .ok_or_else(|| "scalars: java.util.NoSuchElementException: last of empty list".into()),
        ("tail", 0) => {
            if items.is_empty() {
                Err("scalars: java.lang.UnsupportedOperationException: tail of empty list".into())
            } else {
                Ok(same(items[1..].to_vec()))
            }
        }
        ("reverse", 0) => Ok(same(items.iter().rev().cloned().collect())),
        ("sum", 0) => Ok(seq_sum(&items)),
        ("mkString", 0) => Ok(Value::str(
            items.iter().map(scala_str).collect::<Vec<_>>().join(""),
        )),
        ("mkString", 1) => {
            let sep = args[0].as_str_cow().into_owned();
            Ok(Value::str(
                items.iter().map(scala_str).collect::<Vec<_>>().join(&sep),
            ))
        }
        ("contains", 1) => Ok(Value::bool(items.iter().any(|x| value_eq(x, &args[0])))),
        ("apply", 1) => list_index(&items, args[0].to_int()),
        ("toList", 0) => Ok(new_list(items)),
        ("map", 1) => {
            let mut out = Vec::with_capacity(items.len());
            for it in &items {
                out.push(invoke_closure(vm, &args[0], std::slice::from_ref(it))?);
            }
            Ok(same(out))
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
            Ok(same(out))
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
            Ok(Value::Undef)
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
            Ok(same(out))
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
            let joined = items
                .iter()
                .map(scala_str)
                .collect::<Vec<_>>()
                .join(&args[1].as_str_cow());
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
        // `grouped`/`sliding` answer an `Iterator` in Scala; here they answer the
        // materialized `List` of windows (see `BUGS.md`), so the usual
        // `.toList`/`.foreach` consumption matches.
        ("grouped" | "sliding", 1) => {
            let n = args[0].to_int();
            if n < 1 {
                return Err(format!(
                    "scalars: java.lang.IllegalArgumentException: requirement failed: size={n}"
                ));
            }
            let n = n as usize;
            let mut out = Vec::new();
            if name == "grouped" {
                for chunk in items.chunks(n) {
                    out.push(same(chunk.to_vec()));
                }
            } else if items.len() < n {
                out.push(same(items.clone()));
            } else {
                for w in items.windows(n) {
                    out.push(same(w.to_vec()));
                }
            }
            Ok(new_list(out))
        }
        ("toSet", 0) => Ok(new_set(HashRep::Small, items)),
        ("toSeq" | "toIterable", 0) => Ok(new_list(items)),
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
            let ks = keys_of(vm, &args[0], &items)?;
            let mut idx: Vec<usize> = (0..items.len()).collect();
            idx.sort_by(|&a, &b| value_cmp(&ks[a], &ks[b]));
            Ok(same(idx.into_iter().map(|i| items[i].clone()).collect()))
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
                if value_cmp(&ks[i], &ks[best]) == want {
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
fn seq_product(items: &[Value]) -> Value {
    if items.iter().all(|v| matches!(v, Value::Int(_))) {
        Value::int(items.iter().map(Value::to_int).product())
    } else {
        Value::float(items.iter().map(Value::to_float).product())
    }
}

/// `Map` methods — a faithful subset. A method that answers another map keeps
/// the receiver's representation (a `HashMap` stays hashed however few entries
/// survive); one that answers a bare sequence answers a `List`, as Scala's
/// `Map.to*` do.
fn map_method(vm: &mut VM, recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    let (rep, entries) = map_rep_entries(recv).unwrap_or((HashRep::Small, Vec::new()));
    // Every closure-taking `Map` method passes one `Tuple2` argument.
    let pairs: Vec<Value> = entries
        .iter()
        .map(|(k, v)| new_pair(k.clone(), v.clone()))
        .collect();
    // In-place mutation, before the pure paths: on a `mutable.Map` the names
    // `+`/`-`/`++` still build a new map, but `+=`/`-=`/`update`/`put` mutate.
    if let HashRep::Mutable(len) = rep {
        if let Some(r) = mut_map_method(recv, len as usize, &entries, name, args) {
            return r;
        }
    }
    match (name, args.len()) {
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
            let mut out = Vec::with_capacity(pairs.len());
            for p in &pairs {
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
            for (p, e) in pairs.iter().zip(entries.iter()) {
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
            for (p, e) in pairs.iter().zip(entries.iter()) {
                if truthy(&invoke_closure(vm, &args[0], std::slice::from_ref(p))?) {
                    yes.push(e.clone());
                } else {
                    no.push(e.clone());
                }
            }
            Ok(new_pair(new_map(rep, yes), new_map(rep, no)))
        }
        ("head", 0) | ("last", 0) | ("headOption", 0) | ("lastOption", 0) => {
            let pick = if name.starts_with("head") {
                pairs.first()
            } else {
                pairs.last()
            };
            if name.ends_with("Option") {
                return Ok(opt(pick.cloned()));
            }
            pick.cloned().ok_or_else(|| {
                "scalars: java.util.NoSuchElementException: head of empty map".into()
            })
        }
        // Everything else that only reads the entries as a pair sequence is the
        // sequence implementation over `Map`'s `Tuple2` elements.
        (
            "exists" | "forall" | "count" | "find" | "collectFirst" | "foldLeft" | "foldRight"
            | "fold" | "reduce" | "maxBy" | "minBy" | "groupBy" | "toList" | "toSeq" | "toVector"
            | "toArray" | "toSet" | "mkString" | "sortBy" | "unzip" | "zipWithIndex",
            _,
        ) => {
            let seq = new_list(pairs);
            seq_method(vm, &seq, name, args)
        }
        ("toMap", 0) => Ok(recv.clone()),
        ("foreach", 1) => {
            for p in &pairs {
                invoke_closure(vm, &args[0], std::slice::from_ref(p))?;
            }
            Ok(Value::Undef)
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
        ("apply", 1) => map_get(entries, &args[0])
            .ok_or_else(|| format!("scalars: key not found: {}", scala_str(&args[0]))),
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
        (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => a
            .to_float()
            .partial_cmp(&b.to_float())
            .unwrap_or(Ordering::Equal),
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

/// `java.lang.String.compareTo` — by UTF-16 code unit, then by length.
fn java_str_cmp(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
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
        ("init", 0) => items[..len.saturating_sub(1)].to_vec(),
        ("distinct", 0) => {
            let mut out: Vec<Value> = Vec::with_capacity(len);
            for it in items {
                if !out.iter().any(|u| value_eq(u, it)) {
                    out.push(it.clone());
                }
            }
            out
        }
        ("sorted", 0) => {
            let mut out = items.to_vec();
            out.sort_by(value_cmp);
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
fn seq_sum(items: &[Value]) -> Value {
    if items.iter().all(|v| matches!(v, Value::Int(_))) {
        Value::int(items.iter().map(|v| v.to_int()).sum())
    } else {
        Value::float(items.iter().map(|v| v.to_float()).sum())
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
        return fault(vm, "scalars: java.lang.NegativeArraySizeException");
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
    let end = vm.pop().to_int();
    let start = vm.pop().to_int();
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
    let f = |i: usize| args[i].to_float();
    match (name, args.len()) {
        ("Pi" | "PI", 0) => Ok(Value::float(std::f64::consts::PI)),
        ("E", 0) => Ok(Value::float(std::f64::consts::E)),
        ("abs", 1) if ints => Ok(Value::int(args[0].to_int().abs())),
        ("abs", 1) => Ok(Value::float(f(0).abs())),
        ("signum", 1) if ints => Ok(Value::int(args[0].to_int().signum())),
        ("signum", 1) => Ok(Value::float(if f(0) == 0.0 { f(0) } else { f(0).signum() })),
        ("max", 2) if ints => Ok(Value::int(args[0].to_int().max(args[1].to_int()))),
        ("max", 2) => Ok(Value::float(f(0).max(f(1)))),
        ("min", 2) if ints => Ok(Value::int(args[0].to_int().min(args[1].to_int()))),
        ("min", 2) => Ok(Value::float(f(0).min(f(1)))),
        // `Math.round` is "floor(x + 0.5)", which is NOT `f64::round`
        // (`round(-2.5)` is `-2` on the JVM, `-3` in Rust).
        ("round", 1) => Ok(Value::int((f(0) + 0.5).floor() as i64)),
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
    heap_push(HeapVal::Seq(SeqKind::Set(HashRep::Mutable(len as u32)), {
        mut_ordered(&uniq, len, Clone::clone).unwrap_or(uniq)
    }))
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
    heap_push(HeapVal::Map(HashRep::Mutable(len as u32), {
        mut_ordered(&uniq, len, |(k, _)| k.clone()).unwrap_or(uniq)
    }))
}

/// Build an immutable `Map` from already-deduplicated `entries` — the `Set`
/// treatment of [`new_set`], keyed by the entry key.
fn new_map(rep: HashRep, entries: Vec<(Value, Value)>) -> Value {
    if matches!(rep, HashRep::Mutable(_)) {
        return mut_map_from(mut_table_size_for(MUT_INITIAL_CAPACITY), entries);
    }
    let mut entries = entries;
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
    if rep == HashRep::Hashed || len > 4 {
        HashRep::Hashed
    } else {
        HashRep::Small
    }
}

/// Build a built-in `Some(v)` case-class record.
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
        ("iterator", 0) => Ok(new_list(inner.into_iter().collect())),
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
            Ok(Value::Undef)
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
    // A `Regex`/`Regex.Match` handle is not a record, so it is answered before
    // the record dispatcher below.
    if let Some(r) = regex_method(recv, name, args) {
        return r;
    }
    // Scala's operators ARE methods, so the dotted spelling (`n.+(1)`, `"a".*(3)`)
    // is legal wherever the infix one is. Only the primitive receivers route here:
    // `+`/`-` on a `Set`/`Map` mean set inclusion/removal and stay with the
    // collection dispatcher below.
    if matches!(recv, Value::Str(_) | Value::Int(_) | Value::Float(_)) {
        if let Some(r) = operator_method(recv, name, args) {
            return r;
        }
    }
    match recv {
        Value::Str(s) => string_method(s, name, args),
        Value::Int(n) => int_method(*n, name, args),
        Value::Float(f) => double_method(*f, name, args),
        Value::Bool(b) => bool_method(*b, name, args),
        Value::Obj(_) => obj_method(recv, name, args),
        _ => Err(no_such_method(recv, name)),
    }
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
                let (x, y) = (recv.to_float(), b.to_float());
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
        "+" => Ok(Value::float(recv.to_float() + b.to_float())),
        "-" => Ok(Value::float(recv.to_float() - b.to_float())),
        "*" => Ok(Value::float(recv.to_float() * b.to_float())),
        "/" => Ok(Value::float(recv.to_float() / b.to_float())),
        _ => Ok(Value::float(recv.to_float() % b.to_float())),
    })
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
        ("toList", 0) => Ok(new_list(
            s.chars().map(|c| Value::str(c.to_string())).collect(),
        )),
        // `"abc".toSeq` is a `WrappedString` — a VIEW of the same characters,
        // which prints as the string itself (`abc`, not `List(a, b, c)`) and
        // answers every `Seq` operation through `StringOps`. The string is
        // exactly that view here.
        ("toSeq", 0) => Ok(Value::str(s)),
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
                // `java.lang.String.charAt`'s exact JDK message.
                Err(format!(
                    "scalars: java.lang.StringIndexOutOfBoundsException: Index {i} out of bounds for length {}",
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
        ("head", 0) => s
            .chars()
            .next()
            .map(|c| Value::str(c.to_string()))
            .ok_or_else(|| {
                "scalars: java.util.NoSuchElementException: head of empty String".to_string()
            }),
        ("last", 0) => s
            .chars()
            .next_back()
            .map(|c| Value::str(c.to_string()))
            .ok_or_else(|| {
                "scalars: java.lang.UnsupportedOperationException: last of empty String".to_string()
            }),
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
        ("toCharArray", 0) => Ok(new_seq(
            SeqKind::Array,
            s.chars().map(|c| Value::str(c.to_string())).collect(),
        )),
        // `StringOps.zipWithIndex` answers an `IndexedSeq`, printed `Vector(…)`.
        ("zipWithIndex", 0) => Ok(new_seq(
            SeqKind::Vector,
            s.chars()
                .enumerate()
                .map(|(i, c)| {
                    heap_push(HeapVal::Tuple(vec![
                        Value::str(c.to_string()),
                        Value::int(i as i64),
                    ]))
                })
                .collect(),
        )),
        // A one-char string stands in for `Char` (the lexer gives `'a'` that
        // shape), so `Char`'s predicates live here. `String` has none of these
        // names, so a valid Scala program can never reach them on a real string.
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
            | "reverse" | "toInt" | "toDouble" | "charAt" | "contains" | "startsWith" | "endsWith"
            | "substring",
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
                    let i = args[0].to_int();
                    if i == 0 {
                        Ok(Value::str(matched.to_string()))
                    } else {
                        match usize::try_from(i).ok().and_then(|i| groups.get(i - 1)) {
                            Some(Some(g)) => Ok(Value::str(g.to_string())),
                            Some(None) => Ok(Value::Undef),
                            None => Err(format!(
                                "scalars: java.lang.IndexOutOfBoundsException: No group {i}"
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
            })
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
/// The element type is a one-char `String` (this frontend's `Char` model).
/// `map` therefore decides its result type by inspecting the results: all
/// one-char strings rebuild a `String` (Scala's `Char => Char` overload), and
/// anything else answers an `IndexedSeq`, printed `Vector(…)`. The one shape
/// that model gets wrong is `s.map(_.toString)`, whose Scala result is a
/// `Vector` of one-char strings — recorded in `BUGS.md`.
fn string_fn_method(
    vm: &mut VM,
    s: &str,
    name: &str,
    args: &[Value],
) -> Option<Result<Value, String>> {
    let chars: Vec<Value> = s.chars().map(|c| Value::str(c.to_string())).collect();
    macro_rules! call {
        ($x:expr) => {
            match invoke_closure(vm, &args[0], std::slice::from_ref($x)) {
                Ok(v) => v,
                Err(e) => return Some(Err(e)),
            }
        };
    }
    // Re-join a char run into the `String` the receiver's type implies.
    let join = |xs: &[Value]| {
        Value::str(
            xs.iter()
                .map(|v| v.as_str_cow().into_owned())
                .collect::<String>(),
        )
    };
    if args.len() != 1 {
        return None;
    }
    Some(match name {
        // `StringOps.map` has two overloads: `Char => Char` rebuilds a `String`,
        // `Char => B` builds an `IndexedSeq[B]`. Scala reads that off the
        // function's static result type. With `Char` modeled as a one-char
        // `String` the results alone cannot tell the two apart when `B` is
        // itself `String` (`_.toString`), so a body whose result is statically a
        // `String` is flagged at compile time (`compiler::yields_strings`) and
        // takes the `Char => B` branch; everything else reads the results.
        "map" => {
            let string_body = as_closure(&args[0]).is_some_and(|c| c.string_body);
            let mut out = Vec::with_capacity(chars.len());
            for c in &chars {
                out.push(call!(c));
            }
            Ok(
                if !string_body
                    && out
                        .iter()
                        .all(|v| matches!(v, Value::Str(t) if one_char(t).is_some()))
                {
                    join(&out)
                } else {
                    new_seq(SeqKind::ArraySeq, out)
                },
            )
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
        ("toDouble" | "toFloat", 0) => Ok(Value::float(n as f64)),
        ("toInt" | "toLong", 0) => Ok(Value::int(n)),
        ("max", 1) => Ok(Value::int(n.max(args[0].to_int()))),
        ("min", 1) => Ok(Value::int(n.min(args[0].to_int()))),
        ("abs" | "toDouble" | "toFloat" | "toInt" | "toLong" | "max" | "min", _) => {
            Err(format!("scalars: Int.{name}: wrong number of arguments"))
        }
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
        _ => Err(no_such_method(&Value::bool(b), name)),
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
        // A host-heap instance: `Class(f0,f1)` for a `case` type, `Class@hex`
        // for a plain class (see [`obj_to_string`]).
        Value::Obj(_) => obj_to_string(v),
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
/// non-numeric operand. That is Scala's `String` `+` overload plus
/// value comparisons against strings; all-numeric arithmetic never reaches here
/// (it stays on the native fast path and the JIT).
pub fn numeric_hook(op: NumOp, a: &Value, b: &Value) -> Result<Value, String> {
    // Suppressed while unwinding: an operand is the `Undef` a raise left behind,
    // and Scala 3's strict `+`/comparison rules would reject it as a type error,
    // displacing the real exception (see `Exception unwinding`).
    if unwinding() {
        return Ok(Value::Undef);
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
