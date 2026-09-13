//! Shared deterministic normalization for numbers, currency, dates, and free text (owner:
//! extraction, board `decision.accuracy_first` / analyst RULES.md S7). Used by BOTH
//! `extract::images` and `extract::messages` so number/currency/date parsing rules never
//! drift between the two evidence paths -- a rule fixed for one must not silently miss the
//! other. Every function here is total (returns `Option`): a genuinely ambiguous input is
//! rejected (`None`), never resolved by a guess. Analyst audit #276: a self-consistent 10x
//! misread of an Indian lakh grouping ("1,00,000" -> 1,000,000) slipped past internal
//! document reconciliation, which is why ambiguity here means reject, not "best effort".

use chrono::NaiveDate;

/// Parses a currency-formatted number, tolerant of the grouping/decimal conventions this
/// dataset's currencies actually use (analyst RULES.md S7): plain thousands separators
/// ("3,543.54"), Indian lakh grouping ("1,00,000"), EU/Indonesian dot-thousands with
/// comma-decimal ("Rp 30.780.000" / "1.234,56"), parenthesized or leading-minus negatives,
/// and a space-separated whole/fractional pair ("4543 00" -> 4543.00, a Rs/Ps column
/// layout). `currency_hint` (an already-normalized ISO code from `parse_currency`)
/// disambiguates dot-vs-comma-as-decimal when the digits alone don't settle it; without a
/// hint, or when the grouping pattern itself is inconsistent, the amount is rejected rather
/// than guessed. Text with no digits at all (a spelled-out amount) is also rejected.
pub fn parse_amount(raw: &str, currency_hint: Option<&str>) -> Option<f64> {
    let mut s = raw.trim().to_string();
    if s.is_empty() {
        return None;
    }

    let mut negative = false;
    if s.starts_with('(') && s.ends_with(')') && s.len() >= 2 {
        negative = true;
        s = s[1..s.len() - 1].trim().to_string();
    }
    s = strip_currency_affix(&s);
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (s, leading_neg) = match s.strip_prefix('-') {
        Some(rest) => (rest.trim(), true),
        None => (s, false),
    };
    negative = negative || leading_neg;
    if !s.chars().any(|c| c.is_ascii_digit()) {
        return None; // spelled-out ("Three thousand") or otherwise non-numeric: never guessed
    }

    // Rs/Ps split-column layout: two whitespace-separated groups, the second exactly two
    // digits (paise/cents), e.g. "4543 00" -> 4543.00.
    if let Some((whole, frac)) = s.split_once(char::is_whitespace) {
        let whole = whole.trim();
        let frac = frac.trim();
        if frac.len() == 2
            && frac.chars().all(|c| c.is_ascii_digit())
            && !whole.is_empty()
            && whole.chars().all(|c| c.is_ascii_digit() || c == ',')
        {
            let digits = normalize_grouping(whole, ',', currency_hint)?;
            let value: f64 = format!("{digits}.{frac}").parse().ok()?;
            return Some(if negative { -value } else { value });
        }
    }
    if s.contains(char::is_whitespace) {
        return None; // any other embedded whitespace is not a recognized shape
    }

    let comma_decimal_locale = matches!(currency_hint, Some("IDR") | Some("EUR"));
    let dots = s.matches('.').count();
    let commas = s.matches(',').count();

    let digits = match (dots, commas) {
        (0, 0) => s.to_string(),
        (d, c) if d > 0 && c > 0 => {
            let last_dot = s.rfind('.').unwrap();
            let last_comma = s.rfind(',').unwrap();
            if last_dot > last_comma {
                combine_thousands_and_decimal(s, ',', '.', currency_hint)?
            } else {
                combine_thousands_and_decimal(s, '.', ',', currency_hint)?
            }
        }
        (d, 0) if d >= 2 => s.replace('.', ""),
        (1, 0) => {
            let idx = s.rfind('.').unwrap();
            let after = &s[idx + 1..];
            if !after.chars().all(|c| c.is_ascii_digit()) || after.is_empty() {
                return None;
            }
            match after.len() {
                1 | 2 => s.to_string(), // ordinary decimal
                3 if comma_decimal_locale => s.replace('.', ""), // dot-thousands, no decimal part
                _ => return None,       // ambiguous without a stronger locale signal
            }
        }
        (0, c) if c >= 2 => {
            if !is_valid_grouping(s, ',', currency_hint) {
                return None;
            }
            s.replace(',', "")
        }
        (0, 1) => {
            let idx = s.find(',').unwrap();
            let after = &s[idx + 1..];
            if !after.chars().all(|c| c.is_ascii_digit()) || after.is_empty() {
                return None;
            }
            match after.len() {
                3 => s.replace(',', ""), // standard thousands grouping
                1 | 2 if comma_decimal_locale => s.replacen(',', ".", 1), // decimal comma
                _ => return None,        // ambiguous (e.g. "$33,50" under a comma-thousands currency)
            }
        }
        _ => return None,
    };

    let value: f64 = digits.parse().ok()?;
    Some(if negative { -value } else { value })
}

