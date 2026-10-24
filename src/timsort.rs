//! The two stable sorts a Scala program's comparisons run through, ported so
//! the comparator is asked exactly what the JVM asks it — in the same order,
//! the same number of times. An `Ordering.by(f)` runs `f` once per operand of
//! every comparison and a `sortWith` predicate is a user closure, so both the
//! count and the sequence are observable.
//!
//! * [`tim_sort`] is `java.util.TimSort` (JDK 21), what
//!   `java.util.Arrays.sort(Object[], Comparator)` runs and therefore what
//!   `Seq.sorted`/`sortBy`/`sortWith` run: a binary insertion sort under 32
//!   elements, natural runs merged with galloping above. An inconsistent
//!   comparator can make a merge find an impossible state, which the JDK
//!   reports as `IllegalArgumentException: Comparison method violates its
//!   general contract!`.
//! * [`stable_merge_sort`] is `scala.util.Sorting.mergeSort`, which
//!   `ArrayOps.sorted` uses for an array of primitives under a non-default
//!   ordering and fewer than 300 elements.
//!
//! Both sort a permutation: the slice holds element indices and the comparator
//! is handed two of them.

use std::cmp::Ordering;

type R<T> = Result<T, String>;

/// A comparator over element indices that may run user code.
pub type Cmp<'a> = &'a mut dyn FnMut(usize, usize) -> R<Ordering>;

const MIN_MERGE: usize = 32;
const MIN_GALLOP: usize = 7;

fn contract_violation() -> String {
    "scalars: java.lang.IllegalArgumentException: Comparison method violates its general contract!"
        .to_string()
}

/// `TimSort.sort(a, 0, a.length, c, null, 0, 0)`.
pub fn tim_sort(a: &mut [usize], c: Cmp) -> R<()> {
    let hi = a.len();
    let mut lo = 0;
    let mut n_remaining = hi;
    if n_remaining < 2 {
        return Ok(());
    }
    if n_remaining < MIN_MERGE {
        let init = count_run_and_make_ascending(a, lo, hi, c)?;
        return binary_sort(a, lo, hi, lo + init, c);
    }
    let mut ts = TimSort {
        tmp: Vec::new(),
        min_gallop: MIN_GALLOP,
        run_base: Vec::new(),
        run_len: Vec::new(),
    };
    let min_run = min_run_length(n_remaining);
    loop {
        let mut run_len = count_run_and_make_ascending(a, lo, hi, c)?;
        if run_len < min_run {
            let force = n_remaining.min(min_run);
            binary_sort(a, lo, lo + force, lo + run_len, c)?;
            run_len = force;
        }
        ts.run_base.push(lo);
        ts.run_len.push(run_len);
        ts.merge_collapse(a, c)?;
        lo += run_len;
        n_remaining -= run_len;
        if n_remaining == 0 {
            break;
        }
    }
    ts.merge_force_collapse(a, c)
}

/// `binarySort`: `a[lo..start)` is sorted; insert the rest one by one, each
/// at the place a binary search finds (after any equal element, so stably).
fn binary_sort(a: &mut [usize], lo: usize, hi: usize, start: usize, c: Cmp) -> R<()> {
    let mut start = if start == lo { start + 1 } else { start };
    while start < hi {
        let pivot = a[start];
        let (mut left, mut right) = (lo, start);
        while left < right {
            let mid = (left + right) >> 1;
            if c(pivot, a[mid])? == Ordering::Less {
                right = mid;
            } else {
                left = mid + 1;
            }
        }
        a.copy_within(left..start, left + 1);
        a[left] = pivot;
        start += 1;
    }
    Ok(())
}

