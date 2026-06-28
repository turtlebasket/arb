//! Exact integer port of Balancer V2 weighted-pool swap math.
//!
//! Ported verbatim (algorithm + rounding directions) from:
//!   - LogExpMath.sol   (MIT)        exp / ln / pow + magic constants
//!   - FixedPoint.sol   (GPL-3.0)    mulUp/mulDown/divUp/divDown/powUp/powDown/complement
//!   - WeightedMath.sol (GPL-3.0)    _calcOutGivenIn
//!
//! All Solidity `int256` map to [`I256`], all `uint256` to [`U256`]. Every
//! `_require(...)` revert becomes an `Err`.
//!
//! ## Division / modulo semantics
//! Solidity integer division and `%` TRUNCATE TOWARD ZERO (the remainder takes
//! the sign of the dividend). `alloy_primitives::I256` `Div`/`Rem` also truncate
//! toward zero, so the two match bit-for-bit. This matters in `exp` (negative
//! exponent recursion) and in `pow` (`ln_36_x % ONE_18` with negative ln).
//!
//! ## Overflow semantics
//! `LogExpMath` targets solc ^0.7.0 (no automatic overflow checks); all
//! intermediate products are designed to fit a signed 256-bit word for in-bounds
//! inputs, so we use ordinary `I256` operators inside exp/ln. `FixedPoint`
//! performs EXPLICIT overflow checks (`product / a == b`), reproduced here with
//! `checked_mul` returning an `Err`.

use crate::types::{I256, U256};
use std::sync::LazyLock;

/// One variant per distinct `_require` in the original contracts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MathError {
    XOutOfBounds,       // pow: x >> 255 != 0
    YOutOfBounds,       // pow: y >= MILD_EXPONENT_BOUND
    ProductOutOfBounds, // pow: y*ln(x) outside [MIN,MAX]_NATURAL_EXPONENT
    InvalidExponent,    // exp: x outside [MIN,MAX]_NATURAL_EXPONENT
    OutOfBounds,        // ln: a <= 0
    MulOverflow,        // FixedPoint mul* overflow check
    DivInternal,        // FixedPoint div* internal mul-overflow check
    ZeroDivision,       // FixedPoint div* by zero
    AddOverflow,        // FixedPoint add overflow
    SubOverflow,        // FixedPoint sub underflow
    MaxInRatio,         // WeightedMath: amountIn too large
}

pub type Result<T> = core::result::Result<T, MathError>;

#[inline]
fn pi(s: &str) -> I256 {
    s.parse::<I256>().unwrap()
}
#[inline]
fn from_i(v: i128) -> I256 {
    I256::try_from(v).unwrap()
}

// ===========================================================================
// LogExpMath constants — copied VERBATIM from LogExpMath.sol.
// ===========================================================================

static ONE_18: LazyLock<I256> = LazyLock::new(|| pi("1000000000000000000"));
static ONE_20: LazyLock<I256> = LazyLock::new(|| pi("100000000000000000000"));
static ONE_36: LazyLock<I256> = LazyLock::new(|| pi("1000000000000000000000000000000000000"));

static MAX_NATURAL_EXPONENT: LazyLock<I256> = LazyLock::new(|| pi("130000000000000000000"));
static MIN_NATURAL_EXPONENT: LazyLock<I256> = LazyLock::new(|| pi("-41000000000000000000"));

static LN_36_LOWER_BOUND: LazyLock<I256> = LazyLock::new(|| pi("900000000000000000"));
static LN_36_UPPER_BOUND: LazyLock<I256> = LazyLock::new(|| pi("1100000000000000000"));

// uint256 constant MILD_EXPONENT_BOUND = 2**254 / uint256(ONE_20);
static MILD_EXPONENT_BOUND: LazyLock<U256> =
    LazyLock::new(|| (U256::from(1u8) << 254usize) / U256::from(100_000_000_000_000_000_000u128));

static X0: LazyLock<I256> = LazyLock::new(|| pi("128000000000000000000")); // 2^7
static A0: LazyLock<I256> =
    LazyLock::new(|| pi("38877084059945950922200000000000000000000000000000000000")); // e^(x0)
