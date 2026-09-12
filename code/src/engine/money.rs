//! Exact money arithmetic in minor units (hundredths) and every money formatting/rounding
//! rule in one place (PLAN.md risk table: "keep rounding in one function").

use std::fmt;
use std::iter::Sum;
use std::ops::{Add, AddAssign, Neg, Sub, SubAssign};

use serde::{Deserialize, Serialize};

/// An amount in hundredths of the currency unit. All engine arithmetic is integer, so the
/// forecast is exact and deterministic; floats only exist at the CSV boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Cents(pub i64);

impl Cents {
    pub const ZERO: Cents = Cents(0);

    /// From a CSV float that carries at most two decimals.
    pub fn from_f64(v: f64) -> Cents {
        Cents((v * 100.0).round() as i64)
    }

    pub fn from_units(units: i64) -> Cents {
        Cents(units * 100)
    }

    pub fn is_negative(self) -> bool {
        self.0 < 0
    }

    pub fn max(self, other: Cents) -> Cents {
        if self >= other { self } else { other }
    }

    pub fn min(self, other: Cents) -> Cents {
        if self <= other { self } else { other }
    }

    pub fn abs(self) -> Cents {
        Cents(self.0.abs())
    }

    pub fn is_whole(self) -> bool {
        self.0 % 100 == 0
    }

    /// Multiply by an exact decimal rate, rounding half away from zero to the cent.
    pub fn convert(self, rate: &DecimalRate) -> Cents {
        let num = self.0 as i128 * rate.mantissa;
        let den = 10i128.pow(rate.scale);
        let q = num / den;
        let r = num % den;
        let q = if 2 * r.abs() >= den { q + num.signum() } else { q };
        Cents(q as i64)
    }

    /// Plain CSV form with trailing zeros trimmed: `25256`, `603.3`, `87170.56`.
    pub fn fmt_plain(self) -> String {
        let s = self.fmt_fixed2();
        if let Some(stripped) = s.strip_suffix(".00") {
            return stripped.to_string();
        }
        if s.contains('.') && s.ends_with('0') {
            return s[..s.len() - 1].to_string();
        }
        s
    }

    /// Payment-plan / reduce_to form: whole amounts bare, otherwise two decimals
    /// (`25256`, `620.40`, `23.50`).
    pub fn fmt_plan(self) -> String {
        if self.is_whole() {
            format!("{}", self.0 / 100)
        } else {
            self.fmt_fixed2()
        }
    }

    /// Explanation form with thousands separators: `25,256`, `620.40`, `15,952,906.67`.
    pub fn fmt_grouped(self) -> String {
        let neg = self.0 < 0;
        let abs = self.0.unsigned_abs();
        let units = abs / 100;
        let frac = abs % 100;
        let digits = units.to_string();
        let mut grouped = String::new();
        for (i, ch) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i) % 3 == 0 {
                grouped.push(',');
            }
            grouped.push(ch);
        }
        let sign = if neg { "-" } else { "" };
        if frac == 0 {
            format!("{sign}{grouped}")
        } else {
            format!("{sign}{grouped}.{frac:02}")
        }
    }

    fn fmt_fixed2(self) -> String {
        let neg = self.0 < 0;
        let abs = self.0.unsigned_abs();
        format!("{}{}.{:02}", if neg { "-" } else { "" }, abs / 100, abs % 100)
    }
}

impl fmt::Display for Cents {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.fmt_plain())
    }
}

impl Add for Cents {
    type Output = Cents;
    fn add(self, o: Cents) -> Cents {
        Cents(self.0 + o.0)
    }
}
impl Sub for Cents {
    type Output = Cents;
    fn sub(self, o: Cents) -> Cents {
        Cents(self.0 - o.0)
    }
}
impl Neg for Cents {
    type Output = Cents;
    fn neg(self) -> Cents {
        Cents(-self.0)
    }
}
impl AddAssign for Cents {
    fn add_assign(&mut self, o: Cents) {
        self.0 += o.0;
    }
}
impl SubAssign for Cents {
    fn sub_assign(&mut self, o: Cents) {
        self.0 -= o.0;
    }
}
impl Sum for Cents {
    fn sum<I: Iterator<Item = Cents>>(iter: I) -> Cents {
        Cents(iter.map(|c| c.0).sum())
    }
}

/// An exchange rate held as an exact decimal (`mantissa / 10^scale`) so conversion of
/// large IDR amounts never picks up float error.
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
        let (int, frac) = match s.split_once('.') {
            Some((i, f)) => (i, f),
            None => (s, ""),
        };
        let digits = format!("{int}{frac}");
        let mantissa: i128 = digits.parse().ok()?;
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
        assert_eq!(Cents::from_f64(25256.0).fmt_plain(), "25256");
        assert_eq!(Cents::from_f64(603.3).fmt_plain(), "603.3");
        assert_eq!(Cents::from_f64(17229139.2).fmt_plain(), "17229139.2");
        assert_eq!(Cents::from_f64(620.4).fmt_plan(), "620.40");
        assert_eq!(Cents::from_f64(25256.0).fmt_plan(), "25256");
        assert_eq!(Cents::from_f64(15952906.67).fmt_grouped(), "15,952,906.67");
        assert_eq!(Cents::from_f64(620.4).fmt_grouped(), "620.40");
        assert_eq!(Cents::from_f64(122400.0).fmt_grouped(), "122,400");
    }

    #[test]
    fn converts_exactly() {
        let r = DecimalRate::from_f64(16250.5);
        assert_eq!(Cents::from_f64(1800.0).convert(&r), Cents::from_f64(29250900.0));
        let r = DecimalRate::from_f64(0.92);
        assert_eq!(Cents::from_f64(10.01).convert(&r), Cents(921));
    }
}
