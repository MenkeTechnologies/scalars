//! `scala.collection.immutable.RedBlackTree`, ported from the Scala 3.10
//! library (`scala/collection/immutable/RedBlackTree.scala`) — the storage of an
//! immutable `TreeSet`/`TreeMap`.
//!
//! What a program can observe of a sorted collection besides its contents is
//! which comparisons its ordering is asked to make — an `Ordering.by(f)` runs
//! `f` once per operand of every one — and which results are the receiver
//! itself (`s + x` for an `x` already held is `s`, `eq` and all). Both are
//! decided by the tree's exact shape and colouring and by the algorithm that
//! walks it, so this is the library's algorithm step for step: the Okasaki /
//! Kahrs insert and delete, the "Just Join" bulk operations (`union`,
//! `intersect`, `difference`, `filter`, `partition`, `take`/`drop`/`slice`,
//! the range views), and `fromOrderedKeys`' perfectly balanced build.
//!
//! Nodes are immutable and shared between versions through `Rc`; "the same
//! subtree" (`eq` in the library) is [`Rc::ptr_eq`]. The library's builders
//! insert through MUTABLE nodes (`mutableUpd`) to save allocations; their
//! balancing reads only the inserted child's colours, never the node being
//! rebuilt's, so they produce the same shapes and make the same comparisons as
//! the immutable insert and are not reproduced separately.
//!
//! The ordering is supplied by a [`Cx`], whose comparisons may run user code
//! and so may fail: every comparing operation answers a `Result`.

use std::cmp::Ordering;
use std::rc::Rc;

/// One node: a key, its value (unit for a set), both subtrees, the colour, and
/// the size of the subtree it roots.
pub struct Node<K, V> {
    pub key: K,
    pub value: V,
    pub left: Tree<K, V>,
    pub right: Tree<K, V>,
    black: bool,
    count: usize,
}

/// A tree: `None` is the empty tree, which the library represents as `null`.
pub type Tree<K, V> = Option<Rc<Node<K, V>>>;

type R<T> = Result<T, String>;

/// A test of two values for equality that may run user code.
pub type ValueEq<'a, V> = &'a mut dyn FnMut(&V, &V) -> R<bool>;

/// The ordering a tree is kept in, and the JVM reference identity the library
/// tests before comparing (`k eq tree.key`, `v ne tree.value`).
pub trait Cx<K, V> {
    fn compare(&mut self, a: &K, b: &K) -> R<Ordering>;
    fn lt(&mut self, a: &K, b: &K) -> R<bool>;
    fn lteq(&mut self, a: &K, b: &K) -> R<bool>;
    fn equiv(&mut self, a: &K, b: &K) -> R<bool>;
    fn same_key(&self, a: &K, b: &K) -> bool;
    fn same_value(&self, a: &V, b: &V) -> bool;
}

/// `a eq b` for two trees.
pub fn same<K, V>(a: &Tree<K, V>, b: &Tree<K, V>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => Rc::ptr_eq(x, y),
        _ => false,
    }
}

pub fn count<K, V>(t: &Tree<K, V>) -> usize {
    t.as_ref().map_or(0, |n| n.count)
}

fn is_red<K, V>(t: &Tree<K, V>) -> bool {
    t.as_ref().is_some_and(|n| !n.black)
}

fn is_black_tree<K, V>(t: &Tree<K, V>) -> bool {
    t.as_ref().is_some_and(|n| n.black)
}

fn node<K: Clone, V: Clone>(
    black: bool,
    key: K,
    value: V,
    left: Tree<K, V>,
    right: Tree<K, V>,
) -> Rc<Node<K, V>> {
    let count = count(&left) + count(&right) + 1;
    Rc::new(Node {
        key,
        value,
        left,
        right,
        black,
        count,
    })
}

fn red_tree<K: Clone, V: Clone>(k: K, v: V, l: Tree<K, V>, r: Tree<K, V>) -> Rc<Node<K, V>> {
    node(false, k, v, l, r)
}

fn black_tree<K: Clone, V: Clone>(k: K, v: V, l: Tree<K, V>, r: Tree<K, V>) -> Rc<Node<K, V>> {
    node(true, k, v, l, r)
}

// The `Tree` instance methods. Each answers the receiver itself when nothing
// would change, which is where the library's structural sharing comes from.

fn black<K: Clone, V: Clone>(t: &Rc<Node<K, V>>) -> Rc<Node<K, V>> {
    if t.black {
        t.clone()
    } else {
        recolour(t, true)
    }
}

fn red<K: Clone, V: Clone>(t: &Rc<Node<K, V>>) -> Rc<Node<K, V>> {
    if t.black {
        recolour(t, false)
    } else {
        t.clone()
    }
}

fn recolour<K: Clone, V: Clone>(t: &Rc<Node<K, V>>, black: bool) -> Rc<Node<K, V>> {
    Rc::new(Node {
        key: t.key.clone(),
        value: t.value.clone(),
        left: t.left.clone(),
        right: t.right.clone(),
        black,
        count: t.count,
    })
}

fn with_v<K: Clone, V: Clone>(t: &Rc<Node<K, V>>, v: V) -> Rc<Node<K, V>> {
    Rc::new(Node {
        key: t.key.clone(),
        value: v,
        left: t.left.clone(),
        right: t.right.clone(),
        black: t.black,
        count: t.count,
    })
}

/// `withLeftRight` and its coloured forms: `colour` is `None` to keep the
/// node's own.
fn with_lr<K: Clone, V: Clone>(
    t: &Rc<Node<K, V>>,
    colour: Option<bool>,
    l: Tree<K, V>,
    r: Tree<K, V>,
) -> Rc<Node<K, V>> {
    let black = colour.unwrap_or(t.black);
    if same(&l, &t.left) && same(&r, &t.right) && black == t.black {
        t.clone()
    } else {
        node(black, t.key.clone(), t.value.clone(), l, r)
    }
}

fn with_left<K: Clone, V: Clone>(t: &Rc<Node<K, V>>, l: Tree<K, V>) -> Rc<Node<K, V>> {
    with_lr(t, None, l, t.right.clone())
}

fn with_right<K: Clone, V: Clone>(t: &Rc<Node<K, V>>, r: Tree<K, V>) -> Rc<Node<K, V>> {
    with_lr(t, None, t.left.clone(), r)
}

