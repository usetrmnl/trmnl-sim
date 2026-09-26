//! Single-precision FPU arithmetic for the Xtensa core.
//!
//! Round-to-nearest-even (the reset and only mode ESP-IDF uses) runs on the host's
//! native `f32` operations. The directed rounding modes selectable through FCR.RM are
//! honoured for add/sub/mul/madd/msub and int->float conversions by computing the exact
//! result (value + residual) in `f64` and rounding that. FSR exception flags are not
//! tracked (they read back as whatever was last written).
//!
//! NaN results are made deterministic across hosts: the first NaN operand (quieted) is
//! propagated, otherwise the default NaN 0x7fc0_0000 is produced.

pub const DEFAULT_NAN: u32 = 0x7fc0_0000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Round {
    NearestEven,
    TowardZero,
    Up,
    Down,
}

impl Round {
    #[inline(always)]
    pub fn from_fcr(fcr: u32) -> Round {
        match fcr & 3 {
            0 => Round::NearestEven,
            1 => Round::TowardZero,
            2 => Round::Up,
            _ => Round::Down,
        }
    }
}

#[inline(always)]
fn f(b: u32) -> f32 {
    f32::from_bits(b)
}

#[inline(always)]
fn is_nan(b: u32) -> bool {
    b & 0x7fff_ffff > 0x7f80_0000
}

/// Fix up a result that came out as NaN.
#[inline(always)]
fn nan_fix(r: f32, ins: &[u32]) -> u32 {
    let rb = r.to_bits();
    if !is_nan(rb) {
        return rb;
    }
    for &i in ins {
        if is_nan(i) {
            return i | 0x0040_0000;
        }
    }
    DEFAULT_NAN
}

fn next_up(x: f32) -> f32 {
    let b = x.to_bits();
    if x.is_nan() || x == f32::INFINITY {
        return x;
    }
    if b == 0x8000_0000 || b == 0 {
        return f32::from_bits(1);
    }
    f32::from_bits(if b & 0x8000_0000 == 0 { b + 1 } else { b - 1 })
}

fn next_down(x: f32) -> f32 {
    -next_up(-x)
}

/// Round the exact value `x + err` (|err| well below one f64 ulp of x) to f32.
fn round_exact(x: f64, err: f64, mode: Round) -> f32 {
    let r = x as f32; // nearest-even of x
    // infinite (or NaN) results are exact: no rounding (the residual may be NaN)
    if mode == Round::NearestEven || !x.is_finite() {
        return r;
    }
    let rd = r as f64;
    if rd == x && err == 0.0 {
        return r;
    }
    // Bracket the true value v = x + err between two adjacent f32s.
    let (lo, hi) = if rd > x || (rd == x && err < 0.0) { (next_down(r), r) } else { (r, next_up(r)) };
    match mode {
        Round::Up => hi,
        Round::Down => lo,
        _ => {
            if x > 0.0 || (x == 0.0 && err > 0.0) {
                lo
            } else {
                hi
            }
        }
    }
}

/// Exact sum of two f64s as (sum, error).
#[inline(always)]
fn two_sum(a: f64, b: f64) -> (f64, f64) {
    let s = a + b;
    let bb = s - a;
    let err = (a - (s - bb)) + (b - bb);
    (s, err)
}

pub fn add(a: u32, b: u32, mode: Round) -> u32 {
    let r = if mode == Round::NearestEven {
        f(a) + f(b)
    } else {
        let (s, e) = two_sum(f(a) as f64, f(b) as f64);
        round_exact(s, e, mode)
    };
    nan_fix(r, &[a, b])
}

pub fn sub(a: u32, b: u32, mode: Round) -> u32 {
    let r = if mode == Round::NearestEven {
        f(a) - f(b)
    } else {
        let (s, e) = two_sum(f(a) as f64, -(f(b) as f64));
        round_exact(s, e, mode)
    };
    nan_fix(r, &[a, b])
}

pub fn mul(a: u32, b: u32, mode: Round) -> u32 {
    let r = if mode == Round::NearestEven { f(a) * f(b) } else { round_exact(f(a) as f64 * f(b) as f64, 0.0, mode) };
    nan_fix(r, &[a, b])
}

/// Fused `acc + b*c` (negated product when `neg`).
pub fn madd(acc: u32, b: u32, c: u32, neg: bool, mode: Round) -> u32 {
    let bb = if neg { -f(b) } else { f(b) };
    let r = if mode == Round::NearestEven {
        bb.mul_add(f(c), f(acc))
    } else {
        let p = bb as f64 * f(c) as f64; // exact
        let (s, e) = two_sum(f(acc) as f64, p);
        round_exact(s, e, mode)
    };
    nan_fix(r, &[acc, b, c])
}

pub fn div(a: u32, b: u32) -> u32 {
    nan_fix(f(a) / f(b), &[a, b])
}

pub fn sqrt(a: u32) -> u32 {
    nan_fix(f(a).sqrt(), &[a])
}

pub fn recip(a: u32) -> u32 {
    nan_fix(1.0 / f(a), &[a])
}

pub fn rsqrt(a: u32) -> u32 {
    let x = f(a) as f64;
    nan_fix((1.0 / x.sqrt()) as f32, &[a])
}

/// FLOAT.S / UFLOAT.S: integer to float, scaled by 2^-scale.
pub fn from_int(v: u32, signed: bool, scale: u32, mode: Round) -> u32 {
    let x = if signed { v as i32 as f64 } else { v as f64 };
    let r = round_exact(x, 0.0, mode);
    (r * (-(scale as i32) as f32).exp2()).to_bits()
}

