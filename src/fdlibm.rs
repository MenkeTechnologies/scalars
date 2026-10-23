//! `java.lang.StrictMath`: fdlibm 5.3, as the JDK transliterates it in
//! `java.lang.FdLibm` (JDK 21).
//!
//! `scala.math.exp(x)` is `java.lang.Math.exp(x)`, and the JVM's answer is not
//! the platform libm's: Rust's `f64::exp` goes to the system libm, which lands
//! one ulp away from the JDK on a few percent of inputs (`math.exp(1)` is
//! `2.7182818284590455` on the JVM, `2.718281828459045` through macOS libm), and
//! a different libm (glibc, musl) moves different inputs. Every function here is
//! a line-for-line port of the JDK's Java fdlibm, so the result is the JVM's to
//! the bit and identical on every host.
//!
//! Measured on aarch64 HotSpot (OpenJDK 21.0.12), 200 000 random inputs each:
//! `Math.tan`, `asin`, `acos`, `atan`, `atan2`, `exp`, `log`, `log10`, `log1p`,
//! `expm1`, `sinh`, `cosh`, `tanh`, `cbrt`, `hypot` and `pow` are bit-identical
//! to `StrictMath`, so they are wired to this module. `Math.sin` and `Math.cos`
//! are NOT: they are HotSpot intrinsics that differ from `StrictMath` on about
//! 7% of inputs, so `math.sin`/`math.cos` stay on the platform libm and
//! [`strict_sin`]/[`strict_cos`] are only `StrictMath`'s answer (and `tan`'s
//! kernels).
//!
//! Java `int` arithmetic is `i32` here; the constants above `0x7fff_ffff` are
//! the negative `int`s Java reads them as, and `>>>` is a `u32` shift.
//!
//! The original fdlibm notice, which this port preserves:
//!
//! > Copyright (C) 1993 by Sun Microsystems, Inc. All rights reserved.
//! >
//! > Developed at SunSoft, a Sun Microsystems, Inc. business.
//! > Permission to use, copy, modify, and distribute this
//! > software is freely granted, provided that this notice
//! > is preserved.

// The polynomial coefficients are fdlibm's, some of which are near (not equal
// to) the `std::f64::consts` values; and `x - x` / `(x - x) / (x - x)` is
// fdlibm's idiom for "NaN for a NaN or infinite input, raising invalid".
#![allow(clippy::approx_constant, clippy::eq_op)]

use std::f64::consts::PI;

const TWO24: f64 = 16777216.0;
const TWO54: f64 = 1.8014398509481984e16;
const HUGE: f64 = 1.0e300;
const EXP_BITS: i32 = 0x7ff0_0000;
const EXP_SIGNIF_BITS: i32 = 0x7fff_ffff;

/// The high 32 bits of `x`: sign, exponent and the top of the significand.
fn hi(x: f64) -> i32 {
    (x.to_bits() >> 32) as i32
}

/// The low 32 bits of `x`'s significand.
fn lo(x: f64) -> i32 {
    x.to_bits() as i32
}

/// `x` with its high word replaced.
fn with_hi(x: f64, high: i32) -> f64 {
    f64::from_bits((x.to_bits() & 0xffff_ffff) | (u64::from(high as u32) << 32))
}

/// `x` with its low word replaced.
fn with_lo(x: f64, low: i32) -> f64 {
    f64::from_bits((x.to_bits() & 0xffff_ffff_0000_0000) | u64::from(low as u32))
}

fn hi_lo(high: i32, low: i32) -> f64 {
    f64::from_bits((u64::from(high as u32) << 32) | u64::from(low as u32))
}

/// Java's `Math.powerOfTwoD`: `2^n` for `n` in the normal exponent range.
fn power_of_two(n: i32) -> f64 {
    f64::from_bits(((i64::from(n) + 1023) as u64) << 52 & 0x7ff0_0000_0000_0000)
}

/// Java's `Math.scalb`: `d * 2^n`, rounded once, even into the subnormals.
fn scalb(mut d: f64, scale: i32) -> f64 {
    const MAX_SCALE: i32 = 1023 + 1022 + 53 + 1;
    let (mut scale, increment, delta) = if scale < 0 {
        (scale.max(-MAX_SCALE), -512, power_of_two(-512))
    } else {
        (scale.min(MAX_SCALE), 512, power_of_two(512))
    };
    let t = ((scale >> 8) as u32 >> 23) as i32;
    let adjust = ((scale + t) & 511) - t;
    d *= power_of_two(adjust);
    scale -= adjust;
    while scale != 0 {
        d *= delta;
        scale -= increment;
    }
    d
}

// ───────────────────────────── sin / cos / tan ─────────────────────────────

/// `StrictMath.sin`.
pub fn strict_sin(x: f64) -> f64 {
    let ix = hi(x) & EXP_SIGNIF_BITS;
    if ix <= 0x3fe9_21fb {
        return kernel_sin(x, 0.0, 0);
    }
    if ix >= EXP_BITS {
        return x - x;
    }
    let (n, y0, y1) = rem_pio2(x);
    match n & 3 {
        0 => kernel_sin(y0, y1, 1),
        1 => kernel_cos(y0, y1),
        2 => -kernel_sin(y0, y1, 1),
        _ => -kernel_cos(y0, y1),
    }
}

/// `StrictMath.cos`.
pub fn strict_cos(x: f64) -> f64 {
    let ix = hi(x) & EXP_SIGNIF_BITS;
    if ix <= 0x3fe9_21fb {
        return kernel_cos(x, 0.0);
    }
    if ix >= EXP_BITS {
        return x - x;
    }
    let (n, y0, y1) = rem_pio2(x);
    match n & 3 {
        0 => kernel_cos(y0, y1),
        1 => -kernel_sin(y0, y1, 1),
        2 => -kernel_cos(y0, y1),
        _ => kernel_sin(y0, y1, 1),
    }
}

fn kernel_sin(x: f64, y: f64, iy: i32) -> f64 {
    const S1: f64 = -0.16666666666666632;
    const S2: f64 = 0.00833333333332249;
    const S3: f64 = -0.0001984126982985795;
    const S4: f64 = 2.7557313707070068e-6;
    const S5: f64 = -2.5050760253406863e-8;
    const S6: f64 = 1.58969099521155e-10;
    let ix = hi(x) & EXP_SIGNIF_BITS;
    if ix < 0x3e40_0000 && x as i32 == 0 {
        return x;
    }
    let z = x * x;
    let v = z * x;
    let r = S2 + z * (S3 + z * (S4 + z * (S5 + z * S6)));
    if iy == 0 {
        x + v * (S1 + z * r)
    } else {
        x - ((z * (0.5 * y - v * r) - y) - v * S1)
    }
}

fn kernel_cos(x: f64, y: f64) -> f64 {
    const C1: f64 = 0.0416666666666666;
    const C2: f64 = -0.001388888888887411;
    const C3: f64 = 0.00002480158728947673;
    const C4: f64 = -2.7557314351390663e-7;
    const C5: f64 = 2.087572321298175e-9;
    const C6: f64 = -1.1359647557788195e-11;
    let ix = hi(x) & EXP_SIGNIF_BITS;
    if ix < 0x3e40_0000 && x as i32 == 0 {
        return 1.0;
    }
    let z = x * x;
    let r = z * (C1 + z * (C2 + z * (C3 + z * (C4 + z * (C5 + z * C6)))));
    if ix < 0x3fd3_3333 {
        return 1.0 - (0.5 * z - (z * r - x * y));
    }
    let qx = if ix > 0x3fe9_0000 {
        0.28125
    } else {
        hi_lo(ix - 0x0020_0000, 0)
    };
    let hz = 0.5 * z - qx;
    let a = 1.0 - qx;
    a - (hz - (z * r - x * y))
}

