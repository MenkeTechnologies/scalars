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

use fusevm::{Frame, NumOp, VMResult, Value, VM};
use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
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
/// holds the closure body's name-pool index and its parameter count (two
/// integers), then the captured upvalue values (deepest first); `argc` is
/// capture-count + 2. Registers a heap `Closure` and returns its `Value::Obj`
/// handle (invoked later via `invoke_closure`).
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
/// Builtin id for materializing an integer range as a `List` (a range generator
/// inside a desugared collection for-comprehension). The stack holds `start`,
/// `end`, and an `inclusive` `Bool` (top); `argc` is 3. Step is 1.
pub const RANGE_LIST: u16 = 720;

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
    vm.register_builtin(OBJ_NEW, b_obj_new);
    vm.register_builtin(OBJ_CLASS, b_obj_class);
    vm.register_builtin(OBJ_COPY, b_obj_copy);
    vm.register_builtin(OBJ_SET, b_obj_set);
    vm.register_builtin(MAKE_CLOSURE, b_make_closure);
    vm.register_builtin(APPLY, b_apply);
    vm.register_builtin(MAKE_LIST, b_make_list);
    vm.register_builtin(MAKE_MAP, b_make_map);
    vm.register_builtin(MAKE_TUPLE, b_make_tuple);
    vm.register_builtin(LIST_CONS, b_list_cons);
    vm.register_builtin(RANGE_LIST, b_range_list);
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
    /// An insertion-ordered map (matches `Map1`..`Map4`'s stable order and
    /// `m + (k -> v)` mutation semantics — a new key appends, an existing key
    /// keeps its position with the new value).
    Map(Vec<(Value, Value)>),
    /// A tuple (`(a, b)`, `a -> b`).
    Tuple(Vec<Value>),
    /// A first-class function value (lambda) — see [`Closure`].
    Closure(Closure),
}

/// The rendered prefix of a [`HeapVal::Seq`].
#[derive(Clone, Copy, PartialEq)]
enum SeqKind {
    List,
    Vector,
    Set,
    Iterable,
}

