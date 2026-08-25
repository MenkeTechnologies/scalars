# Known gaps

An honest list of what scalars does **not** do yet. Unsupported constructs are
reported as parse/compile errors, never silently mis-run.

## Supported

- **All three Scala entry points, and top-level definitions.** `object T extends
  App { … }`, `object T { def main(args: Array[String]) = … }`, and Scala 3's
  `@main def go(…)`. Top-level `def`s and `val`s — the members Scala 3 collects
  into a synthetic `Foo$package` object — are hoisted into the same flat
  function namespace an entry object's helpers use, and a top-level `val`'s
  initializer runs BEFORE the entry body and before the command line is read.

  An `extends App` body's `val`s are FIELDS of the entry object, so a forward
  reference to one reads the JVM field default rather than an absent value:
  `println(later); val later: Int = 7` prints `0`, and `0.0`/`false`/`null` for
  a `Double`/`Boolean`/reference type. The declared type answers that (the width
  analysis could not — `NumTy` separates `Int` from `Long` and nothing else); an
  unannotated binding falls back to the shape of its initializer. A `val` inside
  a `def` is a local, and Scala rejects a forward reference to one outright, so
  no default applies there.

  A `@main` method's parameters come from the command line through the reader
  Scala generates, `CommandLineParser.parseArgument[T]`, for the types that have
  one and are representable here: `Int`, `Long`, `Byte`, `Short`, `Double`,
  `Boolean`, `String`. That reader's failure behaviour is unlike anything else in
  the language and is reproduced exactly — the diagnostic goes to **stdout** and
  the process exits **0**:

  ```
  Illegal command line: more arguments expected
  Illegal command line after first argument: java.lang.NumberFormatException: For input string: "zz"
  Illegal command line after 2 arguments: java.lang.NumberFormatException: Value out of range. Value:"300" Radix:10
  Illegal command line after 4 arguments: java.lang.IllegalArgumentException: For input string: "yes"
  ```

  Extra arguments are ignored, as Scala ignores them. `def main`'s `args` is the
  real argument vector; `extends App`'s `args` is deliberately left null, because
  that is what the reference does — the `DelayedInit` body runs during the
  object's `<clinit>`, ahead of the field, so `args.length` there raises
  `NullPointerException` on both sides.
- **`apply` — `receiver(args)` — wherever Scala writes it.** The receiver may be
  a `List`/`Array`/`Vector` (indexing), a `Map` (lookup), a `String` (`s(i)`, i.e.
  `charAt`), or a function value; it may be a literal (`"pear"(1)`), a top-level
  or block `val`, a `def` parameter, a lambda parameter, a captured binding read
  from inside a lambda or a `def` body, or a FIELD of a record (`r.name(0)` is
  `r.name.apply(0)`, not a method named `name`). A selector chains onto the
  result (`"pear"(1).toUpper`), and applications compose (`getFn()(arg)`). The
  placeholder participates on both sides: `_(i)` applies the placeholder
  (`xs.map(_(1))`), and a bare `_` as an argument eta-expands the ENCLOSING call
  (`xs.map(f(_))` is `xs.map(x => f(x))`, per Scala's "smallest expression
  properly containing the underscore" rule) instead of passing the identity
  function. Covered by the parity fuzzer's `apply` mode.
- **The BRACE form of an argument, and `_` inside it.** `xs.map { _ * 2 }`,
  `xs.foldLeft(0) { _ + _ }`, `xs.sortBy { -_ }`, `xs.map { x => … }` and
  `{ case … }` are all the same substitution: a `{ … }` group standing in for a
  parenthesized argument clause. It works on a method (`xs.map { … }`), on a
  plain `def` call (`once { 7 }`), and on the trailing clause of a CURRIED `def`
  (`use(3) { _ + 1 }`, where the braces are a second argument clause and not an
  `apply` on what the first clause returned — the compiler rejoins the clauses
  when the callee's declared arity is exactly what they supply, so
  `def mk(n: Int): Int => Int` called `mk(1)(2)` still dispatches through
  `apply`).

  A brace group is a BLOCK whose value is the argument, so it may hold several
  statements and is evaluated ONCE, to produce the function — `xs.map { c += 1;
  _ + 1 }` increments `c` once, not once per element. The block's value is its
  trailing expression, including a trailing `if` (`xs.map { x => if (p(x)) a else
  b }`).

  A placeholder expands at the smallest expression that PROPERLY contains it, and
  a block statement is one of those boundaries — which is what makes the brace
  and parenthesized spellings mean the same function. So is a `val`'s initializer
  (`val f: Int => Int = _ + 1`). Two consequences follow, both matching Scala
  3.8.4: `(_: Int) + 1` is the TYPED placeholder — the parentheses carry the
  ascription, not the function boundary, so the whole thing is `x => x + 1` — and
  a statement that is nothing but `_` (`xs.map { _ }`) is refused, since there is
  no containing expression to expand at ("Unbound placeholder parameter" in
  Scala). Covered by the parity fuzzer's `braces` mode.
- **User-defined methods (`def`), lexically scoped.** Helper `def`s alongside
  `main` (or inside an `extends App` body) are compiled to fusevm's native
  `Op::Call` frame ABI: parameters bind to per-call frame slots, recursion and
  mutual recursion work, the body's last expression (including a tail
  `if`/`else`) is the result, and `return` performs an early exit. A
  zero-parameter `def` is callable paren-less (`def x = …; x`). Parameter types
  and return types are parsed but not checked.
- **Block-local `def`s.** A `def` declared inside a block belongs to that block:
  two blocks may each declare `def f`, an inner one shadows an outer one, a
  later `val f` shadows both, and mutually recursive local `def`s (plus forward
  references inside one block) resolve. Implemented in `src/resolve.rs`, which
  uniquely renames each block-local `def` and **lambda-lifts** whatever
  enclosing-frame locals its body reads into extra trailing parameters that
  every call site passes; capture sets propagate through calls to a fixpoint.
- **Type ascription in expression position, and the unit literal.** Both the
  parenthesised `(e: T)` and the bare `e: T` parse wherever an expression does
  (`(None: Option[Int])`, `f(xs: Seq[Int])`, `val n = 3: Long`), which is what
  Scala's `Expr1 ::= PostfixExpr Ascription` allows. In an argument the spread
  `xs: _*` is recognized first, so it stays a spread rather than becoming an
  ascription of the type `_*`.
  The runtime is dynamically typed, so the annotation is dropped — except for a
  numeric widening, which is observable WITHOUT a type checker and so is
  performed: `(3: Double)` answers `3.0`, not `3`. `()` is the `Unit` value; it
  lowers to an empty tuple, which is exactly what Scala prints it as.
- **A `y = e` value definition in a `for` comprehension.**
  `for { x <- xs; y = f(x); if y > 0 } yield y`. Over a *range* generator it
  lowers inline (a store into a loop-body slot, so the counted loop is kept);
  over a *collection* generator it takes Scala's own translation, which pairs the
  value onto the generator (`for ((x, y) <- xs.map(x => (x, f(x))))`), so a later
  guard and the body both see the defined name. A run of definitions nests one
  more pair each.
- **Regular expressions (`java.util.regex` / `scala.util.matching`).**
  `String.matches`/`replaceAll`/`replaceFirst`, and the regex-based
  `String.split`; `"…".r` and `new Regex(…)` building a `Regex` that answers
  `findFirstIn`, `findAllIn`, `findFirstMatchIn`, `findAllMatchIn`,
  `replaceAllIn`, `replaceFirstIn`, `matches`, `split` and `regex`; and
  `Regex.Match` with `group`/`subgroups`/`matched`. A `Regex` also works in
  PATTERN position via `unapplySeq` — `case r(a, b) => …`, which matches the
  WHOLE input (so `"a1"` does not match `"([0-9]+)".r`) and binds `null` for a
  group that did not participate. A replacement's `$N` splices are Java's,
  including its "longest group number that exists" rule and its
  `IndexOutOfBoundsException` for a group the pattern does not have. The match
  scan follows `java.util.regex.Matcher.find` rather than the Rust iterator: Java
  resumes AT the previous match's end (so an empty match is allowed there) and
  only steps forward a character after an empty match — which is what makes
  `"xx9".split("x*")` answer `["", "", "9"]` and `"abc".split("")` answer
  `["a", "b", "c"]`. Backed by `fancy-regex`, whose backtracking engine accepts
  the lookaround and backreferences `java.util.regex` has and the `regex` crate
  rejects.
- **A closure that ASSIGNS an enclosing `var`.** `var t = 0;
  xs.foreach(x => t += x)` inside a `def` reaches the declaring frame. Captures
  are threaded by value, so such a binding is BOXED into a shared heap cell
  (`host::CELL_NEW`/`CELL_GET`/`CELL_SET`) exactly as Scala boxes it into a
  `scala.runtime.IntRef`; the closure captures the cell handle, so the write is
  shared, and it may outlive the frame (`def mk() = { var i = 0; () => { i += 1;
  i } }`). Only the vars a closure actually writes are boxed
  (`compiler::boxed_vars`), so every other local keeps its plain frame slot and a
  program with no mutating closure emits the bytecode it did before.
- **Every operator is a method.** `+ - * / % < > <= >= == !=` dispatch the same
  whether written infix or dotted (`n.+(1)`, `7./(2)`, `"a".*(3)`), including
  `/`'s Int-vs-Double split and its zero-divisor throw. `s * n` is
  `StringOps.*` in both spellings.
- **Compound assignment to a target that is not a plain name.** `a(i) += 1`,
  `m(k) *= 2`, `counts(w) += 1`, `g(0)(1) += 9`, `obj.field += 5` and
  `obj.items += 9`, in all of `+= -= *= /= %=` plus `++=`/`--=`. Scala resolves
  `l op= r` by preferring an `op=` **member** on `l` and falling back to
  `l = l op r`, which for an *application* target expands to
  `l.update(args, l.apply(args) op r)` (SLS 6.12.4). Both halves are taken: an
  `Int` element takes the arithmetic expansion through `update`, while an
  element that is itself growable (`m(k) += 7` over `ListBuffer` values, or a
  selection like `nb.head += 3`) takes the member call and mutates in place, so
  no write-back is emitted and the receiver need not be a record at all. The
  receiver and every index are evaluated exactly ONCE, into temporaries, because
  Scala evaluates them once and they may have side effects
  (`counts(next()) += 1` advances one step). The choice between the two halves is
  the same run-time `IS_GROWABLE` test a plain-name `+=` already took, so a
  program with no mutable collection still emits exactly the arithmetic it did
  before. Covered by the parity fuzzer's `placeassign` mode.
- **Plain `=` to a selection, and to a singleton's `var`.** `obj.field = v`,
  `bs(1).n = 5`, `mk(b).n = 3`, and `Cfg.n = 10` / `Cfg.n += 5` / `Cfg.s += "!"`
  for an `object Cfg { var n = … }`. `recv.field = v` used to be a parse error
  (`unexpected token Assign in expression`), which left a `var` field writable
  only from inside its own class; it takes the same target path the `op=` forms
  take, with `=` as the operator, so the receiver is still evaluated exactly
  once. A singleton's `var` is the one target that is NOT a record field —
  reading `Cfg.n` lowers to `GetVar("Cfg.n")` — so every operator form routes to
  that global rather than to the heap builtins, which is what makes an outside
  write visible to the object's own `def`s (`Cfg.n = 10; Cfg.bump()` answers
  `11`).