/// `countRunAndMakeAscending`: the length of the run at `lo`, a strictly
/// descending one reversed in place.
fn count_run_and_make_ascending(a: &mut [usize], lo: usize, hi: usize, c: Cmp) -> R<usize> {
    let mut run_hi = lo + 1;
    if run_hi == hi {
        return Ok(1);
    }
    let descending = c(a[run_hi], a[lo])? == Ordering::Less;
    run_hi += 1;
    if descending {
        while run_hi < hi && c(a[run_hi], a[run_hi - 1])? == Ordering::Less {
            run_hi += 1;
        }
        a[lo..run_hi].reverse();
    } else {
        while run_hi < hi && c(a[run_hi], a[run_hi - 1])? != Ordering::Less {
            run_hi += 1;
        }
    }
    Ok(run_hi - lo)
}

/// `minRunLength`.
fn min_run_length(mut n: usize) -> usize {
    let mut r = 0;
    while n >= MIN_MERGE {
        r |= n & 1;
        n >>= 1;
    }
    n + r
}

/// `gallopLeft`: where `key` goes in the sorted `a[base..base + len)` —
/// before any element equal to it — searching outward from `hint`.
fn gallop_left(key: usize, a: &[usize], base: usize, len: usize, hint: usize, c: Cmp) -> R<usize> {
    let (mut last_ofs, mut ofs): (isize, isize);
    let hint_i = hint as isize;
    let len_i = len as isize;
    if c(key, a[base + hint])? == Ordering::Greater {
        let max_ofs = len_i - hint_i;
        last_ofs = 0;
        ofs = 1;
        while ofs < max_ofs && c(key, a[base + (hint_i + ofs) as usize])? == Ordering::Greater {
            last_ofs = ofs;
            ofs = (ofs << 1) + 1;
        }
        ofs = ofs.min(max_ofs);
        last_ofs += hint_i;
        ofs += hint_i;
    } else {
        let max_ofs = hint_i + 1;
        last_ofs = 0;
        ofs = 1;
        while ofs < max_ofs && c(key, a[base + (hint_i - ofs) as usize])? != Ordering::Greater {
            last_ofs = ofs;
            ofs = (ofs << 1) + 1;
        }
        ofs = ofs.min(max_ofs);
        let tmp = last_ofs;
        last_ofs = hint_i - ofs;
        ofs = hint_i - tmp;
    }
    last_ofs += 1;
    while last_ofs < ofs {
        let m = last_ofs + ((ofs - last_ofs) >> 1);
        if c(key, a[base + m as usize])? == Ordering::Greater {
            last_ofs = m + 1;
        } else {
            ofs = m;
        }
    }
    Ok(ofs as usize)
}

/// `gallopRight`: as [`gallop_left`], but after any element equal to `key`.
fn gallop_right(key: usize, a: &[usize], base: usize, len: usize, hint: usize, c: Cmp) -> R<usize> {
    let (mut last_ofs, mut ofs): (isize, isize) = (0, 1);
    let hint_i = hint as isize;
    let len_i = len as isize;
    if c(key, a[base + hint])? == Ordering::Less {
        let max_ofs = hint_i + 1;
        while ofs < max_ofs && c(key, a[base + (hint_i - ofs) as usize])? == Ordering::Less {
            last_ofs = ofs;
            ofs = (ofs << 1) + 1;
        }
        ofs = ofs.min(max_ofs);
        let tmp = last_ofs;
        last_ofs = hint_i - ofs;
        ofs = hint_i - tmp;
    } else {
        let max_ofs = len_i - hint_i;
        while ofs < max_ofs && c(key, a[base + (hint_i + ofs) as usize])? != Ordering::Less {
            last_ofs = ofs;
            ofs = (ofs << 1) + 1;
        }
        ofs = ofs.min(max_ofs);
        last_ofs += hint_i;
        ofs += hint_i;
    }
    last_ofs += 1;
    while last_ofs < ofs {
        let m = last_ofs + ((ofs - last_ofs) >> 1);
        if c(key, a[base + m as usize])? == Ordering::Less {
            ofs = m;
        } else {
            last_ofs = m + 1;
        }
    }
    Ok(ofs as usize)
}