/// `StrictMath.tan` (also `Math.tan` on the measured JVM).
pub fn tan(x: f64) -> f64 {
    let ix = hi(x) & EXP_SIGNIF_BITS;
    if ix <= 0x3fe9_21fb {
        return kernel_tan(x, 0.0, 1);
    }
    if ix >= EXP_BITS {
        return x - x;
    }
    let (n, y0, y1) = rem_pio2(x);
    kernel_tan(y0, y1, 1 - ((n & 1) << 1))
}

fn kernel_tan(mut x: f64, mut y: f64, iy: i32) -> f64 {
    const PIO4: f64 = 0.7853981633974483;
    const PIO4LO: f64 = 3.061616997868383e-17;
    const T: [f64; 13] = [
        0.3333333333333341,
        0.13333333333320124,
        0.05396825397622605,
        0.021869488294859542,
        0.0088632398235993,
        0.0035920791075913124,
        0.0014562094543252903,
        0.0005880412408202641,
        0.0002464631348184699,
        0.00007817944429395571,
        0.00007140724913826082,
        -0.000018558637485527546,
        0.00002590730518636337,
    ];
    let hx = hi(x);
    let ix = hx & EXP_SIGNIF_BITS;
    if ix < 0x3e30_0000 && x as i32 == 0 {
        if ((ix | lo(x)) | (iy + 1)) == 0 {
            return 1.0 / x.abs();
        }
        if iy == 1 {
            return x;
        }
        let w = x + y;
        let z = with_lo(w, 0);
        let v = y - (z - x);
        let a = -1.0 / w;
        let t = with_lo(a, 0);
        let s = 1.0 + t * z;
        return t + a * (s + t * v);
    }
    if ix >= 0x3fe5_9428 {
        if hx < 0 {
            x = -x;
            y = -y;
        }
        let z = PIO4 - x;
        let w = PIO4LO - y;
        x = z + w;
        y = 0.0;
    }
    let z = x * x;
    let w = z * z;
    let mut r = T[1] + w * (T[3] + w * (T[5] + w * (T[7] + w * (T[9] + w * T[11]))));
    let v = z * (T[2] + w * (T[4] + w * (T[6] + w * (T[8] + w * (T[10] + w * T[12])))));
    let s = z * x;
    r = y + z * (s * (r + v) + y);
    r += T[0] * s;
    let w = x + r;
    if ix >= 0x3fe5_9428 {
        let v = f64::from(iy);
        return f64::from(1 - ((hx >> 30) & 2)) * (v - 2.0 * (x - (w * w / (w + v) - r)));
    }
    if iy == 1 {
        return w;
    }
    let z = with_lo(w, 0);
    let v = r - (z - x);
    let a = -1.0 / w;
    let t = with_lo(a, 0);
    let s = 1.0 + t * z;
    t + a * (s + t * v)
}

// ─────────────────────────── argument reduction ────────────────────────────

const TWO_OVER_PI: [i32; 66] = [
    0xA2F983, 0x6E4E44, 0x1529FC, 0x2757D1, 0xF534DD, 0xC0DB62, 0x95993C, 0x439041, 0xFE5163,
    0xABDEBB, 0xC561B7, 0x246E3A, 0x424DD2, 0xE00649, 0x2EEA09, 0xD1921C, 0xFE1DEB, 0x1CB129,
    0xA73EE8, 0x8235F5, 0x2EBB44, 0x84E99C, 0x7026B4, 0x5F7E41, 0x3991D6, 0x398353, 0x39F49C,
    0x845F8B, 0xBDF928, 0x3B1FF8, 0x97FFDE, 0x05980F, 0xEF2F11, 0x8B5A0A, 0x6D1F6D, 0x367ECF,
    0x27CB09, 0xB74F46, 0x3F669E, 0x5FEA2D, 0x7527BA, 0xC7EBE5, 0xF17B3D, 0x0739F7, 0x8A5292,
    0xEA6BFB, 0x5FB11F, 0x8D5D08, 0x560330, 0x46FC7B, 0x6BABF0, 0xCFBC20, 0x9AF436, 0x1DA9E3,
    0x91615E, 0xE61B08, 0x659985, 0x5F14A0, 0x68408D, 0xFFD880, 0x4D7327, 0x310606, 0x1556CA,
    0x73A8C9, 0x60E27B, 0xC08C6B,
];

const NPIO2_HW: [i32; 32] = [
    0x3FF921FB, 0x400921FB, 0x4012D97C, 0x401921FB, 0x401F6A7A, 0x4022D97C, 0x4025FDBB, 0x402921FB,
    0x402C463A, 0x402F6A7A, 0x4031475C, 0x4032D97C, 0x40346B9C, 0x4035FDBB, 0x40378FDB, 0x403921FB,
    0x403AB41B, 0x403C463A, 0x403DD85A, 0x403F6A7A, 0x40407E4C, 0x4041475C, 0x4042106C, 0x4042D97C,
    0x4043A28C, 0x40446B9C, 0x404534AC, 0x4045FDBB, 0x4046C6CB, 0x40478FDB, 0x404858EB, 0x404921FB,
];

/// `__ieee754_rem_pio2`: `x` reduced by `n * pi/2`, as `(n, y0, y1)` with the
/// remainder `y0 + y1`.
fn rem_pio2(x: f64) -> (i32, f64, f64) {
    const INVPIO2: f64 = 0.6366197723675814;
    const PIO2_1: f64 = 1.5707963267341256;
    const PIO2_1T: f64 = 6.077100506506192e-11;
    const PIO2_2: f64 = 6.077100506303966e-11;
    const PIO2_2T: f64 = 2.0222662487959506e-21;
    const PIO2_3: f64 = 2.0222662487111665e-21;
    const PIO2_3T: f64 = 8.4784276603689e-32;
    let hx = hi(x);
    let ix = hx & EXP_SIGNIF_BITS;
    if ix <= 0x3fe9_21fb {
        return (0, x, 0.0);
    }
    if ix < 0x4002_d97c {
        // |x| < 3pi/4: one subtraction of pi/2.
        let (y0, y1);
        if hx > 0 {
            let mut z = x - PIO2_1;
            if ix != 0x3ff9_21fb {
                y0 = z - PIO2_1T;
                y1 = (z - y0) - PIO2_1T;
            } else {
                z -= PIO2_2;
                y0 = z - PIO2_2T;
                y1 = (z - y0) - PIO2_2T;
            }
            return (1, y0, y1);
        }
        let mut z = x + PIO2_1;
        if ix != 0x3ff9_21fb {
            y0 = z + PIO2_1T;
            y1 = (z - y0) + PIO2_1T;
        } else {
            z += PIO2_2;
            y0 = z + PIO2_2T;
            y1 = (z - y0) + PIO2_2T;
        }
        return (-1, y0, y1);
    }
    if ix <= 0x4139_21fb {
        // |x| <= 2^19 * pi/2: medium size.
        let mut t = x.abs();
        let n = (t * INVPIO2 + 0.5) as i32;
        let fn_ = f64::from(n);
        let mut r = t - fn_ * PIO2_1;
        let mut w = fn_ * PIO2_1T;
        let mut y0;
        if n < 32 && ix != NPIO2_HW[(n - 1) as usize] {
            y0 = r - w;
        } else {
            let j = ix >> 20;
            y0 = r - w;
            let i = j - ((hi(y0) >> 20) & 0x7ff);
            if i > 16 {
                // Second iteration, good to 118 bits.
                t = r;
                w = fn_ * PIO2_2;
                r = t - w;
                w = fn_ * PIO2_2T - ((t - r) - w);
                y0 = r - w;
                let i = j - ((hi(y0) >> 20) & 0x7ff);
                if i > 49 {
                    // Third iteration, 151 bits.
                    t = r;
                    w = fn_ * PIO2_3;
                    r = t - w;
                    w = fn_ * PIO2_3T - ((t - r) - w);
                    y0 = r - w;
                }
            }
        }
        let y1 = (r - y0) - w;
        return if hx < 0 { (-n, -y0, -y1) } else { (n, y0, y1) };
    }
    if ix >= EXP_BITS {
        let nan = x - x;
        return (0, nan, nan);
    }
    // Large |x|: split it into three 24-bit chunks for the kernel.
    let mut z = with_lo(0.0, lo(x));
    let e0 = (ix >> 20) - 1046;
    z = with_hi(z, ix - (e0 << 20));
    let mut tx = [0.0; 3];
    for t in tx.iter_mut().take(2) {
        *t = f64::from(z as i32);
        z = (z - *t) * TWO24;
    }
    tx[2] = z;
    let mut nx = 3;
    while tx[nx - 1] == 0.0 {
        nx -= 1;
    }
    let (n, y0, y1) = kernel_rem_pio2(&tx[..nx], e0);
    if hx < 0 {
        (-n, -y0, -y1)
    } else {
        (n, y0, y1)
    }
}