impl SeqKind {
    fn label(self) -> &'static str {
        match self {
            SeqKind::List => "List",
            SeqKind::Vector => "Vector",
            SeqKind::Set => "Set",
            SeqKind::Iterable => "Iterable",
        }
    }
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
    if let Value::Obj(id) = v {
        HEAP.with(|h| match h.borrow().get(*id as usize) {
            Some(HeapVal::Map(m)) => Some(m.clone()),
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

/// `MAKE_CLOSURE` builtin — pop the capture count (`argc - 2`), then the
/// parameter count and body name index, then the captured upvalue values
/// (deepest first). Registers a heap [`Closure`] and returns its handle.
fn b_make_closure(vm: &mut VM, argc: u8) -> Value {
    let ncap = (argc as usize).saturating_sub(2);
    let mut captures = Vec::with_capacity(ncap);
    for _ in 0..ncap {
        captures.push(vm.pop());
    }
    captures.reverse();
    let params = vm.pop().to_int() as u8;
    let name_idx = vm.pop().to_int() as u16;
    heap_push(HeapVal::Closure(Closure {
        name_idx,
        params,
        captures,
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

/// `MAKE_TUPLE` builtin — pop `argc` element values (deepest first) into a tuple.
fn b_make_tuple(vm: &mut VM, argc: u8) -> Value {
    let mut items = Vec::with_capacity(argc as usize);
    for _ in 0..argc {
        items.push(vm.pop());
    }
    items.reverse();
    heap_push(HeapVal::Tuple(items))
}

/// `MAKE_MAP` builtin — pop `argc` `Tuple2` pair values (deepest first) into an
/// insertion-ordered map. A duplicate key keeps its first position with the last
/// value (Scala's `Map(a -> 1, a -> 2)` == `Map(a -> 2)`).
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
    heap_push(HeapVal::Map(entries))
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

/// `RANGE_LIST` builtin — materialize `[start .. end]` (inclusive when the top
/// `Bool` is `true`) as a step-1 `List`.
fn b_range_list(vm: &mut VM, _argc: u8) -> Value {
    let inclusive = matches!(vm.pop(), Value::Bool(true));
    let end = vm.pop().to_int();
    let start = vm.pop().to_int();
    let last = if inclusive { end } else { end - 1 };
    let items = (start..=last).map(Value::int).collect();
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
    match apply_value(vm, &recv, &args) {
        Ok(v) => v,
        Err(e) => fault(vm, e),
    }
}

/// Dispatch `recv(args)`: closure invocation, list/tuple indexing, or map lookup.
fn apply_value(vm: &mut VM, recv: &Value, args: &[Value]) -> Result<Value, String> {
    if as_closure(recv).is_some() {
        return invoke_closure(vm, recv, args);
    }
    if let Value::Obj(id) = recv {
        let kind = HEAP.with(|h| {
            h.borrow().get(*id as usize).map(|o| match o {
                HeapVal::Seq(..) => 0u8,
                HeapVal::Tuple(_) => 1,
                HeapVal::Map(_) => 2,
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
fn list_index(items: &[Value], i: i64) -> Result<Value, String> {
    if i < 0 || i as usize >= items.len() {
        Err(format!(
            "scalars: java.lang.IndexOutOfBoundsException: {i} (length {})",
            items.len()
        ))
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
    let meta = as_closure(clo).ok_or_else(|| "scalars: value is not a function".to_string())?;
    let entry = vm
        .chunk
        .find_sub(meta.name_idx)
        .ok_or_else(|| "scalars: closure body not found".to_string())?;
    let want = meta.params as usize;
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
        // A paren-less access naming a field reads that field.
        (_, 0) => match fields.iter().find(|(fname, _)| &**fname == name) {
            Some((_, v)) => Ok(v.clone()),
            None => Err(no_such_obj_member(&class, name)),
        },
        _ => Err(no_such_obj_member(&class, name)),
    }
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
                    let inner = o
                        .fields
                        .iter()
                        .map(|(_, val)| scala_str(val))
                        .collect::<Vec<_>>()
                        .join(",");
                    format!("{}({})", o.class, inner)
                } else {
                    format!("{}@{:x}", o.class, id)
                }
            }
            Some(HeapVal::Seq(kind, items)) => {
                let inner = items.iter().map(scala_str).collect::<Vec<_>>().join(", ");
                format!("{}({inner})", kind.label())
            }
            Some(HeapVal::Map(entries)) => {
                let inner = entries
                    .iter()
                    .map(|(k, val)| format!("{} -> {}", scala_str(k), scala_str(val)))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("Map({inner})")
            }
            Some(HeapVal::Tuple(items)) => {
                let inner = items.iter().map(scala_str).collect::<Vec<_>>().join(",");
                format!("({inner})")
            }
            Some(HeapVal::Closure(c)) => format!("<function{}>", c.params),
            None => "null".to_string(),
        }
    })
}

/// A `case` instance's structural hash (equal instances hash equal). A plain
/// instance hashes by handle identity. Masked to a non-negative 31-bit value so
/// it reads like a JVM `hashCode` (the exact bits are not observable-equal to the
/// JVM's MurmurHash, only the equal-implies-equal contract is).
fn obj_hash(class: &str, is_case: bool, fields: &[(Arc<str>, Value)], v: &Value) -> i64 {
    let mut h = DefaultHasher::new();
    if is_case {
        class.hash(&mut h);
        for (_, val) in fields {
            hash_value(val, &mut h);
        }
    } else if let Value::Obj(id) = v {
        id.hash(&mut h);
    }
    (h.finish() & 0x7fff_ffff) as i64
}

/// Hash a field value structurally (recursing into nested `case` objects) so two
/// equal records hash identically.
fn hash_value(v: &Value, h: &mut DefaultHasher) {
    match v {
        Value::Obj(_) => {
            with_obj(v, |o| {
                o.class.hash(h);
                for (_, val) in &o.fields {
                    hash_value(val, h);
                }
            });
        }
        _ => scala_str(v).hash(h),
    }
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
                    oa.is_case
                        && ob.is_case
                        && oa.class == ob.class
                        && oa.fields.len() == ob.fields.len()
                        && oa
                            .fields
                            .iter()
                            .zip(&ob.fields)
                            .all(|((_, x), (_, y))| value_eq(x, y))
                }
                // Two sequences/tuples are equal element-by-element (a `List`
                // equals a `Set` only if both order and elements match — good
                // enough for the collections this frontend builds).
                (HeapVal::Seq(_, xa), HeapVal::Seq(_, xb))
                | (HeapVal::Tuple(xa), HeapVal::Tuple(xb)) => {
                    xa.len() == xb.len() && xa.iter().zip(&xb).all(|(x, y)| value_eq(x, y))
                }
                (HeapVal::Map(ma), HeapVal::Map(mb)) => {
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
    // A range `for … yield` result is a fusevm `Value::Array` (rendered as
    // `Vector`); promote it to a heap `Vector` seq so the collection methods
    // (`map`, `toList`, …) apply uniformly.
    let recv = match &recv {
        Value::Array(items) => heap_push(HeapVal::Seq(SeqKind::Vector, items.clone())),
        _ => recv,
    };

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
                Some(HeapVal::Seq(..) | HeapVal::Map(_) | HeapVal::Tuple(_) | HeapVal::Closure(_))
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
                HeapVal::Map(_) => 1,
                HeapVal::Tuple(_) => 2,
                HeapVal::Closure(_) => 3,
                HeapVal::Record(_) => 4,
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
    match heap_kind(recv) {
        Some(0) => seq_method(vm, recv, name, args),
        Some(1) => map_method(vm, recv, name, args),
        Some(2) => tuple_method(recv, name, args),
        Some(3) => closure_method(vm, recv, name, args),
        _ => Err(no_such_method(recv, name)),
    }
}

/// `Seq` (`List`/`Set`/`Iterable`) methods — a faithful subset. Closure-taking
/// ops run their function argument through [`invoke_closure`].
fn seq_method(vm: &mut VM, recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    let (kind, items) = seq_kind_items(recv).unwrap_or((SeqKind::List, Vec::new()));
    // A transforming op keeps the receiver's collection kind (`List.map` → `List`,
    // a range-derived `Vector.map` → `Vector`).
    let same = |v: Vec<Value>| new_seq(kind, v);
    match (name, args.len()) {
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
        _ => Err(no_such_method(recv, name)),
    }
}

/// `Map` methods — a faithful subset.
fn map_method(vm: &mut VM, recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    let entries = as_map(recv).unwrap_or_default();
    match (name, args.len()) {
        ("size", 0) => Ok(Value::int(entries.len() as i64)),
        ("isEmpty", 0) => Ok(Value::bool(entries.is_empty())),
        ("nonEmpty", 0) => Ok(Value::bool(!entries.is_empty())),
        ("contains", 1) => Ok(Value::bool(map_get(&entries, &args[0]).is_some())),
        ("apply", 1) => map_get(&entries, &args[0])
            .ok_or_else(|| format!("scalars: key not found: {}", scala_str(&args[0]))),
        ("get", 1) => Ok(match map_get(&entries, &args[0]) {
            Some(v) => make_some(v),
            None => make_none(),
        }),
        ("getOrElse", 2) => Ok(map_get(&entries, &args[0]).unwrap_or_else(|| args[1].clone())),
        ("keys" | "keySet", 0) => Ok(new_seq(
            SeqKind::Set,
            entries.iter().map(|(k, _)| k.clone()).collect(),
        )),
        ("values", 0) => Ok(new_seq(
            SeqKind::Iterable,
            entries.iter().map(|(_, v)| v.clone()).collect(),
        )),
        ("toList", 0) => Ok(new_list(
            entries
                .iter()
                .map(|(k, v)| heap_push(HeapVal::Tuple(vec![k.clone(), v.clone()])))
                .collect(),
        )),
        ("foreach", 1) => {
            for (k, v) in &entries {
                let pair = heap_push(HeapVal::Tuple(vec![k.clone(), v.clone()]));
                invoke_closure(vm, &args[0], std::slice::from_ref(&pair))?;
            }
            Ok(Value::Undef)
        }
        _ => Err(no_such_method(recv, name)),
    }
}

/// `Tuple` methods — element accessors `_1`/`_2`/… and indexing.
fn tuple_method(recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    let items = as_seq_or_tuple(recv).unwrap_or_default();
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
    match name {
        "apply" | "call" => invoke_closure(vm, recv, args),
        _ => Err(no_such_method(recv, name)),
    }
}

/// Scala truthiness of a closure result used as a predicate (`Boolean`).
fn truthy(v: &Value) -> bool {
    matches!(v, Value::Bool(true))
}

/// Sum a numeric sequence (`Int` result when all `Int`, else `Double`).
fn seq_sum(items: &[Value]) -> Value {
    if items.iter().all(|v| matches!(v, Value::Int(_))) {
        Value::int(items.iter().map(|v| v.to_int()).sum())
    } else {
        Value::float(items.iter().map(|v| v.to_float()).sum())
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
        Value::Obj(_) => obj_method(recv, name, args),
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
            // `map + (k -> v)` — a new map with the pair added (`Map`'s `+`).
            Value::Obj(_) if as_map(a).is_some() => {
                let mut entries = as_map(a).unwrap();
                match as_seq_or_tuple(b) {
                    Some(t) if t.len() == 2 => {
                        map_put(&mut entries, t[0].clone(), t[1].clone());
                        Ok(heap_push(HeapVal::Map(entries)))
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
