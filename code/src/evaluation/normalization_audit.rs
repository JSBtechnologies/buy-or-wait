//! Independent re-normalization of raw page/model surface forms (RULES.md S7), for the signoff
//! check that every applied model/parsed value carries its raw form and a normalized value that
//! this module reproduces, and that no ambiguous or non-money raw form was normalized.
//!
//! This is a second implementation on purpose: it shares no code with `extract/normalize.rs`.
//! Conventions follow RULES.md S7.4:
//! - amounts: western (`9,999,999.99`), Indian lakh (`9,99,999.99`), dot thousands (`99.999.999`),
//!   comma decimal (`99,99`), Rupee/Paise columns (`9999 99`), currency prefixes (`₹`, `Rs.`, `$`,
//!   ISO codes); 10+ digit or leading-zero digit runs are ids, never money;
//! - dates: day-first on pages; a slash/dash date whose day and month are both ≤ 12 (and differ)
//!   resolves only when the day-first reading is confirmed (linked event date, or another
//!   unambiguous day-first date on the same page); otherwise it stays unresolved;
//! - currency: `₹`/`Rs`/`Rupees` → INR, `$` → USD, `"null"`/`Amount` → absent; an ISO code other
//!   than the event currency is a mismatch, never silently remapped.
//!
//! Ambiguous here means two structurally valid readings with different values and nothing on the
//! value itself to pick one: a single dot followed by exactly three digits outside IDR, or a number
//! wrapped across lines. Under decision.accuracy_first those must stay missing.

use std::sync::OnceLock;

use chrono::NaiveDate;
use regex::Regex;

use super::contract::{Finding, Severity};
use super::data::{parse_cents, Cents};

#[derive(Debug, Clone, PartialEq)]
pub enum AmountRead {
    Value(Cents),
    Ambiguous(&'static str),
    NotAmount(&'static str),
    Unparseable,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DateRead {
    Date(NaiveDate),
    /// (day-first, month-first) readings of a date whose day and month are both ≤ 12.
    Ambiguous(NaiveDate, NaiveDate),
    /// A period such as `Aug-2019`: not a cash date.
    Period,
    Unparseable,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CurrencyRead {
    Iso(String),
    Absent,
    Unknown,
}

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).unwrap())
}

/// Strip a currency marker; returns (rest, ISO code implied by the marker).
fn strip_currency(s: &str) -> (&str, Option<&'static str>) {
    static PREFIX: OnceLock<Regex> = OnceLock::new();
    static SUFFIX: OnceLock<Regex> = OnceLock::new();
    let prefix = re(&PREFIX, r"^(₹|Rs\.?|RS\.?|Rp\.?|US\$|\$|€|INR|IDR|USD|EUR|ZAR)\s*");
    let suffix = re(&SUFFIX, r"\s*(INR|IDR|USD|EUR|ZAR)$");
    let code = |m: &str| -> Option<&'static str> {
        match m.trim_end_matches('.') {
            "₹" | "Rs" | "RS" | "INR" => Some("INR"),
            "$" | "US$" | "USD" => Some("USD"),
            "€" | "EUR" => Some("EUR"),
            "Rp" | "IDR" => Some("IDR"),
            "ZAR" => Some("ZAR"),
            _ => None,
        }
    };
    if let Some(m) = prefix.captures(s) {
        let c = code(&m[1]);
        return (&s[m.get(0).unwrap().end()..], c);
    }
    if let Some(m) = suffix.captures(s) {
        let c = code(&m[1]);
        return (&s[..m.get(0).unwrap().start()], c);
    }
    (s, None)
}