/// Strips a known currency symbol/word prefix or suffix ("Rp 30.780.000" -> "30.780.000",
/// "40 USD" -> "40"), leniently -- an unrecognized affix is left in place (the digit-parsing
/// step above will then simply fail to find any recognizable numeric shape).
fn strip_currency_affix(s: &str) -> String {
    let trimmed = s.trim();
    if let Some((first, rest)) = trimmed.split_once(char::is_whitespace) {
        if is_recognized_currency_token(first) {
            return rest.trim().to_string();
        }
    }
    if let Some((rest, last)) = trimmed.rsplit_once(char::is_whitespace) {
        if is_recognized_currency_token(last) {
            return rest.trim().to_string();
        }
    }
    // No whitespace between a symbol and the digits ("$40", "Rp30780000"): strip a leading
    // run of the same known-symbol characters (never a bare digit run -- a plain number
    // with no affix at all must pass through untouched).
    let symbol_end = trimmed
        .find(|c: char| c.is_ascii_digit() || c == '(' || c == '-' || c.is_whitespace())
        .unwrap_or(trimmed.len());
    if symbol_end > 0 && is_recognized_currency_token(&trimmed[..symbol_end]) {
        return trimmed[symbol_end..].trim_start().to_string();
    }
    trimmed.to_string()
}

/// True only for a token this dataset's five currencies actually use as a symbol, code, or
/// name -- unlike `parse_currency`, an unrecognized token is NOT treated as a match (that
/// function's lenient "pass through as cleaned text" fallback exists for currency-code
/// comparison, not for deciding whether a token is an amount's currency affix).
fn is_recognized_currency_token(token: &str) -> bool {
    let upper = token.trim().trim_end_matches('.').to_uppercase();
    matches!(
        upper.as_str(),
        "INR" | "RS" | "RUPEES" | "RUPEE" | "\u{20B9}"
            | "USD" | "US$" | "$" | "DOLLAR" | "DOLLARS"
            | "EUR" | "\u{20AC}" | "EURO" | "EUROS"
            | "IDR" | "RP" | "RUPIAH"
            | "ZAR" | "R" | "RAND"
    )
}

/// Validates that `s` (digits and `sep` only) follows either the western thousands pattern
/// (every group after the first is exactly 3 digits) or, when the currency hint is INR, the
/// Indian lakh pattern (first group 1-2 digits, then any number of 2-digit groups, then a
/// final 3-digit group; also accepts the plain western shape, since INR documents use both).
fn is_valid_grouping(s: &str, sep: char, currency_hint: Option<&str>) -> bool {
    let groups: Vec<&str> = s.split(sep).collect();
    if groups.len() < 2 || groups.iter().any(|g| g.is_empty() || !g.chars().all(|c| c.is_ascii_digit())) {
        return false;
    }
    let western = groups[0].len() <= 3 && groups[1..].iter().all(|g| g.len() == 3);
    if western {
        return true;
    }
    if currency_hint == Some("INR") && groups.len() >= 2 {
        let last = groups.last().unwrap();
        let middle = &groups[1..groups.len() - 1];
        return groups[0].len() <= 2 && last.len() == 3 && middle.iter().all(|g| g.len() == 2);
    }
    false
}