fn black_with_left<K: Clone, V: Clone>(t: &Rc<Node<K, V>>, l: Tree<K, V>) -> Rc<Node<K, V>> {
    with_lr(t, Some(true), l, t.right.clone())
}

fn black_with_right<K: Clone, V: Clone>(t: &Rc<Node<K, V>>, r: Tree<K, V>) -> Rc<Node<K, V>> {
    with_lr(t, Some(true), t.left.clone(), r)
}

fn red_with_left<K: Clone, V: Clone>(t: &Rc<Node<K, V>>, l: Tree<K, V>) -> Rc<Node<K, V>> {
    with_lr(t, Some(false), l, t.right.clone())
}

fn red_with_right<K: Clone, V: Clone>(t: &Rc<Node<K, V>>, r: Tree<K, V>) -> Rc<Node<K, V>> {
    with_lr(t, Some(false), t.left.clone(), r)
}

fn blacken<K: Clone, V: Clone>(t: Tree<K, V>) -> Tree<K, V> {
    t.map(|n| black(&n))
}

/// Blacken a red root with a red child — for a subtree built by `upd`/`updNth`.
fn maybe_blacken<K: Clone, V: Clone>(t: Rc<Node<K, V>>) -> Rc<Node<K, V>> {
    if !t.black && (is_red(&t.left) || is_red(&t.right)) {
        black(&t)
    } else {
        t
    }
}

// ───────────────────────────── lookup ─────────────────────────────

/// The node whose key the ordering calls equal to `x`.
pub fn lookup<K, V>(cx: &mut dyn Cx<K, V>, t: &Tree<K, V>, x: &K) -> R<Option<Rc<Node<K, V>>>> {
    let mut t = t.clone();
    while let Some(n) = t {
        match cx.compare(x, &n.key)? {
            Ordering::Less => t = n.left.clone(),
            Ordering::Greater => t = n.right.clone(),
            Ordering::Equal => return Ok(Some(n)),
        }
    }
    Ok(None)
}

pub fn smallest<K, V>(t: &Tree<K, V>) -> Option<&Rc<Node<K, V>>> {
    let mut n = t.as_ref()?;
    while let Some(l) = &n.left {
        n = l;
    }
    Some(n)
}

pub fn greatest<K, V>(t: &Tree<K, V>) -> Option<&Rc<Node<K, V>>> {
    let mut n = t.as_ref()?;
    while let Some(r) = &n.right {
        n = r;
    }
    Some(n)
}

/// The smallest node whose key is at least `x`.
pub fn min_after<K, V>(cx: &mut dyn Cx<K, V>, t: &Tree<K, V>, x: &K) -> R<Option<Rc<Node<K, V>>>> {
    let Some(n) = t else { return Ok(None) };
    match cx.compare(x, &n.key)? {
        Ordering::Equal => Ok(Some(n.clone())),
        Ordering::Less => Ok(min_after(cx, &n.left, x)?.or_else(|| Some(n.clone()))),
        Ordering::Greater => min_after(cx, &n.right, x),
    }
}

/// The largest node whose key is below `x`.
pub fn max_before<K, V>(cx: &mut dyn Cx<K, V>, t: &Tree<K, V>, x: &K) -> R<Option<Rc<Node<K, V>>>> {
    let Some(n) = t else { return Ok(None) };
    if cx.compare(x, &n.key)? != Ordering::Greater {
        max_before(cx, &n.left, x)
    } else {
        Ok(max_before(cx, &n.right, x)?.or_else(|| Some(n.clone())))
    }
}

/// The entries in key order.
pub fn entries<K: Clone, V: Clone>(t: &Tree<K, V>) -> Vec<(K, V)> {
    fn go<K: Clone, V: Clone>(t: &Tree<K, V>, out: &mut Vec<(K, V)>) {
        if let Some(n) = t {
            go(&n.left, out);
            out.push((n.key.clone(), n.value.clone()));
            go(&n.right, out);
        }
    }
    let mut out = Vec::with_capacity(count(t));
    go(t, &mut out);
    out
}

/// What `iterator(tree, Some(start))` yields: the library's `TreeIterator`
/// locates its first node in its constructor (`startFrom`, comparing with
/// `lteq`) and walks without comparing after that, so the whole remaining
/// sequence is decided here.
pub fn entries_from<K: Clone, V: Clone>(
    cx: &mut dyn Cx<K, V>,
    root: &Tree<K, V>,
    start: &K,
) -> R<Vec<(K, V)>> {
    let mut stack: Vec<Rc<Node<K, V>>> = Vec::new();
    let mut t = root.clone();
    let mut lookahead = loop {
        match t {
            None => break stack.pop(),
            Some(n) => {
                t = if cx.lteq(start, &n.key)? {
                    stack.push(n.clone());
                    n.left.clone()
                } else {
                    n.right.clone()
                };
            }
        }
    };
    let mut out = Vec::new();
    while let Some(n) = lookahead {
        out.push((n.key.clone(), n.value.clone()));
        lookahead = left_most_or_pop(&mut stack, n.right.clone());
    }
    Ok(out)
}

fn left_most_or_pop<K, V>(
    stack: &mut Vec<Rc<Node<K, V>>>,
    mut t: Tree<K, V>,
) -> Option<Rc<Node<K, V>>> {
    loop {
        match t {
            None => return stack.pop(),
            Some(n) if n.left.is_none() => return Some(n),
            Some(n) => {
                t = n.left.clone();
                stack.push(n);
            }
        }
    }
}

/// `keysEqual` / `entriesEqual`: equal sizes, then the two in-order walks
/// side by side, skipping a subtree the two trees share and comparing keys by
/// `ordering.equiv` (after reference identity) — and values by `==` too when
/// `values` is given.
pub fn equal<K, V>(
    cx: &mut dyn Cx<K, V>,
    a: &Tree<K, V>,
    b: &Tree<K, V>,
    values: Option<ValueEq<V>>,
) -> R<bool> {
    if same(a, b) {
        return Ok(true);
    }
    if a.is_none() || b.is_none() || count(a) != count(b) {
        return Ok(false);
    }
    let mut values = values;
    let (mut sa, mut sb) = (Vec::new(), Vec::new());
    let mut la = left_most_or_pop(&mut sa, a.clone());
    let mut lb = left_most_or_pop(&mut sb, b.clone());
    let mut eq = true;
    while eq {
        let (Some(x), Some(y)) = (la.clone(), lb.clone()) else {
            break;
        };
        if Rc::ptr_eq(&x, &y) {
            la = sa.pop();
            lb = sb.pop();
            continue;
        }
        eq = cx.same_key(&x.key, &y.key) || cx.equiv(&x.key, &y.key)?;
        if eq {
            if let Some(f) = values.as_mut() {
                eq = f(&x.value, &y.value)?;
            }
        }
        la = left_most_or_pop(&mut sa, x.right.clone());
        lb = left_most_or_pop(&mut sb, y.right.clone());
    }
    Ok(eq && la.is_none() && lb.is_none())
}

