//! Amount handling — exact decimal money (audit M-01).
//!
//! f64 cannot represent decimal amounts exactly (0.05 → 0.0500000…0027).
//! For an audit-grade settlement layer, amounts are expressed in
//! **minor units** (e.g. 6 decimals, like USDC) as integers, matching
//! the on-chain contract (`GapEscrow` uses `amount * 1_000_000`).

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};

/// Decimal places used across GAP settlement (6, matching USDC/EURC).
pub const DECIMALS: u32 = 6;

/// The scale factor: 1 unit = 1_000_000 minor units.
pub const SCALE: u128 = 1_000_000;

/// An exact monetary amount in minor units (integer).
///
/// Serializes as the decimal string ("10.000000") on the wire so JSON
/// round-trips are lossless and human-readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Amount(u128);

impl Amount {
    /// Zero.
    pub const ZERO: Amount = Amount(0);

    /// Build from minor units directly.
    pub fn from_minor(units: u128) -> Self {
        Self(units)
    }

    /// Parse from a decimal string ("10.5", "0.05", "10"). Rejects
    /// more than 6 decimal places and negative values.
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.is_empty() || s.starts_with('-') {
            return Err(Error::Other(format!("invalid amount: {s}")));
        }
        let (int_part, frac_part) = match s.split_once('.') {
            Some((i, f)) => (i, f),
            None => (s, ""),
        };
        if int_part.is_empty() && frac_part.is_empty() {
            return Err(Error::Other(format!("invalid amount: {s}")));
        }
        if !int_part.chars().all(|c| c.is_ascii_digit())
            || !frac_part.chars().all(|c| c.is_ascii_digit())
        {
            return Err(Error::Other(format!("invalid amount: {s}")));
        }
        if frac_part.len() > DECIMALS as usize {
            return Err(Error::Other(format!(
                "amount has more than {DECIMALS} decimal places: {s}"
            )));
        }
        let int: u128 = if int_part.is_empty() {
            0
        } else {
            int_part
                .parse()
                .map_err(|_| Error::Other(format!("amount out of range: {s}")))?
        };
        let frac: u128 = if frac_part.is_empty() {
            0
        } else {
            let pad = DECIMALS as usize;
            let padded = format!("{frac_part:0<pad$}");
            padded
                .parse()
                .map_err(|_| Error::Other(format!("amount out of range: {s}")))?
        };
        Ok(Self(
            int
                .checked_mul(SCALE)
                .and_then(|v| v.checked_add(frac))
                .ok_or_else(|| Error::Other(format!("amount overflow: {s}")))?,
        ))
    }

    /// Convert from f64 with rounding to the nearest minor unit.
    /// This is a LAST-RESORT path for legacy callers; new code should
    /// use [`Amount::parse`] or [`Amount::from_minor`].
    pub fn from_f64_rounding(value: f64) -> Self {
        let scaled = (value * SCALE as f64).round();
        Self(scaled.max(0.0) as u128)
    }

    /// The amount in minor units.
    pub fn minor_units(&self) -> u128 {
        self.0
    }

    /// The amount as a decimal string ("10.05").
    pub fn to_string_decimal(&self) -> String {
        let int = self.0 / SCALE;
        let frac = self.0 % SCALE;
        format!("{int}.{frac:06}")
    }

    /// Addition, checked.
    pub fn checked_add(&self, other: Amount) -> Result<Amount> {
        self.0
            .checked_add(other.0)
            .map(Amount)
            .ok_or_else(|| Error::Other("amount overflow".into()))
    }

    /// Subtraction, checked (errors if result would be negative).
    pub fn checked_sub(&self, other: Amount) -> Result<Amount> {
        self.0
            .checked_sub(other.0)
            .map(Amount)
            .ok_or_else(|| Error::Other("amount underflow".into()))
    }

    /// Is this amount zero?
    pub fn is_zero(&self) -> bool {
        self.0 == 0
    }
}

impl Serialize for Amount {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string_decimal())
    }
}

impl<'de> Deserialize<'de> for Amount {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Amount::parse(&s).map_err(serde::de::Error::custom)
    }
}

impl std::fmt::Display for Amount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_string_decimal())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_roundtrips() {
        assert_eq!(Amount::parse("0.05").unwrap().to_string_decimal(), "0.050000");
        assert_eq!(Amount::parse("10").unwrap().to_string_decimal(), "10.000000");
        assert_eq!(Amount::parse("10.5").unwrap().to_string_decimal(), "10.500000");
        assert_eq!(Amount::parse("0.000001").unwrap().minor_units(), 1);
        assert_eq!(Amount::parse("123456789.123456").unwrap().minor_units(), 123456789123456);
    }

    #[test]
    fn parse_rejects_invalid() {
        assert!(Amount::parse("").is_err());
        assert!(Amount::parse("-5").is_err());
        assert!(Amount::parse("abc").is_err());
        assert!(Amount::parse("1.2345678").is_err()); // 7 decimals
        assert!(Amount::parse(".").is_err());
    }

    #[test]
    fn arithmetic_is_exact() {
        let a = Amount::parse("0.05").unwrap();
        let b = Amount::parse("0.10").unwrap();
        assert_eq!(a.checked_add(b).unwrap(), Amount::parse("0.15").unwrap());
        assert_eq!(b.checked_sub(a).unwrap(), Amount::parse("0.05").unwrap());
        assert!(a.checked_sub(b).is_err()); // underflow
    }

    #[test]
    fn json_roundtrip_is_lossless() {
        let a = Amount::parse("9.99").unwrap();
        let wire = serde_json::to_string(&a).unwrap();
        assert_eq!(wire, "\"9.990000\"");
        let back: Amount = serde_json::from_str(&wire).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn f64_rounding_matches_contract() {
        // The on-chain contract computes amount * 1_000_000.
        let a = Amount::from_f64_rounding(10.0);
        assert_eq!(a.minor_units(), 10 * SCALE);
        let b = Amount::from_f64_rounding(0.05);
        assert_eq!(b.minor_units(), 50_000);
    }
}