/// Read one raw amount string. `currency` is the event/page currency when known.
pub fn read_amount(raw: &str, currency: Option<&str>) -> AmountRead {
    static PATTERNS: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    let s = raw.trim();
    if s.lines().filter(|l| !l.trim().is_empty()).count() > 1 {
        return AmountRead::Ambiguous("number wrapped across lines");
    }
    let (s, marker) = strip_currency(s);
    let s = s.trim();
    let idr = marker == Some("IDR") || (marker.is_none() && currency == Some("IDR"));
    let patterns = PATTERNS.get_or_init(|| {
        [
            (r"^\d+$", "digits"),
            (r"^\d+\.\d{1,2}$", "dot_decimal"),
            (r"^\d{1,3}\.\d{3}$", "single_dot_three"),
            (r"^\d{1,3}(\.\d{3}){2,}$", "dot_thousands"),
            (r"^\d{1,3}(\.\d{3})+,\d{1,2}$", "dot_thousands_comma_decimal"),
            (r"^\d{1,3}(,\d{3})+(\.\d{1,2})?$", "western"),
            (r"^\d{1,2}(,\d{2})+,\d{3}(\.\d{1,2})?$", "lakh"),
            (r"^\d+,\d{2}$", "comma_decimal"),
            (r"^\d+ \d{2}$", "rupee_paise_columns"),
        ]
        .into_iter()
        .map(|(p, n)| (Regex::new(p).unwrap(), n))
        .collect()
    });
    let Some(kind) = patterns.iter().find(|(r, _)| r.is_match(s)).map(|(_, n)| *n) else { return AmountRead::Unparseable };
    let plain: String = match kind {
        "digits" => {
            if s.len() >= 10 {
                return AmountRead::NotAmount("10+ digit run (phone, tax id, UPC, transaction id)");
            }
            if s.len() > 1 && s.starts_with('0') {
                return AmountRead::NotAmount("leading-zero digit run");
            }
            s.to_string()
        }
        "dot_decimal" => s.to_string(),
        "single_dot_three" if idr => s.replace('.', ""),
        "single_dot_three" => return AmountRead::Ambiguous("single dot + 3 digits: thousands separator or 3-dp decimal"),
        "dot_thousands" => s.replace('.', ""),
        "dot_thousands_comma_decimal" => s.replace('.', "").replace(',', "."),
        "western" | "lakh" => s.replace(',', ""),
        "comma_decimal" => s.replace(',', "."),
        "rupee_paise_columns" => s.replace(' ', "."),
        _ => unreachable!(),
    };
    parse_cents(&plain).map(AmountRead::Value).unwrap_or(AmountRead::Unparseable)
}

pub fn read_date(raw: &str) -> DateRead {
    static NUMERIC: OnceLock<Regex> = OnceLock::new();
    static ISO: OnceLock<Regex> = OnceLock::new();
    static PERIOD: OnceLock<Regex> = OnceLock::new();
    let s = raw.trim();
    if let Some(m) = re(&ISO, r"^(\d{4})-(\d{2})-(\d{2})\b").captures(s) {
        return NaiveDate::from_ymd_opt(m[1].parse().unwrap(), m[2].parse().unwrap(), m[3].parse().unwrap()).map(DateRead::Date).unwrap_or(DateRead::Unparseable);
    }
    if let Some(m) = re(&NUMERIC, r"^(\d{1,2})([/-])(\d{1,2})([/-])(\d{4}|\d{2})\b").captures(s) {
        if m[2] != m[4] {
            return DateRead::Unparseable;
        }
        let (a, b): (u32, u32) = (m[1].parse().unwrap(), m[3].parse().unwrap());
        let year: i32 = if m[5].len() == 2 { 2000 + m[5].parse::<i32>().unwrap() } else { m[5].parse().unwrap() };
        let day_first = NaiveDate::from_ymd_opt(year, b, a);
        let month_first = NaiveDate::from_ymd_opt(year, a, b);
        return match (day_first, month_first) {
            (Some(d), Some(m)) if d != m => DateRead::Ambiguous(d, m),
            (Some(d), _) => DateRead::Date(d),
            (None, Some(m)) => DateRead::Date(m),
            (None, None) => DateRead::Unparseable,
        };
    }
    let head = s.split(|c: char| c == ',').next().unwrap_or(s);
    let words: Vec<&str> = head.split_whitespace().take(3).collect();
    let candidate = words.join(" ");
    for (text, fmt) in [(head.split_whitespace().next().unwrap_or(""), "%d-%b-%Y"), (candidate.as_str(), "%d %b %Y"), (candidate.as_str(), "%d %B %Y")] {
        if let Ok(d) = NaiveDate::parse_from_str(text, fmt) {
            return DateRead::Date(d);
        }
    }
    if re(&PERIOD, r"^(?i)(jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)[a-z]*[- ]\d{4}$").is_match(s) {
        return DateRead::Period;
    }
    DateRead::Unparseable
}