// ───────────────────────────── insert ─────────────────────────────

/// `RB.update`: `k` inserted, or — with `overwrite` — its value replaced.
pub fn update<K: Clone, V: Clone>(
    cx: &mut dyn Cx<K, V>,
    t: &Tree<K, V>,
    k: &K,
    v: &V,
    overwrite: bool,
) -> R<Tree<K, V>> {
    Ok(blacken(Some(upd(cx, t, k, v, overwrite)?)))
}

fn upd<K: Clone, V: Clone>(
    cx: &mut dyn Cx<K, V>,
    t: &Tree<K, V>,
    k: &K,
    v: &V,
    overwrite: bool,
) -> R<Rc<Node<K, V>>> {
    let Some(n) = t else {
        return Ok(red_tree(k.clone(), v.clone(), None, None));
    };
    if cx.same_key(k, &n.key) {
        return Ok(if overwrite {
            with_v(n, v.clone())
        } else {
            n.clone()
        });
    }
    Ok(match cx.compare(k, &n.key)? {
        Ordering::Less => {
            let l = upd(cx, &n.left, k, v, overwrite)?;
            balance_left(n, l)
        }
        Ordering::Greater => {
            let r = upd(cx, &n.right, k, v, overwrite)?;
            balance_right(n, r)
        }
        Ordering::Equal if overwrite && !cx.same_value(v, &n.value) => with_v(n, v.clone()),
        Ordering::Equal => n.clone(),
    })
}

fn upd_nth<K: Clone, V: Clone>(t: &Tree<K, V>, idx: usize, k: &K, v: &V) -> Rc<Node<K, V>> {
    let Some(n) = t else {
        return red_tree(k.clone(), v.clone(), None, None);
    };
    let rank = count(&n.left) + 1;
    if idx < rank {
        let l = upd_nth(&n.left, idx, k, v);
        balance_left(n, l)
    } else if idx > rank {
        let r = upd_nth(&n.right, idx - rank, k, v);
        balance_right(n, r)
    } else {
        n.clone()
    }
}

fn balance_left<K: Clone, V: Clone>(t: &Rc<Node<K, V>>, nl: Rc<Node<K, V>>) -> Rc<Node<K, V>> {
    if t.left.as_ref().is_some_and(|l| Rc::ptr_eq(l, &nl)) {
        return t.clone();
    }
    if !nl.black {
        if let Some(nll) = nl.left.as_ref().filter(|x| !x.black) {
            let rl = black(nll);
            let rr = black_with_left(t, nl.right.clone());
            return with_lr(&nl, None, Some(rl), Some(rr));
        }
        if let Some(nlr) = nl.right.as_ref().filter(|x| !x.black) {
            let rl = black_with_right(&nl, nlr.left.clone());
            let rr = black_with_left(t, nlr.right.clone());
            return with_lr(nlr, None, Some(rl), Some(rr));
        }
    }
    with_left(t, Some(nl))
}

fn balance_right<K: Clone, V: Clone>(t: &Rc<Node<K, V>>, nr: Rc<Node<K, V>>) -> Rc<Node<K, V>> {
    if t.right.as_ref().is_some_and(|r| Rc::ptr_eq(r, &nr)) {
        return t.clone();
    }
    if !nr.black {
        if let Some(nrl) = nr.left.as_ref().filter(|x| !x.black) {
            let rl = black_with_right(t, nrl.left.clone());
            let rr = black_with_left(&nr, nrl.right.clone());
            return with_lr(nrl, None, Some(rl), Some(rr));
        }
        if let Some(nrr) = nr.right.as_ref().filter(|x| !x.black) {
            let rl = black_with_right(t, nr.left.clone());
            let rr = black(nrr);
            return with_lr(&nr, None, Some(rl), Some(rr));
        }
    }
    with_right(t, Some(nr))
}

// ───────────────────────────── delete ─────────────────────────────

/// `RB.delete`: the key the ordering calls equal to `k` removed.
pub fn delete<K: Clone, V: Clone>(cx: &mut dyn Cx<K, V>, t: &Tree<K, V>, k: &K) -> R<Tree<K, V>> {
    Ok(blacken(del(cx, t, k)?))
}

fn del<K: Clone, V: Clone>(cx: &mut dyn Cx<K, V>, t: &Tree<K, V>, k: &K) -> R<Tree<K, V>> {
    let Some(n) = t else { return Ok(None) };
    Ok(match cx.compare(k, &n.key)? {
        Ordering::Less => {
            let nl = del(cx, &n.left, k)?;
            if same(&nl, &n.left) {
                Some(n.clone())
            } else if is_black_tree(&n.left) {
                Some(bal_left(n, nl, n.right.clone()))
            } else {
                Some(red_with_left(n, nl))
            }
        }
        Ordering::Greater => {
            let nr = del(cx, &n.right, k)?;
            if same(&nr, &n.right) {
                Some(n.clone())
            } else if is_black_tree(&n.right) {
                Some(bal_right(n, n.left.clone(), nr))
            } else {
                Some(red_with_right(n, nr))
            }
        }
        Ordering::Equal => append(&n.left, &n.right),
    })
}

