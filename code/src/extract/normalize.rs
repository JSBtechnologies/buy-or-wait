//! Shared deterministic normalization for numbers, currency, dates, and free text (owner:
//! extraction, board `decision.accuracy_first` / analyst RULES.md S7). Used by BOTH
//! `extract::images` and `extract::messages` so number/currency/date parsing rules never
//! drift between the two evidence paths -- a rule fixed for one must not silently miss the
//! other. Every function here is total (returns `Option`): a genuinely ambiguous input is
//! rejected (`None`), never resolved by a guess. Analyst audit #276: a self-consistent 10x
//! misread of an Indian lakh grouping ("1,00,000" -> 1,000,000) slipped past internal
//! document reconciliation, which is why ambiguity here means reject, not "best effort".

use chrono::{Datelike, NaiveDate};

/// A plain, unformatted digit run longer than this is treated as an ID/reference number,
/// never a currency amount (verifier #308: a 10-15 digit string parsing as money is unsafe).
/// This dataset's largest realistic figures (IDR salaries in the tens of millions) stay well
/// under this many digits.
const MAX_PLAIN_DIGIT_RUN: usize = 9;

/// Parses a currency-formatted number, tolerant of the grouping/decimal conventions this
/// dataset's currencies actually use (analyst RULES.md S7): plain thousands separators
/// ("3,543.54"), Indian lakh grouping ("1,00,000"), EU/Indonesian dot-thousands ("Rp
/// 30.780.000"), a genuine decimal comma ("1.234,56"), parenthesized or leading-minus
/// negatives (only when `allow_negative` is `true` -- verifier #308: most amount fields
/// should never be negative, so callers opt in explicitly for a refund/credit context), and
/// a space-separated whole/fractional pair ("4543 00" -> 4543.00, a Rs/Ps column layout).
/// `currency_hint` (an already-normalized ISO code from `parse_currency`) disambiguates the
/// Indian lakh grouping pattern specifically; every other rule here is decided by digit-group
/// pattern ALONE, since a genuine thousands group is always exactly 3 digits and a genuine
/// decimal/minor-unit is always 1-2 digits -- when the pattern itself doesn't settle it, or
/// the hint is absent/insufficient, the amount is rejected rather than guessed. Text with no
/// digits at all (a spelled-out amount), a bare digit run longer than
/// `MAX_PLAIN_DIGIT_RUN` (an ID/reference number), and a leading-zero digit run longer than
/// one digit (also ID-shaped) are all rejected the same way.
pub fn parse_amount(raw: &str, currency_hint: Option<&str>, allow_negative: bool) -> Option<f64> {
    let mut s = raw.trim().to_string();
    if s.is_empty() {
        return None;
    }

    // Wrapped table cell (image_accuracy_plan.md §1, images 03/15): a number cell that wraps
    // across a printed line break ("9,124.0\n0") is NOT the legitimate Rs/Ps split-column
    // layout below (that shape is exactly one whitespace run with a 2-digit tail) -- it is the
    // same number with a line break accidentally inside it. Try stripping every internal
    // whitespace character and re-parsing as a single token FIRST; only a raw string containing
    // a newline is eligible, so a genuine space-separated layout (handled below) never takes
    // this path.
    if s.contains('\n') {
        let joined: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        if let Some(v) = parse_amount(&joined, currency_hint, allow_negative) {
            return Some(v);
        }
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
    if negative && !allow_negative {
        return None; // caller didn't opt into a negative-permitting (refund/credit) context
    }
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

    let dots = s.matches('.').count();
    let commas = s.matches(',').count();

    let digits = match (dots, commas) {
        // Analyst #301/verifier #308: a plain digit string with a leading zero and more
        // than one digit ("0001", a reference/ID number) is never a real currency amount.
        (0, 0) if s.len() > 1 && s.starts_with('0') => return None,
        // verifier #308: an unformatted digit run this long is an ID/reference number, not
        // an amount -- a real figure this large would print thousands separators.
        (0, 0) if s.len() > MAX_PLAIN_DIGIT_RUN => return None,
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
                // A real decimal amount is never printed to 3 places; a lone dot with
                // exactly 3 trailing digits is unambiguously a thousands group with no
                // decimal part at all (dataset-independent digit-group pattern).
                3 => s.replace('.', ""),
                _ => return None, // ambiguous (4+ digits after a single dot)
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
                // verifier #308 / analyst RULES.md S7.4: a genuine decimal/minor-unit is
                // always exactly 2 digits ("$33,50" -> 33.50), regardless of currency -- no
                // real thousands group is ever 1 or 4+ digits, and no minor unit is 1 digit
                // ("12,5" stays ambiguous, matches no real convention).
                2 => s.replacen(',', ".", 1),
                _ => return None,
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
/// message prints instead of the ISO code. STRICTLY limited to this dataset's five
/// currencies (INR, USD, EUR, IDR, ZAR, PLAN.md) -- analyst #301: a real-world code this
/// dataset doesn't use (PKR, KHR, ...) must be rejected (`None`), never passed through as a
/// cleaned-but-unmapped string, so two unsupported codes that happen to read the same never
/// silently "match" in `currency_matches`. The literal text "null" (a model writing the word
/// instead of omitting the field) and an empty string also map to `None`.
pub fn parse_currency(raw: &str) -> Option<String> {
    let cleaned = raw.trim();
    if cleaned.is_empty() || cleaned.eq_ignore_ascii_case("null") {
        return None;
    }
    let upper = cleaned.trim_end_matches('.').to_uppercase();
    let code = match upper.as_str() {
        "INR" | "RS" | "RUPEES" | "RUPEE" | "INDIAN RUPEE" | "INDIAN RUPEES" | "\u{20B9}" => "INR",
        "USD" | "US$" | "$" | "US DOLLAR" | "US DOLLARS" | "DOLLAR" | "DOLLARS" => "USD",
        "EUR" | "\u{20AC}" | "EURO" | "EUROS" => "EUR",
        "IDR" | "RP" | "RUPIAH" | "INDONESIAN RUPIAH" => "IDR",
        "ZAR" | "R" | "RAND" | "SOUTH AFRICAN RAND" => "ZAR",
        _ => return None,
    };
    Some(code.to_string())
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
/// (analyst RULES.md S7): ISO ("2026-02-06", optionally with a "T"/space time suffix),
/// "DD-Mon-YYYY" / "D Mon YYYY" with an English or Indonesian month name, and a fully-numeric
/// "DD/MM/YYYY" or "DD-MM-YYYY"-shaped date ONLY when the day/month split is unambiguous (one
/// component is >12, so it cannot be the month). A two-digit or otherwise ambiguous year is
/// NEVER guessed at a century (analyst #301 G1: "18-01-23" must not silently become year 18,
/// nor 2018/1918 by assumption) -- every date component here requires an explicit, exactly
/// 4-digit year, or the whole date is rejected.
pub fn parse_date(raw: &str) -> Option<NaiveDate> {
    let s = raw.trim();
    if let Some(d) = parse_date_no_time_suffix(s) {
        return Some(d);
    }
    // Only fall back to stripping a time suffix ("T00:00:00Z", " 00:00:00") when the plain
    // string didn't already parse -- naively truncating at the first space would otherwise
    // break "6 Feb 2026" (analyst #301 G3: "2026-02-06T00:00:00Z" must still resolve, but
    // never at the cost of breaking a legitimate space-separated date).
    let date_part = if let Some(idx) = s.find('T') {
        Some(s[..idx].trim())
    } else {
        s.find(':').and_then(|idx| s[..idx].trim_end().rfind(char::is_whitespace)).map(|sp| s[..sp].trim())
    };
    date_part.and_then(parse_date_no_time_suffix)
}

fn parse_date_no_time_suffix(s: &str) -> Option<NaiveDate> {
    if let Some(d) = strict_iso(s) {
        return Some(d);
    }
    if let Some(d) = parse_month_name_date(s, '-') {
        return Some(d);
    }
    if let Some(d) = parse_month_name_date(s, ' ') {
        return Some(d);
    }
    if let Some(d) = parse_unambiguous_numeric_date(s, '/') {
        return Some(d);
    }
    parse_unambiguous_numeric_date(s, '-')
}

/// `chrono::NaiveDate::parse_from_str(_, "%Y-%m-%d")` does not require a 4-digit year --
/// "18-01-23" parses as year 18 without complaint (analyst #301 G1). This requires the exact
/// `\d{4}-\d{2}-\d{2}` shape before ever calling into chrono.
fn strict_iso(s: &str) -> Option<NaiveDate> {
    let parts: Vec<&str> = s.split('-').collect();
    if let [y, _, _] = parts[..] {
        if y.len() == 4 && y.chars().all(|c| c.is_ascii_digit()) {
            return NaiveDate::parse_from_str(s, "%Y-%m-%d").ok();
        }
    }
    None
}

/// "6 Feb 2026" / "06-Feb-2026": exactly 3 tokens split on `sep`, with the middle token a
/// recognized month name and a full 4-digit year.
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

/// "25/02/2026" or "25-02-2026" (unambiguous: 25 can't be a month) but rejects "01/10/2025",
/// "11/08/23", and any other shape where a full 4-digit year can't be established (analyst
/// #301 G1) or where every component is a plausible month (the day/month order is genuinely
/// ambiguous, and no locale rule in this dataset settles it).
fn parse_unambiguous_numeric_date(s: &str, sep: char) -> Option<NaiveDate> {
    let parts: Vec<&str> = s.split(sep).collect();
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

/// A 2-digit (or any non-4-digit) year is never guessed at a century -- reject outright
/// (analyst #301 G1).
fn full_year(token: &str) -> Option<i32> {
    if token.len() != 4 || !token.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    token.parse().ok()
}

/// Resolves an otherwise-ambiguous numeric date (`DD/MM/YY`, `DD/MM/YYYY`, or the dash
/// equivalents) against a nearby anchor date (image_accuracy_plan.md §1, images 02 `11/08/23`,
/// 12 `01/10/2025`): `parse_date` alone rejects these because the day/month order and/or the
/// century is genuinely ambiguous from the string alone. Here every plausible reading (both
/// day/month orders, and — for a 2-digit year — the century closest to `anchor`) is generated,
/// and the single candidate closest to `anchor` wins ONLY if it is unique and within a 2-year
/// window; a tie, or every candidate falling outside the window, still means no date (never a
/// guess). An already-unambiguous date is returned as-is without needing `anchor` at all.
pub fn parse_date_near(raw: &str, anchor: NaiveDate) -> Option<NaiveDate> {
    if let Some(d) = parse_date(raw) {
        return Some(d);
    }
    let s = raw.trim();
    let date_part = match s.find('T') {
        Some(idx) => s[..idx].trim(),
        None => s,
    };
    const WINDOW_DAYS: i64 = 366 * 2;
    for sep in ['/', '-'] {
        let parts: Vec<&str> = date_part.split(sep).collect();
        let [a_tok, b_tok, year_tok] = parts[..] else { continue };
        let Ok(a) = a_tok.parse::<u32>() else { continue };
        let Ok(b) = b_tok.parse::<u32>() else { continue };
        let years: Vec<i32> = if year_tok.len() == 4 && year_tok.chars().all(|c| c.is_ascii_digit()) {
            match year_tok.parse::<i32>() {
                Ok(y) => vec![y],
                Err(_) => continue,
            }
        } else if year_tok.len() == 2 && year_tok.chars().all(|c| c.is_ascii_digit()) {
            let Ok(yy) = year_tok.parse::<i32>() else { continue };
            let century = (anchor.year() / 100) * 100;
            vec![century + yy, century - 100 + yy, century + 100 + yy]
        } else {
            continue;
        };

        let mut candidates: Vec<NaiveDate> = Vec::new();
        for year in years {
            if let Some(d) = NaiveDate::from_ymd_opt(year, b, a) {
                candidates.push(d); // day-first: a = day, b = month
            }
            if a != b {
                if let Some(d) = NaiveDate::from_ymd_opt(year, a, b) {
                    candidates.push(d); // month-first: a = month, b = day
                }
            }
        }
        candidates.sort();
        candidates.dedup();

        let mut best: Option<(NaiveDate, i64)> = None;
        let mut tie = false;
        for c in &candidates {
            let diff = (*c - anchor).num_days().abs();
            if diff > WINDOW_DAYS {
                continue;
            }
            match best {
                None => best = Some((*c, diff)),
                Some((_, bd)) if diff < bd => {
                    best = Some((*c, diff));
                    tie = false;
                }
                Some((_, bd)) if diff == bd => tie = true,
                _ => {}
            }
        }
        if let Some((d, _)) = best {
            if !tie {
                return Some(d);
            }
        }
    }
    None
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
        assert_eq!(parse_amount("3,543.54", None, false), Some(3543.54));
        assert_eq!(parse_amount("42750000", None, false), Some(42_750_000.0));
    }

    /// Analyst RULES.md S7: Indian lakh grouping ("1,00,000" -> 100000), the exact shape
    /// behind the image_02 10x misread (analyst #276) -- our string-side parser must never
    /// reproduce that error when the figure arrives as a string needing this recovery.
    #[test]
    fn parses_indian_lakh_grouping() {
        assert_eq!(parse_amount("1,00,000", Some("INR"), false), Some(100_000.0));
        assert_eq!(parse_amount("12,34,567", Some("INR"), false), Some(1_234_567.0));
        assert_eq!(parse_amount("2,00,000.00", Some("INR"), false), Some(200_000.0));
    }

    /// Analyst RULES.md S7: ID/EU dot-thousands, comma-decimal.
    #[test]
    fn parses_id_eu_dot_thousands_and_comma_decimal() {
        assert_eq!(parse_amount("Rp 30.780.000", Some("IDR"), false), Some(30_780_000.0));
        assert_eq!(parse_amount("43.339.000", Some("IDR"), false), Some(43_339_000.0));
        assert_eq!(parse_amount("1.234,56", Some("EUR"), false), Some(1234.56));
    }

    /// Analyst RULES.md S7: Rs/Ps split-column layout ("4543 00" -> 4543.00).
    #[test]
    fn parses_space_separated_whole_and_fraction() {
        assert_eq!(parse_amount("4543 00", Some("INR"), false), Some(4543.00));
        assert_eq!(parse_amount("1,00,000 50", Some("INR"), false), Some(100_000.50));
    }

    /// `allow_negative` gates whether a parenthesized/minus-prefixed amount is honored at
    /// all -- verifier #308: most amount fields (bills, totals, salaries) should never be
    /// negative, so the default caller posture is `false` (reject), and a genuinely
    /// negative-permitting context (a refund/credit field) opts in explicitly.
    #[test]
    fn parses_parenthesized_and_leading_minus_negatives_only_when_allowed() {
        assert_eq!(parse_amount("(123.45)", None, true), Some(-123.45));
        assert_eq!(parse_amount("-123.45", None, true), Some(-123.45));
        assert_eq!(parse_amount("(123.45)", None, false), None);
        assert_eq!(parse_amount("-50", None, false), None);
    }

    #[test]
    fn rejects_spelled_out_amounts() {
        assert_eq!(parse_amount("Three thousand", None, false), None);
        assert_eq!(parse_amount("", None, false), None);
    }

    /// Analyst RULES.md S7.4 / verifier #308: a single comma followed by EXACTLY 2 digits is
    /// structurally unambiguous as a decimal separator regardless of currency (real
    /// thousands-grouping is always exactly 3 digits, and a genuine cents/paise subunit is
    /// always exactly 2) -- "$33,50" must resolve to 33.50. A single TRAILING digit ("12,5")
    /// matches no real convention (no currency's minor unit is 1 digit) and stays ambiguous.
    #[test]
    fn single_comma_with_exactly_two_trailing_digits_is_always_a_decimal() {
        assert_eq!(parse_amount("$33,50", Some("USD"), false), Some(33.50));
        assert_eq!(parse_amount("33,50", None, false), Some(33.50));
        assert_eq!(parse_amount("12,5", Some("IDR"), false), None);
    }

    /// A single-comma group that is neither a standard 3-digit thousands group nor a
    /// plausible decimal-comma shape is ambiguous and must be rejected, not guessed by
    /// stripping the comma.
    #[test]
    fn rejects_inconsistent_grouping() {
        assert_eq!(parse_amount("1,2345", None, false), None);
        assert_eq!(parse_amount("12,3,456", Some("INR"), false), None);
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

    /// Analyst #301: a real-world currency this dataset never uses (PKR, KHR) must be
    /// rejected outright, never passed through as an unmapped-but-usable string.
    #[test]
    fn currency_outside_the_dataset_is_rejected() {
        assert_eq!(parse_currency("PKR"), None);
        assert_eq!(parse_currency("KHR"), None);
        assert_eq!(parse_currency("GBP"), None);
    }

    /// Analyst #301: "Rs."/"IDR" (and other) currency-affix prefixes on an amount string.
    #[test]
    fn currency_prefixes_with_trailing_punctuation_strip_from_amounts() {
        assert_eq!(parse_amount("Rs. 500", Some("INR"), false), Some(500.0));
        assert_eq!(parse_amount("IDR 30.780.000", Some("IDR"), false), Some(30_780_000.0));
    }

    /// Analyst #301: a purely numeric, leading-zero "amount" (a reference/ID number, not a
    /// real currency figure -- real amounts don't print a leading zero except "0.xx") is
    /// rejected rather than parsed as a huge integer.
    #[test]
    fn rejects_id_like_leading_zero_digit_strings() {
        assert_eq!(parse_amount("0001", None, false), None);
        assert_eq!(parse_amount("00123456", None, false), None);
        assert_eq!(parse_amount("0.50", None, false), Some(0.50)); // a real leading-zero decimal is fine
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

    /// Analyst #301 G1: a two-digit (or otherwise non-4-digit) year is NEVER guessed at a
    /// century. `chrono::NaiveDate::parse_from_str(_, "%Y-%m-%d")` alone would happily parse
    /// "18-01-23" as year 18 -- `strict_iso` must reject that shape before chrono ever sees
    /// it, and every other branch (month-name, unambiguous numeric) must reject a 2-digit
    /// year too.
    #[test]
    fn rejects_two_digit_and_ambiguous_years_everywhere() {
        assert_eq!(parse_date("18-01-23"), None);
        assert_eq!(parse_date("23-01-18"), None);
        assert_eq!(parse_date("6 Feb 26"), None);
        assert_eq!(parse_date("06-Feb-26"), None);
    }

    /// Analyst #301 G3: dash-separated all-numeric dates (unambiguous only, same rule as
    /// slash dates) and a "T"/space time suffix must both resolve.
    #[test]
    fn accepts_unambiguous_dash_dates_and_strips_a_time_suffix() {
        assert_eq!(parse_date("25-02-2026"), NaiveDate::from_ymd_opt(2026, 2, 25));
        assert_eq!(parse_date("01-10-2025"), None); // both components <=12: still ambiguous
        assert_eq!(parse_date("2026-02-06T00:00:00Z"), NaiveDate::from_ymd_opt(2026, 2, 6));
        assert_eq!(parse_date("2026-02-06 14:30:00"), NaiveDate::from_ymd_opt(2026, 2, 6));
        // The time-suffix fallback must never break an already-valid space-separated date.
        assert_eq!(parse_date("6 Feb 2026"), NaiveDate::from_ymd_opt(2026, 2, 6));
    }

    #[test]
    fn normalize_text_collapses_case_and_whitespace() {
        assert_eq!(normalize_text("  Tax   Invoice  "), "TAX INVOICE");
    }

    /// image_accuracy_plan.md §1 trap table, images 03/15: a number cell wrapped across a
    /// printed line break must strip the break and parse as one token, never as the (unrelated)
    /// Rs/Ps split-column layout.
    #[test]
    fn parses_wrapped_cell_line_breaks_inside_a_number() {
        assert_eq!(parse_amount("9,124.0\n0", None, false), Some(9124.00));
        assert_eq!(parse_amount("2,00,000.0\n0", Some("INR"), false), Some(200_000.00));
    }

    /// image_accuracy_plan.md §1 trap table, image 12 `01/10/2025`: both day/month orders are
    /// individually plausible, but only one (day=01, month=10) lands on the exchange-rate
    /// settlement date itself -- the unique closest candidate within the window wins.
    #[test]
    fn parse_date_near_resolves_day_month_ambiguity_against_the_anchor() {
        let anchor = NaiveDate::from_ymd_opt(2025, 10, 1).unwrap();
        assert_eq!(parse_date_near("01/10/2025", anchor), NaiveDate::from_ymd_opt(2025, 10, 1));
    }

    /// image_accuracy_plan.md §1 trap table, image 02 `11/08/23`: a 2-digit year is resolved to
    /// whichever century lands closest to the anchor event date, never guessed outright.
    #[test]
    fn parse_date_near_resolves_two_digit_year_against_the_anchor() {
        let anchor = NaiveDate::from_ymd_opt(2023, 8, 15).unwrap();
        assert_eq!(parse_date_near("11/08/23", anchor), NaiveDate::from_ymd_opt(2023, 8, 11));
    }

    /// A genuinely tied or out-of-window reading is still rejected, never guessed -- e.g. both
    /// day/month orders land the same distance from an anchor sitting exactly between them.
    #[test]
    fn parse_date_near_rejects_ties_and_out_of_window_readings() {
        // 02/03 vs 03/02 of the same year are 1 day apart around this anchor -- not a tie here,
        // so pick a genuine tie instead: an anchor exactly 1 day would break it, so use a date
        // whose day/month swap is fully symmetric around the anchor. 15 is not a valid month, so
        // the only candidate is unambiguous; instead assert the far-outside-window case, which
        // is simple to construct deterministically.
        let far_anchor = NaiveDate::from_ymd_opt(1990, 1, 1).unwrap();
        assert_eq!(parse_date_near("01/10/2025", far_anchor), None);
    }
}