static X1: LazyLock<I256> = LazyLock::new(|| pi("64000000000000000000")); // 2^6
static A1: LazyLock<I256> = LazyLock::new(|| pi("6235149080811616882910000000")); // e^(x1)

static X2: LazyLock<I256> = LazyLock::new(|| pi("3200000000000000000000")); // 2^5
static A2: LazyLock<I256> = LazyLock::new(|| pi("7896296018268069516100000000000000")); // e^(x2)
static X3: LazyLock<I256> = LazyLock::new(|| pi("1600000000000000000000")); // 2^4
static A3: LazyLock<I256> = LazyLock::new(|| pi("888611052050787263676000000")); // e^(x3)
static X4: LazyLock<I256> = LazyLock::new(|| pi("800000000000000000000")); // 2^3
static A4: LazyLock<I256> = LazyLock::new(|| pi("298095798704172827474000")); // e^(x4)
static X5: LazyLock<I256> = LazyLock::new(|| pi("400000000000000000000")); // 2^2
static A5: LazyLock<I256> = LazyLock::new(|| pi("5459815003314423907810")); // e^(x5)
static X6: LazyLock<I256> = LazyLock::new(|| pi("200000000000000000000")); // 2^1
static A6: LazyLock<I256> = LazyLock::new(|| pi("738905609893065022723")); // e^(x6)
static X7: LazyLock<I256> = LazyLock::new(|| pi("100000000000000000000")); // 2^0
static A7: LazyLock<I256> = LazyLock::new(|| pi("271828182845904523536")); // e^(x7)
static X8: LazyLock<I256> = LazyLock::new(|| pi("50000000000000000000")); // 2^-1
static A8: LazyLock<I256> = LazyLock::new(|| pi("164872127070012814685")); // e^(x8)
static X9: LazyLock<I256> = LazyLock::new(|| pi("25000000000000000000")); // 2^-2
static A9: LazyLock<I256> = LazyLock::new(|| pi("128402541668774148407")); // e^(x9)
static X10: LazyLock<I256> = LazyLock::new(|| pi("12500000000000000000")); // 2^-3
static A10: LazyLock<I256> = LazyLock::new(|| pi("113314845306682631683")); // e^(x10)
static X11: LazyLock<I256> = LazyLock::new(|| pi("6250000000000000000")); // 2^-4
static A11: LazyLock<I256> = LazyLock::new(|| pi("106449445891785942956")); // e^(x11)

// ===========================================================================
// LogExpMath::pow / exp / ln / _ln / _ln_36
// ===========================================================================

/// `x^y` with unsigned 18-decimal fixed point base and exponent.
/// Reverts if `ln(x) * y` is outside `[MIN, MAX]_NATURAL_EXPONENT`.
pub fn pow(x: U256, y: U256) -> Result<U256> {
    if y.is_zero() {
        return Ok(one_18_u());
    }
    if x.is_zero() {
        return Ok(U256::ZERO);
    }

    // `_require(x >> 255 == 0, X_OUT_OF_BOUNDS)`
    if !(x >> 255usize).is_zero() {
        return Err(MathError::XOutOfBounds);
    }
    let x_int: I256 = I256::from_raw(x);

    // `_require(y < MILD_EXPONENT_BOUND, Y_OUT_OF_BOUNDS)`
    if y >= *MILD_EXPONENT_BOUND {
        return Err(MathError::YOutOfBounds);
    }
    let y_int: I256 = I256::from_raw(y);

    let logx_times_y: I256 = if *LN_36_LOWER_BOUND < x_int && x_int < *LN_36_UPPER_BOUND {
        let ln_36_x = ln_36(x_int);
        // 36-decimal ln split into hi/lo to avoid overflow when multiplying by y.
        // Both `/` and `%` truncate toward zero, matching Solidity exactly.
        (ln_36_x / *ONE_18) * y_int + ((ln_36_x % *ONE_18) * y_int) / *ONE_18
    } else {
        ln_internal(x_int) * y_int
    };
    let logx_times_y = logx_times_y / *ONE_18;

    if !(*MIN_NATURAL_EXPONENT <= logx_times_y && logx_times_y <= *MAX_NATURAL_EXPONENT) {
        return Err(MathError::ProductOutOfBounds);
    }

    Ok(exp(logx_times_y)?.into_raw())
}