fn balance<K: Clone, V: Clone>(
    t: &Rc<Node<K, V>>,
    tl: Tree<K, V>,
    tr: Tree<K, V>,
) -> Rc<Node<K, V>> {
    if let Some(l) = tl.as_ref().filter(|x| !x.black) {
        if let Some(r) = tr.as_ref().filter(|x| !x.black) {
            return with_lr(t, Some(false), Some(black(l)), Some(black(r)));
        }
        if let Some(ll) = l.left.as_ref().filter(|x| !x.black) {
            let rr = with_lr(t, Some(true), l.right.clone(), tr);
            return with_lr(l, None, Some(black(ll)), Some(rr));
        }
        if let Some(lr) = l.right.as_ref().filter(|x| !x.black) {
            let rl = black_with_right(l, lr.left.clone());
            let rr = with_lr(t, Some(true), lr.right.clone(), tr);
            return with_lr(lr, None, Some(rl), Some(rr));
        }
        return with_lr(t, Some(true), tl, tr);
    }
    if let Some(r) = tr.as_ref().filter(|x| !x.black) {
        if let Some(rr) = r.right.as_ref().filter(|x| !x.black) {
            let rl = with_lr(t, Some(true), tl, r.left.clone());
            return with_lr(r, None, Some(rl), Some(black(rr)));
        }
        if let Some(rl) = r.left.as_ref().filter(|x| !x.black) {
            let a = with_lr(t, Some(true), tl, rl.left.clone());
            let b = with_lr(r, Some(true), rl.right.clone(), r.right.clone());
            return with_lr(rl, None, Some(a), Some(b));
        }
    }
    with_lr(t, Some(true), tl, tr)
}

fn bal_left<K: Clone, V: Clone>(
    t: &Rc<Node<K, V>>,
    tl: Tree<K, V>,
    tr: Tree<K, V>,
) -> Rc<Node<K, V>> {
    if let Some(l) = tl.as_ref().filter(|x| !x.black) {
        return with_lr(t, Some(false), Some(black(l)), tr);
    }
    let r = tr.as_ref().expect("Defect: invariance violation");
    if r.black {
        return balance(t, tl, Some(red(r)));
    }
    let rl = r
        .left
        .as_ref()
        .filter(|x| x.black)
        .expect("Defect: invariance violation");
    let a = with_lr(t, Some(true), tl, rl.left.clone());
    let rr = red(r.right.as_ref().expect("Defect: invariance violation"));
    let b = balance(r, rl.right.clone(), Some(rr));
    with_lr(rl, Some(false), Some(a), Some(b))
}

fn bal_right<K: Clone, V: Clone>(
    t: &Rc<Node<K, V>>,
    tl: Tree<K, V>,
    tr: Tree<K, V>,
) -> Rc<Node<K, V>> {
    if let Some(r) = tr.as_ref().filter(|x| !x.black) {
        return with_lr(t, Some(false), tl, Some(black(r)));
    }
    let l = tl.as_ref().expect("Defect: invariance violation");
    if l.black {
        return balance(t, Some(red(l)), tr);
    }
    let lr = l
        .right
        .as_ref()
        .filter(|x| x.black)
        .expect("Defect: invariance violation");
    let ll = red(l.left.as_ref().expect("Defect: invariance violation"));
    let a = balance(l, Some(ll), lr.left.clone());
    let b = with_lr(t, Some(true), lr.right.clone(), tr);
    with_lr(lr, Some(false), Some(a), Some(b))
}

/// Two trees of equal black height, every key of `tl` below every key of `tr`,
/// made one.
fn append<K: Clone, V: Clone>(tl: &Tree<K, V>, tr: &Tree<K, V>) -> Tree<K, V> {
    let (Some(l), Some(r)) = (tl, tr) else {
        return tl.clone().or_else(|| tr.clone());
    };
    Some(match (l.black, r.black) {
        (false, false) => {
            let bc = append(&l.right, &r.left);
            match bc.as_ref().filter(|x| !x.black) {
                Some(b) => {
                    let a = with_right(l, b.left.clone());
                    let c = with_left(r, b.right.clone());
                    with_lr(b, None, Some(a), Some(c))
                }
                None => with_right(l, Some(with_left(r, bc))),
            }
        }
        (false, true) => with_right(l, append(&l.right, tr)),
        (true, true) => {
            let bc = append(&l.right, &r.left);
            match bc.as_ref().filter(|x| !x.black) {
                Some(b) => {
                    let a = with_right(l, b.left.clone());
                    let c = with_left(r, b.right.clone());
                    with_lr(b, None, Some(a), Some(c))
                }
                None => bal_left(l, l.left.clone(), Some(with_left(r, bc))),
            }
        }
        (true, false) => with_left(r, append(tl, &r.left)),
    })
}

/// `RB.tail`: the smallest entry removed.
pub fn tail<K: Clone, V: Clone>(t: &Tree<K, V>) -> Tree<K, V> {
    fn go<K: Clone, V: Clone>(n: &Rc<Node<K, V>>) -> Tree<K, V> {
        match &n.left {
            None => n.right.clone(),
            Some(l) if l.black => Some(bal_left(n, go(l), n.right.clone())),
            Some(l) => Some(red_with_left(n, go(l))),
        }
    }
    blacken(t.as_ref().and_then(go))
}

/// `RB.init`: the greatest entry removed.
pub fn init<K: Clone, V: Clone>(t: &Tree<K, V>) -> Tree<K, V> {
    fn go<K: Clone, V: Clone>(n: &Rc<Node<K, V>>) -> Tree<K, V> {
        match &n.right {
            None => n.left.clone(),
            Some(r) if r.black => Some(bal_right(n, n.left.clone(), go(r))),
            Some(r) => Some(red_with_right(n, go(r))),
        }
    }
    blacken(t.as_ref().and_then(go))
}

// ───────────────────────────── ranges ─────────────────────────────

/// The bounds of a range view: `from` inclusive, `until` exclusive, `to`
/// inclusive (`rangeImpl` and the `to` the library builds `rangeTo` from).
pub enum Bound<K> {
    From(K),
    Until(K),
    To(K),
    Range(K, K),
}

pub fn range<K: Clone, V: Clone>(
    cx: &mut dyn Cx<K, V>,
    t: &Tree<K, V>,
    b: &Bound<K>,
) -> R<Tree<K, V>> {
    Ok(blacken(match b {
        Bound::From(k) => do_from(cx, t, k)?,
        Bound::Until(k) => do_until(cx, t, k)?,
        Bound::To(k) => do_to(cx, t, k)?,
        Bound::Range(f, u) => do_range(cx, t, f, u)?,
    }))
}