/// A TimSort in progress: the pending runs and the merge state.
struct TimSort {
    tmp: Vec<usize>,
    min_gallop: usize,
    run_base: Vec<usize>,
    run_len: Vec<usize>,
}

impl TimSort {
    /// `mergeCollapse`: merge until the run lengths satisfy the invariants.
    fn merge_collapse(&mut self, a: &mut [usize], c: Cmp) -> R<()> {
        while self.run_len.len() > 1 {
            let rl = &self.run_len;
            let mut n = rl.len() - 2;
            if (n > 0 && rl[n - 1] <= rl[n] + rl[n + 1])
                || (n > 1 && rl[n - 2] <= rl[n] + rl[n - 1])
            {
                if rl[n - 1] < rl[n + 1] {
                    n -= 1;
                }
            } else if rl[n] > rl[n + 1] {
                break;
            }
            self.merge_at(a, n, c)?;
        }
        Ok(())
    }

    /// `mergeForceCollapse`: merge every pending run.
    fn merge_force_collapse(&mut self, a: &mut [usize], c: Cmp) -> R<()> {
        while self.run_len.len() > 1 {
            let mut n = self.run_len.len() - 2;
            if n > 0 && self.run_len[n - 1] < self.run_len[n + 1] {
                n -= 1;
            }
            self.merge_at(a, n, c)?;
        }
        Ok(())
    }

    /// `mergeAt(i)`: merge runs `i` and `i + 1`.
    fn merge_at(&mut self, a: &mut [usize], i: usize, c: Cmp) -> R<()> {
        let (mut base1, mut len1) = (self.run_base[i], self.run_len[i]);
        let (base2, mut len2) = (self.run_base[i + 1], self.run_len[i + 1]);
        self.run_len[i] = len1 + len2;
        self.run_base.remove(i + 1);
        self.run_len.remove(i + 1);
        let k = gallop_right(a[base2], a, base1, len1, 0, c)?;
        base1 += k;
        len1 -= k;
        if len1 == 0 {
            return Ok(());
        }
        len2 = gallop_left(a[base1 + len1 - 1], a, base2, len2, len2 - 1, c)?;
        if len2 == 0 {
            return Ok(());
        }
        if len1 <= len2 {
            self.merge_lo(a, base1, len1, base2, len2, c)
        } else {
            self.merge_hi(a, base1, len1, base2, len2, c)
        }
    }