/// `__kernel_rem_pio2` at the double-precision setting (`prec == 2`, the only
/// one `rem_pio2` asks for).
fn kernel_rem_pio2(x: &[f64], e0: i32) -> (i32, f64, f64) {
    const PIO2: [f64; 8] = [
        1.570796251296997,
        7.549789415861596e-8,
        5.390302529957765e-15,
        3.282003415807913e-22,
        1.270655753080676e-29,
        1.2293330898111133e-36,
        2.7337005381646456e-44,
        2.1674168387780482e-51,
    ];
    const TWON24: f64 = 5.960464477539063e-8;
    let mut iq = [0i32; 20];
    let mut f = [0.0f64; 20];
    let mut fq = [0.0f64; 20];
    let mut q = [0.0f64; 20];
    let jk: i32 = 4;
    let jp = jk;
    let jx = x.len() as i32 - 1;
    let jv = ((e0 - 3) / 24).max(0);
    let mut q0 = e0 - 24 * (jv + 1);
    let m = jx + jk;
    for (i, j) in (0..=m).zip(jv - jx..) {
        f[i as usize] = if j < 0 {
            0.0
        } else {
            f64::from(TWO_OVER_PI[j as usize])
        };
    }
    let dot = |f: &[f64; 20], i: i32| -> f64 {
        let mut fw = 0.0;
        for j in 0..=jx {
            fw += x[j as usize] * f[(jx + i - j) as usize];
        }
        fw
    };
    for i in 0..=jk {
        q[i as usize] = dot(&f, i);
    }
    let mut jz = jk;
    let (mut z, mut n, mut ih);
    loop {
        // Distill q[] into iq[], reversed.
        z = q[jz as usize];
        let mut i = 0;
        let mut j = jz;
        while j > 0 {
            let fw = f64::from((TWON24 * z) as i32);
            iq[i] = (z - TWO24 * fw) as i32;
            z = q[(j - 1) as usize] + fw;
            i += 1;
            j -= 1;
        }
        z = scalb(z, q0);
        z -= 8.0 * (z * 0.125).floor();
        n = z as i32;
        z -= f64::from(n);
        ih = 0;
        let top = (jz - 1) as usize;
        if q0 > 0 {
            let i = iq[top] >> (24 - q0);
            n += i;
            iq[top] -= i << (24 - q0);
            ih = iq[top] >> (23 - q0);
        } else if q0 == 0 {
            ih = iq[top] >> 23;
        } else if z >= 0.5 {
            ih = 2;
        }
        if ih > 0 {
            n += 1;
            let mut carry = 0;
            for v in iq.iter_mut().take(jz as usize) {
                let j = *v;
                if carry == 0 {
                    if j != 0 {
                        carry = 1;
                        *v = 0x100_0000 - j;
                    }
                } else {
                    *v = 0xff_ffff - j;
                }
            }
            match q0 {
                1 => iq[top] &= 0x7f_ffff,
                2 => iq[top] &= 0x3f_ffff,
                _ => {}
            }
            if ih == 2 {
                z = 1.0 - z;
                if carry != 0 {
                    z -= scalb(1.0, q0);
                }
            }
        }
        if z != 0.0 {
            break;
        }
        let mut j = 0;
        for i in (jk..jz).rev() {
            j |= iq[i as usize];
        }
        if j != 0 {
            break;
        }
        // Recompute with more terms.
        let mut k = 1;
        while iq[(jk - k) as usize] == 0 {
            k += 1;
        }
        for i in jz + 1..=jz + k {
            f[(jx + i) as usize] = f64::from(TWO_OVER_PI[(jv + i) as usize]);
            q[i as usize] = dot(&f, i);
        }
        jz += k;
    }
    if z == 0.0 {
        jz -= 1;
        q0 -= 24;
        while iq[jz as usize] == 0 {
            jz -= 1;
            q0 -= 24;
        }
    } else {
        z = scalb(z, -q0);
        if z >= TWO24 {
            let fw = f64::from((TWON24 * z) as i32);
            iq[jz as usize] = (z - TWO24 * fw) as i32;
            jz += 1;
            q0 += 24;
            iq[jz as usize] = fw as i32;
        } else {
            iq[jz as usize] = z as i32;
        }
    }
    let mut fw = scalb(1.0, q0);
    for i in (0..=jz).rev() {
        q[i as usize] = fw * f64::from(iq[i as usize]);
        fw *= TWON24;
    }
    for i in (0..=jz).rev() {
        let mut fw = 0.0;
        let mut k = 0;
        while k <= jp && k <= jz - i {
            fw += PIO2[k as usize] * q[(i + k) as usize];
            k += 1;
        }
        fq[(jz - i) as usize] = fw;
    }
    let mut fw = 0.0;
    for i in (0..=jz).rev() {
        fw += fq[i as usize];
    }
    let y0 = if ih == 0 { fw } else { -fw };
    fw = fq[0] - fw;
    for i in 1..=jz {
        fw += fq[i as usize];
    }
    let y1 = if ih == 0 { fw } else { -fw };
    (n & 7, y0, y1)
}

// ───────────────────────── inverse trigonometric ──────────────────────────

const PIO2_HI: f64 = 1.5707963267948966;
const PIO2_LO: f64 = 6.123233995736766e-17;
const PS0: f64 = 0.16666666666666666;
const PS1: f64 = -0.3255658186224009;
const PS2: f64 = 0.20121253213486293;
const PS3: f64 = -0.04005553450067941;
const PS4: f64 = 0.0007915349942898145;
const PS5: f64 = 0.00003479331075960212;
const QS1: f64 = -2.403394911734414;
const QS2: f64 = 2.0209457602335057;
const QS3: f64 = -0.6882839716054533;
const QS4: f64 = 0.07703815055590194;

/// The rational approximation `asin` and `acos` share: `(p(t), q(t))`.
fn asin_pq(t: f64) -> (f64, f64) {
    let p = t * (PS0 + t * (PS1 + t * (PS2 + t * (PS3 + t * (PS4 + t * PS5)))));
    let q = 1.0 + t * (QS1 + t * (QS2 + t * (QS3 + t * QS4)));
    (p, q)
}

/// `StrictMath.asin`.
pub fn asin(x: f64) -> f64 {
    const PIO4_HI: f64 = 0.7853981633974483;
    let hx = hi(x);
    let ix = hx & EXP_SIGNIF_BITS;
    if ix >= 0x3ff0_0000 {
        if ((ix - 0x3ff0_0000) | lo(x)) == 0 {
            return x * PIO2_HI + x * PIO2_LO;
        }
        return (x - x) / (x - x);
    }
    if ix < 0x3fe0_0000 {
        let mut t = 0.0;
        if ix < 0x3e40_0000 {
            if HUGE + x > 1.0 {
                return x;
            }
        } else {
            t = x * x;
        }
        let (p, q) = asin_pq(t);
        return x + x * (p / q);
    }
    let w = 1.0 - x.abs();
    let mut t = w * 0.5;
    let (p, q) = asin_pq(t);
    let s = t.sqrt();
    if ix >= 0x3fef_3333 {
        let w = p / q;
        t = PIO2_HI - (2.0 * (s + s * w) - PIO2_LO);
    } else {
        let w = with_lo(s, 0);
        let c = (t - w * w) / (s + w);
        let r = p / q;
        let p = 2.0 * s * r - (PIO2_LO - 2.0 * c);
        let q = PIO4_HI - 2.0 * w;
        t = PIO4_HI - (p - q);
    }
    if hx > 0 {
        t
    } else {
        -t
    }
}