fn do_from<K: Clone, V: Clone>(cx: &mut dyn Cx<K, V>, t: &Tree<K, V>, from: &K) -> R<Tree<K, V>> {
    let Some(n) = t else { return Ok(None) };
    if cx.lt(&n.key, from)? {
        return do_from(cx, &n.right, from);
    }
    let nl = do_from(cx, &n.left, from)?;
    Ok(Some(if same(&nl, &n.left) {
        n.clone()
    } else if nl.is_none() {
        maybe_blacken(upd(cx, &n.right, &n.key, &n.value, false)?)
    } else {
        join(&nl, &n.key, &n.value, &n.right)
    }))
}

fn do_to<K: Clone, V: Clone>(cx: &mut dyn Cx<K, V>, t: &Tree<K, V>, to: &K) -> R<Tree<K, V>> {
    let Some(n) = t else { return Ok(None) };
    if cx.lt(to, &n.key)? {
        return do_to(cx, &n.left, to);
    }
    let nr = do_to(cx, &n.right, to)?;
    Ok(Some(if same(&nr, &n.right) {
        n.clone()
    } else if nr.is_none() {
        maybe_blacken(upd(cx, &n.left, &n.key, &n.value, false)?)
    } else {
        join(&n.left, &n.key, &n.value, &nr)
    }))
}

fn do_until<K: Clone, V: Clone>(cx: &mut dyn Cx<K, V>, t: &Tree<K, V>, until: &K) -> R<Tree<K, V>> {
    let Some(n) = t else { return Ok(None) };
    if cx.lteq(until, &n.key)? {
        return do_until(cx, &n.left, until);
    }
    let nr = do_until(cx, &n.right, until)?;
    Ok(Some(if same(&nr, &n.right) {
        n.clone()
    } else if nr.is_none() {
        maybe_blacken(upd(cx, &n.left, &n.key, &n.value, false)?)
    } else {
        join(&n.left, &n.key, &n.value, &nr)
    }))
}

fn do_range<K: Clone, V: Clone>(
    cx: &mut dyn Cx<K, V>,
    t: &Tree<K, V>,
    from: &K,
    until: &K,
) -> R<Tree<K, V>> {
    let Some(n) = t else { return Ok(None) };
    if cx.lt(&n.key, from)? {
        return do_range(cx, &n.right, from, until);
    }
    if cx.lteq(until, &n.key)? {
        return do_range(cx, &n.left, from, until);
    }
    let nl = do_from(cx, &n.left, from)?;
    let nr = do_until(cx, &n.right, until)?;
    Ok(Some(if same(&nl, &n.left) && same(&nr, &n.right) {
        n.clone()
    } else if nl.is_none() {
        upd(cx, &nr, &n.key, &n.value, false)?
    } else if nr.is_none() {
        upd(cx, &nl, &n.key, &n.value, false)?
    } else {
        join(&nl, &n.key, &n.value, &nr)
    }))
}

// ─────────────────────── positional slices ───────────────────────

pub fn drop<K: Clone, V: Clone>(t: &Tree<K, V>, n: usize) -> Tree<K, V> {
    blacken(do_drop(t, n))
}

pub fn take<K: Clone, V: Clone>(t: &Tree<K, V>, n: usize) -> Tree<K, V> {
    blacken(do_take(t, n))
}

pub fn slice<K: Clone, V: Clone>(t: &Tree<K, V>, from: usize, until: usize) -> Tree<K, V> {
    blacken(do_slice(t, from, until))
}

fn do_drop<K: Clone, V: Clone>(t: &Tree<K, V>, n: usize) -> Tree<K, V> {
    let Some(x) = t.as_ref().filter(|_| n > 0) else {
        return t.clone();
    };
    if n >= x.count {
        return None;
    }
    let l = count(&x.left);
    if n > l {
        do_drop(&x.right, n - l - 1)
    } else if n == l {
        Some(join(&None, &x.key, &x.value, &x.right))
    } else {
        Some(join(&do_drop(&x.left, n), &x.key, &x.value, &x.right))
    }
}

fn do_take<K: Clone, V: Clone>(t: &Tree<K, V>, n: usize) -> Tree<K, V> {
    let x = t.as_ref().filter(|_| n > 0)?;
    if n >= x.count {
        return t.clone();
    }
    let l = count(&x.left);
    if n <= l {
        do_take(&x.left, n)
    } else if n == l + 1 {
        Some(maybe_blacken(upd_nth(&x.left, n, &x.key, &x.value)))
    } else {
        Some(join(
            &x.left,
            &x.key,
            &x.value,
            &do_take(&x.right, n - l - 1),
        ))
    }
}

fn do_slice<K: Clone, V: Clone>(t: &Tree<K, V>, from: usize, until: usize) -> Tree<K, V> {
    let x = t
        .as_ref()
        .filter(|x| from < until && from < x.count && until > 0)?;
    if from == 0 && until >= x.count {
        return t.clone();
    }
    let l = count(&x.left);
    if until <= l {
        do_slice(&x.left, from, until)
    } else if from > l {
        do_slice(&x.right, from - l - 1, until - l - 1)
    } else {
        Some(join(
            &do_drop(&x.left, from),
            &x.key,
            &x.value,
            &do_take(&x.right, until - l - 1),
        ))
    }
}

// ─────────────────────────── bulk builds ───────────────────────────

/// `fromOrderedKeys` / `fromOrderedEntries`: a perfectly balanced tree of
/// entries already in order, black but for the deepest level.
pub fn from_ordered<K: Clone, V: Clone>(xs: Vec<(K, V)>) -> Tree<K, V> {
    let size = xs.len();
    let max_used_depth = usize::BITS - size.leading_zeros();
    fn f<K: Clone, V: Clone>(
        it: &mut std::vec::IntoIter<(K, V)>,
        level: u32,
        size: usize,
        max: u32,
    ) -> Tree<K, V> {
        match size {
            0 => None,
            1 => {
                let (k, v) = it.next().expect("sized");
                Some(node(level != max || level == 1, k, v, None, None))
            }
            _ => {
                let left_size = (size - 1) / 2;
                let left = f(it, level + 1, left_size, max);
                let (k, v) = it.next().expect("sized");
                let right = f(it, level + 1, size - 1 - left_size, max);
                Some(black_tree(k, v, left, right))
            }
        }
    }
    f(&mut xs.into_iter(), 1, size, max_used_depth)
}

