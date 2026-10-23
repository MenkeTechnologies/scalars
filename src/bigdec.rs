//! `scala.math.BigDecimal` — `java.math.BigDecimal` under `MathContext.DECIMAL128`.
//!
//! A value is an unscaled integer and a scale: `unscaled * 10^-scale`. Scala's
//! `BigDecimal` rounds every `+`, `-`, `*`, `/` and `pow` result to 34
//! significant digits (half-even), which is what makes `BigDecimal(10) /
//! BigDecimal(3)` end after 34 digits instead of running forever. This module is
//! the arithmetic only; [`crate::host`] gives it a heap representation, its
//! operators and its members.

use num_bigint::BigInt;
use num_integer::Integer;
use num_traits::{One, Signed, ToPrimitive, Zero};
use std::cmp::Ordering;

/// A `java.math.MathContext`: a precision in significant digits (0 is
/// UNLIMITED — exact results only) and the rounding mode applied past it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MathCtx {
    pub precision: usize,
    pub mode: Round,
}

impl MathCtx {
    /// `MathContext.DECIMAL128`, which every Scala `BigDecimal` carries unless
    /// it was built with another.
    pub const DECIMAL128: MathCtx = MathCtx {
        precision: 34,
        mode: Round::HalfEven,
    };
}

/// A decimal number: `unscaled * 10^-scale`.
#[derive(Clone, Debug)]
pub struct BigDec {
    pub unscaled: BigInt,
    pub scale: i32,
    /// The context this value's own arithmetic rounds in: Scala's
    /// `BigDecimal` carries one, and `a op b` uses `a`'s.
    pub ctx: MathCtx,
}

/// `java.math.RoundingMode`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Round {
    Up,
    Down,
    Ceiling,
    Floor,
    HalfUp,
    HalfDown,
    HalfEven,
    Unnecessary,
}

impl Round {
    /// The mode a `RoundingMode.X` / `BigDecimal.RoundingMode.X` names.
    pub fn parse(name: &str) -> Option<Round> {
        Some(match name {
            "UP" => Round::Up,
            "DOWN" => Round::Down,
            "CEILING" => Round::Ceiling,
            "FLOOR" => Round::Floor,
            "HALF_UP" => Round::HalfUp,
            "HALF_DOWN" => Round::HalfDown,
            "HALF_EVEN" => Round::HalfEven,
            "UNNECESSARY" => Round::Unnecessary,
            _ => return None,
        })
    }
}

/// The error text of a rounding that would lose digits under `UNNECESSARY`.
pub const ROUNDING_NECESSARY: &str = "Rounding necessary";

fn pow10(n: usize) -> BigInt {
    BigInt::from(10).pow(n as u32)
}

/// The number of decimal digits of `|n|` (one for zero).
fn digits(n: &BigInt) -> usize {
    if n.is_zero() {
        1
    } else {
        n.abs().to_string().len()
    }
}

/// `num / den` rounded to an integer under `mode`. `den` is non-zero.
fn divide_round(num: &BigInt, den: &BigInt, mode: Round) -> Result<BigInt, String> {
    let (q, r) = num.div_rem(den);
    if r.is_zero() {
        return Ok(q);
    }
    let negative = num.is_negative() != den.is_negative();
    // How the discarded fraction compares with one half.
    let half = (r.abs() * 2u32).cmp(&den.abs());
    let away = match mode {
        Round::Unnecessary => return Err(ROUNDING_NECESSARY.to_string()),
        Round::Up => true,
        Round::Down => false,
        Round::Ceiling => !negative,
        Round::Floor => negative,
        Round::HalfUp => half != Ordering::Less,
        Round::HalfDown => half == Ordering::Greater,
        Round::HalfEven => match half {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => q.is_odd(),
        },
    };
    Ok(if away {
        if negative {
            q - BigInt::one()
        } else {
            q + BigInt::one()
        }
    } else {
        q
    })
}

