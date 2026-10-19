//! Floats formatted the way Python's `repr(float)` does, because the legacy API is
//! written in Python and clients see its spelling (`8.3549e-05`, `19.0`, `1e+16`).
//! serde_json would write `8.3549e-5` and `19`.

use serde::{Serialize, Serializer};

/// Python `repr(float)`: shortest round-trip digits; fixed notation when the decimal
/// exponent is in [-4, 16), otherwise scientific with a signed two-digit exponent.
pub fn repr(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0.0" } else { "0.0" }.into();
    }
    // Rust's LowerExp prints the shortest round-trip digits, e.g. "8.3549e-5".
    let sci = format!("{f:e}");
    let (mantissa, exp) = sci.split_once('e').expect("LowerExp has an exponent");
    let exp: i32 = exp.parse().expect("integer exponent");
    let (sign, mantissa) = match mantissa.strip_prefix('-') {
        Some(m) => ("-", m),
        None => ("", mantissa),
    };
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();

    if (-4..16).contains(&exp) {
        let point = exp + 1; // digits before the decimal point
        let s = if point <= 0 {
            format!("0.{}{}", "0".repeat((-point) as usize), digits)
        } else if point as usize >= digits.len() {
            format!("{}{}.0", digits, "0".repeat(point as usize - digits.len()))
        } else {
            let (a, b) = digits.split_at(point as usize);
            format!("{a}.{b}")
        };
        format!("{sign}{s}")
    } else {
        let m = if digits.len() == 1 {
            digits
        } else {
            format!("{}.{}", &digits[..1], &digits[1..])
        };
        let esign = if exp < 0 { '-' } else { '+' };
        format!("{sign}{m}e{esign}{:02}", exp.abs())
    }
}

/// A float that serializes as a JSON number spelled like Python's `repr`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PyFloat(pub f64);

impl Serialize for PyFloat {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if !self.0.is_finite() {
            return s.serialize_none();
        }
        let raw = serde_json::value::RawValue::from_string(repr(self.0))
            .map_err(serde::ser::Error::custom)?;
        raw.serialize(s)
    }
}

#[cfg(test)]
mod tests {
    use super::repr;

    #[test]
    fn matches_python_repr() {
        // Expected values produced by CPython 3.
        let cases = [
            (8.3549e-05, "8.3549e-05"),
            (0.00015, "0.00015"),
            (6.9e-05, "6.9e-05"),
            (0.00996565344948533, "0.00996565344948533"),
            (19.51236555, "19.51236555"),
            (19.0, "19.0"),
            (3133943541.8449254, "3133943541.8449254"),
            (4982826724.742665, "4982826724.742665"),
            (1e16, "1e+16"),
            (1234567890123456.0, "1234567890123456.0"),
            (0.0001, "0.0001"),
            (1e-05, "1e-05"),
            (-2.5, "-2.5"),
            (-30.35, "-30.35"),
            (0.0, "0.0"),
            (1.5e300, "1.5e+300"),
        ];
        for (f, want) in cases {
            assert_eq!(repr(f), want, "{f}");
        }
    }
}