/// The date a raw form may normalize to. An ambiguous form needs the day-first reading confirmed
/// by the linked event date or by an unambiguous day-first date on the same page.
pub fn resolve_date(raw: &str, event_date: Option<NaiveDate>, page_day_first_confirmed: bool) -> Option<NaiveDate> {
    match read_date(raw) {
        DateRead::Date(d) => Some(d),
        DateRead::Ambiguous(day_first, _) if page_day_first_confirmed || event_date == Some(day_first) => Some(day_first),
        _ => None,
    }
}

pub fn read_currency(raw: &str) -> CurrencyRead {
    let s = raw.trim();
    match s.trim_end_matches('.') {
        "" => CurrencyRead::Absent,
        x if x.eq_ignore_ascii_case("null") || x.eq_ignore_ascii_case("none") || x.eq_ignore_ascii_case("amount") => CurrencyRead::Absent,
        "₹" | "Rs" | "RS" | "rs" | "Rupees" | "rupees" | "INR" => CurrencyRead::Iso("INR".into()),
        "$" | "US$" | "USD" => CurrencyRead::Iso("USD".into()),
        "€" | "EUR" => CurrencyRead::Iso("EUR".into()),
        "Rp" | "IDR" => CurrencyRead::Iso("IDR".into()),
        x if x.len() == 3 && x.bytes().all(|b| b.is_ascii_uppercase()) => CurrencyRead::Iso(x.into()),
        _ => CurrencyRead::Unknown,
    }
}

fn err(ctx: &str, code: &'static str, detail: String) -> Option<Finding> {
    Some(Finding { request_id: ctx.to_string(), severity: Severity::Error, code, detail })
}

/// A normalized amount must come with its raw form and equal this module's reading of it.
/// Nothing normalized is never a finding (a missing value beats a wrong one).
pub fn audit_amount(ctx: &str, raw: Option<&str>, normalized: Option<f64>, currency: Option<&str>) -> Option<Finding> {
    let n = normalized?;
    let Some(raw) = raw else { return err(ctx, "NP1_normalized_without_raw", format!("normalized amount {n} has no raw form")) };
    let cents = (n * 100.0).round() as Cents;
    match read_amount(raw, currency) {
        AmountRead::Value(v) if v == cents => None,
        AmountRead::Value(v) => err(ctx, "NP3_normalized_mismatch", format!("raw {raw:?} reads {} but normalized {n}", super::data::fmt_cents_2dp(v))),
        AmountRead::Ambiguous(why) => err(ctx, "NP2_ambiguous_raw_normalized", format!("raw {raw:?} is ambiguous ({why}) but was normalized to {n}")),
        AmountRead::NotAmount(why) => err(ctx, "NP4_not_amount_normalized", format!("raw {raw:?} is not money ({why}) but was normalized to {n}")),
        AmountRead::Unparseable => err(ctx, "NP5_unparseable_raw_normalized", format!("raw {raw:?} has no recognized amount form but was normalized to {n}")),
    }
}

pub fn audit_date(ctx: &str, raw: Option<&str>, normalized: Option<NaiveDate>, event_date: Option<NaiveDate>, page_day_first_confirmed: bool) -> Option<Finding> {
    let n = normalized?;
    let Some(raw) = raw else { return err(ctx, "NP1_normalized_without_raw", format!("normalized date {n} has no raw form")) };
    match (read_date(raw), resolve_date(raw, event_date, page_day_first_confirmed)) {
        (_, Some(d)) if d == n => None,
        (_, Some(d)) => err(ctx, "NP3_normalized_mismatch", format!("raw {raw:?} reads {d} but normalized {n}")),
        (DateRead::Ambiguous(df, mf), None) => err(ctx, "NP2_ambiguous_raw_normalized", format!("raw {raw:?} is {df} day-first or {mf} month-first, unconfirmed, but was normalized to {n}")),
        (DateRead::Period, None) => err(ctx, "NP4_not_amount_normalized", format!("raw {raw:?} is a period, not a date, but was normalized to {n}")),
        (_, None) => err(ctx, "NP5_unparseable_raw_normalized", format!("raw {raw:?} has no recognized date form but was normalized to {n}")),
    }
}

