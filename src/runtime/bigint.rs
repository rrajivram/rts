//! BigInt arithmetic. As in rjs, a BigInt is an `i128` rather than arbitrary
//! precision: results that don't fit raise a RangeError ("Maximum BigInt size
//! exceeded") instead of growing. No heap, no GC: a typed `bigint` lives in a
//! pair of number slots (see `bytecode.rs`); only untyped ones are boxed.

use std::cmp::Ordering;

use crate::hir::{CmpOp, NumOp};

/// Why a BigInt operation failed; the VM turns it into a RangeError or TypeError.
#[derive(Debug, Clone, Copy)]
pub enum BigIntError {
    Range(&'static str),
    Type(&'static str),
}

pub const MIX: BigIntError = BigIntError::Type("Cannot mix BigInt and other types, use explicit conversions");
pub const TO_NUMBER: BigIntError = BigIntError::Type("Cannot convert a BigInt value to a number");
const OVERFLOW: BigIntError = BigIntError::Range("Maximum BigInt size exceeded");
const DIVISION_BY_ZERO: BigIntError = BigIntError::Range("Division by zero");

/// `a op b` for two BigInts.
pub fn arithmetic(op: NumOp, a: i128, b: i128) -> Result<i128, BigIntError> {
    match op {
        NumOp::Add => a.checked_add(b).ok_or(OVERFLOW),
        NumOp::Sub => a.checked_sub(b).ok_or(OVERFLOW),
        NumOp::Mul => a.checked_mul(b).ok_or(OVERFLOW),
        // Both truncate, so the result takes the dividend's sign: `-7n % 3n` is -1n.
        NumOp::Div => match b {
            0 => Err(DIVISION_BY_ZERO),
            _ => a.checked_div(b).ok_or(OVERFLOW),
        },
        NumOp::Mod => match b {
            0 => Err(DIVISION_BY_ZERO),
            _ => Ok(a.wrapping_rem(b)),
        },
        NumOp::Exp => pow(a, b),
        NumOp::BitAnd => Ok(a & b),
        NumOp::BitOr => Ok(a | b),
        NumOp::BitXor => Ok(a ^ b),
        NumOp::Shl => shl(a, b),
        NumOp::Shr => shl(a, b.checked_neg().unwrap_or(i128::MAX)),
        NumOp::UShr => Err(BigIntError::Type("BigInts have no unsigned right shift, use >> instead")),
    }
}

/// `-a`
pub fn negate(a: i128) -> Result<i128, BigIntError> {
    a.checked_neg().ok_or(OVERFLOW)
}

/// `a << by`; a negative `by` shifts right.
fn shl(a: i128, by: i128) -> Result<i128, BigIntError> {
    if by < 0 {
        let by = by.unsigned_abs();
        return Ok(if by >= 128 {
            if a < 0 { -1 } else { 0 }
        } else {
            a >> by
        });
    }
    if a == 0 {
        return Ok(0);
    }
    if by >= 128 {
        return Err(OVERFLOW);
    }
    let shifted = a << by;
    if shifted >> by == a { Ok(shifted) } else { Err(OVERFLOW) }
}

fn pow(base: i128, exp: i128) -> Result<i128, BigIntError> {
    if exp < 0 {
        return Err(BigIntError::Range("Exponent must be non-negative"));
    }
    match base {
        0 | 1 => Ok(if exp == 0 { 1 } else { base }),
        -1 => Ok(if exp % 2 == 0 { 1 } else { -1 }),
        _ => u32::try_from(exp).ok().and_then(|e| base.checked_pow(e)).ok_or(OVERFLOW),
    }
}

/// `a op b` for two BigInts.
pub fn compare(op: CmpOp, a: i128, b: i128) -> bool {
    match op {
        CmpOp::Lt => a < b,
        CmpOp::Le => a <= b,
        CmpOp::Gt => a > b,
        CmpOp::Ge => a >= b,
        CmpOp::Eq => a == b,
        CmpOp::Ne => a != b,
    }
}

/// Compare a BigInt with a Number by exact mathematical value. `None` if the
/// Number is NaN.
pub fn cmp_number(x: i128, y: f64) -> Option<Ordering> {
    let limit = 2f64.powi(127);
    if y.is_nan() {
        None
    } else if y >= limit {
        Some(Ordering::Less)
    } else if y < -limit {
        Some(Ordering::Greater)
    } else {
        let whole = y.trunc();
        let fraction = if y > whole {
            Ordering::Less
        } else if y < whole {
            Ordering::Greater
        } else {
            Ordering::Equal
        };
        Some(x.cmp(&(whole as i128)).then(fraction))
    }
}

/// StringToBigInt: `" 42 "` is 42n, `""` is 0n, `"0x1F"` is 31n. `None` when
/// the string isn't an integer (or doesn't fit in an `i128`).
pub fn parse(s: &str) -> Option<i128> {
    let s = s.trim_matches(|c: char| c.is_whitespace() || c == '\u{FEFF}');
    if s.is_empty() {
        return Some(0);
    }
    for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        if s.len() > 2 && s[..2].eq_ignore_ascii_case(prefix) {
            let digits = &s[2..];
            return if digits.chars().all(|c| c.is_digit(radix)) { i128::from_str_radix(digits, radix).ok() } else { None };
        }
    }
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// `BigInt(n)` for a Number: only integers convert.
pub fn from_number(n: f64) -> Result<i128, BigIntError> {
    if n.fract() != 0.0 || !n.is_finite() {
        return Err(BigIntError::Range("The number cannot be converted to a BigInt because it is not an integer"));
    }
    if n.abs() >= 2f64.powi(127) {
        return Err(OVERFLOW);
    }
    Ok(n as i128)
}

/// `n.toString(radix)`
pub fn to_string_radix(n: i128, radix: u32) -> String {
    if radix == 10 {
        return n.to_string();
    }
    let mut digits = Vec::new();
    let mut m = n.unsigned_abs();
    loop {
        digits.push(std::char::from_digit((m % radix as u128) as u32, radix).unwrap());
        m /= radix as u128;
        if m == 0 {
            break;
        }
    }
    if n < 0 {
        digits.push('-');
    }
    digits.iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overflow_is_a_range_error() {
        assert!(matches!(arithmetic(NumOp::Mul, i128::MAX, 2), Err(BigIntError::Range(_))));
        assert!(matches!(arithmetic(NumOp::Div, 1, 0), Err(BigIntError::Range(_))));
        assert_eq!(arithmetic(NumOp::Mod, -7, 3).unwrap(), -1);
        assert_eq!(arithmetic(NumOp::Shr, -9, 1).unwrap(), -5);
        assert_eq!(arithmetic(NumOp::Exp, 2, 100).unwrap(), 1 << 100);
    }

    #[test]
    fn parsing_and_printing() {
        assert_eq!(parse(" 0x1F "), Some(31));
        assert_eq!(parse("-12"), Some(-12));
        assert_eq!(parse("1.5"), None);
        assert_eq!(to_string_radix(-255, 16), "-ff");
        assert_eq!(cmp_number(2, 2.5), Some(Ordering::Less));
    }
}