/// `StrictMath.acos`.
pub fn acos(x: f64) -> f64 {
    let hx = hi(x);
    let ix = hx & EXP_SIGNIF_BITS;
    if ix >= 0x3ff0_0000 {
        if ((ix - 0x3ff0_0000) | lo(x)) == 0 {
            return if hx > 0 { 0.0 } else { PI + 2.0 * PIO2_LO };
        }
        return (x - x) / (x - x);
    }
    if ix < 0x3fe0_0000 {
        if ix <= 0x3c60_0000 {
            return PIO2_HI + PIO2_LO;
        }
        let (p, q) = asin_pq(x * x);
        return PIO2_HI - (x - (PIO2_LO - x * (p / q)));
    }
    if hx < 0 {
        let z = (1.0 + x) * 0.5;
        let (p, q) = asin_pq(z);
        let s = z.sqrt();
        let w = (p / q) * s - PIO2_LO;
        return PI - 2.0 * (s + w);
    }
    let z = (1.0 - x) * 0.5;
    let s = z.sqrt();
    let df = with_lo(s, 0);
    let c = (z - df * df) / (s + df);
    let (p, q) = asin_pq(z);
    let w = (p / q) * s + c;
    2.0 * (df + w)
}

/// `StrictMath.atan`.
pub fn atan(mut x: f64) -> f64 {
    const ATANHI: [f64; 4] = [
        0.4636476090008061,
        0.7853981633974483,
        0.982793723247329,
        1.5707963267948966,
    ];
    const ATANLO: [f64; 4] = [
        2.2698777452961687e-17,
        3.061616997868383e-17,
        1.3903311031230998e-17,
        6.123233995736766e-17,
    ];
    const AT: [f64; 11] = [
        0.3333333333333293,
        -0.19999999999876483,
        0.14285714272503466,
        -0.11111110405462356,
        0.09090887133436507,
        -0.0769187620504483,
        0.06661073137387531,
        -0.058335701337905735,
        0.049768779946159324,
        -0.036531572744216916,
        0.016285820115365782,
    ];
    let hx = hi(x);
    let ix = hx & EXP_SIGNIF_BITS;
    if ix >= 0x4410_0000 {
        // |x| >= 2^66
        if ix > EXP_BITS || (ix == EXP_BITS && lo(x) != 0) {
            return x + x;
        }
        return if hx > 0 {
            ATANHI[3] + ATANLO[3]
        } else {
            -ATANHI[3] - ATANLO[3]
        };
    }
    let id: i32;
    if ix < 0x3fdc_0000 {
        if ix < 0x3e20_0000 && HUGE + x > 1.0 {
            return x;
        }
        id = -1;
    } else {
        x = x.abs();
        if ix < 0x3ff3_0000 {
            if ix < 0x3fe6_0000 {
                id = 0;
                x = (2.0 * x - 1.0) / (2.0 + x);
            } else {
                id = 1;
                x = (x - 1.0) / (x + 1.0);
            }
        } else if ix < 0x4003_8000 {
            id = 2;
            x = (x - 1.5) / (1.0 + 1.5 * x);
        } else {
            id = 3;
            x = -1.0 / x;
        }
    }
    let z = x * x;
    let w = z * z;
    let s1 = z * (AT[0] + w * (AT[2] + w * (AT[4] + w * (AT[6] + w * (AT[8] + w * AT[10])))));
    let s2 = w * (AT[1] + w * (AT[3] + w * (AT[5] + w * (AT[7] + w * AT[9]))));
    if id < 0 {
        return x - x * (s1 + s2);
    }
    let id = id as usize;
    let z = ATANHI[id] - ((x * (s1 + s2) - ATANLO[id]) - x);
    if hx < 0 {
        -z
    } else {
        z
    }
}

/// `StrictMath.atan2`.
pub fn atan2(y: f64, x: f64) -> f64 {
    const TINY: f64 = 1.0e-300;
    const PI_O_4: f64 = 0.7853981633974483;
    const PI_O_2: f64 = 1.5707963267948966;
    const PI_LO: f64 = 1.2246467991473532e-16;
    let (hx, lx) = (hi(x), lo(x));
    let ix = hx & EXP_SIGNIF_BITS;
    let (hy, ly) = (hi(y), lo(y));
    let iy = hy & EXP_SIGNIF_BITS;
    if x.is_nan() || y.is_nan() {
        return x + y;
    }
    if (hx.wrapping_sub(0x3ff0_0000) | lx) == 0 {
        return atan(y);
    }
    let m = ((hy >> 31) & 1) | ((hx >> 30) & 2);
    if (iy | ly) == 0 {
        match m {
            0 | 1 => return y,
            2 => return PI + TINY,
            _ => return -PI - TINY,
        }
    }
    if (ix | lx) == 0 {
        return if hy < 0 {
            -PI_O_2 - TINY
        } else {
            PI_O_2 + TINY
        };
    }
    if ix == EXP_BITS {
        return if iy == EXP_BITS {
            match m {
                0 => PI_O_4 + TINY,
                1 => -PI_O_4 - TINY,
                2 => 3.0 * PI_O_4 + TINY,
                _ => -3.0 * PI_O_4 - TINY,
            }
        } else {
            match m {
                0 => 0.0,
                1 => -0.0,
                2 => PI + TINY,
                _ => -PI - TINY,
            }
        };
    }
    if iy == EXP_BITS {
        return if hy < 0 {
            -PI_O_2 - TINY
        } else {
            PI_O_2 + TINY
        };
    }
    let k = (iy - ix) >> 20;
    let z = if k > 60 {
        PI_O_2 + 0.5 * PI_LO
    } else if hx < 0 && k < -60 {
        0.0
    } else {
        atan((y / x).abs())
    };
    match m {
        0 => z,
        1 => -z,
        2 => PI - (z - PI_LO),
        _ => (z - PI_LO) - PI,
    }
}

// ──────────────────────────── roots and hypot ─────────────────────────────

/// `StrictMath.cbrt`.
pub fn cbrt(mut x: f64) -> f64 {
    const B1: i32 = 715_094_163;
    const B2: i32 = 696_219_795;
    const C: f64 = 0.5428571428571428;
    const D: f64 = -0.7053061224489796;
    const E: f64 = 1.4142857142857144;
    const F: f64 = 1.6071428571428572;
    const G: f64 = 0.35714285714285715;
    if x == 0.0 || !x.is_finite() {
        return x;
    }
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    x = x.abs();
    let mut t = if x < f64::MIN_POSITIVE {
        let t = TWO54 * x;
        with_hi(t, hi(t) / 3 + B2)
    } else {
        with_hi(0.0, hi(x) / 3 + B1)
    };
    let r = t * t / x;
    let s = C + r * t;
    t *= G + F / (s + E + D / s);
    t = with_lo(t, 0);
    t = with_hi(t, hi(t) + 1);
    let s = t * t;
    let r = x / s;
    let w = t + t;
    let r = (r - t) / (w + r);
    t += t * r;
    sign * t
}

