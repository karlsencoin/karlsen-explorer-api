//! Compact `bits` -> difficulty, following the Kaspa convention:
//! difficulty = max_target / target, where max_target = 2^255 - 1.
//!
//! NOTE: must be verified against live `rest-v2` output (golden capture)
//! before cut-over; see docs/VERIFY.md.

/// Expand compact bits into an `f64` target (mantissa * 256^(exp-3)).
pub fn target_from_bits(bits: u32) -> f64 {
    let exp = (bits >> 24) as i32;
    let mant = (bits & 0x007f_ffff) as f64;
    if exp <= 3 {
        mant / 256f64.powi(3 - exp)
    } else {
        mant * 256f64.powi(exp - 3)
    }
}

pub fn difficulty_from_bits(bits: u32) -> f64 {
    let target = target_from_bits(bits);
    if target <= 0.0 {
        return 0.0;
    }
    2f64.powi(255) / target
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_target_is_zero_difficulty() {
        assert_eq!(difficulty_from_bits(0), 0.0);
    }

    #[test]
    fn harder_target_gives_higher_difficulty() {
        assert!(difficulty_from_bits(0x1b00_ffff) > difficulty_from_bits(0x1d00_ffff));
    }
}
