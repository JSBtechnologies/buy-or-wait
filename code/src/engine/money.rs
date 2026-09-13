//! Exact money arithmetic and every money formatting/rounding rule in one place (PLAN.md
//! risk table: "keep rounding in one function").
//!
//! Amounts are held in 1/10,000 of the currency unit: CSV amounts and rates carry at most two
//! decimals, so every converted amount is exact and RULES S2.2 ("do not round converted
//! amounts, round only at output") holds. Rounding to cents happens only when formatting.

use std::fmt;
use std::iter::Sum;
use std::ops::{Add, AddAssign, Neg, Sub, SubAssign};

use serde::{Deserialize, Serialize};

/// Internal units per currency unit.
pub const SCALE: i64 = 10_000;
/// Internal units per cent.
const CENT: i64 = SCALE / 100;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Money(pub i64);

impl Money {
    pub const ZERO: Money = Money(0);

    /// From a CSV float (at most a few decimals).
    pub fn from_f64(v: f64) -> Money {
        Money((v * SCALE as f64).round() as i64)
    }

    pub fn from_units(units: i64) -> Money {
        Money(units * SCALE)
    }

    pub fn to_f64(self) -> f64 {
        self.0 as f64 / SCALE as f64
    }

    pub fn abs(self) -> Money {
        Money(self.0.abs())
    }

    pub fn max(self, other: Money) -> Money {
        if self >= other { self } else { other }
    }

    pub fn min(self, other: Money) -> Money {
        if self <= other { self } else { other }
    }

    /// Multiply by an exact decimal rate, rounding half away from zero to the internal unit.
    pub fn convert(self, rate: &DecimalRate) -> Money {
        Money(div_round(self.0 as i128 * rate.mantissa, 10i128.pow(rate.scale)) as i64)
    }

    /// Round half away from zero to whole cents.
    pub fn round_to_cent(self) -> Money {
        Money(div_round(self.0 as i128, CENT as i128) as i64 * CENT)
    }

    /// Round toward negative infinity to whole cents.
    pub fn floor_to_cent(self) -> Money {
        Money(self.0.div_euclid(CENT) * CENT)
    }

    fn cents(self) -> i64 {
        self.round_to_cent().0 / CENT
    }

    /// Plain CSV form, rounded to 2 dp with trailing zeros trimmed: `25256`, `603.3`, `87170.56`.
    pub fn fmt_plain(self) -> String {
        let s = fixed2(self.cents());
        if let Some(stripped) = s.strip_suffix(".00") {
            return stripped.to_string();
        }
        if s.ends_with('0') {
            return s[..s.len() - 1].to_string();
        }
        s
    }

    /// Payment-plan / reduce_to form: whole amounts bare, otherwise exactly 2 dp
    /// (`25256`, `620.40`, `23.50`).
    pub fn fmt_plan(self) -> String {
        let c = self.cents();
        if c % 100 == 0 { format!("{}", c / 100) } else { fixed2(c) }
    }

    /// Explanation form with thousands separators: `25,256`, `620.40`, `15,952,906.67`.
    pub fn fmt_grouped(self) -> String {
        let c = self.cents();
        let abs = c.unsigned_abs();
        let digits = (abs / 100).to_string();
        let mut grouped = String::new();
        for (i, ch) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i) % 3 == 0 {
                grouped.push(',');
            }
            grouped.push(ch);
        }
        let sign = if c < 0 { "-" } else { "" };
        match abs % 100 {
            0 => format!("{sign}{grouped}"),
            frac => format!("{sign}{grouped}.{frac:02}"),
        }
    }
}

fn fixed2(cents: i64) -> String {
    let abs = cents.unsigned_abs();
    format!("{}{}.{:02}", if cents < 0 { "-" } else { "" }, abs / 100, abs % 100)
}

fn div_round(num: i128, den: i128) -> i128 {
    let q = num / den;
    let r = num % den;
    if 2 * r.abs() >= den { q + num.signum() } else { q }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.fmt_plain())
    }
}

impl Add for Money {
    type Output = Money;
    fn add(self, o: Money) -> Money {
        Money(self.0 + o.0)
    }
}
impl Sub for Money {
    type Output = Money;
    fn sub(self, o: Money) -> Money {
        Money(self.0 - o.0)
    }
}
impl Neg for Money {
    type Output = Money;
    fn neg(self) -> Money {
        Money(-self.0)
    }
}
impl AddAssign for Money {
    fn add_assign(&mut self, o: Money) {
        self.0 += o.0;
    }
}
impl SubAssign for Money {
    fn sub_assign(&mut self, o: Money) {
        self.0 -= o.0;
    }
}
impl Sum for Money {
    fn sum<I: Iterator<Item = Money>>(iter: I) -> Money {
        Money(iter.map(|c| c.0).sum())
    }
}

/// An exchange rate held as an exact decimal (`mantissa / 10^scale`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecimalRate {
    pub mantissa: i128,
    pub scale: u32,
}

impl DecimalRate {
    /// Parse the shortest round-trip decimal form of the CSV float (`0.92` -> 92 / 10^2).
    pub fn from_f64(v: f64) -> DecimalRate {
        DecimalRate::parse(&format!("{v}")).expect("finite rate")
    }

    pub fn parse(s: &str) -> Option<DecimalRate> {
        let s = s.trim();
        let (int, frac) = s.split_once('.').unwrap_or((s, ""));
        let mantissa: i128 = format!("{int}{frac}").parse().ok()?;
        Some(DecimalRate { mantissa, scale: frac.len() as u32 })
    }

    /// 1/rate to 12 decimal places, for pairs only supplied in the opposite direction.
    pub fn inverse(&self) -> DecimalRate {
        const S: u32 = 12;
        let num = 10i128.pow(self.scale + S);
        DecimalRate { mantissa: (num + self.mantissa / 2) / self.mantissa, scale: S }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(Money::from_f64(25256.0).fmt_plain(), "25256");
        assert_eq!(Money::from_f64(603.3).fmt_plain(), "603.3");
        assert_eq!(Money::from_f64(17229139.2).fmt_plain(), "17229139.2");
        assert_eq!(Money::from_f64(284.565).fmt_plain(), "284.57");
        assert_eq!(Money::from_f64(620.4).fmt_plan(), "620.40");
        assert_eq!(Money::from_f64(25256.0).fmt_plan(), "25256");
        assert_eq!(Money::from_f64(15952906.67).fmt_grouped(), "15,952,906.67");
        assert_eq!(Money::from_f64(620.4).fmt_grouped(), "620.40");
        assert_eq!(Money::from_f64(122400.0).fmt_grouped(), "122,400");
    }

    #[test]
    fn converts_exactly() {
        let r = DecimalRate::from_f64(15833.33);
        assert_eq!(Money::from_f64(1800.0).convert(&r), Money::from_f64(28499994.0));
        let r = DecimalRate::from_f64(0.92);
        assert_eq!(Money::from_f64(10.01).convert(&r), Money(92092));
    }
}