pub fn audit_currency(ctx: &str, raw: Option<&str>, normalized: Option<&str>, event_currency: &str) -> Option<Finding> {
    let n = normalized?;
    let Some(raw) = raw else { return err(ctx, "NP1_normalized_without_raw", format!("normalized currency {n} has no raw form")) };
    match read_currency(raw) {
        CurrencyRead::Iso(c) if c == n && (c == event_currency) => None,
        CurrencyRead::Iso(c) if c == n => err(ctx, "NP6_currency_mismatch_accepted", format!("raw {raw:?} = {c} but the event is {event_currency}: must be rejected, not applied")),
        CurrencyRead::Iso(c) => err(ctx, "NP6_currency_remapped", format!("raw {raw:?} = {c} but normalized {n}")),
        CurrencyRead::Absent => err(ctx, "NP7_absent_normalized", format!("raw {raw:?} means no currency but was normalized to {n}")),
        CurrencyRead::Unknown => err(ctx, "NP5_unparseable_raw_normalized", format!("raw {raw:?} is not a known currency form but was normalized to {n}")),
    }
}

#[cfg(test)]
mod tests {
    //! Shapes from RULES.md S7.4 with synthetic figures (no audit figures).
    use super::*;

    fn v(raw: &str) -> AmountRead {
        read_amount(raw, None)
    }
    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn amount_forms() {
        assert_eq!(v("3,00,000.00"), AmountRead::Value(300_000_00));
        assert_eq!(v("8,12,345.67"), AmountRead::Value(812_345_67));
        assert_eq!(v("12,34,567"), AmountRead::Value(1_234_567_00));
        assert_eq!(v("4,321,000"), AmountRead::Value(4_321_000_00));
        assert_eq!(v("65,432.10"), AmountRead::Value(65_432_10));
        assert_eq!(v("12.345.678"), AmountRead::Value(12_345_678_00));
        assert_eq!(v("1.234.567,89"), AmountRead::Value(1_234_567_89));
        assert_eq!(v("$41,75"), AmountRead::Value(41_75));
        assert_eq!(v("3217 00"), AmountRead::Value(3217_00));
        assert_eq!(v("₹3141.00"), AmountRead::Value(3141_00));
        assert_eq!(v("Rs.7000"), AmountRead::Value(7000_00));
        assert_eq!(v("Rs 0.00"), AmountRead::Value(0));
        assert_eq!(v("IDR 70,000"), AmountRead::Value(70_000_00));
        assert_eq!(v("51234.0"), AmountRead::Value(51_234_00));
        assert_eq!(v("2717"), AmountRead::Value(2717_00));
        // Ambiguous / not money / unparseable.
        assert!(matches!(v("5.250"), AmountRead::Ambiguous(_)));
        assert_eq!(read_amount("5.250", Some("IDR")), AmountRead::Value(5250_00));
        assert_eq!(v("IDR 5.250"), AmountRead::Value(5250_00));
        assert!(matches!(v("7,321.0\n0"), AmountRead::Ambiguous(_)));
        assert!(matches!(v("07203051188"), AmountRead::NotAmount(_)));
        assert!(matches!(v("712345678901234"), AmountRead::NotAmount(_)));
        assert!(matches!(v("0412"), AmountRead::NotAmount(_)));
        assert_eq!(v("EMP-0042"), AmountRead::Unparseable);
        assert_eq!(v("12%"), AmountRead::Unparseable);
        assert_eq!(v("1,2,3"), AmountRead::Unparseable);
        assert_eq!(v("12.3456"), AmountRead::Unparseable);
    }