/// Natural exponentiation `e^x` with signed 18-decimal fixed point exponent.
pub fn exp(mut x: I256) -> Result<I256> {
    if !(x >= *MIN_NATURAL_EXPONENT && x <= *MAX_NATURAL_EXPONENT) {
        return Err(MathError::InvalidExponent);
    }

    if x < I256::ZERO {
        // e^(-x) = 1 / e^x ; fixed point division multiplies by ONE_18.
        let pos = exp(-x)?;
        return Ok((*ONE_18 * *ONE_18) / pos);
    }

    let first_an: I256;
    if x >= *X0 {
        x -= *X0;
        first_an = *A0;
    } else if x >= *X1 {
        x -= *X1;
        first_an = *A1;
    } else {
        first_an = from_i(1); // 1 with no decimals
    }

    x *= from_i(100); // to 20-decimal fixed point

    let mut product = *ONE_20;
    if x >= *X2 { x -= *X2; product = (product * *A2) / *ONE_20; }
    if x >= *X3 { x -= *X3; product = (product * *A3) / *ONE_20; }
    if x >= *X4 { x -= *X4; product = (product * *A4) / *ONE_20; }
    if x >= *X5 { x -= *X5; product = (product * *A5) / *ONE_20; }
    if x >= *X6 { x -= *X6; product = (product * *A6) / *ONE_20; }
    if x >= *X7 { x -= *X7; product = (product * *A7) / *ONE_20; }
    if x >= *X8 { x -= *X8; product = (product * *A8) / *ONE_20; }
    if x >= *X9 { x -= *X9; product = (product * *A9) / *ONE_20; }
    // x10, x11 not needed: remaining precision already sufficient.

    let mut series_sum = *ONE_20;
    let mut term = x;
    series_sum += term;

    term = ((term * x) / *ONE_20) / from_i(2);  series_sum += term;
    term = ((term * x) / *ONE_20) / from_i(3);  series_sum += term;
    term = ((term * x) / *ONE_20) / from_i(4);  series_sum += term;
    term = ((term * x) / *ONE_20) / from_i(5);  series_sum += term;
    term = ((term * x) / *ONE_20) / from_i(6);  series_sum += term;
    term = ((term * x) / *ONE_20) / from_i(7);  series_sum += term;
    term = ((term * x) / *ONE_20) / from_i(8);  series_sum += term;
    term = ((term * x) / *ONE_20) / from_i(9);  series_sum += term;
    term = ((term * x) / *ONE_20) / from_i(10); series_sum += term;
    term = ((term * x) / *ONE_20) / from_i(11); series_sum += term;
    term = ((term * x) / *ONE_20) / from_i(12); series_sum += term;

    Ok((((product * series_sum) / *ONE_20) * first_an) / from_i(100))
}

/// Natural logarithm `ln(a)`, signed 18-decimal fixed point argument.
pub fn ln(a: I256) -> Result<I256> {
    if a <= I256::ZERO {
        return Err(MathError::OutOfBounds);
    }
    if *LN_36_LOWER_BOUND < a && a < *LN_36_UPPER_BOUND {
        Ok(ln_36(a) / *ONE_18)
    } else {
        Ok(ln_internal(a))
    }
}