/// `StrictMath.hypot`.
pub fn hypot(x: f64, y: f64) -> f64 {
    const TWO_MINUS_600: f64 = 2.409919865102884e-181;
    const TWO_PLUS_600: f64 = 4.149515568880993e180;
    let mut a = x.abs();
    let mut b = y.abs();
    if !a.is_finite() || !b.is_finite() {
        return if a == f64::INFINITY || b == f64::INFINITY {
            f64::INFINITY
        } else {
            a + b
        };
    }
    if b > a {
        std::mem::swap(&mut a, &mut b);
    }
    let mut ha = hi(a);
    let mut hb = hi(b);
    if (ha - hb) > 0x3c0_0000 {
        return a + b;
    }
    let mut k = 0;
    if a > 3.2733937296446915e150 {
        ha -= 0x2580_0000;
        hb -= 0x2580_0000;
        a *= TWO_MINUS_600;
        b *= TWO_MINUS_600;
        k += 600;
    }
    if b < 3.054936363499605e-151 {
        if b < f64::MIN_POSITIVE {
            if b == 0.0 {
                return a;
            }
            let t1 = 4.49423283715579e307;
            b *= t1;
            a *= t1;
            k -= 1022;
        } else {
            ha += 0x2580_0000;
            hb += 0x2580_0000;
            a *= TWO_PLUS_600;
            b *= TWO_PLUS_600;
            k -= 600;
        }
    }
    let mut w = a - b;
    if w > b {
        let t1 = with_hi(0.0, ha);
        let t2 = a - t1;
        w = (t1 * t1 - (b * (-b) - t2 * (a + t1))).sqrt();
    } else {
        a += a;
        let y1 = with_hi(0.0, hb);
        let y2 = b - y1;
        let t1 = with_hi(0.0, ha + 0x0010_0000);
        let t2 = a - t1;
        w = (t1 * y1 - (w * (-w) - (t1 * y2 + t2 * b))).sqrt();
    }
    if k != 0 {
        power_of_two(k) * w
    } else {
        w
    }
}

// ───────────────────────────────── pow ─────────────────────────────────────

/// `StrictMath.pow`.
pub fn pow(x: f64, y: f64) -> f64 {
    if y == 0.0 {
        return 1.0;
    }
    if x.is_nan() || y.is_nan() {
        return x + y;
    }
    let y_abs = y.abs();
    let mut x_abs = x.abs();
    if y == 2.0 {
        return x * x;
    } else if y == 0.5 {
        if x >= -f64::MAX {
            return (x + 0.0).sqrt();
        }
    } else if y_abs == 1.0 {
        return if y == 1.0 { x } else { 1.0 / x };
    } else if y_abs == f64::INFINITY {
        return if x_abs == 1.0 {
            y - y
        } else if x_abs > 1.0 {
            if y >= 0.0 {
                y
            } else {
                0.0
            }
        } else if y < 0.0 {
            -y
        } else {
            0.0
        };
    }
    let hx = hi(x);
    let mut ix = hx & EXP_SIGNIF_BITS;
    // 0: y not an integer, 1: odd integer, 2: even integer. Only asked for x < 0.
    let mut y_is_int = 0;
    if hx < 0 {
        if y_abs >= 9007199254740992.0 {
            y_is_int = 2;
        } else if y_abs >= 1.0 {
            let y_long = y_abs as i64;
            if y_long as f64 == y_abs {
                y_is_int = 2 - (y_long & 1) as i32;
            }
        }
    }
    if x_abs == 0.0 || x_abs == f64::INFINITY || x_abs == 1.0 {
        let mut z = x_abs;
        if y < 0.0 {
            z = 1.0 / z;
        }
        if hx < 0 {
            if ((ix - 0x3ff0_0000) | y_is_int) == 0 {
                z = (z - z) / (z - z);
            } else if y_is_int == 1 {
                z = -z;
            }
        }
        return z;
    }
    let mut n = (hx >> 31) + 1;
    if (n | y_is_int) == 0 {
        return (x - x) / (x - x);
    }
    let mut s = 1.0;
    if (n | (y_is_int - 1)) == 0 {
        s = -1.0;
    }
    let (mut p_h, mut p_l, t1, t2);
    if y_abs > 2147485695.9999995 {
        const INV_LN2: f64 = 1.4426950408889634;
        const INV_LN2_H: f64 = 1.4426950216293335;
        const INV_LN2_L: f64 = 1.9259629911266175e-8;
        if x_abs < 0.9999995231628418 {
            return if y < 0.0 { s * f64::INFINITY } else { s * 0.0 };
        }
        if x_abs > 1.0000009536743162 {
            return if y > 0.0 { s * f64::INFINITY } else { s * 0.0 };
        }
        let t = x_abs - 1.0;
        let w = (t * t) * (0.5 - t * (0.3333333333333333 - t * 0.25));
        let u = INV_LN2_H * t;
        let v = t * INV_LN2_L - w * INV_LN2;
        let a = with_lo(u + v, 0);
        t1 = a;
        t2 = v - (a - u);
    } else {
        const CP: f64 = 0.9617966939259756;
        const CP_H: f64 = 0.9617967009544373;
        const CP_L: f64 = -7.028461650952758e-9;
        const BP: [f64; 2] = [1.0, 1.5];
        const DP_H: [f64; 2] = [0.0, 0.5849624872207642];
        const DP_L: [f64; 2] = [0.0, 1.350039202129749e-8];
        const L1: f64 = 0.5999999999999946;
        const L2: f64 = 0.4285714285785502;
        const L3: f64 = 0.33333332981837743;
        const L4: f64 = 0.272728123808534;
        const L5: f64 = 0.23066074577556175;
        const L6: f64 = 0.20697501780033842;
        n = 0;
        if ix < 0x0010_0000 {
            x_abs *= 9007199254740992.0;
            n -= 53;
            ix = hi(x_abs);
        }
        n += (ix >> 20) - 0x3ff;
        let j = ix & 0x000f_ffff;
        ix = j | 0x3ff0_0000;
        let k: usize = if j <= 0x3988E {
            0
        } else if j < 0xBB67A {
            1
        } else {
            n += 1;
            ix -= 0x0010_0000;
            0
        };
        x_abs = with_hi(x_abs, ix);
        let u = x_abs - BP[k];
        let v = 1.0 / (x_abs + BP[k]);
        let ss = u * v;
        let s_h = with_lo(ss, 0);
        let t_h = with_hi(
            0.0,
            ((ix >> 1) | 0x2000_0000) + 0x0008_0000 + ((k as i32) << 18),
        );
        let t_l = x_abs - (t_h - BP[k]);
        let s_l = v * ((u - s_h * t_h) - s_h * t_l);
        let s2 = ss * ss;
        let mut r = s2 * s2 * (L1 + s2 * (L2 + s2 * (L3 + s2 * (L4 + s2 * (L5 + s2 * L6)))));
        r += s_l * (s_h + ss);
        let s2 = s_h * s_h;
        let t_h = with_lo(3.0 + s2 + r, 0);
        let t_l = r - ((t_h - 3.0) - s2);
        let u = s_h * t_h;
        let v = s_l * t_h + t_l * ss;
        p_h = with_lo(u + v, 0);
        p_l = v - (p_h - u);
        let z_h = CP_H * p_h;
        let z_l = CP_L * p_h + p_l * CP + DP_L[k];
        let t = f64::from(n);
        let a = with_lo(((z_h + z_l) + DP_H[k]) + t, 0);
        t1 = a;
        t2 = z_l - (((a - t) - DP_H[k]) - z_h);
    }
    // Split y into y1 + y2 and compute (y1 + y2) * (t1 + t2).
    let y1 = with_lo(y, 0);
    p_l = (y - y1) * t1 + y * t2;
    p_h = y1 * t1;
    let mut z = p_l + p_h;
    let mut j = hi(z);
    let i = lo(z);
    if j >= 0x4090_0000 {
        // z >= 1024
        if ((j - 0x4090_0000) | i) != 0 {
            return s * f64::INFINITY;
        }
        const OVT: f64 = 8.008566259537294e-17;
        if p_l + OVT > z - p_h {
            return s * f64::INFINITY;
        }
    } else if (j & EXP_SIGNIF_BITS) >= 0x4090_cc00 {
        // z <= -1075
        if (j.wrapping_sub(0xc090_cc00_u32 as i32) | i) != 0 {
            return s * 0.0;
        }
        if p_l <= z - p_h {
            return s * 0.0;
        }
    }
    const P1: f64 = 0.16666666666666602;
    const P2: f64 = -0.0027777777777015593;
    const P3: f64 = 0.00006613756321437934;
    const P4: f64 = -1.6533902205465252e-6;
    const P5: f64 = 4.1381367970572385e-8;
    const LG2: f64 = 0.6931471805599453;
    const LG2_H: f64 = 0.6931471824645996;
    const LG2_L: f64 = -1.904654299957768e-9;
    let i = j & EXP_SIGNIF_BITS;
    let mut k = (i >> 20) - 0x3ff;
    let mut n = 0;
    if i > 0x3fe0_0000 {
        // |z| > 0.5: n = [z + 0.5]
        n = j + (0x0010_0000 >> (k + 1));
        k = ((n & EXP_SIGNIF_BITS) >> 20) - 0x3ff;
        let t = with_hi(0.0, n & !(0x000f_ffff_i32.wrapping_shr(k as u32)));
        n = ((n & 0x000f_ffff) | 0x0010_0000).wrapping_shr((20 - k) as u32);
        if j < 0 {
            n = -n;
        }
        p_h -= t;
    }
    let t = with_lo(p_l + p_h, 0);
    let u = t * LG2_H;
    let v = (p_l - (t - p_h)) * LG2 + t * LG2_L;
    z = u + v;
    let w = v - (z - u);
    let t = z * z;
    let t1 = z - t * (P1 + t * (P2 + t * (P3 + t * (P4 + t * P5))));
    let r = (z * t1) / (t1 - 2.0) - (w + z * w);
    z = 1.0 - (r - z);
    j = hi(z);
    j = j.wrapping_add(n << 20);
    if (j >> 20) <= 0 {
        z = scalb(z, n);
    } else {
        z = with_hi(z, hi(z).wrapping_add(n << 20));
    }
    s * z
}