fn normalize_grouping(s: &str, sep: char, currency_hint: Option<&str>) -> Option<String> {
    if !s.contains(sep) {
        return Some(s.to_string());
    }
    if is_valid_grouping(s, sep, currency_hint) {
        Some(s.replace(sep, ""))
    } else {
        None
    }
}

/// Both a thousands separator and a decimal separator are present: splits at the LAST
/// occurrence of `decimal_sep` (the genuine decimal point is always the rightmost
/// separator when both kinds appear together), validates the integer part's grouping, and
/// recombines as a plain `int.frac` string.
fn combine_thousands_and_decimal(
    s: &str,
    thousands_sep: char,
    decimal_sep: char,
    currency_hint: Option<&str>,
) -> Option<String> {
    let idx = s.rfind(decimal_sep)?;
    let (int_part, frac_part) = (&s[..idx], &s[idx + 1..]);
    if frac_part.is_empty() || !frac_part.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let int_digits = normalize_grouping(int_part, thousands_sep, currency_hint)?;
    Some(format!("{int_digits}.{frac_part}"))
}

/// Currency-code normalization (analyst audit #194): tolerant of symbols/names a VLM or
/// message prints instead of the ISO code. Limited to this dataset's five currencies (INR,
/// USD, EUR, IDR, ZAR); an unrecognized-but-nonempty value passes through as a cleaned
/// uppercase string (still usable for an exact-string fallback match). The literal text
/// "null" (a model writing the word instead of omitting the field) and an empty string both
/// map to `None`.
pub fn parse_currency(raw: &str) -> Option<String> {
    let cleaned = raw.trim();
    if cleaned.is_empty() || cleaned.eq_ignore_ascii_case("null") {
        return None;
    }
    let upper = cleaned.trim_end_matches('.').to_uppercase();
    Some(
        match upper.as_str() {
            "INR" | "RS" | "RUPEES" | "RUPEE" | "INDIAN RUPEE" | "INDIAN RUPEES" | "\u{20B9}" => "INR",
            "USD" | "US$" | "$" | "US DOLLAR" | "US DOLLARS" | "DOLLAR" | "DOLLARS" => "USD",
            "EUR" | "\u{20AC}" | "EURO" | "EUROS" => "EUR",
            "IDR" | "RP" | "RUPIAH" | "INDONESIAN RUPIAH" => "IDR",
            "ZAR" | "R" | "RAND" | "SOUTH AFRICAN RAND" => "ZAR",
            _ => return Some(upper),
        }
        .to_string(),
    )
}

/// Indonesian month names (full and common abbreviations) alongside English, so a document
/// or message date in either language parses.
const MONTH_NAMES: &[(&str, u32)] = &[
    ("jan", 1), ("january", 1), ("januari", 1),
    ("feb", 2), ("february", 2), ("februari", 2),
    ("mar", 3), ("march", 3), ("maret", 3),
    ("apr", 4), ("april", 4),
    ("may", 5), ("mei", 5),
    ("jun", 6), ("june", 6), ("juni", 6),
    ("jul", 7), ("july", 7), ("juli", 7),
    ("aug", 8), ("august", 8), ("agu", 8), ("ags", 8), ("agustus", 8),
    ("sep", 9), ("sept", 9), ("september", 9),
    ("oct", 10), ("october", 10), ("okt", 10), ("oktober", 10),
    ("nov", 11), ("november", 11),
    ("dec", 12), ("december", 12), ("des", 12), ("desember", 12),
];

fn month_number(token: &str) -> Option<u32> {
    let cleaned = token.trim().trim_end_matches('.').to_lowercase();
    MONTH_NAMES.iter().find(|(name, _)| *name == cleaned).map(|(_, n)| *n)
}