    #[test]
    fn date_forms_and_day_first_confirmation() {
        assert_eq!(read_date("2025-04-17"), DateRead::Date(d(2025, 4, 17)));
        assert_eq!(read_date("09-Mar-2027"), DateRead::Date(d(2027, 3, 9)));
        assert_eq!(read_date("4 Sep 2020 10:15 PM"), DateRead::Date(d(2020, 9, 4)));
        assert_eq!(read_date("24 July 2026"), DateRead::Date(d(2026, 7, 24)));
        assert_eq!(read_date("27/03/2026"), DateRead::Date(d(2026, 3, 27)));
        assert_eq!(read_date("18-01-23 12:19 PM"), DateRead::Date(d(2023, 1, 18)));
        assert_eq!(read_date("05/09/24"), DateRead::Ambiguous(d(2024, 9, 5), d(2024, 5, 9)));
        assert_eq!(read_date("04/04/2024"), DateRead::Date(d(2024, 4, 4)));
        assert_eq!(read_date("Sep-2021"), DateRead::Period);
        assert_eq!(read_date("CHARGED ON"), DateRead::Unparseable);
        assert_eq!(read_date("05/09-24"), DateRead::Unparseable);
        // Ambiguous: only a confirmed day-first reading resolves.
        assert_eq!(resolve_date("05/09/24", Some(d(2024, 9, 5)), false), Some(d(2024, 9, 5)));
        assert_eq!(resolve_date("05/09/24", None, true), Some(d(2024, 9, 5)));
        assert_eq!(resolve_date("05/09/24", Some(d(2024, 5, 9)), false), None);
        assert_eq!(resolve_date("05/09/24", None, false), None);
    }

    #[test]
    fn currency_forms() {
        for r in ["₹", "Rs.", "RS", "Rupees", "INR"] {
            assert_eq!(read_currency(r), CurrencyRead::Iso("INR".into()), "{r}");
        }
        assert_eq!(read_currency("$"), CurrencyRead::Iso("USD".into()));
        assert_eq!(read_currency("null"), CurrencyRead::Absent);
        assert_eq!(read_currency("Amount"), CurrencyRead::Absent);
        assert_eq!(read_currency("PHP"), CurrencyRead::Iso("PHP".into()));
        assert_eq!(read_currency("rupiah?"), CurrencyRead::Unknown);
    }

    /// Differential report vs extraction's normalize.rs over S7.4 shapes and edge cases.
    /// UNSAFE = extraction yields a value this reader does not (different value, ambiguous, not
    /// money, unparseable); CONSERVATIVE = extraction rejects what this reader resolves.
    #[test]
    #[ignore]
    fn differential_vs_extract_normalize() {
        use crate::extract::normalize as ext;
        let amounts = [
            "3,00,000.00", "12,34,567", "9,99,999.99", "1,00,00,000.50", "100,00,000", "4,321,000", "65,432.10", "12.345.678", "1.234.567,89",
            "10.000,00", "$41,75", "1,50", "12,5", "3217 00", "1 000", "₹3141.00", "Rs.7000", "Rs 0.00", "Rs. 1,00,000/-", "IDR 70,000", "USD 12.5",
            "51234.0", "2717", "1.5", "5.250", "IDR 5.250", "1,500", "7,321.0\n0", "07203051188", "712345678901234", "8906143890487", "411056",
            "0412", "EMP-0042", "12%", "1,2,3", "12.3456", "-50", "Rp 43.339.000", "₹ 2,854", "$5,00",
        ];
        let mut unsafe_ = 0;
        for hint in [None, Some("INR"), Some("IDR"), Some("USD")] {
            for raw in amounts {
                let e = ext::parse_amount(raw, hint);
                let m = read_amount(raw, hint);
                let tag = match (&e, &m) {
                    (Some(a), AmountRead::Value(b)) if (a * 100.0).round() as Cents == *b => continue,
                    (None, AmountRead::Value(_)) => "CONSERVATIVE",
                    (None, _) => continue,
                    (Some(_), _) => {
                        unsafe_ += 1;
                        "UNSAFE"
                    }
                };
                println!("{tag} amount {raw:?} hint={hint:?} extract={e:?} verifier={m:?}");
            }
        }
        for raw in ["05/09/24", "11/08/23", "01/10/2025", "27/03/2026", "07-06-2026", "18-01-23 12:19 PM", "09-Mar-2027", "4 Sep 2020 10:15 PM", "24 July 2026", "2025-04-17", "Sep-2021", "04/04/2024", "13/13/2024", "CHARGED ON"] {
            let e = ext::parse_date(raw);
            let m = read_date(raw);
            let tag = match (&e, &m) {
                (Some(a), DateRead::Date(b)) if a == b => continue,
                (None, DateRead::Date(_)) => "CONSERVATIVE",
                (None, _) => continue,
                (Some(_), _) => {
                    unsafe_ += 1;
                    "UNSAFE"
                }
            };
            println!("{tag} date {raw:?} extract={e:?} verifier={m:?}");
        }
        for raw in ["₹", "Rs.", "RS", "Rupees", "INR", "$", "US$", "null", "Amount", "PHP", "KMR", "PKR", "Rp", "€", "rupiah?", ""] {
            let e = ext::parse_currency(raw);
            let m = read_currency(raw);
            let same = match (&e, &m) {
                (Some(a), CurrencyRead::Iso(b)) => a == b,
                (None, CurrencyRead::Absent | CurrencyRead::Unknown) => true,
                _ => false,
            };
            if !same {
                let tag = if e.is_some() { unsafe_ += 1; "UNSAFE" } else { "CONSERVATIVE" };
                println!("{tag} currency {raw:?} extract={e:?} verifier={m:?}");
            }
        }
        println!("UNSAFE_TOTAL {unsafe_}");
    }