    /// `mergeLo`: merge two adjacent runs, the first (shorter) copied out.
    fn merge_lo(
        &mut self,
        a: &mut [usize],
        base1: usize,
        mut len1: usize,
        base2: usize,
        mut len2: usize,
        c: Cmp,
    ) -> R<()> {
        self.tmp.clear();
        self.tmp.extend_from_slice(&a[base1..base1 + len1]);
        let tmp = std::mem::take(&mut self.tmp);
        let (mut cursor1, mut cursor2, mut dest) = (0usize, base2, base1);
        a[dest] = a[cursor2];
        dest += 1;
        cursor2 += 1;
        len2 -= 1;
        if len2 == 0 {
            a[dest..dest + len1].copy_from_slice(&tmp[cursor1..cursor1 + len1]);
            self.tmp = tmp;
            return Ok(());
        }
        if len1 == 1 {
            a.copy_within(cursor2..cursor2 + len2, dest);
            a[dest + len2] = tmp[cursor1];
            self.tmp = tmp;
            return Ok(());
        }
        let mut min_gallop = self.min_gallop as isize;
        'outer: loop {
            let (mut count1, mut count2) = (0usize, 0usize);
            loop {
                if c(a[cursor2], tmp[cursor1])? == Ordering::Less {
                    a[dest] = a[cursor2];
                    dest += 1;
                    cursor2 += 1;
                    count2 += 1;
                    count1 = 0;
                    len2 -= 1;
                    if len2 == 0 {
                        break 'outer;
                    }
                } else {
                    a[dest] = tmp[cursor1];
                    dest += 1;
                    cursor1 += 1;
                    count1 += 1;
                    count2 = 0;
                    len1 -= 1;
                    if len1 == 1 {
                        break 'outer;
                    }
                }
                if ((count1 | count2) as isize) >= min_gallop {
                    break;
                }
            }
            loop {
                count1 = gallop_right(a[cursor2], &tmp, cursor1, len1, 0, c)?;
                if count1 != 0 {
                    a[dest..dest + count1].copy_from_slice(&tmp[cursor1..cursor1 + count1]);
                    dest += count1;
                    cursor1 += count1;
                    len1 -= count1;
                    if len1 <= 1 {
                        break 'outer;
                    }
                }
                a[dest] = a[cursor2];
                dest += 1;
                cursor2 += 1;
                len2 -= 1;
                if len2 == 0 {
                    break 'outer;
                }
                count2 = gallop_left(tmp[cursor1], a, cursor2, len2, 0, c)?;
                if count2 != 0 {
                    a.copy_within(cursor2..cursor2 + count2, dest);
                    dest += count2;
                    cursor2 += count2;
                    len2 -= count2;
                    if len2 == 0 {
                        break 'outer;
                    }
                }
                a[dest] = tmp[cursor1];
                dest += 1;
                cursor1 += 1;
                len1 -= 1;
                if len1 == 1 {
                    break 'outer;
                }
                min_gallop -= 1;
                if !(count1 >= MIN_GALLOP || count2 >= MIN_GALLOP) {
                    break;
                }
            }
            if min_gallop < 0 {
                min_gallop = 0;
            }
            min_gallop += 2;
        }
        self.min_gallop = min_gallop.max(1) as usize;
        let result = if len1 == 1 {
            a.copy_within(cursor2..cursor2 + len2, dest);
            a[dest + len2] = tmp[cursor1];
            Ok(())
        } else if len1 == 0 {
            Err(contract_violation())
        } else {
            a[dest..dest + len1].copy_from_slice(&tmp[cursor1..cursor1 + len1]);
            Ok(())
        };
        self.tmp = tmp;
        result
    }

    /// `mergeHi`: merge two adjacent runs from the right, the second
    /// (shorter) copied out.
    fn merge_hi(
        &mut self,
        a: &mut [usize],
        base1: usize,
        mut len1: usize,
        base2: usize,
        mut len2: usize,
        c: Cmp,
    ) -> R<()> {
        self.tmp.clear();
        self.tmp.extend_from_slice(&a[base2..base2 + len2]);
        let tmp = std::mem::take(&mut self.tmp);
        // Signed cursors: each may step one below its run.
        let mut cursor1 = (base1 + len1) as isize - 1;
        let mut cursor2 = len2 as isize - 1;
        let mut dest = (base2 + len2) as isize - 1;
        macro_rules! at {
            ($i:expr) => {
                $i as usize
            };
        }
        a[at!(dest)] = a[at!(cursor1)];
        dest -= 1;
        cursor1 -= 1;
        len1 -= 1;
        if len1 == 0 {
            let start = at!(dest - (len2 as isize - 1));
            a[start..start + len2].copy_from_slice(&tmp[..len2]);
            self.tmp = tmp;
            return Ok(());
        }
        if len2 == 1 {
            dest -= len1 as isize;
            cursor1 -= len1 as isize;
            a.copy_within(at!(cursor1 + 1)..at!(cursor1 + 1) + len1, at!(dest + 1));
            a[at!(dest)] = tmp[at!(cursor2)];
            self.tmp = tmp;
            return Ok(());
        }
        let mut min_gallop = self.min_gallop as isize;
        'outer: loop {
            let (mut count1, mut count2) = (0usize, 0usize);
            loop {
                if c(tmp[at!(cursor2)], a[at!(cursor1)])? == Ordering::Less {
                    a[at!(dest)] = a[at!(cursor1)];
                    dest -= 1;
                    cursor1 -= 1;
                    count1 += 1;
                    count2 = 0;
                    len1 -= 1;
                    if len1 == 0 {
                        break 'outer;
                    }
                } else {
                    a[at!(dest)] = tmp[at!(cursor2)];
                    dest -= 1;
                    cursor2 -= 1;
                    count2 += 1;
                    count1 = 0;
                    len2 -= 1;
                    if len2 == 1 {
                        break 'outer;
                    }
                }
                if ((count1 | count2) as isize) >= min_gallop {
                    break;
                }
            }
            loop {
                count1 = len1 - gallop_right(tmp[at!(cursor2)], a, base1, len1, len1 - 1, c)?;
                if count1 != 0 {
                    dest -= count1 as isize;
                    cursor1 -= count1 as isize;
                    len1 -= count1;
                    a.copy_within(at!(cursor1 + 1)..at!(cursor1 + 1) + count1, at!(dest + 1));
                    if len1 == 0 {
                        break 'outer;
                    }
                }
                a[at!(dest)] = tmp[at!(cursor2)];
                dest -= 1;
                cursor2 -= 1;
                len2 -= 1;
                if len2 == 1 {
                    break 'outer;
                }
                count2 = len2 - gallop_left(a[at!(cursor1)], &tmp, 0, len2, len2 - 1, c)?;
                if count2 != 0 {
                    dest -= count2 as isize;
                    cursor2 -= count2 as isize;
                    len2 -= count2;
                    let from = at!(cursor2 + 1);
                    a[at!(dest + 1)..at!(dest + 1) + count2]
                        .copy_from_slice(&tmp[from..from + count2]);
                    if len2 <= 1 {
                        break 'outer;
                    }
                }
                a[at!(dest)] = a[at!(cursor1)];
                dest -= 1;
                cursor1 -= 1;
                len1 -= 1;
                if len1 == 0 {
                    break 'outer;
                }
                min_gallop -= 1;
                if !(count1 >= MIN_GALLOP || count2 >= MIN_GALLOP) {
                    break;
                }
            }
            if min_gallop < 0 {
                min_gallop = 0;
            }
            min_gallop += 2;
        }
        self.min_gallop = min_gallop.max(1) as usize;
        let result = if len2 == 1 {
            dest -= len1 as isize;
            cursor1 -= len1 as isize;
            a.copy_within(at!(cursor1 + 1)..at!(cursor1 + 1) + len1, at!(dest + 1));
            a[at!(dest)] = tmp[at!(cursor2)];
            Ok(())
        } else if len2 == 0 {
            Err(contract_violation())
        } else {
            let start = at!(dest - (len2 as isize - 1));
            a[start..start + len2].copy_from_slice(&tmp[..len2]);
            Ok(())
        };
        self.tmp = tmp;
        result
    }
}