impl BigDec {
    pub fn new(unscaled: BigInt, scale: i32) -> BigDec {
        BigDec {
            unscaled,
            scale,
            ctx: MathCtx::DECIMAL128,
        }
    }

    /// This value under the context `ctx` (not rounded to it).
    pub fn with_ctx(mut self, ctx: MathCtx) -> BigDec {
        self.ctx = ctx;
        self
    }

    /// `round(mc)`: rounded to `ctx`, keeping this value's own context.
    pub fn round_to(&self, ctx: MathCtx) -> Result<BigDec, String> {
        let own = self.ctx;
        Ok(self.clone().with_ctx(ctx).try_rounded()?.with_ctx(own))
    }

    pub fn from_int(n: impl Into<BigInt>) -> BigDec {
        BigDec::new(n.into(), 0)
    }

    /// `new BigDecimal(String)`: an optional sign, digits with an optional
    /// point, and an optional `e`/`E` exponent.
    pub fn parse(text: &str) -> Result<BigDec, String> {
        let bad = || "NumberFormatException".to_string();
        let (sign, rest) = match text.strip_prefix('-') {
            Some(r) => (-1, r),
            None => (1, text.strip_prefix('+').unwrap_or(text)),
        };
        let (mantissa, exponent) = match rest.find(['e', 'E']) {
            Some(i) => (&rest[..i], Some(&rest[i + 1..])),
            None => (rest, None),
        };
        let (int_part, frac_part) = match mantissa.split_once('.') {
            Some((i, f)) => (i, f),
            None => (mantissa, ""),
        };
        if int_part.is_empty() && frac_part.is_empty() {
            return Err(bad());
        }
        if !int_part.bytes().all(|b| b.is_ascii_digit())
            || !frac_part.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(bad());
        }
        let mut scale = frac_part.len() as i64;
        if let Some(e) = exponent {
            let e: i64 = e
                .strip_prefix('+')
                .unwrap_or(e)
                .parse()
                .map_err(|_| bad())?;
            scale -= e;
        }
        let digits_text = format!("{int_part}{frac_part}");
        let magnitude = BigInt::parse_bytes(digits_text.as_bytes(), 10).ok_or_else(bad)?;
        let scale = i32::try_from(scale).map_err(|_| bad())?;
        Ok(BigDec::new(magnitude * sign, scale))
    }

    pub fn signum(&self) -> i32 {
        match self.unscaled.sign() {
            num_bigint::Sign::Minus => -1,
            num_bigint::Sign::NoSign => 0,
            num_bigint::Sign::Plus => 1,
        }
    }

    /// The number of significant digits of the unscaled value.
    pub fn precision(&self) -> usize {
        digits(&self.unscaled)
    }

    /// `BigDecimal.toString`: plain notation unless the scale is negative or
    /// the adjusted exponent is below -6, then `d.dddE+x`.
    pub fn render(&self) -> String {
        let coeff = self.unscaled.abs().to_string();
        let sign = if self.unscaled.is_negative() { "-" } else { "" };
        if self.scale == 0 {
            return format!("{sign}{coeff}");
        }
        let adjusted = -(self.scale as i64) + (coeff.len() as i64 - 1);
        if self.scale > 0 && adjusted >= -6 {
            let point = coeff.len() as i64 - self.scale as i64;
            return if point > 0 {
                let (a, b) = coeff.split_at(point as usize);
                format!("{sign}{a}.{b}")
            } else {
                format!("{sign}0.{}{coeff}", "0".repeat((-point) as usize))
            };
        }
        let mantissa = if coeff.len() > 1 {
            format!("{}.{}", &coeff[..1], &coeff[1..])
        } else {
            coeff
        };
        let exp_sign = if adjusted >= 0 { "+" } else { "-" };
        format!("{sign}{mantissa}E{exp_sign}{}", adjusted.abs())
    }

    /// `setScale(new_scale, mode)`.
    pub fn set_scale(&self, new_scale: i32, mode: Round) -> Result<BigDec, String> {
        match new_scale.cmp(&self.scale) {
            Ordering::Equal => Ok(self.clone()),
            Ordering::Greater => {
                let up = pow10((new_scale - self.scale) as usize);
                Ok(BigDec::new(&self.unscaled * up, new_scale))
            }
            Ordering::Less => {
                let down = pow10((self.scale - new_scale) as usize);
                Ok(BigDec::new(
                    divide_round(&self.unscaled, &down, mode)?,
                    new_scale,
                ))
            }
        }
    }

    /// Round to this value's context: at most `ctx.precision` significant
    /// digits (none dropped when it is 0), by `ctx.mode`.
    fn rounded(self) -> BigDec {
        let keep = self.clone();
        self.try_rounded().unwrap_or(keep)
    }

    /// [`BigDec::rounded`], reporting `UNNECESSARY` rounding as an error.
    fn try_rounded(self) -> Result<BigDec, String> {
        let (prec, ctx) = (self.ctx.precision, self.ctx);
        let p = self.precision();
        if prec == 0 || p <= prec {
            return Ok(self);
        }
        let drop = p - prec;
        let q = divide_round(&self.unscaled, &pow10(drop), ctx.mode)?;
        let r = BigDec::new(q, self.scale - drop as i32).with_ctx(ctx);
        // Rounding up can carry into one digit more (999… -> 1000…).
        if r.precision() > prec {
            return Ok(BigDec::new(&r.unscaled / 10, r.scale - 1).with_ctx(ctx));
        }
        Ok(r)
    }

    /// Both operands at the larger of their scales.
    fn align(&self, other: &BigDec) -> (BigInt, BigInt, i32) {
        let scale = self.scale.max(other.scale);
        let lift = |d: &BigDec| &d.unscaled * pow10((scale - d.scale) as usize);
        (lift(self), lift(other), scale)
    }

    pub fn add(&self, other: &BigDec) -> BigDec {
        let (a, b, scale) = self.align(other);
        BigDec::new(a + b, scale).with_ctx(self.ctx).rounded()
    }

    pub fn sub(&self, other: &BigDec) -> BigDec {
        let (a, b, scale) = self.align(other);
        BigDec::new(a - b, scale).with_ctx(self.ctx).rounded()
    }

    pub fn mul(&self, other: &BigDec) -> BigDec {
        BigDec::new(&self.unscaled * &other.unscaled, self.scale + other.scale)
            .with_ctx(self.ctx)
            .rounded()
    }

    pub fn neg(&self) -> BigDec {
        BigDec::new(-&self.unscaled, self.scale).with_ctx(self.ctx)
    }

    pub fn abs(&self) -> BigDec {
        BigDec::new(self.unscaled.abs(), self.scale).with_ctx(self.ctx)
    }

    /// `divide(that, this.mc)`: the quotient to the context's precision (by its
    /// rounding mode), with trailing zeros stripped down to the preferred scale
    /// (`this.scale - that.scale`) when it is exact. Under an UNLIMITED context
    /// the quotient must terminate, or it is the JDK's `ArithmeticException`.
    pub fn div(&self, other: &BigDec) -> Result<BigDec, String> {
        if other.unscaled.is_zero() {
            return Err(if self.unscaled.is_zero() {
                "Division undefined".to_string()
            } else {
                "Division by zero".to_string()
            });
        }
        let ctx = self.ctx;
        let preferred = self.scale as i64 - other.scale as i64;
        if self.unscaled.is_zero() {
            let s = preferred.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
            return Ok(BigDec::new(BigInt::zero(), s).with_ctx(ctx));
        }
        let ten = BigInt::from(10);
        let strip = |mut out: BigDec| {
            while (out.scale as i64) > preferred && out.unscaled.is_multiple_of(&ten) {
                out = BigDec::new(&out.unscaled / &ten, out.scale - 1).with_ctx(ctx);
            }
            out
        };
        if ctx.precision == 0 {
            // Exact: a terminating quotient has a denominator of only 2s and 5s.
            let g = self.unscaled.gcd(&other.unscaled);
            let mut den = (&other.unscaled / &g).abs();
            let mut k = 0usize;
            for p in [2u32, 5] {
                let mut n = 0usize;
                while den.is_multiple_of(&BigInt::from(p)) {
                    den /= p;
                    n += 1;
                }
                k = k.max(n);
            }
            if !den.is_one() {
                return Err(
                    "Non-terminating decimal expansion; no exact representable decimal result."
                        .to_string(),
                );
            }
            let q = (&self.unscaled * pow10(k)) / &other.unscaled;
            let scale = (self.scale as i64 - other.scale as i64 + k as i64)
                .clamp(i32::MIN as i64, i32::MAX as i64) as i32;
            return Ok(strip(BigDec::new(q, scale).with_ctx(ctx)));
        }
        let prec = ctx.precision;
        // Enough extra digits that the integer quotient has more than `prec`
        // of them, so one rounding step finishes the job.
        let extra = (prec + 1 + digits(&other.unscaled)).saturating_sub(digits(&self.unscaled)) + 1;
        let numerator = &self.unscaled * pow10(extra);
        let (q0, rem) = numerator.div_rem(&other.unscaled);
        let mut scale = self.scale as i64 - other.scale as i64 + extra as i64;
        let mut q = q0;
        let mut exact = rem.is_zero();
        let have = digits(&q);
        if have > prec {
            let drop = have - prec;
            // One STICKY digit below what is kept carries "the remainder was not
            // zero", so the rounding sees a value strictly between neighbours —
            // never a false exact half.
            let negative = q.is_negative() != (rem.is_negative() && q.is_zero());
            let sticky = if exact {
                BigInt::zero()
            } else if negative {
                -BigInt::one()
            } else {
                BigInt::one()
            };
            let ext = &q * &ten + sticky;
            exact = exact && (&q % pow10(drop)).is_zero();
            q = divide_round(&ext, &pow10(drop + 1), ctx.mode)?;
            scale -= drop as i64;
            if digits(&q) > prec {
                q /= 10;
                scale -= 1;
            }
        }
        let out =
            BigDec::new(q, scale.clamp(i32::MIN as i64, i32::MAX as i64) as i32).with_ctx(ctx);
        Ok(if exact { strip(out) } else { out })
    }

    /// `this % that` (`remainder`): `this - this.divideToIntegralValue(that) *
    /// that`, with the dividend's sign.
    pub fn rem(&self, other: &BigDec) -> Result<BigDec, String> {
        if other.unscaled.is_zero() {
            return Err("Division by zero".to_string());
        }
        let (a, b, scale) = self.align(other);
        Ok(BigDec::new(a % b, scale))
    }

    /// `this.pow(n)` for a non-negative `n`, rounded like every other result.
    pub fn pow(&self, n: u32) -> BigDec {
        BigDec::new(self.unscaled.pow(n), self.scale.saturating_mul(n as i32))
            .with_ctx(self.ctx)
            .rounded()
    }

    pub fn compare(&self, other: &BigDec) -> Ordering {
        let (a, b, _) = self.align(other);
        a.cmp(&b)
    }

    /// The integral part, truncating toward zero.
    pub fn to_bigint(&self) -> BigInt {
        match self.scale.cmp(&0) {
            Ordering::Equal => self.unscaled.clone(),
            Ordering::Less => &self.unscaled * pow10((-self.scale) as usize),
            Ordering::Greater => &self.unscaled / pow10(self.scale as usize),
        }
    }

    pub fn is_whole(&self) -> bool {
        match self.scale.cmp(&0) {
            Ordering::Greater => (&self.unscaled % pow10(self.scale as usize)).is_zero(),
            _ => true,
        }
    }

    pub fn to_f64(&self) -> f64 {
        format!("{}e{}", self.unscaled, -(self.scale as i64))
            .parse()
            .unwrap_or(f64::NAN)
    }

    pub fn to_i64_lossy(&self) -> i64 {
        let b = self.to_bigint();
        (b & BigInt::from(u64::MAX)).to_u64().unwrap_or(0) as i64
    }
}