#[derive(Clone, Copy)]
pub enum ToInt {
    Nearest,
    Trunc,
    Floor,
    Ceil,
}

fn round_f64(x: f64, how: ToInt) -> f64 {
    match how {
        ToInt::Nearest => x.round_ties_even(),
        ToInt::Trunc => x.trunc(),
        ToInt::Floor => x.floor(),
        ToInt::Ceil => x.ceil(),
    }
}

/// ROUND.S/TRUNC.S/FLOOR.S/CEIL.S: float * 2^scale to a saturated int32 (NaN -> 0x7fffffff).
pub fn to_int(a: u32, scale: u32, how: ToInt) -> u32 {
    if is_nan(a) {
        return 0x7fff_ffff;
    }
    let x = round_f64(f(a) as f64 * (scale as f64).exp2(), how);
    if x >= 2147483648.0 {
        0x7fff_ffff
    } else if x < -2147483648.0 {
        0x8000_0000
    } else {
        x as i32 as u32
    }
}

/// UTRUNC.S. Negative (non-NaN) inputs convert as signed; NaN and overflow saturate
/// to 0xffffffff (this matches QEMU's model of the hardware).
pub fn to_uint_trunc(a: u32, scale: u32) -> u32 {
    if is_nan(a) {
        return 0xffff_ffff;
    }
    let v = f(a) as f64 * (scale as f64).exp2();
    if a & 0x8000_0000 != 0 {
        return to_int(a, scale, ToInt::Trunc);
    }
    let x = v.trunc();
    if x >= 4294967296.0 { 0xffff_ffff } else { x as u32 }
}

#[inline(always)]
pub fn un(a: u32, b: u32) -> bool {
    is_nan(a) || is_nan(b)
}
#[inline(always)]
pub fn oeq(a: u32, b: u32) -> bool {
    f(a) == f(b)
}
#[inline(always)]
pub fn olt(a: u32, b: u32) -> bool {
    f(a) < f(b)
}
#[inline(always)]
pub fn ole(a: u32, b: u32) -> bool {
    f(a) <= f(b)
}

/// CONST.S immediates (values 4..15 are reserved; they alias 0..3 like QEMU).
pub fn const_s(imm: u32) -> u32 {
    [0x0000_0000, 0x3f80_0000, 0x4000_0000, 0x3f00_0000][(imm & 3) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directed_rounding() {
        let one = 1.0f32.to_bits();
        let tiny = 1e-30f32.to_bits();
        assert_eq!(f32::from_bits(add(one, tiny, Round::Up)), next_up(1.0));
        assert_eq!(f32::from_bits(add(one, tiny, Round::Down)), 1.0);
        assert_eq!(f32::from_bits(add(one, tiny, Round::NearestEven)), 1.0);
        let m1 = (-1.0f32).to_bits();
        assert_eq!(f32::from_bits(add(m1, tiny, Round::TowardZero)), -next_down(1.0));
        assert_eq!(f32::from_bits(sub(one, tiny, Round::Down)), next_down(1.0));
        // overflow in round-toward-zero saturates at MAX
        let big = f32::MAX.to_bits();
        assert_eq!(f32::from_bits(mul(big, 2.0f32.to_bits(), Round::TowardZero)), f32::MAX);
        assert_eq!(f32::from_bits(mul(big, 2.0f32.to_bits(), Round::Up)), f32::INFINITY);
        // 16777217 is not representable
        assert_eq!(f32::from_bits(from_int(16777217, true, 0, Round::Up)), 16777218.0);
        assert_eq!(f32::from_bits(from_int(16777217, true, 0, Round::Down)), 16777216.0);
    }

    #[test]
    fn conversions() {
        assert_eq!(to_int(2.5f32.to_bits(), 0, ToInt::Nearest), 2);
        assert_eq!(to_int(3.5f32.to_bits(), 0, ToInt::Nearest), 4);
        assert_eq!(to_int((-2.5f32).to_bits(), 0, ToInt::Floor) as i32, -3);
        assert_eq!(to_int(1e20f32.to_bits(), 0, ToInt::Trunc), 0x7fff_ffff);
        assert_eq!(to_int((-1e20f32).to_bits(), 0, ToInt::Trunc), 0x8000_0000);
        assert_eq!(to_int(DEFAULT_NAN, 0, ToInt::Trunc), 0x7fff_ffff);
        assert_eq!(to_int(1.5f32.to_bits(), 4, ToInt::Trunc), 24);
        assert_eq!(to_uint_trunc(3e9f32.to_bits(), 0), 3_000_000_000);
        assert_eq!(to_uint_trunc((-1.0f32).to_bits(), 0), 0xffff_ffff);
        assert_eq!(to_uint_trunc((-0.5f32).to_bits(), 0), 0);
        assert_eq!(f32::from_bits(from_int(0xffff_ffff, false, 0, Round::NearestEven)), 4294967296.0);
        assert_eq!(f32::from_bits(from_int(3, true, 1, Round::NearestEven)), 1.5);
    }

    #[test]
    fn nans() {
        let snan = 0x7f80_0001;
        assert_eq!(add(snan, 1.0f32.to_bits(), Round::NearestEven), 0x7fc0_0001);
        assert_eq!(add(f32::INFINITY.to_bits(), f32::NEG_INFINITY.to_bits(), Round::NearestEven), DEFAULT_NAN);
    }
}