- **`try` with neither a `catch` nor a `finally`.** Reference `scala` 3.8.4
  compiles it, warns "A try without catch or finally is equivalent to putting
  its body in a block; no exceptions are handled", and runs the body — so it
  parses to a plain block here. It handles nothing: a raise inside one still
  propagates, and the unwind protocol below is not armed for it.
- **Compound assignment in expression position.** `println(buf += 1)`,
  `val r = (n -= 2)`, `xs.map(x => n += x)`, `(buf += 2) += 3`. Scala's
  `l op= r` is an EXPRESSION, and its value is decided by which half of the SLS
  6.12.4 choice ran: the `op=` **member** answers the receiver
  (`println(buf += 1)` prints the buffer), the `l = l op r` expansion is an
  assignment and answers `()` (`println(n += 1)` prints `()`, and `n` still
  moved). That choice is the same run-time `IS_GROWABLE` test the statement
  forms take, so each half stores its own answer into a slot the reader loads
  once both have rejoined — which also means the effect is lowered by exactly
  the statement path, target evaluated once and all
  (`println(a(next()) += 9)` advances one step). Every target shape the
  statement forms accept works, plus one they do not: a target that cannot be
  assigned to at all (`(buf += 2) += 3`, `f() += 1`) has only the member
  reading, since there is nothing to store back into. A `var` written this way
  from inside a closure is boxed exactly as the statement form boxes it.
  Covered by the parity fuzzer's `assignexpr` mode.
- **`Double.toString`, as a choice of DECIMAL rather than of digits.** Java
  picks the decimal `d` for a value `v` and only then lays it out: `d` is the
  shortest that rounds back to `v`, except that when one significant digit
  already suffices it is the closest decimal of length one **or** two, and a tie
  goes to the even significand. Both exceptions are implemented, and both are
  observable only where an ULP is of the same order as the value or its exact
  expansion is one digit longer than its shortest form:
  `Double.MinPositiveValue` is `4.9E-324` (not the one-digit `5E-324` with a
  zero stuck on), and `5 * 2^-23` — exactly `5.9604644775390625E-7` — is
  `5.960464477539062E-7`, the even one. The tie test reads the double's EXACT
  decimal expansion (which always terminates) and requires both candidates to
  round back, which is what makes `2^-24` answer the odd `5.960464477539063E-8`
  off the very same digits: at an exact power of two the gap below is half the
  gap above, so the lower candidate is out of range and there is no tie.
  Layout is then Java's — plain for `1e-3 <= |x| < 1e7`, computerized
  scientific otherwise, always with one fractional digit.
- **Postfix method dispatch on core values.** `s.length`/`.size`,
  `.toUpperCase`/`.toLowerCase`, `.trim`, `.strip`/`.stripLeading`/
  `.stripTrailing`, `.stripMargin`, `.reverse`, `.isEmpty`/`.nonEmpty`,
  `.substring`, `.charAt`, `.contains`/`.startsWith`/`.endsWith`,
  `.toInt`/`.toLong`/`.toDouble` on `String`; `.abs`/`.min`/`.max`/`.signum`/
  `.compareTo`/`.toDouble` on `Int`; `.abs`/`.round`/`.min`/`.max`/`.signum`/
  `.compareTo`/`.isNaN`/`.toInt` on `Double`; and `.toString` on any value.
  Method chaining works (`s.trim.length`). Out-of-range/parse failures throw
  faithfully (`StringIndexOutOfBoundsException`, `NumberFormatException`).
  `trim` and `strip` are not the same cut and neither is Rust's: `trim` removes
  every code point at or below U+0020, `strip` removes what
  `Character.isWhitespace` accepts — which excludes U+00A0.
- **Predef `require`, `assert` and `assume`.** Desugared in the parser to the
  `if`/`throw` they are defined as, so the message parameter keeps its by-name
  semantics and is not evaluated when the condition holds. `require` raises
  `IllegalArgumentException: requirement failed`, `assert` an
  `AssertionError: assertion failed`, `assume` an
  `AssertionError: assumption failed`; a supplied message is appended after
  `": "`. `AssertionError` extends `Error`, so `catch { case e: Exception }`
  does not catch one.
- **Classes, objects, `case class` (host-side object model).** `class C(x: Int)
  { def m = … }` with `new C(…)`, constructor params + body `val`/`var` fields,
  `this`, in-place `var`-field mutation, and instance-method dispatch (runtime
  class-tag dispatch off a `Value::Obj` handle into the frontend heap in
  `src/host.rs`). `object` singletons dispatch `def`s statically and expose
  `val`s as `Name.val`. `case class` derives an ordered-field `toString`
  (`Point(1,2)`), structural `equals`/`hashCode`, `copy(field = …)` (named and
  positional), a companion `apply` (construct without `new`), and `unapply` —
  wired into constructor patterns `case Point(x, y) =>`, nested and guarded.
  All four derived members see the **primary-constructor prefix only**, so a
  `val` declared in the class body is a field but not part of `toString`,
  `equals`, `hashCode` or the extractor's arity, matching Scala.
  Built-in `Option` (`Some(v)`, the `None` case object) rides the same model.
  Plain (non-`case`) classes use reference-identity `equals`/`hashCode` and a
  `Class@hex` `toString`, matching Scala.
- **`override def toString`, honoured wherever a value is rendered.** Scala
  renders every value through its `toString`, so an override answers for
  `println(p)`, `s"$p"` / `f"$p%s"`, `"x" + p`, `String.valueOf(p)`,
  `xs.mkString(…)`, `"%s".format(p)`, `sb.append(p)`, and at every depth of a
  `List`/`Set`/`Map`/tuple/`Option` — not only for an explicit `p.toString`. An
  override on a `case class` replaces the derived `Class(f0,…)` form, and one
  provided by a `trait` is found through the receiver's linearization. Running
  one re-enters the VM, so it may itself print (rendering happens before stdout
  is locked) and it may raise (the `println` it was rendering for does not run).
  Both the compiler's concat rerouting and the host's per-value lookup are gated
  on the program declaring an override at all: a program with none emits and
  runs exactly the bytecode it did before.

  The `+` half of that is a WHOLE-OPERATION reroute, not an operand rewrite.
  fusevm's `Op::Add` reaches a mixed pair through `NumericHook`, a plain
  `Fn(NumOp, &Value, &Value)` with no VM to re-enter, so a `+` either of whose
  operands could be a class instance is emitted as the `SADD` builtin instead
  (`BuiltinHandler` does get a `&mut VM`). `SADD` answers exactly what the hook
  answers for every other pair — `Set`/`Map` `+`, `Char` arithmetic, `Long`
  wrap, `Int`/`Double` promotion, and Scala 3's rejections, which have no
  universal `any2stringadd` — and differs only in rendering a concatenation
  through the VM-aware path. Deciding it at run time is what reaches the sites
  with no `String` in the source text: `pre + p` for a `val pre = "…"`,
  `s + p` for a `String` parameter, `xs(0) + p`, and `acc += p`, whose far
  operand is the assignment TARGET and so offers no syntax to read. The gate is
  the operand: a literal, and anything the width analysis has typed `Int` or
  `Long`, keep the native `Op::Add`, so `n += 1` in a program that happens to
  declare one override is untouched.
- **Overloaded methods, resolved by argument count.** A `class`/`object` may
  declare one name at several arities (`def g()`, `def g(x: Int)`, `def g(x:
  Int, y: Int)`); each registers its own subroutine and every call site — direct,
  `super.m`, virtual dispatch off a runtime tag, and an unqualified self-call —
  picks the one matching the arity it passes. Overloads that differ only in
  parameter TYPE (`f(Int)` / `f(String)`) are refused at compile time: Scala
  resolves those by type, which this frontend does not model, and answering the
  first silently is the worse failure.
- **Traits and inheritance.** `trait T { … }` with abstract members (`def f:
  Int`, `val x: String`) and concrete ones; `class C(x) extends P(x) with T1
  with T2`; `override def`; `super.m(…)`; and virtual dispatch — a method call
  resolves off the receiver's runtime class tag into whichever supertype owns
  the implementation, so a `List[Shape]` of mixed subclasses dispatches
  correctly. Constructor arguments thread up the whole superclass chain
  (`class E(m) extends B(m)`, `class B(m) extends A(m)`), supertype bodies run
  base-most first, and inherited fields join the record. `case class`/`case
  object` ADTs under a `sealed trait` match by constructor pattern and by
  stable identifier. `x.isInstanceOf[T]` and `case x: T =>` consult the
  registered hierarchy, so a subtype instance matches its supertypes.
  Resolution order for `extends P with T1 with T2` is `C, T2, T1, P` (Scala's
  linearization for every non-diamond hierarchy).