/// Parses a date, tolerant of the formats this dataset's documents/messages actually use
/// (analyst RULES.md S7): ISO ("2026-02-06"), "DD-Mon-YYYY" / "D Mon YYYY" with an English
/// or Indonesian month name, and a fully-numeric "DD/MM/YYYY"-shaped date ONLY when the
/// day/month split is unambiguous (one component is >12, so it cannot be the month) --
/// "01/10/2025" and "11/08/23" are rejected outright, exactly like an ambiguous number, since
/// no locale rule in this dataset settles which side is the day.
pub fn parse_date(raw: &str) -> Option<NaiveDate> {
    let s = raw.trim();
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(d);
    }
    if let Some(d) = parse_month_name_date(s, '-') {
        return Some(d);
    }
    if let Some(d) = parse_month_name_date(s, ' ') {
        return Some(d);
    }
    parse_unambiguous_slash_date(s)
}

/// "6 Feb 2026" / "06-Feb-2026": exactly 3 tokens split on `sep`, with the middle token a
/// recognized month name.
fn parse_month_name_date(s: &str, sep: char) -> Option<NaiveDate> {
    let parts: Vec<&str> = s.split(sep).map(str::trim).filter(|p| !p.is_empty()).collect();
    if parts.len() != 3 {
        return None;
    }
    let month = month_number(parts[1])?;
    let day: u32 = parts[0].parse().ok()?;
    let year: i32 = full_year(parts[2])?;
    NaiveDate::from_ymd_opt(year, month, day)
}

/// "25/02/2026" (unambiguous: 25 can't be a month) but rejects "01/10/2025" / "11/08/23"
/// (every component is a plausible month, so the day/month order is genuinely ambiguous).
fn parse_unambiguous_slash_date(s: &str) -> Option<NaiveDate> {
    let parts: Vec<&str> = s.split('/').collect();
    if parts.len() != 3 {
        return None;
    }
    let a: u32 = parts[0].parse().ok()?;
    let b: u32 = parts[1].parse().ok()?;
    let year = full_year(parts[2])?;
    let a_could_be_month = (1..=12).contains(&a);
    let b_could_be_month = (1..=12).contains(&b);
    let (day, month) = match (a_could_be_month, b_could_be_month) {
        (true, true) => return None, // ambiguous -- no locale rule settles it, reject
        (false, true) => (a, b),     // a can't be a month -> a is the day
        (true, false) => (b, a),     // b can't be a month -> b is the day
        (false, false) => return None,
    };
    NaiveDate::from_ymd_opt(year, month, day)
}

fn full_year(token: &str) -> Option<i32> {
    let y: i32 = token.parse().ok()?;
    Some(if token.len() <= 2 { 2000 + y } else { y })
}

