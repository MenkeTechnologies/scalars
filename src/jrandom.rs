//! `java.util.Random` — the 48-bit linear congruential generator behind both
//! `java.util.Random` and `scala.util.Random` (which delegates to it). A seeded
//! generator reproduces the JVM's sequence exactly; the derived draws
//! (`nextInt(bound)`, `nextDouble`, `nextGaussian`) are ports of the JDK
//! algorithms, and `between`/`nextLong(n)`/`shuffle` are ports of
//! `scala.util.Random` in the 2.13 library, which Scala 3 runs on.

const MULTIPLIER: u64 = 0x5DEECE66D;
const ADDEND: u64 = 0xB;
const MASK: u64 = (1 << 48) - 1;

/// One generator: the 48-bit state plus the cached second Gaussian of the
/// polar method.
#[derive(Clone, Debug)]
pub struct JRandom {
    seed: u64,
    next_gaussian: Option<f64>,
}

impl JRandom {
    pub fn new(seed: i64) -> Self {
        JRandom {
            seed: (seed as u64 ^ MULTIPLIER) & MASK,
            next_gaussian: None,
        }
    }

    /// A generator seeded from the clock, as `new Random()` is.
    pub fn from_entropy() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        Self::new(nanos ^ i64::from(std::process::id()).rotate_left(32))
    }

    pub fn set_seed(&mut self, seed: i64) {
        *self = Self::new(seed);
    }

    /// `Random.next(bits)`: advance the state and return its top `bits` bits as
    /// a signed 32-bit value.
    fn next(&mut self, bits: u32) -> i32 {
        self.seed = (self.seed.wrapping_mul(MULTIPLIER).wrapping_add(ADDEND)) & MASK;
        (self.seed >> (48 - bits)) as i64 as i32
    }

    pub fn next_int(&mut self) -> i32 {
        self.next(32)
    }

    /// `nextInt(bound)`; `None` when `bound <= 0` (the caller raises
    /// `IllegalArgumentException`).
    pub fn next_int_bound(&mut self, bound: i32) -> Option<i32> {
        if bound <= 0 {
            return None;
        }
        let mut r = self.next(31);
        let m = bound - 1;
        if bound & m == 0 {
            r = ((i64::from(bound) * i64::from(r)) >> 31) as i32;
        } else {
            let mut u = r;
            loop {
                r = u % bound;
                if u.wrapping_sub(r).wrapping_add(m) >= 0 {
                    break;
                }
                u = self.next(31);
            }
        }
        Some(r)
    }

    pub fn next_long(&mut self) -> i64 {
        (i64::from(self.next(32)) << 32).wrapping_add(i64::from(self.next(32)))
    }

    /// `scala.util.Random.nextLong(n)`; `None` when `n <= 0`. The library halves
    /// `n` at random until it fits `nextInt`, so the draw is NOT the JDK's
    /// `Random.nextLong(bound)`.
    pub fn next_long_bound(&mut self, n: i64) -> Option<i64> {
        if n <= 0 {
            return None;
        }
        let mut offset = 0i64;
        let mut n = n;
        while n >= i64::from(i32::MAX) {
            let bits = self.next_int_bound(2)?;
            let halfn = ((n as u64) >> 1) as i64;
            let nextn = if bits & 2 == 0 { halfn } else { n - halfn };
            if bits & 1 == 0 {
                offset += n - nextn;
            }
            n = nextn;
        }
        Some(offset + i64::from(self.next_int_bound(n as i32)?))
    }

    pub fn next_double(&mut self) -> f64 {
        let hi = i64::from(self.next(26)) << 27;
        let lo = i64::from(self.next(27));
        (hi + lo) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    pub fn next_boolean(&mut self) -> bool {
        self.next(1) != 0
    }

    /// The polar method of `java.util.Random.nextGaussian`.
    pub fn next_gaussian(&mut self) -> f64 {
        if let Some(g) = self.next_gaussian.take() {
            return g;
        }
        loop {
            let v1 = 2.0 * self.next_double() - 1.0;
            let v2 = 2.0 * self.next_double() - 1.0;
            let s = v1 * v1 + v2 * v2;
            if s < 1.0 && s != 0.0 {
                let multiplier = (-2.0 * s.ln() / s).sqrt();
                self.next_gaussian = Some(v2 * multiplier);
                return v1 * multiplier;
            }
        }
    }

    /// `scala.util.Random.between(min, max)` for `Int`; `None` on empty bounds.
    pub fn between_int(&mut self, min: i32, max: i32) -> Option<i32> {
        if min >= max {
            return None;
        }
        let difference = max.wrapping_sub(min);
        if difference >= 0 {
            return self.next_int_bound(difference).map(|n| n + min);
        }
        loop {
            let r = self.next_int();
            if r >= min && r < max {
                return Some(r);
            }
        }
    }

    /// `scala.util.Random.between(min, max)` for `Long`; `None` on empty bounds.
    pub fn between_long(&mut self, min: i64, max: i64) -> Option<i64> {
        if min >= max {
            return None;
        }
        let difference = max.wrapping_sub(min);
        if difference >= 0 {
            return self.next_long_bound(difference).map(|n| n + min);
        }
        loop {
            let r = self.next_long();
            if r >= min && r < max {
                return Some(r);
            }
        }
    }

    /// `scala.util.Random.between(min, max)` for `Double`; `None` on empty bounds.
    pub fn between_double(&mut self, min: f64, max: f64) -> Option<f64> {
        if min.partial_cmp(&max) != Some(std::cmp::Ordering::Less) {
            return None;
        }
        let next = self.next_double() * (max - min) + min;
        Some(if next < max { next } else { next_down(max) })
    }

    /// The in-place shuffle `scala.util.Random.shuffle` performs.
    pub fn shuffle<T>(&mut self, buf: &mut [T]) {
        for n in (2..=buf.len()).rev() {
            let k = self.next_int_bound(n as i32).unwrap_or(0) as usize;
            buf.swap(n - 1, k);
        }
    }
}

/// `Math.nextAfter(x, -Infinity)` for a finite `x`.
fn next_down(x: f64) -> f64 {
    if x == 0.0 {
        return -f64::from_bits(1);
    }
    let bits = x.to_bits();
    f64::from_bits(if x > 0.0 { bits - 1 } else { bits + 1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reference values captured from a JVM: `new java.util.Random(42)`.
    #[test]
    fn seeded_sequence_matches_jvm() {
        let mut r = JRandom::new(42);
        assert_eq!(r.next_int_bound(100), Some(30));
        assert_eq!(r.next_int_bound(100), Some(63));
        let mut r = JRandom::new(42);
        assert_eq!(r.next_int(), -1170105035);
        assert_eq!(r.next_int(), 234785527);
    }

    #[test]
    fn bound_must_be_positive() {
        assert_eq!(JRandom::new(1).next_int_bound(0), None);
        assert_eq!(JRandom::new(1).next_long_bound(-3), None);
        assert_eq!(JRandom::new(1).between_int(5, 5), None);
    }

    #[test]
    fn shuffle_is_a_permutation() {
        let mut v: Vec<i32> = (0..20).collect();
        JRandom::new(7).shuffle(&mut v);
        let mut s = v.clone();
        s.sort();
        assert_eq!(s, (0..20).collect::<Vec<_>>());
        assert_ne!(v, s);
    }
}