// ────────────────────────────── exp and log ───────────────────────────────

const LN2_HI: f64 = 0.6931471803691238;
const LN2_LO: f64 = 1.9082149292705877e-10;
const INVLN2: f64 = 1.4426950408889634;
const O_THRESHOLD: f64 = 709.782712893384;

/// `StrictMath.exp`.
pub fn exp(mut x: f64) -> f64 {
    const HALF: [f64; 2] = [0.5, -0.5];
    const TWOM1000: f64 = 9.332636185032189e-302;
    const U_THRESHOLD: f64 = -745.1332191019411;
    const P1: f64 = 0.16666666666666602;
    const P2: f64 = -0.0027777777777015593;
    const P3: f64 = 0.00006613756321437934;
    const P4: f64 = -1.6533902205465252e-6;
    const P5: f64 = 4.1381367970572385e-8;
    let (mut hi_part, mut lo_part) = (0.0, 0.0);
    let mut k = 0;
    let mut hx = hi(x);
    let xsb = ((hx >> 31) & 1) as usize;
    hx &= EXP_SIGNIF_BITS;
    if hx >= 0x4086_2E42 {
        // |x| >= 709.78
        if hx >= EXP_BITS {
            if ((hx & 0xf_ffff) | lo(x)) != 0 {
                return x + x;
            }
            return if xsb == 0 { x } else { 0.0 };
        }
        if x > O_THRESHOLD {
            return HUGE * HUGE;
        }
        if x < U_THRESHOLD {
            return TWOM1000 * TWOM1000;
        }
    }
    if hx > 0x3fd6_2e42 {
        // |x| > 0.5 ln2: reduce.
        if hx < 0x3FF0_A2B2 {
            let sign = if xsb == 0 { 1.0 } else { -1.0 };
            hi_part = x - sign * LN2_HI;
            lo_part = sign * LN2_LO;
            k = 1 - xsb as i32 - xsb as i32;
        } else {
            k = (INVLN2 * x + HALF[xsb]) as i32;
            let t = f64::from(k);
            hi_part = x - t * LN2_HI;
            lo_part = t * LN2_LO;
        }
        x = hi_part - lo_part;
    } else if hx < 0x3e30_0000 {
        if HUGE + x > 1.0 {
            return 1.0 + x;
        }
    } else {
        k = 0;
    }
    let t = x * x;
    let c = x - t * (P1 + t * (P2 + t * (P3 + t * (P4 + t * P5))));
    if k == 0 {
        return 1.0 - ((x * c) / (c - 2.0) - x);
    }
    let y = 1.0 - ((lo_part - (x * c) / (2.0 - c)) - hi_part);
    if k >= -1021 {
        with_hi(y, hi(y) + (k << 20))
    } else {
        with_hi(y, hi(y) + ((k + 1000) << 20)) * TWOM1000
    }
}

const LG1: f64 = 0.6666666666666735;
const LG2: f64 = 0.3999999999940942;
const LG3: f64 = 0.2857142874366239;
const LG4: f64 = 0.22222198432149784;
const LG5: f64 = 0.1818357216161805;
const LG6: f64 = 0.15313837699209373;
const LG7: f64 = 0.14798198605116586;

/// `StrictMath.log`.
pub fn log(mut x: f64) -> f64 {
    let mut hx = hi(x);
    let lx = lo(x);
    let mut k = 0;
    if hx < 0x0010_0000 {
        // x < 2^-1022
        if ((hx & EXP_SIGNIF_BITS) | lx) == 0 {
            return -TWO54 / 0.0;
        }
        if hx < 0 {
            return (x - x) / 0.0;
        }
        k -= 54;
        x *= TWO54;
        hx = hi(x);
    }
    if hx >= EXP_BITS {
        return x + x;
    }
    k += (hx >> 20) - 1023;
    hx &= 0x000f_ffff;
    let i = (hx + 0x9_5f64) & 0x10_0000;
    x = with_hi(x, hx | (i ^ 0x3ff0_0000));
    k += i >> 20;
    let f = x - 1.0;
    let dk = f64::from(k);
    if (0x000f_ffff & (2 + hx)) < 3 {
        // -2^-20 <= f < 2^-20
        if f == 0.0 {
            return if k == 0 {
                0.0
            } else {
                dk * LN2_HI + dk * LN2_LO
            };
        }
        let r = f * f * (0.5 - 0.3333333333333333 * f);
        return if k == 0 {
            f - r
        } else {
            dk * LN2_HI - ((r - dk * LN2_LO) - f)
        };
    }
    let s = f / (2.0 + f);
    let z = s * s;
    let mut i = hx - 0x6_147a;
    let w = z * z;
    let j = 0x6b851 - hx;
    let t1 = w * (LG2 + w * (LG4 + w * LG6));
    let t2 = z * (LG1 + w * (LG3 + w * (LG5 + w * LG7)));
    i |= j;
    let r = t2 + t1;
    if i > 0 {
        let hfsq = 0.5 * f * f;
        if k == 0 {
            f - (hfsq - s * (hfsq + r))
        } else {
            dk * LN2_HI - ((hfsq - (s * (hfsq + r) + dk * LN2_LO)) - f)
        }
    } else if k == 0 {
        f - s * (f - r)
    } else {
        dk * LN2_HI - ((s * (f - r) - dk * LN2_LO) - f)
    }
}

/// `StrictMath.log10`.
pub fn log10(mut x: f64) -> f64 {
    const IVLN10: f64 = 0.4342944819032518;
    const LOG10_2HI: f64 = 0.30102999566361177;
    const LOG10_2LO: f64 = 3.694239077158931e-13;
    let mut hx = hi(x);
    let lx = lo(x);
    let mut k = 0;
    if hx < 0x0010_0000 {
        if ((hx & EXP_SIGNIF_BITS) | lx) == 0 {
            return -TWO54 / 0.0;
        }
        if hx < 0 {
            return (x - x) / 0.0;
        }
        k -= 54;
        x *= TWO54;
        hx = hi(x);
    }
    if hx >= EXP_BITS {
        return x + x;
    }
    k += (hx >> 20) - 1023;
    let i = ((k as u32) >> 31) as i32;
    hx = (hx & 0x000f_ffff) | ((0x3ff - i) << 20);
    let y = f64::from(k + i);
    x = with_hi(x, hx);
    let z = y * LOG10_2LO + IVLN10 * log(x);
    z + y * LOG10_2HI
}