/// Case/whitespace/punctuation normalization shared by free-text matchers (e.g.
/// `extract::images::DocType::from_free_text`) so the same cleanup rule applies everywhere
/// free text is matched against a known vocabulary, in either English or Indonesian.
pub fn normalize_text(raw: &str) -> String {
    raw.trim()
        .to_uppercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_thousands_and_decimal() {
        assert_eq!(parse_amount("3,543.54", None), Some(3543.54));
        assert_eq!(parse_amount("42750000", None), Some(42_750_000.0));
    }

    /// Analyst RULES.md S7: Indian lakh grouping ("1,00,000" -> 100000), the exact shape
    /// behind the image_02 10x misread (analyst #276) -- our string-side parser must never
    /// reproduce that error when the figure arrives as a string needing this recovery.
    #[test]
    fn parses_indian_lakh_grouping() {
        assert_eq!(parse_amount("1,00,000", Some("INR")), Some(100_000.0));
        assert_eq!(parse_amount("12,34,567", Some("INR")), Some(1_234_567.0));
        assert_eq!(parse_amount("2,00,000.00", Some("INR")), Some(200_000.0));
    }

    /// Analyst RULES.md S7: ID/EU dot-thousands, comma-decimal.
    #[test]
    fn parses_id_eu_dot_thousands_and_comma_decimal() {
        assert_eq!(parse_amount("Rp 30.780.000", Some("IDR")), Some(30_780_000.0));
        assert_eq!(parse_amount("43.339.000", Some("IDR")), Some(43_339_000.0));
        assert_eq!(parse_amount("1.234,56", Some("EUR")), Some(1234.56));
    }

    /// Analyst RULES.md S7: Rs/Ps split-column layout ("4543 00" -> 4543.00).
    #[test]
    fn parses_space_separated_whole_and_fraction() {
        assert_eq!(parse_amount("4543 00", Some("INR")), Some(4543.00));
        assert_eq!(parse_amount("1,00,000 50", Some("INR")), Some(100_000.50));
    }

    #[test]
    fn parses_parenthesized_and_leading_minus_negatives() {
        assert_eq!(parse_amount("(123.45)", None), Some(-123.45));
        assert_eq!(parse_amount("-123.45", None), Some(-123.45));
    }

    #[test]
    fn rejects_spelled_out_amounts() {
        assert_eq!(parse_amount("Three thousand", None), None);
        assert_eq!(parse_amount("", None), None);
    }

    /// Analyst RULES.md S7: "$33,50" under a comma-THOUSANDS currency (USD) is genuinely
    /// ambiguous (comma-decimal is not a USD convention) -- reject, never guess 33.50 or
    /// 3350.
    #[test]
    fn rejects_ambiguous_comma_decimal_under_a_thousands_locale() {
        assert_eq!(parse_amount("$33,50", Some("USD")), None);
        assert_eq!(parse_amount("33,50", None), None);
    }

    /// A single-comma group that is neither a standard 3-digit thousands group nor a
    /// plausible decimal-comma shape is ambiguous and must be rejected, not guessed by
    /// stripping the comma.
    #[test]
    fn rejects_inconsistent_grouping() {
        assert_eq!(parse_amount("1,2345", None), None);
        assert_eq!(parse_amount("12,3,456", Some("INR")), None);
    }

    #[test]
    fn currency_symbols_names_and_codes_normalize() {
        assert_eq!(parse_currency("Rs"), Some("INR".to_string()));
        assert_eq!(parse_currency("\u{20B9}"), Some("INR".to_string()));
        assert_eq!(parse_currency("Indian Rupees"), Some("INR".to_string()));
        assert_eq!(parse_currency("$"), Some("USD".to_string()));
        assert_eq!(parse_currency("Rp"), Some("IDR".to_string()));
        assert_eq!(parse_currency("R"), Some("ZAR".to_string()));
        assert_eq!(parse_currency("\u{20AC}"), Some("EUR".to_string()));
    }

    #[test]
    fn currency_null_and_empty_map_to_none() {
        assert_eq!(parse_currency("null"), None);
        assert_eq!(parse_currency("NULL"), None);
        assert_eq!(parse_currency("  "), None);
    }

    #[test]
    fn dates_parse_iso_and_month_name_forms_in_english_and_indonesian() {
        assert_eq!(parse_date("2026-02-06"), NaiveDate::from_ymd_opt(2026, 2, 6));
        assert_eq!(parse_date("06-Feb-2026"), NaiveDate::from_ymd_opt(2026, 2, 6));
        assert_eq!(parse_date("6 Feb 2026"), NaiveDate::from_ymd_opt(2026, 2, 6));
        assert_eq!(parse_date("6 Februari 2026"), NaiveDate::from_ymd_opt(2026, 2, 6));
        assert_eq!(parse_date("31 Agustus 2019"), NaiveDate::from_ymd_opt(2019, 8, 31));
    }

    /// Analyst RULES.md S7: day-first ambiguous numeric dates must be rejected, not guessed.
    #[test]
    fn rejects_ambiguous_day_first_dates() {
        assert_eq!(parse_date("01/10/2025"), None);
        assert_eq!(parse_date("11/08/23"), None);
    }

    #[test]
    fn accepts_unambiguous_slash_dates() {
        // 25 cannot be a month, so the day/month order is settled regardless of position.
        assert_eq!(parse_date("25/02/2026"), NaiveDate::from_ymd_opt(2026, 2, 25));
        assert_eq!(parse_date("02/25/2026"), NaiveDate::from_ymd_opt(2026, 2, 25));
    }

    #[test]
    fn normalize_text_collapses_case_and_whitespace() {
        assert_eq!(normalize_text("  Tax   Invoice  "), "TAX INVOICE");
    }
}