/// `RB.transform`: every value replaced by `f(key, value)`, the shape kept.
pub fn transform<K: Clone, V: Clone>(
    t: &Tree<K, V>,
    f: &mut dyn FnMut(&K, &V) -> R<V>,
    same_value: &dyn Fn(&V, &V) -> bool,
) -> R<Tree<K, V>> {
    let Some(n) = t else { return Ok(None) };
    let l2 = transform(&n.left, f, same_value)?;
    let v2 = f(&n.key, &n.value)?;
    let r2 = transform(&n.right, f, same_value)?;
    if same_value(&v2, &n.value) && same(&l2, &n.left) && same(&r2, &n.right) {
        return Ok(t.clone());
    }
    Ok(Some(node(n.black, n.key.clone(), v2, l2, r2)))
}

/// `RB.filterEntries`: the entries `p` keeps, in order — `p` sees every
/// entry once, in key order.
pub fn filter<K: Clone, V: Clone>(
    t: &Tree<K, V>,
    p: &mut dyn FnMut(&K, &V) -> R<bool>,
) -> R<Tree<K, V>> {
    fn fk<K: Clone, V: Clone>(
        n: &Rc<Node<K, V>>,
        p: &mut dyn FnMut(&K, &V) -> R<bool>,
    ) -> R<Tree<K, V>> {
        let l2 = match &n.left {
            Some(l) => fk(l, p)?,
            None => None,
        };
        let keep = p(&n.key, &n.value)?;
        let r2 = match &n.right {
            Some(r) => fk(r, p)?,
            None => None,
        };
        Ok(if !keep {
            join2(&l2, &r2)
        } else if same(&l2, &n.left) && same(&r2, &n.right) {
            Some(n.clone())
        } else {
            Some(join(&l2, &n.key, &n.value, &r2))
        })
    }
    match t {
        None => Ok(None),
        Some(n) => Ok(blacken(fk(n, p)?)),
    }
}

/// `RB.partitionEntries`: the entries `p` keeps and the ones it rejects.
pub fn partition<K: Clone, V: Clone>(
    t: &Tree<K, V>,
    p: &mut dyn FnMut(&K, &V) -> R<bool>,
) -> R<(Tree<K, V>, Tree<K, V>)> {
    fn fk<K: Clone, V: Clone>(
        n: &Rc<Node<K, V>>,
        p: &mut dyn FnMut(&K, &V) -> R<bool>,
    ) -> R<(Tree<K, V>, Tree<K, V>)> {
        let (l2k, l2d) = match &n.left {
            Some(l) => fk(l, p)?,
            None => (None, None),
        };
        let keep = p(&n.key, &n.value)?;
        let (r2k, r2d) = match &n.right {
            Some(r) => fk(r, p)?,
            None => (None, None),
        };
        let side = |kept: bool, l2: &Tree<K, V>, r2: &Tree<K, V>| {
            if !kept {
                join2(l2, r2)
            } else if same(l2, &n.left) && same(r2, &n.right) {
                Some(n.clone())
            } else {
                Some(join(l2, &n.key, &n.value, r2))
            }
        };
        Ok((side(keep, &l2k, &r2k), side(!keep, &l2d, &r2d)))
    }
    match t {
        None => Ok((None, None)),
        Some(n) => {
            let (k, d) = fk(n, p)?;
            Ok((blacken(k), blacken(d)))
        }
    }
}

// ─────────────── join-based bulk operations (BFS16) ───────────────

fn rank<K, V>(t: &Tree<K, V>, bh: i64) -> i64 {
    match t {
        None => 0,
        Some(n) if n.black => 2 * (bh - 1),
        Some(_) => 2 * bh - 1,
    }
}

fn join_right<K: Clone, V: Clone>(
    tl: &Tree<K, V>,
    k: &K,
    v: &V,
    tr: &Tree<K, V>,
    bhtl: i64,
    rtr: i64,
) -> Rc<Node<K, V>> {
    let rtl = rank(tl, bhtl);
    if rtl == (rtr / 2) * 2 {
        return red_tree(k.clone(), v.clone(), tl.clone(), tr.clone());
    }
    let l = tl.as_ref().expect("a higher-ranked tree is not empty");
    let bhtlr = if l.black { bhtl - 1 } else { bhtl };
    let ttr = join_right(&l.right, k, v, tr, bhtlr, rtr);
    if l.black && !ttr.black && is_red(&ttr.right) {
        let a = black_tree(
            l.key.clone(),
            l.value.clone(),
            l.left.clone(),
            ttr.left.clone(),
        );
        let b = black(ttr.right.as_ref().expect("red"));
        red_tree(ttr.key.clone(), ttr.value.clone(), Some(a), Some(b))
    } else {
        node(
            l.black,
            l.key.clone(),
            l.value.clone(),
            l.left.clone(),
            Some(ttr),
        )
    }
}

fn join_left<K: Clone, V: Clone>(
    tl: &Tree<K, V>,
    k: &K,
    v: &V,
    tr: &Tree<K, V>,
    rtl: i64,
    bhtr: i64,
) -> Rc<Node<K, V>> {
    let rtr = rank(tr, bhtr);
    if rtr == (rtl / 2) * 2 {
        return red_tree(k.clone(), v.clone(), tl.clone(), tr.clone());
    }
    let r = tr.as_ref().expect("a higher-ranked tree is not empty");
    let bhtrl = if r.black { bhtr - 1 } else { bhtr };
    let ttl = join_left(tl, k, v, &r.left, rtl, bhtrl);
    if r.black && !ttl.black && is_red(&ttl.left) {
        let a = black(ttl.left.as_ref().expect("red"));
        let b = black_tree(
            r.key.clone(),
            r.value.clone(),
            ttl.right.clone(),
            r.right.clone(),
        );
        red_tree(ttl.key.clone(), ttl.value.clone(), Some(a), Some(b))
    } else {
        node(
            r.black,
            r.key.clone(),
            r.value.clone(),
            Some(ttl),
            r.right.clone(),
        )
    }
}