/// Internal `ln(a)` (the `_ln` private function).
fn ln_internal(mut a: I256) -> I256 {
    if a < *ONE_18 {
        // ln(a) = -ln(1/a) ; fixed point division multiplies by ONE_18.
        return -ln_internal((*ONE_18 * *ONE_18) / a);
    }

    let mut sum = I256::ZERO;
    if a >= *A0 * *ONE_18 {
        a /= *A0; // integer (not fixed point) division
        sum += *X0;
    }
    if a >= *A1 * *ONE_18 {
        a /= *A1;
        sum += *X1;
    }

    sum *= from_i(100);
    a *= from_i(100);

    if a >= *A2  { a = (a * *ONE_20) / *A2;  sum += *X2; }
    if a >= *A3  { a = (a * *ONE_20) / *A3;  sum += *X3; }
    if a >= *A4  { a = (a * *ONE_20) / *A4;  sum += *X4; }
    if a >= *A5  { a = (a * *ONE_20) / *A5;  sum += *X5; }
    if a >= *A6  { a = (a * *ONE_20) / *A6;  sum += *X6; }
    if a >= *A7  { a = (a * *ONE_20) / *A7;  sum += *X7; }
    if a >= *A8  { a = (a * *ONE_20) / *A8;  sum += *X8; }
    if a >= *A9  { a = (a * *ONE_20) / *A9;  sum += *X9; }
    if a >= *A10 { a = (a * *ONE_20) / *A10; sum += *X10; }
    if a >= *A11 { a = (a * *ONE_20) / *A11; sum += *X11; }

    let z = ((a - *ONE_20) * *ONE_20) / (a + *ONE_20);
    let z_squared = (z * z) / *ONE_20;

    let mut num = z;
    let mut series_sum = num;
    num = (num * z_squared) / *ONE_20; series_sum += num / from_i(3);
    num = (num * z_squared) / *ONE_20; series_sum += num / from_i(5);
    num = (num * z_squared) / *ONE_20; series_sum += num / from_i(7);
    num = (num * z_squared) / *ONE_20; series_sum += num / from_i(9);
    num = (num * z_squared) / *ONE_20; series_sum += num / from_i(11);

    series_sum *= from_i(2);

    (sum + series_sum) / from_i(100)
}

/// High precision (36 decimal) `ln(x)` for x near one (the `_ln_36` function).
fn ln_36(mut x: I256) -> I256 {
    x *= *ONE_18; // to 36-decimal fixed point

    let z = ((x - *ONE_36) * *ONE_36) / (x + *ONE_36);
    let z_squared = (z * z) / *ONE_36;

    let mut num = z;
    let mut series_sum = num;
    num = (num * z_squared) / *ONE_36; series_sum += num / from_i(3);
    num = (num * z_squared) / *ONE_36; series_sum += num / from_i(5);
    num = (num * z_squared) / *ONE_36; series_sum += num / from_i(7);
    num = (num * z_squared) / *ONE_36; series_sum += num / from_i(9);
    num = (num * z_squared) / *ONE_36; series_sum += num / from_i(11);
    num = (num * z_squared) / *ONE_36; series_sum += num / from_i(13);
    num = (num * z_squared) / *ONE_36; series_sum += num / from_i(15);

    series_sum * from_i(2)
}

// ===========================================================================
// FixedPoint  (uint256, 18 decimals)
// ===========================================================================

#[inline]
fn one_18_u() -> U256 {
    U256::from(1_000_000_000_000_000_000u128)
}
/// FixedPoint.ONE
pub fn one() -> U256 {
    one_18_u()
}
#[inline]
fn two_u() -> U256 {
    U256::from(2_000_000_000_000_000_000u128)
}
#[inline]
fn four_u() -> U256 {
    U256::from(4_000_000_000_000_000_000u128)
}
// uint256 internal constant MAX_POW_RELATIVE_ERROR = 10000; // 10^(-14)
#[inline]
fn max_pow_relative_error() -> U256 {
    U256::from(10_000u64)
}

pub fn add(a: U256, b: U256) -> Result<U256> {
    a.checked_add(b).ok_or(MathError::AddOverflow)
}

pub fn sub(a: U256, b: U256) -> Result<U256> {
    if b > a {
        return Err(MathError::SubOverflow);
    }
    Ok(a - b)
}

/// `a * b` rounding DOWN.
pub fn mul_down(a: U256, b: U256) -> Result<U256> {
    let product = a.checked_mul(b).ok_or(MathError::MulOverflow)?;
    Ok(product / one_18_u())
}