- **`Array`.** `Array(a, b, c)`, `new Array[T](n)` (filled with `T`'s zero
  value), `a(i)` reads, `a(i) = v` writes (Scala's `update` sugar), and the
  sequence operations (`length`, `map`, `filter`, `sum`, `mkString`, `toList`,
  `reverse`, `contains`, `foreach`, …).
- **Collections: `List`/`Seq`/`Vector`/`Set`/`Map`.** Literals for all five
  (`Seq` is `List` and `IndexedSeq` is `Vector`, as in Scala 3), `Nil`, `::`,
  and the combinator set: `map`/`flatMap`/`filter`/`filterNot`/`foreach`;
  `foldLeft`/`foldRight`/`fold`/`reduce`/`reduceLeft`/`reduceRight`;
  `sum`/`product`/`min`/`max`/`minBy`/`maxBy`/`count`/`length`/`size`;
  `exists`/`forall`/`find`/`indexOf`/`lastIndexOf`/`indexWhere`/`contains`;
  `sorted`/`sortBy`/`sortWith`/`reverse`/`distinct`;
  `take`/`drop`/`takeRight`/`dropRight`/`takeWhile`/`dropWhile`/`slice`/
  `splitAt`/`span`/`partition`/`init`/`tail`/`head`/`last`/`headOption`/
  `lastOption`; `zip`/`zipWithIndex`/`unzip`/`flatten`/`grouped`/`sliding`;
  `groupBy`; `mkString` in all three arities; `toList`/`toSeq`/`toVector`/
  `toArray`/`toSet`/`toMap`; the set algebra `union`/`intersect`/`diff`/
  `subsetOf`/`incl`/`excl`; and the operators `++`, `:+`, `+:`, `+`, `-`.
  `Map` adds `apply`/`get`/`getOrElse`/`contains`/`keys`/`keySet`/`values`/
  `updated`/`removed`/`+`/`-`/`++`, and its closure-taking methods pass the
  `Tuple2` Scala does, so `m.map { case (k, v) => … }` works.
- **Mutable collections.** `mutable.ListBuffer`, `mutable.ArrayBuffer`
  (also `mutable.Buffer`), `mutable.Queue`, `mutable.Stack`,
  `mutable.ArrayDeque`, `mutable.Set`/`mutable.HashSet`,
  `mutable.Map`/`mutable.HashMap`, `mutable.LinkedHashSet` and
  `mutable.LinkedHashMap`, spelled `mutable.X`,
  `collection.mutable.X`, `scala.collection.mutable.X`, `new …[T]()`, or —
  for the names that cannot also mean an immutable collection in default
  scope — unqualified. They ride the same host heap and answer
  the same read combinators as the immutable ones, plus the mutators
  `+=`/`-=`/`++=`/`--=`, `add`/`addOne`/`addAll`, `append`/`prepend`/
  `appendAll`/`prependAll`, `insert`/`insertAll`, `remove`, `subtractOne`/
  `subtractAll`, `clear`, and — on a `Map` — `put`, `update` (`m(k) = v`),
  `getOrElseUpdate` and `remove`. Each prints with its own prefix
  (`ListBuffer(…)`, `ArrayBuffer(…)`, `Queue(…)`, `Stack(…)`,
  `ArrayDeque(…)`, `LinkedHashSet(…)`/`LinkedHashMap(…)`, and
  `HashSet(…)`/`HashMap(…)` at every size), and a derived collection keeps the
  receiver's kind, so `ListBuffer(1,2).map(_ * 2)` is a `ListBuffer`.
  A **user type of the same name shadows the built-in one**: these constructors
  are recognized by name, so a program that declares its own `class Stack` or
  `case class Queue` constructs that instead.
- **The end-taking buffers, and where each end is.** `Queue.enqueue` appends and
  `dequeue`/`front` read the head; `Stack.push` PREPENDS and `pop`/`top` read the
  head, because a `Stack`'s head is its top; `ArrayDeque` grows at both ends
  (`prepend`/`append`, `removeHead`/`removeLast`). `+=` is `Growable.addOne` on
  all three and therefore always APPENDS — `Stack(1,2,3) += 8` is
  `Stack(1, 2, 3, 8)` where `push(8)` is `Stack(8, 1, 2, 3)`. A removal from an
  empty one raises `java.util.NoSuchElementException: empty collection`; an empty
  `top`/`front` raises `head of empty Stack`/`head of empty Queue`.
- **`LinkedHashSet`/`LinkedHashMap` keep INSERTION order.** They are their own
  representation, never an alias for the table-ordered ones: the stored order is
  the linked list Scala threads through the table, so an add appends, a re-added
  element keeps its original position, and one removed and re-added moves to the
  end. They print `LinkedHashSet(…)`/`LinkedHashMap(…)` at every size.
- **`StringBuilder`.** `new StringBuilder`, `new StringBuilder(s)` and
  `StringBuilder(s)`. It is a growable `Seq[Char]` whose `toString` is its
  CONTENTS with no wrapper, so `println(b)` prints the text. `append` takes
  `String.valueOf` of any value (`b.append(7)` appends `'7'`), `+=` takes a
  `Char` and `++=` a `String` or a `Char` sequence; `insert`, `setCharAt`,
  `deleteCharAt`, `setLength`, `clear` and `result()` mutate or freeze it, and
  the `CharSequence` members `substring`/`indexOf`/`lastIndexOf`/`charAt` work
  alongside every sequence one. A SELECTING op (`take`, `filter`, `reverse`)
  answers another `StringBuilder` through `fromSpecific`; `map`, whose element
  type may change, reads `mutable.IndexedSeq`'s factory and so answers an
  `ArrayBuffer` — exactly as Scala does.
- **An explicit `Ordering`.** `Ordering.Int` and its siblings (the natural
  ordering), `.reverse`, `Ordering.by(f)`, `Ordering.fromLessThan(lt)`,
  `Ordering[T]` and `ord.on(f)`, plus the value's own `compare`, `lt`, `gt`,
  `lteq`, `gteq`, `equiv`, `max` and `min`. Scala passes it in a second
  (implicit) parameter list, which this frontend flattens into the same call, so
  `xs.sorted(ord)` arrives with one argument and `xs.sortBy(f)(ord)` with two;
  `sorted`, `sortBy`, `max`, `min`, `maxBy` and `minBy` all take it. The sort is
  stable and the comparison may run a user closure, so it is an insertion merge
  rather than `slice::sort_by` (which would panic on an inconsistent
  comparator). The NAMED orderings all collapse onto the one natural ordering,
  because there are no static types here to tell `Ordering.Int` from
  `Ordering.String` at a call site and nothing observable distinguishes them
  once applied to values of that type.
- **Boxed-primitive and `String` companion statics.** `scala.Int`'s
  `MaxValue`/`MinValue` family (and `Double`'s `MinPositiveValue`,
  `PositiveInfinity`, `NegativeInfinity`, `NaN`), and the `java.lang` boxes'
  statics: the parses `Integer.parseInt`/`parseLong`/`parseByte`/`parseShort`/
  `parseUnsignedInt`/`decode`/`valueOf` (most taking an optional radix), the
  renderings `toString(i, radix)`, `toBinaryString`/`toHexString`/
  `toOctalString`, `toUnsignedString`, `toUnsignedLong`, the comparisons
  `compare`, `compareUnsigned`, `max`, `min`, `sum`, `signum`, `hashCode`, the
  unsigned arithmetic `divideUnsigned`/`remainderUnsigned`, the bit twiddling
  `bitCount`, `reverse`, `reverseBytes`, `highestOneBit`, `lowestOneBit`,
  `numberOfLeadingZeros`, `numberOfTrailingZeros`, `rotateLeft`, `rotateRight`,
  and `MAX_VALUE`/`MIN_VALUE`;
  `Double.parseDouble`/`isNaN`/`isInfinite`/`toString`; `Boolean.parseBoolean`;
  `Character.isDigit`/`isLetter`/`isLetterOrDigit`/`isWhitespace`/`isUpperCase`/
  `isLowerCase`/`toUpperCase`/`toLowerCase`/`getNumericValue`; and
  `String.valueOf`. The two namespaces stay APART, exactly as Scala's do —
  `Double.parseDouble` really is "not a member of object Double" there — and a
  fixed-width rendering follows its box, so `Integer.toHexString(-1)` is
  `ffffffff` and `java.lang.Long.toHexString(-1L)` is `ffffffffffffffff`. The
  same is true of every bit-twiddling static (`Integer.reverse(1)` is
  `-2147483648`, `Long.reverse(1L)` is `-9223372036854775808`), and of the
  parses: which range a string is checked against is the METHOD's, which is why
  `"128".toByte` reports `Value out of range. Value:"128" Radix:10` while
  `"2147483648".toInt` reports `For input string: "2147483648"`. A radix outside
  `2..36` is rejected before any digit is read, with `Character.MIN_RADIX` /
  `MAX_RADIX` naming the bound.
- **The narrowing conversions.** `toByte` and `toShort` are the JVM's `i2b`/
  `i2s` — the low bits of the two's complement, sign-extended — on an `Int`, a
  `Long`, a `Char` and a `Double` receiver, and the result re-enters arithmetic
  at `Int` width (`128.toByte + 1` is `-127`). A `Double` composes the two JVM
  steps in order, saturating to an `Int` first and truncating second, so
  `1e20.toByte` is `-1`. `String` has the parsing side of the same widths
  (`toByte`/`toShort`/`toBoolean` alongside `toInt`/`toLong`/`toDouble`), and
  none of the INTEGER parses trim: `" 42".toInt` raises where `" 42".trim.toInt`
  answers 42, while `toDouble` accepts the padding because
  `Double.parseDouble` does.
- **`getClass`.** Answers a `java.lang.Class` carrying `getName` and
  `getSimpleName`, and printing as `class <name>` (a reference type) or the bare
  name (`int`, `double`, `boolean`, `char`). Modeled for a `String`, the
  primitives, a user `class`/`case class`/`object` (whose JVM class takes the
  `$` suffix Scala appends to an object) and a throwable — which is the usual
  reason to call it, `e.getClass.getSimpleName`.
- **`f(xs: _*)` — the varargs spread.** The sequence is handed STRAIGHT to the
  repeated parameter rather than collected into a one-element `ArraySeq`, so
  `def f(xs: Int*) = xs.sum; f(List(1,2,3): _*)` is `6`. Every collection
  FACTORY is varargs too, so `List(xs: _*)`, `mutable.Queue(xs: _*)` and
  `Map(ps: _*)` work and each answers its own kind; the element count is not a
  compile-time constant there, so those go through
  [`crate::host::FROM_SEQ`] rather than the fixed-arity `MAKE_*` builtins. A
  spread aimed at a parameter that is not repeated is a compile error, as it is
  in Scala.
- **Selection and application alternate in a postfix chain.** `"abc"(1).toInt`
  is an apply and then a selection, and so is `List(List(1,2))(0)(1)`. A
  `scala.collection.mutable` FACTORY is not curried either, so the second group
  of `mutable.ArrayBuffer(1,2,3)(1)` is an INDEX (answering `2`) rather than a
  fourth element.
- **A mutable `Set`/`Map` prints in its hash *table's* order.** Nothing like the
  immutable CHAMP trie: `scala.collection.mutable.HashSet`/`HashMap` are a flat,
  separately chained table, and `src/host.rs` ports it from the 2.13 sources —
  `improveHash(h) = h ^ (h >>> 16)`, the bucket `hash & (len - 1)`, buckets kept
  sorted by improved hash (equal hashes keeping insertion order), the
  `tableSizeFor`/`threshold` sizing that `HashSet.from` starts from and `add`
  doubles at three-quarters full, the `sizeHint` a `++=` applies when its source
  knows its size, and iteration over table indices ascending. The table length
  is therefore part of the collection's state (removal never shrinks it, and a
  derived collection starts a fresh table at the default capacity), which is why
  `mutable.Set(1,2,3)` and `mutable.Set(1,2,3,4,5)` order the same elements
  differently.
- **`Set`/`Map` print in their representation's order.** Up to four entries
  Scala's factories answer `Set1`..`Set4` / `Map1`..`Map4`, which keep insertion
  order and print `Set(…)`/`Map(…)`; beyond that they answer a CHAMP
  `HashSet`/`HashMap`, which prints `HashSet(…)`/`HashMap(…)` in *trie* order.
  Both are reproduced exactly: `src/host.rs` ports the JVM/Scala `##` hash codes
  (`String`, `Int`/`Long`, `Statics.doubleHash`, `Boolean`,
  `MurmurHash3.productHash` for tuples and `case` records, and its
  `orderedHash`/`unorderedHash` for collections), the trie's `improve`
  scramble, and the depth-first order its iterator walks. A derived collection
  keeps the receiver's representation, so `Set(1,2,3,4,5).filter(_ > 1)` is a
  four-element `HashSet` and `groupBy` is always a `HashMap`. A `case class`'s
  `hashCode` is therefore Scala's exact `MurmurHash3` value, not just a
  contract-satisfying one.