/// `scala.util.Sorting.mergeSort` over the whole slice: a binary insertion
/// sort below 32 elements, otherwise both halves sorted and merged — skipped
/// when they are already in order.
pub fn stable_merge_sort(a: &mut [usize], c: Cmp) -> R<()> {
    let mut scratch = vec![0usize; a.len() / 2 + 1];
    merge_sort(a, 0, a.len(), c, &mut scratch)
}

fn merge_sort(a: &mut [usize], i0: usize, i_n: usize, c: Cmp, scratch: &mut [usize]) -> R<()> {
    if i_n - i0 < MIN_MERGE {
        return insertion_sort(a, i0, i_n, c);
    }
    let i_k = (i0 + i_n) >> 1;
    merge_sort(a, i0, i_k, c, scratch)?;
    merge_sort(a, i_k, i_n, c, scratch)?;
    merge_sorted(a, i0, i_k, i_n, c, scratch)
}

/// `Sorting.insertionSort`: the first two put in order, then each next
/// element checked against the last sorted one before a binary search.
fn insertion_sort(a: &mut [usize], i0: usize, i_n: usize, c: Cmp) -> R<()> {
    let n = i_n - i0;
    if n < 2 {
        return Ok(());
    }
    if c(a[i0], a[i0 + 1])? == Ordering::Greater {
        a.swap(i0, i0 + 1);
    }
    for m in 2..n {
        let next = a[i0 + m];
        if c(next, a[i0 + m - 1])? == Ordering::Less {
            let (mut ia, mut ib) = (i0, i0 + m - 1);
            while ib - ia > 1 {
                let ix = (ia + ib) >> 1;
                if c(next, a[ix])? == Ordering::Less {
                    ib = ix;
                } else {
                    ia = ix;
                }
            }
            let ix = ia + usize::from(c(next, a[ia])? != Ordering::Less);
            a.copy_within(ix..i0 + m, ix + 1);
            a[ix] = next;
        }
    }
    Ok(())
}