fn join<K: Clone, V: Clone>(tl: &Tree<K, V>, k: &K, v: &V, tr: &Tree<K, V>) -> Rc<Node<K, V>> {
    fn h<K, V>(t: &Tree<K, V>) -> i64 {
        let (mut t, mut i) = (t, 0);
        while let Some(n) = t {
            if n.black {
                i += 1;
            }
            t = &n.left;
        }
        i + 1
    }
    let (bhtl, bhtr) = (h(tl), h(tr));
    if bhtl > bhtr {
        let tt = join_right(tl, k, v, tr, bhtl, rank(tr, bhtr));
        if !tt.black && is_red(&tt.right) {
            black(&tt)
        } else {
            tt
        }
    } else if bhtr > bhtl {
        let tt = join_left(tl, k, v, tr, rank(tl, bhtl), bhtr);
        if !tt.black && is_red(&tt.left) {
            black(&tt)
        } else {
            tt
        }
    } else {
        node(
            is_red(tl) || is_red(tr),
            k.clone(),
            v.clone(),
            tl.clone(),
            tr.clone(),
        )
    }
}

/// `split(t, k)`: the entries below `k`, the node equal to it, the entries
/// above it, and the key the split found (`k` itself when none).
type Split<K, V> = (Tree<K, V>, Tree<K, V>, Tree<K, V>, K);

fn split<K: Clone, V: Clone>(cx: &mut dyn Cx<K, V>, t: &Tree<K, V>, k2: &K) -> R<Split<K, V>> {
    let Some(n) = t else {
        return Ok((None, None, None, k2.clone()));
    };
    Ok(match cx.compare(k2, &n.key)? {
        Ordering::Equal => (n.left.clone(), t.clone(), n.right.clone(), n.key.clone()),
        Ordering::Less => {
            let (ll, b, lr, k1) = split(cx, &n.left, k2)?;
            (ll, b, Some(join(&lr, &n.key, &n.value, &n.right)), k1)
        }
        Ordering::Greater => {
            let (rl, b, rr, k1) = split(cx, &n.right, k2)?;
            (Some(join(&n.left, &n.key, &n.value, &rl)), b, rr, k1)
        }
    })
}

fn split_last<K: Clone, V: Clone>(n: &Rc<Node<K, V>>) -> (Tree<K, V>, K, V) {
    match &n.right {
        None => (n.left.clone(), n.key.clone(), n.value.clone()),
        Some(r) => {
            let (tt, kk, vv) = split_last(r);
            (Some(join(&n.left, &n.key, &n.value, &tt)), kk, vv)
        }
    }
}

fn join2<K: Clone, V: Clone>(tl: &Tree<K, V>, tr: &Tree<K, V>) -> Tree<K, V> {
    match (tl, tr) {
        (None, _) => tr.clone(),
        (_, None) => tl.clone(),
        (Some(l), _) => {
            let (ttl, k, v) = split_last(l);
            Some(join(&ttl, &k, &v, tr))
        }
    }
}

/// `RB.union`: `t2`'s entries win (its value, `t1`'s key).
pub fn union<K: Clone, V: Clone>(
    cx: &mut dyn Cx<K, V>,
    t1: &Tree<K, V>,
    t2: &Tree<K, V>,
) -> R<Tree<K, V>> {
    fn go<K: Clone, V: Clone>(
        cx: &mut dyn Cx<K, V>,
        t1: &Tree<K, V>,
        t2: &Tree<K, V>,
    ) -> R<Tree<K, V>> {
        if t1.is_none() || same(t1, t2) {
            return Ok(t2.clone());
        }
        let Some(n2) = t2 else { return Ok(t1.clone()) };
        let (l1, _, r1, k1) = split(cx, t1, &n2.key)?;
        let tl = go(cx, &l1, &n2.left)?;
        let tr = go(cx, &r1, &n2.right)?;
        Ok(Some(join(&tl, &k1, &n2.value, &tr)))
    }
    Ok(blacken(go(cx, t1, t2)?))
}

/// `RB.intersect`.
pub fn intersect<K: Clone, V: Clone>(
    cx: &mut dyn Cx<K, V>,
    t1: &Tree<K, V>,
    t2: &Tree<K, V>,
) -> R<Tree<K, V>> {
    fn go<K: Clone, V: Clone>(
        cx: &mut dyn Cx<K, V>,
        t1: &Tree<K, V>,
        t2: &Tree<K, V>,
    ) -> R<Tree<K, V>> {
        let Some(n2) = t2.as_ref().filter(|_| t1.is_some()) else {
            return Ok(None);
        };
        if same(t1, t2) {
            return Ok(t1.clone());
        }
        let (l1, b, r1, k1) = split(cx, t1, &n2.key)?;
        let tl = go(cx, &l1, &n2.left)?;
        let tr = go(cx, &r1, &n2.right)?;
        Ok(if b.is_some() {
            Some(join(&tl, &k1, &n2.value, &tr))
        } else {
            join2(&tl, &tr)
        })
    }
    Ok(blacken(go(cx, t1, t2)?))
}