/// `a * b` rounding UP.
pub fn mul_up(a: U256, b: U256) -> Result<U256> {
    let product = a.checked_mul(b).ok_or(MathError::MulOverflow)?;
    if product.is_zero() {
        Ok(U256::ZERO)
    } else {
        Ok((product - U256::from(1u8)) / one_18_u() + U256::from(1u8))
    }
}

/// `a / b` rounding DOWN.
pub fn div_down(a: U256, b: U256) -> Result<U256> {
    if b.is_zero() {
        return Err(MathError::ZeroDivision);
    }
    let a_inflated = a.checked_mul(one_18_u()).ok_or(MathError::DivInternal)?;
    Ok(a_inflated / b)
}

/// `a / b` rounding UP.
pub fn div_up(a: U256, b: U256) -> Result<U256> {
    if b.is_zero() {
        return Err(MathError::ZeroDivision);
    }
    let a_inflated = a.checked_mul(one_18_u()).ok_or(MathError::DivInternal)?;
    if a_inflated.is_zero() {
        Ok(U256::ZERO)
    } else {
        Ok((a_inflated - U256::from(1u8)) / b + U256::from(1u8))
    }
}

/// `x^y` rounding DOWN (result guaranteed not above true value).
pub fn pow_down(x: U256, y: U256) -> Result<U256> {
    if y == one_18_u() {
        Ok(x)
    } else if y == two_u() {
        mul_down(x, x)
    } else if y == four_u() {
        let square = mul_down(x, x)?;
        mul_down(square, square)
    } else {
        let raw = pow(x, y)?;
        let max_error = add(mul_up(raw, max_pow_relative_error())?, U256::from(1u8))?;
        if raw < max_error {
            Ok(U256::ZERO)
        } else {
            sub(raw, max_error)
        }
    }
}

/// `x^y` rounding UP (result guaranteed not below true value).
pub fn pow_up(x: U256, y: U256) -> Result<U256> {
    if y == one_18_u() {
        Ok(x)
    } else if y == two_u() {
        mul_up(x, x)
    } else if y == four_u() {
        let square = mul_up(x, x)?;
        mul_up(square, square)
    } else {
        let raw = pow(x, y)?;
        let max_error = add(mul_up(raw, max_pow_relative_error())?, U256::from(1u8))?;
        add(raw, max_error)
    }
}

/// `1 - x`, capped at 0 (the `complement` function).
pub fn complement(x: U256) -> U256 {
    if x < one_18_u() {
        one_18_u() - x
    } else {
        U256::ZERO
    }
}

// ===========================================================================
// WeightedMath
// ===========================================================================

// uint256 internal constant _MAX_IN_RATIO = 0.3e18;
#[inline]
fn max_in_ratio() -> U256 {
    U256::from(300_000_000_000_000_000u128)
}