/// `Sorting.mergeSorted`.
fn merge_sorted(
    a: &mut [usize],
    i0: usize,
    i_k: usize,
    i_n: usize,
    c: Cmp,
    scratch: &mut [usize],
) -> R<()> {
    if c(a[i_k - 1], a[i_k])? != Ordering::Greater {
        return Ok(());
    }
    let jn = i_k - i0;
    scratch[..jn].copy_from_slice(&a[i0..i_k]);
    let (mut i, mut j, mut k) = (i_k, 0, i0);
    while i < i_n && j < jn {
        if c(a[i], scratch[j])? == Ordering::Less {
            a[k] = a[i];
            i += 1;
        } else {
            a[k] = scratch[j];
            j += 1;
        }
        k += 1;
    }
    while j < jn {
        a[k] = scratch[j];
        j += 1;
        k += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted_by(keys: &[i64], f: fn(&mut [usize], Cmp) -> R<()>) -> (Vec<usize>, usize) {
        let mut idx: Vec<usize> = (0..keys.len()).collect();
        let mut n = 0;
        f(&mut idx, &mut |i, j| {
            n += 1;
            Ok(keys[i].cmp(&keys[j]))
        })
        .unwrap();
        (idx, n)
    }

    #[test]
    fn both_sorts_order_stably_at_every_size() {
        for len in [0, 1, 2, 31, 32, 33, 64, 100, 257, 1000] {
            // Few distinct keys, so stability is exercised.
            let keys: Vec<i64> = (0..len).map(|i| (i * 7919 % 13) as i64).collect();
            for f in [
                tim_sort as fn(&mut [usize], Cmp) -> R<()>,
                stable_merge_sort,
            ] {
                let (idx, _) = sorted_by(&keys, f);
                let mut want: Vec<usize> = (0..keys.len()).collect();
                want.sort_by_key(|&i| keys[i]);
                assert_eq!(idx, want, "len {len}");
            }
        }
    }

    #[test]
    fn timsort_takes_one_pass_over_a_sorted_or_reversed_input() {
        let asc: Vec<i64> = (0..500).collect();
        assert_eq!(sorted_by(&asc, tim_sort).1, 499);
        let desc: Vec<i64> = (0..500).rev().collect();
        let (idx, n) = sorted_by(&desc, tim_sort);
        assert_eq!(n, 499);
        assert_eq!(idx[0], 499);
    }

    #[test]
    fn an_inconsistent_comparator_is_the_jdk_contract_error_or_a_permutation() {
        // A comparator that ignores its operands: TimSort either finishes
        // with some permutation or detects the impossible merge.
        let mut idx: Vec<usize> = (0..200).collect();
        let mut flip = false;
        let r = tim_sort(&mut idx, &mut |_, _| {
            flip = !flip;
            Ok(if flip {
                Ordering::Less
            } else {
                Ordering::Greater
            })
        });
        match r {
            Ok(()) => {
                let mut s = idx.clone();
                s.sort();
                assert_eq!(s, (0..200).collect::<Vec<_>>());
            }
            Err(e) => assert!(e.contains("violates its general contract")),
        }
    }
}
