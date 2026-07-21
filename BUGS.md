# Known gaps

An honest list of what scalars does **not** do yet. Slice 1 is a single-object,
single entry-point subset; unsupported constructs are reported as parse errors,
never silently mis-run.

## Supported

- **User-defined methods (`def`).** Helper `def`s alongside `main` (or inside an
  `extends App` body) are compiled to fusevm's native `Op::Call` frame ABI:
  parameters bind to per-call frame slots, recursion and mutual recursion work,
  the body's last expression (including a tail `if`/`else`) is the result, and
  `return` performs an early exit. A zero-parameter `def` is callable paren-less
  (`def x = …; x`). Parameter types and return types are parsed but not checked.
- **Postfix method dispatch on core values.** `s.length`/`.size`,
  `.toUpperCase`/`.toLowerCase`, `.trim`, `.reverse`, `.isEmpty`/`.nonEmpty`,
  `.substring`, `.charAt`, `.contains`/`.startsWith`/`.endsWith`,
  `.toInt`/`.toDouble` on `String`; `.abs`/`.min`/`.max`/`.toDouble` on `Int`;
  `.abs`/`.round`/`.isNaN`/`.toInt` on `Double`; and `.toString` on any value.
  Method chaining works (`s.trim.length`). Out-of-range/parse failures throw
  faithfully (`StringIndexOutOfBoundsException`, `NumberFormatException`).
- **Classes, objects, `case class` (host-side object model).** `class C(x: Int)
  { def m = … }` with `new C(…)`, constructor params + body `val`/`var` fields,
  `this`, in-place `var`-field mutation, and instance-method dispatch (runtime
  class-tag dispatch off a `Value::Obj` handle into the frontend heap in
  `src/host.rs`). `object` singletons dispatch `def`s statically and expose
  `val`s as `Name.val`. `case class` derives an ordered-field `toString`
  (`Point(1,2)`), structural `equals`/`hashCode`, `copy(field = …)` (named and
  positional), a companion `apply` (construct without `new`), and `unapply` —
  wired into constructor patterns `case Point(x, y) =>`, nested and guarded.
  Built-in `Option` (`Some(v)`, the `None` case object) rides the same model.
  Plain (non-`case`) classes use reference-identity `equals`/`hashCode` and a
  `Class@hex` `toString`, matching Scala.

## Not implemented (parse errors / unresolved today)

- **Traits, inheritance, generics.** `trait`, `extends`/`with` linearization
  (parsed and ignored), type parameters (parsed and ignored), abstract members.
  A class's `val`/private access modifiers are parsed but not enforced (every
  field is reachable — a documented simplification).
- **Mutable / other collections.** `Array`, `Seq`/`Vector`/`Set` literals,
  `mutable.*`, and the many collection methods beyond the wired subset. The
  immutable `List` and `Map` are modeled host-side (`List(…)`, `Nil`, `::`,
  `.head`/`.tail`/`.length`/`.isEmpty`/`.reverse`/`.contains`/indexing,
  `.map`/`.filter`/`.flatMap`/`.foreach`/`.foldLeft`/`.foldRight`/`.reduce`/
  `.sum`/`.mkString`/`.exists`/`.forall`/`.count`; `Map(k -> v)`, `.apply`/`.get`/
  `.contains`/`.keys`/`.values`/`.size`/`getOrElse`/`+`); a range-led
  comprehension yields a `Vector`. The `args` parameter of `main` is parsed and
  ignored.
- **The wider standard library.** `math`, `scala.io`, `scala.collection.*`, and
  the many `String`/numeric methods beyond the wired subset above.
- **`if`/`else` as an expression in operand position.** `val r = if (c) a else b`
  is rejected; a tail `if`/`else` as a whole `def` body is supported.
- **`by` step in ranges** — `for (i <- a until|to b)` runs with step 1 only.
- **`do/while`, `try`/`catch`/`finally`, `throw` (user-raised).**
- **By-name params, `given`/`using`, generics, `@main` (Scala 3 annotation
  entry).**

## Modeled with a documented simplification

- **Types are not checked.** Declared types (`Int`, `String`, …) are retained for
  diagnostics but do not gate execution — the runtime is dynamically typed on the
  fusevm value model. Type errors that `scalac` would reject may run.
- **`==` on non-numbers compares by value** (structural), which matches Scala's
  `==`/`equals` for the strings and booleans slice 1 handles. Reference identity
  (`eq`) is not modeled.
- **`char` literals are one-character strings**, not an integer `Char` type.
- **Integer arithmetic uses fusevm's 64-bit semantics.** Scala `Int` 32-bit
  overflow wrapping is not modeled (values behave like `Long`).
- **`/` is dispatched at runtime, not by static type.** Scala picks integer vs.
  floating division from the *static* operand types; slice 1 carries no static
  types, so the [`host::b_div`] builtin decides at runtime — both operands `Int`
  truncate, otherwise a double divide. This matches Scala for every monomorphic
  case (`7 / 2 == 3`, `7 / 2.0 == 3.5`), and floating `/0.0` yields `Infinity` as
  Scala does.
- **Integer division by zero throws** `java.lang.ArithmeticException: / by zero`,
  matching Scala/the JVM `idiv` trap. Since there is no `try`/`catch` yet, the
  uncaught throw aborts the program with that message on stderr. Floating `/0.0`
  is `Infinity` (not an exception), also matching Scala.
- **Uninitialized bindings are unbound** rather than rejected; reading one before
  assignment yields `null` instead of a compile error.
- **`object … extends App` runs the object body directly.** Statements run in
  order and any `def` members are hoisted and callable; other member kinds
  (fields, nested types) are skipped.

## Object model: how it works

`case class` needs an **ordered, type-tagged record** so `toString` renders
`Point(1,2)` in declared field order and structural `equals`/`hashCode` compare
field-by-field. fusevm's `Value::Obj(u32)` is an opaque handle into a
*frontend-owned* object heap; scalars now owns that heap (`src/host.rs`): a
`Value::Obj` indexes a per-run arena of records (`class name`, `is_case`,
`is_object`, ordered `(field, value)` list). Construction, field access,
`toString`/`equals`/`hashCode`, `copy`, and constructor-pattern binding are host
extension builtins (`OBJ_NEW`/`OBJ_CLASS`/`OBJ_COPY`/`OBJ_SET`, plus the `Obj`
arm of `SMETHOD` and the `==` numeric hook) — no fusevm changes. Every
construction site bakes the class name + ordered field names into the bytecode,
so no runtime class registry is needed; instance-method calls resolve by a
runtime class-tag dispatch chain the compiler emits from compile-time class
knowledge.