- **Partial functions and `collect`.** A `{ case … }` literal is Scala's
  `PartialFunction` literal, so besides `apply` it answers `isDefinedAt`: the
  compiler emits a **second** subroutine per literal — the same patterns and
  guards with every arm body replaced by `true` and a trailing `case _ => false`
  — and the closure handle carries both (`src/compiler.rs`, `defined_at_body`;
  `src/host.rs`, `MAKE_PARTIAL`). That is Scala's `applyOrElse` split: an arm
  body never runs for an element the function is not defined at. It powers
  `collect`/`collectFirst` on every sequence, `Set`, `Range` and `Map`, and the
  value-level surface `isDefinedAt`, `applyOrElse`, `lift`, `orElse`, `andThen`
  and `compose` (the last four answer a composed function value, so
  `(a orElse b).isDefinedAt` consults both operands and never runs an arm body).
  A literal used as a *total* function still raises `MatchError` where no arm
  matches, matching Scala's `Function1` reading of the same literal. A
  `PartialFunction[A, B]` annotation is a type ascription, so binding one to a
  `val` and passing it around works.
- **The full `match` pattern grammar.** Beyond literal/wildcard/typed/tuple and
  constructor patterns: `@` binders (`case w @ Pt(0, _) =>`, which bind the whole
  scrutinee alongside its parts), `|` alternations (`case 0 | 1 | 2 =>`, each
  branch tried in turn), the cons pattern `h :: t` and `Nil`, and sequence
  patterns `List(a, b)` / `Seq(…)` / `Vector(…)` / `Array(…)` with an optional
  trailing `_*` (named — `rest @ _*` — or anonymous). A sequence pattern tests the
  receiver's REPRESENTATION first and its length second, so `case List(a, b)`
  does not match a two-element `Vector`, matching Scala. The same grammar backs
  **pattern definitions**: `val (a, b) = pair`, `val Some(x) = opt`,
  `val h :: t = xs`, `val Pt(x, y) = p` and nested forms all bind into the
  enclosing scope and raise `scala.MatchError` on a non-matching value.
- **`scala.Option`'s method surface.** `get`, `getOrElse`, `orNull`, `isEmpty`,
  `isDefined`, `nonEmpty`, `size`, `contains`, `map`, `flatMap`, `filter`,
  `filterNot`, `exists`, `forall`, `count`, `foreach`, `fold(ifEmpty)(f)`,
  `orElse`, `collect`, `flatten`, `head`, `headOption`, `toList`/`toSeq`/
  `toVector`/`iterator`, and `toRight`/`toLeft`. The `Option(x)` factory answers
  `None` for `null`. `List[Option[A]].flatten` drops the empties.
- **`Either`, right-biased as Scala 2.13 made it.** `Left`/`Right` are built-in
  case classes, so `Right(1)`, `case Left(e) =>` and `List[Either[…]].collect`
  work, and the method surface operates on the `Right` while passing a `Left`
  through: `isLeft`/`isRight`, `getOrElse`, `orElse`, `map`, `flatMap`,
  `foreach`, `exists`/`forall` (vacuously true on a `Left`), `contains`, `fold`,
  `swap`, `filterOrElse`, `toOption` and `toSeq`/`toList`. `Either.left`'s
  `LeftProjection` is NOT modeled — it is a distinct type, not a member set —
  so `e.left.getOrElse(…)` is refused rather than approximated.
- **`scala.util.Try`.** `Try(e)` expands in the parser to
  `try Success(e) catch { case t: Throwable => Failure(t) }`, so `e` is by-name
  and evaluated inside the `try`; `Success`/`Failure` are built-in case classes
  like `Left`/`Right`, which is what makes `case Success(v) =>` and
  `Failure(e).exception` work. The surface is `isSuccess`/`isFailure`, `get`
  (which rethrows), `getOrElse`, `orElse`, `map`, `flatMap`, `foreach`,
  `filter`, `recover`/`recoverWith`, `failed`, `toOption`, `toEither` and
  `toSeq`/`toList`. Two deviations: Scala catches only `NonFatal` throwables and
  lets a fatal one (`StackOverflowError`, `OutOfMemoryError`) propagate, where
  this catches every `Throwable`; and a program defining its own `Try` cannot
  shadow the factory, the same way it cannot shadow `List(…)`.
- **`Product` on every `case class`/`case object` and tuple.** `productArity`,
  `productPrefix`, `productElement(i)`, `productElementName(i)`,
  `productIterator` and `productElementNames`, all over the
  primary-constructor prefix (a body `val` is a field but not a product
  element). `Tuple2.swap` and `iterator`/`reverseIterator` on a sequence.
- **Non-local `return`, and `finally` on the way out.** A `return` inside a
  lambda — including the `foreach`/`map` closures a `for` comprehension
  desugars to — leaves the *method* that lexically contains it, not just the
  closure. It is lowered the way Scala lowers it: the value is parked as a
  `scala.runtime.NonLocalReturnControl` and carried out by the same unwind walk
  an exception uses (`NLR_RAISE`/`NLR_TAKE` in `src/host.rs`), so every
  enclosing `finally` runs, innermost first, before the value reaches the
  caller. No `catch` arm can intercept it. `return` is also accepted in
  expression position (`xs.foreach(x => if (p(x)) return x)`), as in Scala,
  where its type is `Nothing`.
- **The wider `String`/`StringOps` surface.** `indexOf`(+`from`), `lastIndexOf`,
  `replace`, `stripPrefix`/`stripSuffix`, `capitalize`, `compareTo`(the JDK's
  char-difference result, not a normalized sign)/`compareToIgnoreCase`/
  `equalsIgnoreCase`, `*` (repeat), the total slicing operations `take`/`drop`/
  `takeRight`/`dropRight`/`slice`/`splitAt` (clamped, never throwing),
  `head`/`last`/`init`/`tail`, `apply(i)`, `distinct`, `sorted`, `mkString`
  (0/1/3-arg), `toCharArray`, `zipWithIndex`, and the closure-taking
  combinators `map`, `flatMap`, `collect`, `filter`/`filterNot`,
  `takeWhile`/`dropWhile`, `count`, `exists`/`forall`, `foreach`, `find`,
  `indexWhere`, `partition`, `span`, `foldLeft`/`foldRight`, `reduce`,
  `scanLeft`, `sortWith`/`sortBy`, `maxBy`/`minBy`, `groupBy`, `zip`, and the
  conversions `toList`/`toVector`/`toArray`/`toSet`. Every accessor that hands
  out an element hands out a `Char` (`head`, `last`, `apply`/`charAt`,
  `headOption`/`lastOption`, `min`/`max`, and the elements of `toList` and
  friends), which is what makes `"abc".toList.map(_.toInt)` the code points.
- **`Char` as its own type.** `'a'` is a distinct runtime value that dispatches
  as a NUMBER in arithmetic (`'a' + 1 == 98`, `'a' / 2 == 48`, `-'a' == -97`)
  and as TEXT everywhere string conversion applies (`println('a')` is `a`,
  `List('a')` is `List(a)`). Its numeric conversions answer the code point
  (`'a'.toInt == 97`, and `'5'.toInt == 53` where `"5".toInt` parses to `5`),
  `toChar` rounds back from an `Int`, and `asDigit`, `toUpper`/`toLower`,
  `isLetter`/`isDigit`/`isLetterOrDigit`/`isUpper`/`isLower`/`isWhitespace`,
  `compare`, `max`/`min` and `hashCode` (the code point) are all present. `+`
  against a `String` stays concatenation (`'a' + "b" == "ab"`), which is the one
  place the numeric face gives way.