    fn code(f: Option<Finding>) -> Option<&'static str> {
        f.map(|x| x.code)
    }

    #[test]
    fn audits_flag_every_unsafe_normalization() {
        // Amounts.
        assert_eq!(code(audit_amount("x", Some("3,00,000.00"), Some(300000.0), Some("INR"))), None);
        assert_eq!(code(audit_amount("x", Some("3,00,000.00"), Some(3000000.0), Some("INR"))), Some("NP3_normalized_mismatch"));
        assert_eq!(code(audit_amount("x", None, Some(300000.0), Some("INR"))), Some("NP1_normalized_without_raw"));
        assert_eq!(code(audit_amount("x", Some("5.250"), Some(5250.0), Some("EUR"))), Some("NP2_ambiguous_raw_normalized"));
        assert_eq!(code(audit_amount("x", Some("5.250"), Some(5.25), Some("EUR"))), Some("NP2_ambiguous_raw_normalized"));
        assert_eq!(code(audit_amount("x", Some("07203051188"), Some(7203051188.0), Some("INR"))), Some("NP4_not_amount_normalized"));
        assert_eq!(code(audit_amount("x", Some("$41,75"), Some(4175.0), Some("USD"))), Some("NP3_normalized_mismatch"));
        assert_eq!(code(audit_amount("x", Some("3217 00"), Some(321700.0), Some("INR"))), Some("NP3_normalized_mismatch"));
        assert_eq!(code(audit_amount("x", Some("5.250"), None, Some("EUR"))), None);
        // Dates.
        assert_eq!(code(audit_date("x", Some("05/09/24"), Some(d(2024, 9, 5)), Some(d(2024, 9, 5)), false)), None);
        assert_eq!(code(audit_date("x", Some("05/09/24"), Some(d(2024, 9, 5)), None, false)), Some("NP2_ambiguous_raw_normalized"));
        assert_eq!(code(audit_date("x", Some("05/09/24"), Some(d(2024, 5, 9)), None, true)), Some("NP3_normalized_mismatch"));
        assert_eq!(code(audit_date("x", Some("Sep-2021"), Some(d(2021, 9, 1)), None, false)), Some("NP4_not_amount_normalized"));
        // Currency.
        assert_eq!(code(audit_currency("x", Some("Rs."), Some("INR"), "INR")), None);
        assert_eq!(code(audit_currency("x", Some("PHP"), Some("INR"), "INR")), Some("NP6_currency_remapped"));
        assert_eq!(code(audit_currency("x", Some("PHP"), Some("PHP"), "INR")), Some("NP6_currency_mismatch_accepted"));
        assert_eq!(code(audit_currency("x", Some("null"), Some("INR"), "INR")), Some("NP7_absent_normalized"));
    }
}