/// `RB.difference`.
pub fn difference<K: Clone, V: Clone>(
    cx: &mut dyn Cx<K, V>,
    t1: &Tree<K, V>,
    t2: &Tree<K, V>,
) -> R<Tree<K, V>> {
    fn go<K: Clone, V: Clone>(
        cx: &mut dyn Cx<K, V>,
        t1: &Tree<K, V>,
        t2: &Tree<K, V>,
    ) -> R<Tree<K, V>> {
        let Some(n2) = t2.as_ref().filter(|_| t1.is_some()) else {
            return Ok(t1.clone());
        };
        if same(t1, t2) {
            return Ok(None);
        }
        let (l1, _, r1, _) = split(cx, t1, &n2.key)?;
        let tl = go(cx, &l1, &n2.left)?;
        let tr = go(cx, &r1, &n2.right)?;
        Ok(join2(&tl, &tr))
    }
    Ok(blacken(go(cx, t1, t2)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Natural `i64` order, counting comparisons.
    struct Nat(usize);
    impl Cx<i64, ()> for Nat {
        fn compare(&mut self, a: &i64, b: &i64) -> R<Ordering> {
            self.0 += 1;
            Ok(a.cmp(b))
        }
        fn lt(&mut self, a: &i64, b: &i64) -> R<bool> {
            self.0 += 1;
            Ok(a < b)
        }
        fn lteq(&mut self, a: &i64, b: &i64) -> R<bool> {
            self.0 += 1;
            Ok(a <= b)
        }
        fn equiv(&mut self, a: &i64, b: &i64) -> R<bool> {
            self.0 += 1;
            Ok(a == b)
        }
        fn same_key(&self, _: &i64, _: &i64) -> bool {
            false
        }
        fn same_value(&self, _: &(), _: &()) -> bool {
            true
        }
    }

    /// The red-black invariants: no red node has a red child, every path has
    /// the same number of black nodes, keys ascend, counts are sizes. Answers
    /// the black height.
    fn valid(t: &Tree<i64, ()>, lo: Option<i64>, hi: Option<i64>) -> usize {
        let Some(n) = t else { return 1 };
        assert!(lo.is_none_or(|l| n.key > l) && hi.is_none_or(|h| n.key < h));
        if !n.black {
            assert!(
                !is_red(&n.left) && !is_red(&n.right),
                "red-red at {}",
                n.key
            );
        }
        assert_eq!(n.count, count(&n.left) + count(&n.right) + 1);
        let bl = valid(&n.left, lo, Some(n.key));
        assert_eq!(
            bl,
            valid(&n.right, Some(n.key), hi),
            "unbalanced at {}",
            n.key
        );
        bl + usize::from(n.black)
    }

    fn keys(t: &Tree<i64, ()>) -> Vec<i64> {
        entries(t).into_iter().map(|(k, _)| k).collect()
    }

    fn build(cx: &mut Nat, xs: impl IntoIterator<Item = i64>) -> Tree<i64, ()> {
        let mut t = None;
        for x in xs {
            t = update(cx, &t, &x, &(), false).unwrap();
            valid(&t, None, None);
        }
        t
    }

    #[test]
    fn every_operation_keeps_the_invariants_and_the_contents() {
        let mut cx = Nat(0);
        let xs: Vec<i64> = (0..300).map(|i| (i * 7919) % 307).collect();
        let t = build(&mut cx, xs.iter().copied());
        let mut want: Vec<i64> = xs.clone();
        want.sort();
        want.dedup();
        assert_eq!(keys(&t), want);

        let mut d = t.clone();
        for x in (0..307).step_by(3) {
            d = delete(&mut cx, &d, &x).unwrap();
            valid(&d, None, None);
        }
        assert_eq!(
            keys(&d),
            want.iter()
                .copied()
                .filter(|x| x % 3 != 0)
                .collect::<Vec<_>>()
        );

        let u = build(&mut cx, (250..400).step_by(2));
        for (op, f) in [
            (
                "union",
                union as fn(&mut dyn Cx<i64, ()>, &Tree<i64, ()>, &Tree<i64, ()>) -> _,
            ),
            ("intersect", intersect),
            ("difference", difference),
        ] {
            let r = f(&mut cx, &t, &u).unwrap();
            valid(&r, None, None);
            let (a, b) = (keys(&t), keys(&u));
            let want: Vec<i64> = match op {
                "union" => {
                    let mut v: Vec<i64> = a.iter().chain(&b).copied().collect();
                    v.sort();
                    v.dedup();
                    v
                }
                "intersect" => a.iter().copied().filter(|x| b.contains(x)).collect(),
                _ => a.iter().copied().filter(|x| !b.contains(x)).collect(),
            };
            assert_eq!(keys(&r), want, "{op}");
        }

        for n in [0, 1, 5, 150, 306, 307, 400] {
            let (tk, dr) = (take(&t, n), drop(&t, n));
            valid(&tk, None, None);
            valid(&dr, None, None);
            assert_eq!(keys(&tk), want.iter().copied().take(n).collect::<Vec<_>>());
            assert_eq!(keys(&dr), want.iter().copied().skip(n).collect::<Vec<_>>());
            let s = slice(&t, n / 3, n);
            valid(&s, None, None);
            assert_eq!(
                keys(&s),
                want.iter()
                    .copied()
                    .skip(n / 3)
                    .take(n - n / 3)
                    .collect::<Vec<_>>()
            );
        }

        let sub = |p: fn(i64) -> bool| want.iter().copied().filter(|&k| p(k)).collect::<Vec<_>>();
        let r = range(&mut cx, &t, &Bound::Range(40, 90)).unwrap();
        valid(&r, None, None);
        assert_eq!(keys(&r), sub(|k| (40..90).contains(&k)));
        let r = range(&mut cx, &t, &Bound::To(90)).unwrap();
        assert_eq!(keys(&r), sub(|k| k <= 90));

        let (ev, od) = partition(&t, &mut |k, _| Ok(k % 2 == 0)).unwrap();
        valid(&ev, None, None);
        valid(&od, None, None);
        assert_eq!(count(&ev) + count(&od), count(&t));
        let f = filter(&t, &mut |k, _| Ok(k % 5 == 0)).unwrap();
        valid(&f, None, None);
        assert_eq!(keys(&f), sub(|k| k % 5 == 0));

        let (tl, it) = (tail(&t), init(&t));
        valid(&tl, None, None);
        valid(&it, None, None);
        assert_eq!(keys(&tl)[0], 1);
        assert_eq!(*keys(&it).last().unwrap(), 305);

        let ob = from_ordered(want.iter().map(|&k| (k, ())).collect());
        valid(&ob, None, None);
        assert!(equal(&mut cx, &ob, &t, None).unwrap());
        assert_eq!(
            entries_from(&mut cx, &t, &300).unwrap().len(),
            want.iter().filter(|&&k| k >= 300).count()
        );
    }

    #[test]
    fn an_unchanged_result_is_the_receiver_itself() {
        let mut cx = Nat(0);
        let t = build(&mut cx, [5, 3, 8, 1, 4]);
        assert!(same(&update(&mut cx, &t, &4, &(), false).unwrap(), &t));
        assert!(same(&delete(&mut cx, &t, &6).unwrap(), &t));
        assert!(same(&filter(&t, &mut |_, _| Ok(true)).unwrap(), &t));
        assert!(same(&range(&mut cx, &t, &Bound::Range(0, 9)).unwrap(), &t));
        assert!(same(&union(&mut cx, &t, &t).unwrap(), &t));
    }

    #[test]
    fn a_lookup_compares_once_per_level_it_descends() {
        let mut cx = Nat(0);
        let t = build(&mut cx, 1..=7);
        cx.0 = 0;
        let found = lookup(&mut cx, &t, &7).unwrap().map(|n| n.key);
        assert_eq!(found, Some(7));
        let mut depth = 0;
        let mut n = t.clone();
        while let Some(x) = n {
            depth += 1;
            if x.key == 7 {
                break;
            }
            n = if 7 < x.key {
                x.left.clone()
            } else {
                x.right.clone()
            };
        }
        assert_eq!(cx.0, depth);
    }
}