/// `_calcOutGivenIn`: tokens out for `amount_in` tokens in.
///
/// Formula: `aO = bO * (1 - (bI / (bI + aI)) ^ (wI / wO))`. All inputs are
/// 18-decimal upscaled. Rounding directions are chosen so the result rounds
/// DOWN (against the trader), exactly as the contract does.
pub fn calc_out_given_in(
    balance_in: U256,
    weight_in: U256,
    balance_out: U256,
    weight_out: U256,
    amount_in: U256,
) -> Result<U256> {
    if amount_in > mul_down(balance_in, max_in_ratio())? {
        return Err(MathError::MaxInRatio);
    }

    let denominator = add(balance_in, amount_in)?;
    let base = div_up(balance_in, denominator)?;
    let exponent = div_down(weight_in, weight_out)?;
    let power = pow_up(base, exponent)?;

    mul_down(balance_out, complement(power))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> U256 {
        U256::from_str_radix(s, 10).unwrap()
    }

    // ---- LogExpMath::pow vectors (cross-validated by two independent ports) ----
    #[test]
    fn pow_short_circuits() {
        assert_eq!(pow(u("1000000000000000000"), U256::ZERO).unwrap(), one());
        assert_eq!(pow(u("2000000000000000000"), U256::ZERO).unwrap(), one());
        assert_eq!(pow(U256::ZERO, u("5000000000000000000")).unwrap(), U256::ZERO);
    }

    #[test]
    fn pow_vectors() {
        // 2^1 -> known LogExpMath truncation (1999999999999999998)
        assert_eq!(
            pow(u("2000000000000000000"), u("1000000000000000000")).unwrap(),
            u("1999999999999999998")
        );
        // 2^0.5 (sqrt 2)
        assert_eq!(
            pow(u("2000000000000000000"), u("500000000000000000")).unwrap(),
            u("1414213562373095047")
        );
        // 2^2
        assert_eq!(
            pow(u("2000000000000000000"), u("2000000000000000000")).unwrap(),
            u("3999999999999999996")
        );
        // 0.5^2
        assert_eq!(
            pow(u("500000000000000000"), u("2000000000000000000")).unwrap(),
            u("250000000000000000")
        );
        // 1.5^0.8
        assert_eq!(
            pow(u("1500000000000000000"), u("800000000000000000")).unwrap(),
            u("1383161867222591646")
        );
        // 3^(1/3)
        assert_eq!(
            pow(u("3000000000000000000"), u("333333333333333333")).unwrap(),
            u("1442249570307408380")
        );
        // 1.05^3 (ln_36 high-precision path)
        assert_eq!(
            pow(u("1050000000000000000"), u("3000000000000000000")).unwrap(),
            u("1157624999999999999")
        );
    }

    #[test]
    fn pow_up_down_bracket_raw() {
        let x = u("1500000000000000000");
        let y = u("800000000000000000");
        let raw = pow(x, y).unwrap();
        let down = pow_down(x, y).unwrap();
        let up = pow_up(x, y).unwrap();
        assert!(down <= raw && raw <= up, "down={down} raw={raw} up={up}");
        assert_eq!(down, u("1383161867222577813"));
        assert_eq!(up, u("1383161867222605479"));
    }

    // ---- WeightedMath::calcOutGivenIn official vectors ----
    #[test]
    fn calc_out_given_in_vectors() {
        // 50/50, exp=1.0 (powUp identity). out = 100*(1-100/110) = 9.0909e18.
        assert_eq!(
            calc_out_given_in(
                u("100000000000000000000"),
                u("500000000000000000"),
                u("100000000000000000000"),
                u("500000000000000000"),
                u("10000000000000000000"),
            )
            .unwrap(),
            u("9090909090909090900")
        );
        // 80/20, exp=4.0 (powUp FOUR shortcut)
        assert_eq!(
            calc_out_given_in(
                u("2000000000000000000000"),
                u("800000000000000000"),
                u("1000000000000000000000"),
                u("200000000000000000"),
                u("100000000000000000000"),
            )
            .unwrap(),
            u("177297525208118015000")
        );
        // 60/40, exp=1.5 (LogExpMath)
        assert_eq!(
            calc_out_given_in(
                u("1000000000000000000000"),
                u("600000000000000000"),
                u("1000000000000000000000"),
                u("400000000000000000"),
                u("50000000000000000000"),
            )
            .unwrap(),
            u("70571359096625771000")
        );
        // 20/80, exp=0.25 (LogExpMath)
        assert_eq!(
            calc_out_given_in(
                u("500000000000000000000"),
                u("200000000000000000"),
                u("500000000000000000000"),
                u("800000000000000000"),
                u("25000000000000000000"),
            )
            .unwrap(),
            u("6061726288458007500")
        );
    }

    #[test]
    fn calc_out_given_in_rejects_max_in_ratio() {
        // amount_in > 30% of balance_in must revert.
        assert_eq!(
            calc_out_given_in(
                u("100000000000000000000"),
                u("500000000000000000"),
                u("100000000000000000000"),
                u("500000000000000000"),
                u("40000000000000000000"),
            ),
            Err(MathError::MaxInRatio)
        );
    }
}