/// `StrictMath.log1p`.
pub fn log1p(x: f64) -> f64 {
    let hx = hi(x);
    let ax = hx & EXP_SIGNIF_BITS;
    let (mut f, mut c, mut hu) = (0.0, 0.0, 0);
    let mut k = 1;
    if hx < 0x3FDA_827A {
        // x < 0.41422
        if ax >= 0x3ff0_0000 {
            return if x == -1.0 {
                f64::NEG_INFINITY
            } else {
                f64::NAN
            };
        }
        if ax < 0x3e20_0000 {
            return if TWO54 + x > 0.0 && ax < 0x3c90_0000 {
                x
            } else {
                x - x * x * 0.5
            };
        }
        if hx > 0 || hx <= 0xbfd2_bec3_u32 as i32 {
            // -0.2929 < x < 0.41422
            k = 0;
            f = x;
            hu = 1;
        }
    }
    if hx >= EXP_BITS {
        return x + x;
    }
    if k != 0 {
        let mut u;
        if hx < 0x4340_0000 {
            u = 1.0 + x;
            hu = hi(u);
            k = (hu >> 20) - 1023;
            c = if k > 0 { 1.0 - (u - x) } else { x - (u - 1.0) };
            c /= u;
        } else {
            u = x;
            hu = hi(u);
            k = (hu >> 20) - 1023;
            c = 0.0;
        }
        hu &= 0x000f_ffff;
        if hu < 0x6_a09e {
            u = with_hi(u, hu | 0x3ff0_0000);
        } else {
            k += 1;
            u = with_hi(u, hu | 0x3fe0_0000);
            hu = (0x0010_0000 - hu) >> 2;
        }
        f = u - 1.0;
    }
    let hfsq = 0.5 * f * f;
    let dk = f64::from(k);
    if hu == 0 {
        // |f| < 2^-20
        if f == 0.0 {
            if k == 0 {
                return 0.0;
            }
            c += dk * LN2_LO;
            return dk * LN2_HI + c;
        }
        let r = hfsq * (1.0 - 0.6666666666666666 * f);
        return if k == 0 {
            f - r
        } else {
            dk * LN2_HI - ((r - (dk * LN2_LO + c)) - f)
        };
    }
    let s = f / (2.0 + f);
    let z = s * s;
    let r = z * (LG1 + z * (LG2 + z * (LG3 + z * (LG4 + z * (LG5 + z * (LG6 + z * LG7))))));
    if k == 0 {
        f - (hfsq - s * (hfsq + r))
    } else {
        dk * LN2_HI - ((hfsq - (s * (hfsq + r) + (dk * LN2_LO + c))) - f)
    }
}

/// `StrictMath.expm1`.
pub fn expm1(mut x: f64) -> f64 {
    const TINY: f64 = 1.0e-300;
    const Q1: f64 = -0.03333333333333313;
    const Q2: f64 = 0.0015873015872548146;
    const Q3: f64 = -0.0000793650757867488;
    const Q4: f64 = 4.008217827329362e-6;
    const Q5: f64 = -2.0109921818362437e-7;
    let mut hx = hi(x);
    let negative = hx < 0;
    hx &= EXP_SIGNIF_BITS;
    if hx >= 0x4043_687A {
        // |x| >= 56 ln2
        if hx >= 0x4086_2E42 {
            // |x| >= 709.78
            if hx >= EXP_BITS {
                if ((hx & 0xf_ffff) | lo(x)) != 0 {
                    return x + x;
                }
                return if negative { -1.0 } else { x };
            }
            if x > O_THRESHOLD {
                return HUGE * HUGE;
            }
        }
        if negative && x + TINY < 0.0 {
            return TINY - 1.0;
        }
    }
    let k: i32;
    let mut c = 0.0;
    if hx > 0x3fd6_2e42 {
        // |x| > 0.5 ln2
        let (hi_part, lo_part);
        if hx < 0x3FF0_A2B2 {
            if negative {
                hi_part = x + LN2_HI;
                lo_part = -LN2_LO;
                k = -1;
            } else {
                hi_part = x - LN2_HI;
                lo_part = LN2_LO;
                k = 1;
            }
        } else {
            k = (INVLN2 * x + if negative { -0.5 } else { 0.5 }) as i32;
            let t = f64::from(k);
            hi_part = x - t * LN2_HI;
            lo_part = t * LN2_LO;
        }
        x = hi_part - lo_part;
        c = (hi_part - x) - lo_part;
    } else if hx < 0x3c90_0000 {
        // |x| < 2^-54
        let t = HUGE + x;
        return x - (t - (HUGE + x));
    } else {
        k = 0;
    }
    let hfx = 0.5 * x;
    let hxs = x * hfx;
    let r1 = 1.0 + hxs * (Q1 + hxs * (Q2 + hxs * (Q3 + hxs * (Q4 + hxs * Q5))));
    let t = 3.0 - r1 * hfx;
    let mut e = hxs * ((r1 - t) / (6.0 - x * t));
    if k == 0 {
        return x - (x * e - hxs);
    }
    e = x * (e - c) - c;
    e -= hxs;
    if k == -1 {
        return 0.5 * (x - e) - 0.5;
    }
    if k == 1 {
        return if x < -0.25 {
            -2.0 * (e - (x + 0.5))
        } else {
            1.0 + 2.0 * (x - e)
        };
    }
    if k <= -2 || k > 56 {
        let y = 1.0 - (e - x);
        return with_hi(y, hi(y) + (k << 20)) - 1.0;
    }
    if k < 20 {
        let t = with_hi(1.0, 0x3ff0_0000 - (0x20_0000 >> k));
        let y = t - (e - x);
        with_hi(y, hi(y) + (k << 20))
    } else {
        let t = with_hi(1.0, (0x3ff - k) << 20);
        let y = x - (e + t) + 1.0;
        with_hi(y, hi(y) + (k << 20))
    }
}

// ─────────────────────────────── hyperbolic ───────────────────────────────

/// `StrictMath.sinh`.
pub fn sinh(x: f64) -> f64 {
    const SHUGE: f64 = 1.0e307;
    let jx = hi(x);
    let ix = jx & EXP_SIGNIF_BITS;
    if ix >= EXP_BITS {
        return x + x;
    }
    let h = if jx < 0 { -0.5 } else { 0.5 };
    if ix < 0x4036_0000 {
        // |x| < 22
        if ix < 0x3e30_0000 && SHUGE + x > 1.0 {
            return x;
        }
        let t = expm1(x.abs());
        if ix < 0x3ff0_0000 {
            return h * (2.0 * t - t * t / (t + 1.0));
        }
        return h * (t + t / (t + 1.0));
    }
    if ix < 0x4086_2E42 {
        return h * exp(x.abs());
    }
    // The JDK compares the SIGN-EXTENDED low word as an unsigned `long`
    // (`Long.compareUnsigned(lx, 0x8fb9_f87d)`), so a low word with its top bit
    // set never passes; reproduced as written.
    let lx = lo(x);
    if ix < 0x4086_33CE || (ix == 0x4086_33ce && (i64::from(lx) as u64) <= 0x8fb9_f87d) {
        let w = exp(0.5 * x.abs());
        let t = h * w;
        return t * w;
    }
    x * SHUGE
}

/// `StrictMath.cosh`.
pub fn cosh(x: f64) -> f64 {
    let ix = hi(x) & EXP_SIGNIF_BITS;
    if ix >= EXP_BITS {
        return x * x;
    }
    if ix < 0x3fd6_2e43 {
        // |x| < 0.5 ln2
        let t = expm1(x.abs());
        let w = 1.0 + t;
        if ix < 0x3c80_0000 {
            return w;
        }
        return 1.0 + (t * t) / (w + w);
    }
    if ix < 0x4036_0000 {
        let t = exp(x.abs());
        return 0.5 * t + 0.5 / t;
    }
    if ix < 0x4086_2E42 {
        return 0.5 * exp(x.abs());
    }
    let lx = lo(x);
    if ix < 0x4086_33CE || (ix == 0x4086_33ce && (lx as u32) <= 0x8fb9_f87d) {
        let w = exp(0.5 * x.abs());
        let t = 0.5 * w;
        return t * w;
    }
    HUGE * HUGE
}