- **The bitwise and shift operators.** `&`, `|`, `^`, `~`, `<<`, `>>` and `>>>`
  on `Int`; `&`, `|` and `^` on `Boolean` (Scala's non-short-circuiting forms);
  and `&`/`|`/`&~` on `Set` (intersection, union, difference). Hexadecimal
  literals (`0x1F`) lex, read at `Int` width. Precedence follows the SLS table
  keyed by an operator's first character — `| < ^ < & < = ! < < > < : < + - <
  * / %` — which is also where `&&`/`||` get theirs, so `a | b && c` is
  `a | (b && c)` and `1 << 2 + 1` is `1 << 3`.
- **`for` comprehensions.** `yield` and the side-effecting form; range and
  collection generators; several generators (nesting through `flatMap`);
  `if` guards, trailing or standing alone; both the `for (…)` and the `for { … }`
  enumerator groups; and a destructuring generator (`for ((k, v) <- m)`), which
  desugars to a pattern-matching anonymous function. A *refutable* generator
  pattern takes Scala 3's `case` spelling — `for (case Some(x) <- opts)`, which
  desugars to a `withFilter` on the pattern and so SKIPS a non-matching element
  rather than raising. Scala 3 requires the keyword (bare `for (Some(x) <- opts)`
  is a compile error there), so it is required here too.
- **General infix method syntax.** `a m b` is `a.m(b)` for any single-argument
  method — `xs contains 2`, `s startsWith "a"`, `1 to n`. An alphanumeric
  operator binds looser than every symbolic one (`1 to n - 1` is `1 to (n - 1)`)
  and chains left-associatively. The symbolic collection operators `++`, `:+`
  and `+:` take their precedence from the first character and, for a name ending
  in `:`, dispatch on the right operand (`0 +: xs` is `xs.+:(0)`). A line break
  between the receiver and the name rules the infix reading out, so a bare
  expression statement is never absorbed into the next line.
- **Pattern-matching anonymous functions.** `{ case p => e }` in argument
  position is a function that matches on its argument, including tuple patterns
  (`case (k, v) =>`, nested and wildcarded). A caller that passes more arguments
  than the literal declares — `foldLeft(z) { case (acc, (k, v)) => … }` — has
  them tupled, as Scala's `FunctionN` conversion does. A brace group also stands
  in for a parenthesized argument generally (`xs.map { x => x + 1 }`).
- **Singleton `object`s inherit concrete members.** `object X extends T` gets
  T's concrete `def`s and `val`s. A singleton has no `this` record, so the
  members are spliced into the object as if declared there (`src/compiler.rs`,
  `inherit_into_objects`) and compile in the object's own scope; a member X
  declares itself wins, supertypes are consulted in linearization order, and
  inherited `val`s initialize base-most first. The inherited body is reachable
  both by name (`X.m`) and by virtual dispatch through a supertype-typed
  binding.
- **Generics, type-erased.** Type parameters on `class`/`case class`/`trait`/
  `def`, type arguments at use sites (`new Box[Int](3)`, `id[String]("hi")`,
  `xs.map[Int](f)`), and parameterized annotations (`val m: Map[String,
  List[Int]]`) all parse and run. Nothing is checked or specialized — the
  runtime is dynamically typed, exactly as the other fusevm frontends are.
- **`Range` as a first-class value.** `1 to 5`, `1 until 5`, `1 to 10 by 3` are
  values, with Scala's `Range.toString` including the `empty `/`inexact `
  prefixes. `map`/`filter` over one yield a `Vector`, `reverse` yields a
  `Range`, and `sum`/`length`/`head`/`last`/`toList`/`mkString`/`contains`/
  `min`/`max` all work. Used as a `for` generator it still compiles to the
  counted loop (no materialization).
- **`scala.math`.** `abs`, `signum`, `min`, `max`, `round`, `floor`, `ceil`,
  `rint`, `sqrt`, `cbrt`, `exp`, `log`, `log10`, `pow`, `hypot`, the trig and
  inverse-trig functions, `atan2`, `toRadians`/`toDegrees`, `Pi`, `E` — under
  the `math`, `scala.math`, `Math` and `java.lang.Math` spellings. The `Int`
  overloads stay integral (`math.abs(-4)` is `4`, not `4.0`), and the two
  namespaces are kept apart where the JDK lacks an overload
  (`Math.signum(5)` is `1.0`, `math.signum(5)` is `1`).

- **`scala.collection.mutable.PriorityQueue`.** `enqueue`/`+=`/`++=`, `dequeue`,
  `dequeueAll`, `head`/`max`, `clone`, `clear`, and the shared read-only
  sequence surface. Its `toString` and its iteration are the raw order of the
  binary HEAP ARRAY behind it — `PriorityQueue(3,1,4,1,5,9,2,6)` prints
  `PriorityQueue(9, 6, 4, 1, 5, 3, 2, 1)`, neither the input order, nor sorted,
  nor the `9, 6, 5, 4, 1, 3, 2, 1` that repeated sift-up insertion leaves — so
  the library's own `fixUp`/`fixDown`/`heapify` are ported rather than a
  max-heap written from scratch. `++=` heapifies from the first new position
  where `+=` sifts one element up, which are different arrays; the factory
  appends raw and runs one bottom-up sweep. `map` answers an `ArrayBuffer` (the
  result element type carries no implied `Ordering`) where `filter` stays a
  `PriorityQueue`. The ordering is the implicit one, including a user
  `Ordered`/`Comparable` class's `compare`; an explicitly passed `Ordering`
  argument is not accepted.

## Not implemented (parse errors / unresolved today)

- **`import` clauses are skipped, not tracked**, at the top level and, since
  they are recognized in statement position too, inside a method body or a
  block. Skipping them costs nothing for a *qualifying* import — `import
  scala.collection.mutable` followed by `mutable.ArrayBuffer(…)` works, because
  the qualified name is what resolves — but a SELECTOR import does not bring its
  member into scope unqualified: `import scala.math.abs` followed by a bare
  `abs(-3)` is `not found: abs`. Write `math.abs(-3)`. The collection names
  (`ListBuffer`, `ArrayBuffer`, `Buffer`) are the exception, and only because
  they resolve unqualified with or without the import. For the same reason an
  unqualified `Set`/`Map` always means the IMMUTABLE one: `import
  scala.collection.mutable.Set` followed by a bare `Set(1, 2)` builds the
  immutable set. Write `mutable.Set(1, 2)`.
- **`xs: _*` outside an argument position.** The spread is parsed where Scala
  allows it — as an argument — and rejected everywhere else, which is also what
  Scala does. A spread handed to a parameter that is not repeated is a compile
  error on both sides.
- **A `Map`'s and `Set`'s inserts are LINEAR, so filling one is quadratic.**
  Both are stored as an ordered entry vector, which is what makes their
  iteration order byte-reproducible (the CHAMP trie order, the mutable table
  order); a lookup therefore scans, where Scala's hashes. Filling a
  `mutable.Map` by key measured 1.27s / 5.06s / 20.41s of user time at n = 4k /
  8k / 16k — 4x per doubling, the growth class rather than a constant factor.
  Element WRITES on a sequence do NOT have this shape: `ListBuffer`/`ArrayBuffer`
  `+=`, `Queue.enqueue`, `StringBuilder.append` and `a(i) = v` all measure
  0.01-0.02 / 0.02-0.03 / 0.04-0.06s across the same n, which is 2x per doubling
  (`append_in_place`). Closing this needs
  a hash index beside the entry vector, which has to preserve that vector as the
  order of record; it is not done, and the cost is stated here rather than
  hidden.
- **Lazy views.** `.view` and `LazyList`. `Iterator` itself is no longer one of
  these gaps: `.iterator`/`.reverseIterator` (and `grouped`/`sliding`) answer a
  real [`SeqKind::Iterator`], which renders `<iterator>`, answers
  `hasNext`/`next()`, and is CONSUMED by traversing it — `it.toList` twice gives
  the elements and then `List()`, and `next()` past the end throws
  `NoSuchElementException: next on empty iterator`. Its elements are still
  MATERIALIZED when it is built, so it is strict in the one way that stays
  unobservable: a downstream combinator sees the same elements Scala's lazy one
  would, but an infinite source cannot be represented.
- **A `Char` range — `'a' to 'e'`, `'a' until 'z'`, `for (c <- 'a' to 'd')`.**
  Scala's is a `NumericRange[Char]`, which this frontend has no representation
  for: a `Char` is a heap value here, so reading the endpoints as integers built
  `Range 0 to 0` and answered `List(0)`, `size` 1, `contains('c')` false. That is
  a silent wrong answer, so it is now REFUSED — at compile time for a `Char`
  literal endpoint, and in `MAKE_RANGE`/`RANGE_LIST` for endpoints that arrive as
  values. Write `('a'.toInt to 'e'.toInt).map(_.toChar)`. The one spelling that
  still escapes both checks is a *counted `for` loop* whose endpoints are BOTH
  `Char` variables (`val a = 'a'; val z = 'z'; for (c <- a to z)`), which lowers
  to inline loop bytecode rather than through either builtin.
- **Overloads that differ only in parameter type.** `def f(x: Int)` and `def
  f(x: String)` in one class are a compile error, not a dispatch: both would key
  the same `C$f$1` subroutine, and the runtime is dynamically typed, so the
  argument's static type — which is what Scala resolves on — is not available.
  Argument COUNT is modelled (see above); argument type is not.
- **`printf`.** `println`, `print`, `"…".format(…)` and `x.formatted(spec)` are
  wired; the bare `printf(fmt, args…)` spelling is not.
- **Symbolic operators beyond the wired set.** `/:`, `:\` and user-defined
  symbolic method names.
- **The wider standard library.** `scala.io`, `scala.collection.*` as a
  namespace, and the many `String`/numeric methods beyond the wired subset
  above.
- **`case NonFatal(e)` and other extractor patterns in `catch`.** Only
  `case e: Type`, `case _: Type`, `case e` and `case _` arms are modeled.
- **User exception classes inside the hierarchy.** `class MyErr(m: String)
  extends Exception` can be thrown and caught *by its own name*, but the JDK
  throwables are not part of the registered class hierarchy, so `case e:
  Exception` will not catch it.
- **Named regex groups.** `(?<name>…)` and `${name}` in a replacement are not
  modeled; numbered groups (`$1`, `Match.group(1)`, `case r(a, b)`) are. Both
  spellings now say so instead of answering something. `m.group("y")` went
  through `to_int`, which reads a `String` as 0 — group 0, the whole match — so
  `"(?<y>[0-9]{4})-(?<m>[0-9]{2})".r` on `2026-08` answered `2026-08` for BOTH
  names where Scala answers `2026` and `08`; and `${d}` in a replacement was
  copied through, so `"a1b2".replaceAll("(?<d>[0-9])", "<${d}>")` answered
  `a<${d}>b<${d}>` against Java's `a<1>b<2>`.
- **`getClass` on a collection, a tuple, a function or an `Array`.** Those
  runtime classes are private implementation details — `List(1).getClass.getName`
  is `scala.collection.immutable.$colon$colon`, a one-element `Vector` is
  `Vector1`, and `(1, 2)` carries the specialization suffix `Tuple2$mcII$sp` —
  so naming them would mean modelling Scala's class hierarchy AND its
  `@specialized` naming. They stay an error; `getClass` on a `String`, a
  primitive, a user `class`/`case class`/`object` or a throwable works (see
  above).

  This one is **settled, not pending**: the answer would have to be a table of
  private class names — which representation a `Vector` of each size picks,
  which specialization suffix each primitive tuple carries — that says nothing
  about how a program runs and goes stale with every library release. Refusing
  it is the intended behaviour, so a program asking for it gets `value getClass
  is not a member of value` rather than a plausible-looking wrong name.
- **A `Float` inside a collection or a record renders as a `Double`.** `Float`
  is a real type now — single-precision literals, arithmetic, conversions,
  constants, `toString`, `getClass` and `hashCode` all match Scala (see
  `README.md`) — and it is a STATIC one: fusevm has a single floating
  representation, so which of the two widths a value carries is knowable only
  from the type the compiler inferred for the expression, exactly as `Int` and
  `Long` share one `i64` and are told apart by the width analysis.

  That is enough everywhere the type reaches the value, and it does not reach
  the ELEMENTS of a container. `println(List(0.1f))` prints
  `List(0.10000000149011612)` and `case class P(v: Float); println(P(0.1f))`
  prints `P(0.10000000149011612)`, because both render their contents at run
  time from values that no longer carry a width. The element ARITHMETIC is
  correct — `List(0.1f, 0.2f).sum` is `0.3` and `xs.map(_ / 3.0f)` computes at
  single precision — and so is any element pulled back out and used on its own
  (`a(0) + a(1)`, `p.v / 3.0f`); it is only the container's own `toString` that
  reads a `Double`. Rendering the elements from the container's static element
  type would answer `List`/`Array`/`Vector` and still miss a record's fields, a
  `Map`'s values, and anything erased through an `Any`, so it is not the fix
  that closes this.

  Two smaller positions where the type does not reach either: a lambda parameter
  typed only by a declared FUNCTION type (`val f: Float => Float = z => …` — a
  parameter typed by a traversal, as in `xs.map(z => …)`, does get the element's
  width), and a name bound by a destructuring `val (a, b) = (0.1f, 0.2f)`.

  A `Float` operation also leaves the JIT's reach: single precision costs a
  `CallBuiltin` (`SF32_ARITH`), because rounding an `f64` result afterwards
  rounds twice and can land a ulp from the value Scala computes
  (`16777217.0f * 0.2f` is `3355443.2`, and the double-rounded product is
  `3355443.3`). fusevm's tracer refuses to record through a builtin, so a hot
  `Float` loop stays interpreted where the same loop over `Int` or `Double`
  reaches native code. This is the same trade the Java frontend makes for the
  identical rule.
- **`given`/`using`.** A `given` declaration and a `using` parameter list are
  both parse errors.

  By-name parameters and `@main` used to be listed here and are not gaps: `def
  f(x: => Int)` passes a thunk forced at every use (the `params` fuzz mode is
  built on it), and `@main def` is an entry point (see below). The line outlived
  both.
- **A repeated `@main` parameter, and two `@main` methods in one file.**
  `@main def go(first: Int, rest: String*)` is refused rather than collecting
  the remainder, and a file declaring two `@main` methods is refused rather than
  picking one — Scala asks for `--main-class` there, which this frontend has no
  flag for.
- **Overloading a block-level `def`.** Two `def`s of one name in the SAME block
  are a Scala overload; the resolver's inner-shadows-outer rule read them as a
  redefinition and pointed every call at the second body, including the calls
  written before it. `def g(x: Int) = "int"` beside `def g(x: String) = "str"`
  answered `str` twice, and even the statically decidable `def h()` beside `def
  h(x: Int)` answered `h1` twice — no diagnostic either time. They are now
  refused. A CLASS or OBJECT member's overload still resolves by argument count
  (`Owner$method$arity`); only the flat `def` namespace lacks that split.
- **A forward reference inside a BLOCK is accepted, where Scala rejects it.**
  Scala's forward-reference rule applies to a block's own definitions, not to a
  template's members: `def main(…) = { val c = new C(3); …; class C(…) }` and
  `def main(…) = { println(later); val later: Int = 7 }` are both compile errors
  ("forward reference to C … extends over the definition of c"), and this
  frontend runs them — answering `8` and `null` respectively. The `extends App`
  spelling of the same two programs is LEGAL Scala, because there the `val`s and
  `class`es are object members; that half is modeled (an object member is
  visible in any order, and a forward-referenced field reads its type's default).
  Only the block rule is unchecked. Found by running the `appmember` mode under
  `--entry mainsig`, which no `app` run could have reached.
- **`do/while` is not a gap.** Scala 3 removed it from the language and the
  reference compiler rejects it, so scalars does not implement it either.

## Modeled with a documented simplification

- **A `String` is indexed by CODE POINT, where the JVM indexes by UTF-16 code
  unit.** `Char` here is a Rust `char` — a whole scalar value — so a character
  outside the Basic Multilingual Plane occupies one position instead of the two
  a JVM `String` gives it. Every index-shaped operation therefore disagrees on a
  string containing one, and only on such a string. Measured against Scala 3.8.4
  on JDK 26.0.2 with `"𝕏a"` (`U+1D54F` followed by `a`):

  | expression | Scala | here |
  | --- | --- | --- |
  | `.length` | `3` | `2` |
  | `.indexOf("a")` | `2` | `1` |
  | `.charAt(0).toInt` | `55349` (the high surrogate) | `120143` (the scalar) |
  | `.head.toInt` | `55349` | `120143` |
  | `.substring(0, 2)` | `𝕏` | `𝕏a` |
  | `.reverse.length` | `3` | `2` |

  This is a REPRESENTATION difference, not a wrong constant: closing it means a
  `Char` that can hold an unpaired surrogate, which `char` cannot. Every string
  whose characters are all in the BMP — which is every string in the frozen
  corpora and every string in the test suite — indexes identically. Noted rather
  than half-fixed, because a partial conversion (say, `length` counting code
  units while `charAt` still counts scalars) would be worse than either
  consistent model.
- **A function value prints `<function1>`, not the JVM lambda's identity.**
  Scala 3.8.4 renders a function through `Object.toString`, which is a class name
  plus an identity hash (`T$$$Lambda/0x0000180001099c0@64c64813`) — a fresh
  number on every run, so it is unreproducible by construction, the same category
  as `Array.toString`. `<function1>` is Scala 2's rendering and at least names the
  arity. It becomes observable wherever a function reaches `println` directly,
  which now includes the eta-expansion of a bare argument placeholder
  (`println(xs.map(_))` prints the function `x => xs.map(x)`, not a list — see the
  placeholder rule below). Nothing DOWNSTREAM of a function value differs: only
  printing the function itself does. For the same reason no frozen corpus record
  prints one.
- **Types are not checked.** Declared types (`Int`, `String`, …) and type
  parameters (`class Box[A]`, `def id[A]`) are retained for diagnostics but do
  not gate execution — the runtime is dynamically typed on the fusevm value
  model. Type errors that `scalac` would reject may run. Related:
  `x.asInstanceOf[T]` is the identity, not a checked cast, so it never throws
  `ClassCastException` and never performs a numeric conversion. A class's
  `val`/private access modifiers are parsed but not enforced (every field is
  reachable).
- **A `String` combinator's result type is decided from the results, except on an
  empty receiver.** Scala reads `map`/`collect`'s `Char => Char` overload (which
  answers a `String`) apart from `Char => B` (which answers an `IndexedSeq`) off
  the function's static result type. `Char` is its own runtime type here, so the
  results themselves answer it exactly — `s.map(_.toUpper)` is a `String`,
  `s.map(_.toString)` and `s.map(c => c + "!")` are sequences. The one case with
  no results to read is an EMPTY receiver, where the decision falls back to the
  syntax of the body (`compiler::yields_chars` / `yields_strings`, carried on the
  closure next to `yields_pairs`): the element itself, a `Char` literal, or
  `toUpper`/`toLower`/`toChar` counts as `Char`-valued, and `flatMap` asks the
  `String` side instead. An empty receiver with a body outside those shapes —
  a call, a variable — takes the sequence branch, which is right for every `B`
  but `Char`. `filter`/`takeWhile`/`dropWhile`/`partition`/`span`/`sortWith`/
  `sortBy` are unaffected: they only select or reorder, so their result is
  always a `String`.
- **A `catch` arm cannot see a non-local return, even as `Throwable`.** Scala's
  `NonLocalReturnControl` extends `ControlThrowable`, so `case e: Exception`
  misses it but a bare `case e =>` (i.e. `Throwable`) would catch it. Here the
  parked control value is not a throwable at all, so NO arm matches it. The
  difference is observable only for a `catch` that deliberately swallows
  `Throwable` around a `return`, which is a bug in the Scala program either way.
- **`Array.toString` renders `Array(1, 2, 3)`.** Scala prints the JVM identity
  form (`[I@1b6d3586`), which no reimplementation can reproduce byte-for-byte,
  so the readable form is emitted instead. This is the one place a supported
  construct deliberately diverges; the parity fuzzer therefore never prints a
  bare `Array`.
- **A local `def` may not *assign* to a binding it captures.** A block-local
  `def` is lambda-LIFTED (its captures become extra parameters), not closed over,
  so a write inside the lifted body could not reach the enclosing frame. It is
  rejected at compile time ("a captured binding is read-only here") rather than
  silently lost. Reads see the value at call time, which matches Scala for the
  `val`s and parameters that make up the common case. A *lambda* has no such
  restriction — it captures a boxed cell and its writes are shared (see the
  closure entry above); only the lifted-`def` path is affected.

  It is also ENTRY-SHAPE dependent, which is why it is easy to miss: under
  `object T extends App` the enclosing binding is a program global, so the write
  lands and nothing is rejected. The restriction bites only when the whole thing
  sits inside a `def` — a `@main` body, or a `def main` body — where the binding
  is a frame slot.
- **A constructor pattern naming a LOCAL extractor.** `case p(a)` resolves `p`
  against the globals, so `val p = "([0-9]+)".r; xs.collect { case p(a) => a }`
  works at the top level of an `extends App` body and is rejected ("not found:
  constructor pattern `p`") when the same two lines sit inside a `def` — i.e.
  inside a `@main` body. Bind the extractor outside the entry point.
- **`"abc".toSeq` is the string itself.** Scala's is a `WrappedString` view,
  which prints as `abc` and answers every `Seq` operation through `StringOps`;
  the string stands in for it. Observably identical except for an equality
  against a `String`, which Scala answers `false` and this answers `true`.
- **A subclass constructor parameter that renames a superclass field.** Our
  parser makes every constructor parameter a field, so in `class D(n: String)
  extends A(f(n))` the superclass argument is stored under `A`'s parameter name
  and `D`'s body then sees that value under it. When the subclass forwards the
  parameter unchanged — `class D(n: String) extends A(n)`, the idiom — this is
  exactly Scala; when it transforms it, the subclass body reads the transformed
  value.
- **Linearization is "self, then parents right-to-left".** This is Scala's order
  for every hierarchy without a diamond; the full C3 rule differs only when one
  supertype is reachable by two paths.
- **`==` on non-numbers compares by value** (structural), which matches Scala's
  `==`/`equals` for the strings and booleans this frontend handles. Reference
  identity (`eq`) is not modeled.
- **A `Char` is an interned heap value**, not a width-tagged integer. It compares
  and hashes by code point (so `'a' == 97` and a `Char` key lands in the same
  CHAMP slot its `Int` code point would), and arithmetic on one leaves the native
  integer fast path for the host's numeric hook — `Int` arithmetic is untouched.
- **`~-1` parses here and does not in Scala.** Scala lexes a run of symbolic
  characters as one operator name, so `~-1` is the undefined prefix operator
  `~-`; this lexer takes `~` and `-1`. Write `~(-1)`. This is a case of
  accepting more than Scala, like the unchecked types above, never of running
  something differently.
- **An *empty* `Map.map`/`Map.collect` picks its builder from the function's
  syntax.** Scala reads the result builder off the function's static result
  type: a pair rebuilds a `Map`, anything else falls back to
  `immutable.Iterable`'s builder, which is `List`. With at least one result the
  runtime reads that off the results themselves, exactly; with none — an empty
  map, or a `collect` no entry matched — it consults a compile-time flag that is
  true when every value the function body can answer is a *pair literal*
  (`compiler::yields_pairs`). A function that returns a pair some other way (a
  call, a variable) and matches nothing therefore answers `List()` where Scala
  answers `Map()`.
- **`m.keys`/`m.keySet` answer the map's own order.** Scala's key view is a
  `HashMap.HashKeySet`, which prints `Set(…)` however large the map and iterates
  in the map's order; that is what is built, so a five-entry map's `keys` prints
  `Set(…)` rather than `HashSet(…)`. Mapping or filtering the view renormalizes
  it into a real `Set`/`HashSet`.
- **A `Long` shift inside a `def` body truncates to 32 bits.** `<<`, `>>` and
  `>>>` are evaluated at the RECEIVER's width, and the host cannot tell an `Int`
  from a `Long` by value, so `compiler::method_inner` selects the 64-bit
  spelling only when `num_ty(recv)` PROVES `NumTy::Long` and otherwise emits the
  32-bit one. Everywhere else an unproven width suppresses narrowing; here it
  causes it. A `def` body starts from an empty width map
  (`std::mem::take(&mut self.widths)`), so a top-level `val` reachable by name
  from that body arrives unproven and its shift truncates:

  ```scala
  val big = 6679246017915255L
  def g(): Long = big >>> 11        // scala 3261350594685, scalars 313469
  def h(): Long = big << 2          // scala 26716984071661020,
                                    // scalars -1727027748
  ```

  The same expressions at the top level are correct, and so is a shift on a
  `def` PARAMETER (`def g(x: Long): Long = x >>> 11`), whose annotation Scala
  requires and which is therefore always proven. A silent wrong answer, and the
  only known place an unproven width narrows rather than declining to.

  Closing it means carrying the widths of program globals into a `def` frame
  rather than clearing the whole map, which widens what narrowing applies to
  across every `def` body — worth doing as its own change, not as a rider.
- **Integer arithmetic narrows to 32 bits only where the width is PROVEN.**
  `Int` overflow now wraps — `2147483647 + 1` is `-2147483648` — and a `Long`
  anywhere in an expression promotes it so it does not
  (`2147483647 + 1L` is `2147483648`). The runtime is still dynamically typed, so
  the decision is made statically by `compiler::NumTy`, which proves a width from
  a literal (`1` vs `1L`), a declared type (`val n: Long`, and a `def`
  parameter's mandatory annotation), an inferred `val` initializer, and the
  methods whose result type is fixed (`toInt`, `toLong`, `length`, `indexOf`, …).
  What it CANNOT prove stays 64-bit, because the wrap is emitted as a shift pair
  that would destroy a `Double` or a `String`. An enclosing scope's widths travel
  into a lambda body, so a captured accumulator still wraps
  (`var t = 0; xs.foreach(t += _)`). The positions Scala types from CONTEXT are
  modelled too: a **lambda parameter** takes the element width of what it
  traverses (`xs.map(x => x * 2147483647)` over a `List[Int]` wraps), a **class
  field** carries its declared width to a use site and to a bare reference inside
  a method, a `def`'s **return annotation** types its call site, and
  `.sum`/`.product` take the collection's element width — which is why
  `(1 to 100000).sum` answers `705082704`. A collection's element width comes
  from a literal's elements, a declared `List[Int]`, a range's endpoints, or an
  element-preserving combinator (`filter`, `take`, `sorted`, …).
  The residual gap is **`map`'s result element type**: `xs.map(f).sum` does not
  know what `f` returns, because that would mean analysing the lambda body with
  its parameter already bound. The elements themselves are still correct (the
  body wraps); only a later `sum`/`product` over them stays 64-bit.
  A `def` with no return annotation is also unproven — Scala infers one from the
  body and this frontend does not.
- **The `%x`/`%X`/`%o` format conversions pick their width from the VALUE, not
  the static type.** Java writes the two's-complement bit pattern at the width of
  the operand's type — `Int` is 32 bits (`"%x".format(-1)` is `ffffffff`), `Long`
  is 64 (`ffffffffffffffff`). With no static types, this frontend uses the
  narrowest width that holds the value, which is right for every `Int` and for
  any `Long` outside `Int` range. A `Long` variable holding a small negative
  number is the residual gap: it renders 32-bit where Scala renders 64.
- **`break`/`breakable` are recognized without their import.** Scala requires
  `import scala.util.control.Breaks._` (or the qualified spelling) before either
  name resolves; this frontend recognizes them in the parser, so a program that
  omits the import still runs here and is rejected by `scalac`. That is a
  superset, not a wrong answer — but it means the parity fuzzer's generated
  programs must emit the import, or the reference rejects the probe and the two
  sides agree on a failure that tests nothing.
- **A parameter list's second (curried) group is flattened into the first.**
  `def f(a: Int)(b: Int)` is callable as `f(1)(2)` only in the sense that the
  parameters exist; the two groups are one flat list, so a partially applied
  `f(1)` is not a function. Default values in a later group also cannot see the
  earlier group's parameters (Scala allows that; Scala 3 forbids it *within* one
  group, which is why call-site splicing of a default is otherwise exact).
- **`/` is dispatched at runtime, not by static type.** Scala picks integer vs.
  floating division from the *static* operand types; there are no static types
  here, so the [`host::b_div`] builtin decides at runtime — both operands `Int`
  truncate, otherwise a double divide. This matches Scala for every monomorphic
  case (`7 / 2 == 3`, `7 / 2.0 == 3.5`), and floating `/0.0` yields `Infinity` as
  Scala does.
- **Integer division by zero throws** `java.lang.ArithmeticException: / by zero`,
  matching Scala/the JVM `idiv` trap. It is catchable (see the exception section
  below); an *uncaught* one aborts the program with
  `scalars: java.lang.ArithmeticException: / by zero` on stderr and exit status
  1, where `scala` prints a JVM stack trace — the message is faithful, the trace
  is not modeled. Floating `/0.0` is `Infinity` (not an exception), also matching
  Scala.
- **A `Range` value is materialized, not lazy.** Scala's `Range` computes
  elements on demand; here they are built when the range is created, capped at
  8M elements (beyond which an `OutOfMemoryError` is raised rather than the
  process hanging). Every observable result matches for the sizes real programs
  use; the memory profile does not.
- **Uninitialized bindings are unbound** rather than rejected; reading one before
  assignment yields `null` instead of a compile error. `Unit` no longer shares
  that value — it is the empty tuple the `()` literal already lowered to, so
  `println(println("x"))` and `println(xs.foreach(f))` render `()` while `null`
  keeps rendering `null`.
- **`x += e` chooses the growable method at run time, not from a static type.**
  Scala calls `x.+=(e)` when the receiver's type has that method and falls back
  to `x = x + e` otherwise. There are no static types here, so a program that
  builds a mutable collection *anywhere* emits a one-op test
  ([`crate::host::IS_GROWABLE`]) that picks the branch on the receiver's runtime
  kind. A program with no mutable collection in it emits exactly the arithmetic
  it did before, so a counted `while` loop stays JIT-trace-eligible.
- **`object … extends App` runs the object body directly.** Statements run in
  order, any `def` members are hoisted and callable, and a **nested type** —
  `class`, `trait`, `case class`, `case object`, `object` — is a member, exactly
  as it is in Scala: `object T extends App { class C(var n: Int); … }` compiles.
  Declaring the type beside the object is a placement, not a requirement, and
  the same declarations are accepted in any block (a `def` body, a brace group).
  Members are not ordered the way statements are, so a member `class` may be
  declared after the statement that constructs it.
- **A type declaration lands in ONE flat namespace, so it cannot be shadowed.**
  Scala scopes same-named types — a member `class Q` shadows a top-level `Q`,
  and `def a() = { case class Q(v: Int); … }` / `def b() = { case class Q(v:
  Int, w: Int); … }` declare two different `Q`s. There is one table here, so the
  second declaration would silently replace the first and every `Q` in the
  program would mean whichever won. That is refused
  (``type `Q` is already declared``) rather than run. A `class Q` beside an
  `object Q` is the companion idiom, not a redeclaration, and still compiles.
- **A type declared inside a `def` body cannot capture that frame's locals.**
  `def f(k: Int) = { class C(val n: Int) { def g = n + k }; new C(1).g }` is
  legal Scala and aborts here: the class is modelled as a top-level one, so `k`
  is simply an unresolved name and fails the way any unresolved name in a class
  body fails (``+` is not defined between `1` and `null``). The `App` body's own
  `val`s are program globals, so a member class DOES read those
  (`val k = 10; class C(val n: Int) { def g = n + k }` answers 11) — it is the
  `def`/lambda frames that a class body cannot reach into. Closing it means
  lambda-lifting captures into constructor fields, the way [`crate::resolve`]
  already lifts them for a nested `def`.
- **A nested `object` that is itself an entry point is refused.**
  `object T extends App { object U extends App { … } }` and the `def main`
  spelling of the same both exit with ``nested entry object `U```, where Scala
  compiles them (and never runs `U`'s body unless something touches `U`). A
  refusal, not a wrong answer.
- **Singleton `object` `val`s initialize eagerly**, before `main`, rather than
  lazily on first access. Observably identical for pure initializers.

## Object model: how it works

`case class` needs an **ordered, type-tagged record** so `toString` renders
`Point(1,2)` in declared field order and structural `equals`/`hashCode` compare
field-by-field. fusevm's `Value::Obj(u32)` is an opaque handle into a
*frontend-owned* object heap; scalars owns that heap (`src/host.rs`): a
`Value::Obj` indexes a per-run arena of records (`class name`, `is_case`,
`is_object`, ordered `(field, value)` list). Construction, field access,
`toString`/`equals`/`hashCode`, `copy`, and constructor-pattern binding are host
extension builtins (`OBJ_NEW`/`OBJ_CLASS`/`OBJ_COPY`/`OBJ_SET`, plus the `Obj`
arm of `SMETHOD` and the `==` numeric hook) — no fusevm changes. Every
construction site bakes the class name + ordered field names into the bytecode,
so no runtime class registry is needed for construction.

Inheritance adds the one thing a flat record cannot carry: **which supertypes an
instance conforms to**, and **how many of its fields came from the primary
constructor**. Both are published once before `main` by a `TYPE_REG` call per
declared type, and read back by `isInstanceOf`/typed patterns and by the derived
`case class` members. Method calls stay a compile-time-emitted class-tag chain:
for each concrete class that responds to the name, compare the receiver's tag and
call the `Owner$method` subroutine of whichever supertype implements it. When a
name has exactly one implementation and the receiver is a known `this`, the chain
collapses to a direct call, so a hierarchy-free program emits the bytecode it did
before traits existed.

A singleton `object` is the one type with no record to dispatch off: its `val`s
are program globals and its `def`s are statically-called subroutines. It
therefore inherits by *copy* — `inherit_into_objects` in `src/compiler.rs`
splices each supertype's concrete `def`s and `val`s into the object before
anything is indexed, so they compile in the object's own name scope and every
later stage (the method table, the `Name.val` globals, the class-tag chain) sees
one flat singleton. Members the object declares itself win, supertypes are
consulted in linearization order, and inherited `val`s are prepended base-most
first so a supertype's initializer runs before a body that reads it.

## Collections: how the `HashSet`/`HashMap` order is reproduced

Scala's immutable `Set`/`Map` factories switch representation at five entries:
`Set1`..`Set4` / `Map1`..`Map4` keep insertion order, and a `HashSet`/`HashMap`
is a CHAMP hash trie whose iteration order is a function of the elements' hash
codes alone. Printing one therefore cannot be faked, and three pieces are ported
into `src/host.rs` to get it byte-for-byte:

1. **`##`** — `String.hashCode` over UTF-16 code units, `Long.hashCode`,
   `Statics.doubleHash` (which agrees with the `Int`/`Long`/`Float` hash wherever
   the value is exactly representable as one), `Boolean`'s 1231/1237, and
   `MurmurHash3.productHash` — seed, `productPrefix` hash, each element's `##`,
   finalized with the arity — for tuples and `case` records. That last one also
   became the `case class` `hashCode`, which is now Scala's exact value.
   A collection hashes through the rest of the `MurmurHash3` family:
   `orderedHash` with the `Seq` seed for every sequence — which is why
   `List(1,2,3)`, `Vector(1,2,3)`, `1 to 3` and `ListBuffer(1,2,3)` all hash
   alike, including the arithmetic-progression case that makes a `Range` agree
   with its elements — and `unorderedHash` with the `Set`/`Map` seed for a
   `Set` and for a `Map`'s `Tuple2` entries. So a `HashSet` of `List`s, or a
   `HashMap` keyed by one, prints in trie order too, at any nesting depth.
2. **`improve`** — the trie's hash scramble.
3. **The traversal** — five bits per level, low bits first; within a node the
   slots holding one element are emitted first in ascending slot order, then the
   sub-tries in ascending slot order. CHAMP compacts on removal, so this shape
   depends only on the set of hashes and can be computed from the elements
   directly.

A collection's representation is stored alongside it (`HashRep`) because a
derived collection keeps the receiver's: `Set(1,2,3,4,5).filter(_ > 1)` is a
four-element `HashSet`, while `Set(1,2,3).map(_ * 2)` is a `Set3`. `groupBy`
builds through a `HashMap` builder and so is always hashed.

## Block-local `def`s: how they work

The compiler consumes a flat function namespace, so `src/resolve.rs` sits between
the parser and the compiler:

1. The AST is walked with a scope stack. Each block pre-binds its own `def`s
   (giving forward references and mutual recursion inside one block), and every
   `Var`/`Call` is rewritten to the unique global name of whichever `def` it
   actually resolves to. `val`s and parameters shadow an outer `def` from their
   declaration onward.
2. Each hoisted `def` gets the enclosing-frame locals its body reads appended as
   extra parameters, and every call site passes them; capture sets propagate
   through calls to a fixpoint, so a local `def` that calls a sibling capturing
   `k` threads `k` too.

Final names are chosen only after the whole program is walked, and the first
claimant of a name keeps it verbatim — a program with no collisions compiles to
exactly the bytecode it did before this pass existed.

## Exception unwinding: how it works, and what it costs

fusevm has no unwind opcode, and scalars lowers `def`s to fusevm's **native**
`Op::Call` frames — so a raise cannot longjmp out of a frame the way a
sub-chunk-interpreting frontend can. `try` is instead a cooperative protocol
split across the host and the compiler:

* **Runtime half (`src/host.rs`).** A raise parks the exception in a
  thread-local `PENDING` slot instead of halting, provided a `try` is
  dynamically active (`TRY_DEPTH > 0`). Every builtin with an observable side
  effect — `println`/`print`, `/`, method dispatch, closure application, the
  `MatchError` fall-through, the numeric hook — short-circuits while an
  exception is in flight, so nothing is printed and nothing faults a second time
  between the raise and its handler.
* **Compile-time half (`src/compiler.rs`).** When the program contains a `try`
  anywhere, an `EXC_PENDING` test is emitted after every statement. The
  innermost enclosing construct decides where a `true` answer jumps: out of a
  loop, out of a `def` frame (`ReturnValue Unit`), into a `catch` dispatch, or —
  at top level — into the terminal abort. Binding stores (`val`/`var` init,
  assignment, and object-`val` update) get their check *before* the store and
  drop the computed value, so a raise part-way through an initializer never
  commits garbage to a binding a handler could read.

Two consequences worth stating plainly:

- **Unwinding is statement-granular.** A raise part-way through one statement
  finishes evaluating that statement's remaining operands before control reaches
  the handler. Those evaluations run on garbage values with the side-effecting
  builtins suppressed, so they are unobservable in the cases the test suite and
  the `exc` fuzzer mode cover — but it is a real approximation, not exact JVM
  semantics.
- **A program with no `try` pays nothing.** The checks are not emitted at all,
  and a fault halts the run exactly as it did before exceptions existed.

`case NonFatal(e)`, exception chaining (`getCause`, `initCause`,
`addSuppressed`), stack traces, and `Try`/`Success`/`Failure` are not modeled.

## What a clean parity-fuzz run does not prove

The `parity-fuzz` binary diffs generated programs against a real `scala`. It is
the main evidence that this frontend is faithful, so it is worth being precise
about the ways a green run has been wrong.

### What each harness is structurally unable to report

Below each is what it CANNOT see — not a gap in how many probes have run, but a
class of divergence the harness has no way to express. The first column of every
entry is the constant that hides the axis.

| Harness | Blind to |
| --- | --- |
| `parity-fuzz` generators | Non-ASCII source. Every literal any generator emits is ASCII, so the lexer's byte-offset operator lookahead splitting a multi-byte character (`List("a", "é")` PANICKED) was unreachable from any seed. |
| `parity-fuzz` `differs` | The exact exit code (only zero-vs-nonzero), all of stderr, and therefore *which* exception went uncaught: two runs that print the same bytes and both die compare equal whatever killed them. |
| `parity-fuzz` `diverges` | An oracle that hangs. A timeout on the reference side is excused as pathology, so a construct that makes real Scala loop forever can never be reported. |
| `parity-fuzz` `build_program` | Anything that differs by ENTRY SHAPE, while the shape was a constant. `--entry` makes it an axis; before it, the by-name-thunk write below could not be seen. |
| `tests/parity.rs` | Any program that must FAIL — every record asserts success — plus stderr, the exit code, and program arguments. A record is one line, so no program containing a TAB or a real newline is expressible. |
| `tests/mixed_numeric.rs` | Whether the compiler ever ROUTES to `numeric_hook`; it calls the hook directly. It was also blind to `Div`'s value until this round: `Div` appeared only in the never-rejected test, which asserts the answer is a `Float` and not which one. |
| `tests/eval.rs` | The same entry-shape constant — `wrap()` is `object T extends App` — and, for the 350-odd tests that use `run` rather than `run_full`, the whole of stderr. |
| oracle gates | Whether an expectation was ever CAPTURED. Gating the reference's JVM version and locale says which Scala answered; it says nothing about a frozen string no Scala ever printed. Only re-running the program answers that. |

The `Div` gap and the entry-shape gap are closed (twelve `div` records; `--entry
app|main|mainsig`). The non-ASCII gap is closed in the frontend and pinned by a
test, though the generators are still ASCII-only. The rest stand as stated.

**A debug-only fault can wrap into agreement under `--release`.** The host used
to fold an integral `sum`/`product` with `Iterator::sum`, whose `+` is the
checked one. `List(Long.MaxValue, 1L).sum` therefore *raised* in a debug build,
where Scala answers `-9223372036854775808`. The same expression under
`--release` wraps silently and agrees — so the bug was visible only in the
profile nobody ships, and a release-mode fuzz run would have scored it clean
forever. Any arithmetic the host performs in Rust must say which overflow
behaviour it means (`wrapping_add`, `checked_*`) rather than inherit it from the
profile; where it does not, a green run is evidence about the profile, not about
the frontend.

**A mode only covers the shapes it generates.** The `overflow` mode exercises
widths, not name resolution, so it cannot see a regression in
`could_be_user_instance` — that needs a program declaring a member whose name
collides with a stdlib one (`class Thing { def abs: Int }`) *and* a receiver
that is neither a literal nor a plain variable. `(-2147483648 + 0).abs`
answers `-2147483648` in Scala; a build that drops the proven-width exemption
answers `2147483648` and still scores a clean `overflow` run across thousands of
programs. Do not read a clean run in one mode as coverage of another.

**A run the machine broke is not a run.** `run_prog` reports `exit == -1` for
its own failures — it could not write the temp file, or could not spawn the
binary — which a full disk causes in bulk. Those are counted and reported
separately (`harness-err`), excused from the divergence count, and once they
exceed a tenth of the run the harness exits 2 rather than green, because those
programs were never compared. An *oracle* exiting non-zero is deliberately NOT
excused: "scala rejects a program we accept" is a real divergence.

**An oracle that disagrees with itself is not an oracle.** The reference `scala`
is a launcher script, and two things outside the repository decide what it
answers. `JAVA_HOME` picks the JVM, and `Double.toString` was reimplemented in
JDK 19 (JDK-4511638) — Corretto 17.0.4.1 prints `1.0e23` as `9.999999999999999E22`
where OpenJDK 21.0.12 and 26.0.2 print `1.0E23`, which is one of the axes this
fuzzer is most biased toward. `LANG`/`LC_ALL` pick the default locale, which
decides the decimal separator and the grouping separator of every `%f`/`%e`/`%,d`
conversion and the case-mapping rules of `toUpperCase`. `resolve_oracle` probes
both and exits 2 rather than comparing against a reference that would make a
whole class of "divergences" spurious.

Neither gate answers the question one layer above them: **whether a frozen
expectation was ever captured at all.** A pin that no version and no locale of
the reference ever printed passes every gate, because the gates check WHICH
Scala answered rather than WHETHER one did. The only test is re-running the
program. Every record in `tests/data/mixed_numeric_expected.txt` was
regenerated from Scala 3.8.4 on JDK 26.0.2 this round and reproduced
byte-for-byte, and every exception message pinned in `tests/eval.rs` was re-run
against the same pair.

## A user member whose name collides with a stdlib one

`method_width` refuses to narrow a receiver's width when any user class
declares a member sharing the name (`class Thing { def abs: Int = 5 }` makes
every `abs` suspect), because a stdlib width rule must not outrank a user
declaration. Receivers that are literals, or whose numeric width is already
proven, are exempt. Two costs of that rule are unfixed:

**A receiver of unproven width answers the unnarrowed result.** The exemption
is driven by `num_ty`, so a receiver it cannot prove — an `if`, for instance —
stays poisoned and the result is not narrowed to 32 bits:

    class Thing { def abs: Int = 5 }
    println((if (true) -2147483648 else 0).abs)   // scala: -2147483648
                                                  // scalars: 2147483648

The instinct here is that skipping a narrow is the conservative choice. It is
not. **Declining to narrow is not the safe direction, because the `Int`
boundary is exactly where the widths disagree** — `abs` is the operation whose
answer differs between 32 and 64 bits, so refusing to commit to a width does
not abstain, it silently picks the 64-bit one. `(-2147483648).abs` and `x.abs`
for a proven `x` are both correct; only the unproven receiver is wrong.

**Compile time is exponential in the length of the method chain.** Deciding the
exemption calls `num_ty` on the receiver, which re-enters `method_width` for a
method-call receiver, which asks about *its* receiver in turn — so an `n`-link
chain is walked about `2^n` times. Measured on a chain of `x.abs.abs…` with
`Thing` declared:

    depth 16   0.15s
    depth 18   0.35s
    depth 20   2.22s
    depth 22   7.76s
    depth 26   136s

Roughly a doubling per link. Programs without a colliding member name are
unaffected — the name is tested first and answers `false` before any receiver
is walked — so this needs both a collision and a deep chain. The fix is to
memoise the width walk or to prove the common receiver shapes without
re-entering `num_ty`; simply treating every non-literal receiver as unprovable
is NOT a fix, because it produces the wrong answer above for `(-2147483648 + 0).abs`
as well.

## Operational: a fuzz number from a shared checkout is not trustworthy

Several agent sessions work this repo at once, against ONE working copy and one
`target/`. `parity-fuzz` spawns `target/debug/scala` fresh for every program, so
it reads that path thousands of times over a run lasting tens of minutes. Any
`cargo build` in any other session — including one that only asks for a
different binary, since the library is shared — relinks `target/debug/scala`
underneath a run already in progress.

The failure is silent, which is what makes it worse than a crash. The peer's
run does not error; it simply compares its first N programs against one binary
and its remaining programs against another, then prints a single clean score
for both halves. Nothing in the output says which binary produced which result,
and a divergence introduced by the newer code reads as a divergence in the
older. This session did it: a 4000-program run was measuring a build that was
replaced mid-run, and its partial number had to be discarded rather than
reported.

So: **a clean fuzz number is evidence only if no other session built during it.**
Before quoting one, confirm the binary's mtime predates the run's start. Before
building, check whether a peer has a run in flight (`pgrep -f parity-fuzz`) —
and if one does, either wait or accept that you are invalidating their result.

An untaken mitigation, recorded rather than implemented: have the harness copy
the frontend to a pid-suffixed path once at startup and spawn that copy, the way
scratch files are already namespaced per pid. The run then holds its own
immutable binary and a peer's rebuild cannot reach it. It costs one file copy
per run and would make the number self-contained; it has NOT been done, so today
the discipline above is the only protection.