/// `StrictMath.tanh`.
pub fn tanh(x: f64) -> f64 {
    const TINY: f64 = 1.0e-300;
    let jx = hi(x);
    let ix = jx & EXP_SIGNIF_BITS;
    if ix >= EXP_BITS {
        return if jx >= 0 {
            1.0 / x + 1.0
        } else {
            1.0 / x - 1.0
        };
    }
    let z = if ix < 0x4036_0000 {
        // |x| < 22
        if ix < 0x3c80_0000 {
            return x * (1.0 + x);
        }
        if ix >= 0x3ff0_0000 {
            let t = expm1(2.0 * x.abs());
            1.0 - 2.0 / (t + 2.0)
        } else {
            let t = expm1(-2.0 * x.abs());
            -t / (t + 2.0)
        }
    } else {
        1.0 - TINY
    };
    if jx >= 0 {
        z
    } else {
        -z
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(function, input bits, JVM result bits)`, each measured through
    /// `scala` 3.10.0 on OpenJDK 21.0.12 and chosen because the platform libm
    /// answers a different double for that input. Arguments are transformed
    /// as the sweep that found them: `log`/`log10`/`log1p` take `|x|`, `pow` is
    /// `pow(|x|, 1.7)`, `asin`/`acos` take `x / 40`, `atan2` is `atan2(x, 1.3)`
    /// and `hypot` is `hypot(x, 2.5)`.
    const JVM: &[(&str, u64, u64)] = &[
        ("exp", 0x401a7ce293a729f4, 0x40877b51e21b5802),
        ("exp", 0xc029cf49ecfd326a, 0x3ec4da8259ce872c),
        ("log", 0xbff742ff8610c978, 0x3fd7f3387433e86b),
        ("log", 0xc014f0c89a8b05ac, 0x3ffa7c7e08d26e22),
        ("log10", 0xc01e9ad9e2512abe, 0x3fec4785cbc54676),
        ("log10", 0x400e31250b108be4, 0x3fe2752863a52f8e),
        ("tan", 0xc01e9ad9e2512abe, 0xc01374e913d23d44),
        ("tan", 0x40302288308337ee, 0x3fdd1d33698759b5),
        ("pow", 0x401a7ce293a729f4, 0x4038debc9923783a),
        ("pow", 0x40302288308337ee, 0x405c420fd3b486c8),
        ("cbrt", 0x40099ba37740cf16, 0x3ff7948b00ae3f2f),
        ("cbrt", 0x402d4143fda06636, 0x400390a385401692),
        ("sinh", 0x400e31250b108be4, 0x4035c3f1d1b1f7c7),
        ("sinh", 0xc02737f5cefc9d34, 0xc0eae24d68b231cb),
        ("cosh", 0x402234811117e2da, 0x40b18911006824a8),
        ("cosh", 0xc021d56eb9d88594, 0x40ad20a44533ad61),
        ("tanh", 0xbfe129f8d09666b8, 0xbfdf600f05f1df52),
        ("tanh", 0xbfbb5f6417c8d5c0, 0xbfbb44cece47d3f1),
        ("atan", 0x40099ba37740cf16, 0x3ff449ba5b992b50),
        ("atan", 0x4032f62018ce79ae, 0x3ff84a2a091b0a7f),
        ("asin", 0xbff76ca9d6db87fc, 0xbfa2be67097fdae8),
        ("asin", 0xc0328a6607503352, 0xbfded86888ffc517),
        ("acos", 0xc021d56eb9d88594, 0x3ffcbace17012902),
        ("acos", 0xc014fea6e0bbf98c, 0x3ffb3d0058c305f7),
        ("atan2", 0xbfe8dd57a87eda60, 0xbfe13d4aae66971d),
        ("atan2", 0xbff17abaa7d9cd9c, 0xbfe65d21d7ec1506),
        ("expm1", 0x401a7ce293a729f4, 0x40877351e21b5802),
        ("expm1", 0x40241a3904df5870, 0x40d6a3bdee8be92e),
        ("log1p", 0xc012263888118b88, 0x3ffb6259ebd3e6bc),
        ("log1p", 0x4032f62018ce79ae, 0x4007f34e583ba7d0),
        ("hypot", 0xc00949ff6f59189e, 0x40101ef49ce38719),
        ("hypot", 0xc014a3562c5f1128, 0x4016eee194c009e4),
    ];

    fn eval(name: &str, x: f64) -> f64 {
        let a = x.abs();
        match name {
            "exp" => exp(x),
            "log" => log(a),
            "log10" => log10(a),
            "tan" => tan(x),
            "pow" => pow(a, 1.7),
            "cbrt" => cbrt(x),
            "sinh" => sinh(x),
            "cosh" => cosh(x),
            "tanh" => tanh(x),
            "atan" => atan(x),
            "asin" => asin(x / 40.0),
            "acos" => acos(x / 40.0),
            "atan2" => atan2(x, 1.3),
            "expm1" => expm1(x),
            "log1p" => log1p(a),
            "hypot" => hypot(x, 2.5),
            _ => unreachable!("{name}"),
        }
    }

    #[test]
    fn every_function_answers_the_jvm_bit_for_bit_where_libm_does_not() {
        for &(name, x, want) in JVM {
            let got = eval(name, f64::from_bits(x));
            assert_eq!(
                got.to_bits(),
                want,
                "{name}({}) = {got:?}, the JVM answers {:?}",
                f64::from_bits(x),
                f64::from_bits(want)
            );
        }
    }

    #[test]
    fn math_exp_of_one_is_the_jvm_e() {
        // `println(math.exp(1))` prints 2.7182818284590455 under scala; libm
        // rounds the other way, to 2.718281828459045 (== `std::f64::consts::E`).
        assert_eq!(exp(1.0), 2.7182818284590455);
        assert_ne!(exp(1.0), std::f64::consts::E);
    }

    #[test]
    fn huge_arguments_reduce_through_the_multi_word_kernel() {
        // |x| > 2^19 * pi/2 takes `kernel_rem_pio2`. `math.tan` and the two
        // `StrictMath` calls, measured on the JVM.
        assert_eq!(tan(1e22), -1.628778225606899);
        assert_eq!(tan(1e300), 1.4214488238747245);
        assert_eq!(strict_sin(1e22), -0.8522008497671888);
        assert_eq!(strict_cos(1e22), 0.523214785395139);
        assert_eq!(strict_sin(3.0e9), 0.9870048864743554);
        assert_eq!(strict_cos(-7.5e15), 0.99342085087232);
    }

    #[test]
    fn special_values_follow_the_jdk() {
        assert!(pow(-8.0, 1.0 / 3.0).is_nan());
        assert_eq!(pow(-2.0, 3.0), -8.0);
        assert_eq!(pow(-0.0, -3.0), f64::NEG_INFINITY);
        assert_eq!(pow(1.0, f64::NAN).to_bits(), f64::NAN.to_bits());
        assert_eq!(pow(f64::NAN, 0.0), 1.0);
        assert!(pow(-1.0, f64::INFINITY).is_nan());
        assert_eq!(pow(2.0, -1074.0), 4.9e-324);
        assert_eq!(hypot(f64::INFINITY, f64::NAN), f64::INFINITY);
        assert_eq!(atan2(0.0, -0.0), PI);
        assert_eq!(atan2(-0.0, -0.0), -PI);
        assert_eq!(atan2(-0.0, 0.0).to_bits(), (-0.0f64).to_bits());
        assert_eq!(log(0.0), f64::NEG_INFINITY);
        assert!(log(-1.0).is_nan());
        assert_eq!(cbrt(-0.0).to_bits(), (-0.0f64).to_bits());
        assert_eq!(exp(-745.2), 0.0);
        assert_eq!(exp(709.79), f64::INFINITY);
        assert_eq!(expm1(f64::NEG_INFINITY), -1.0);
    }
}
