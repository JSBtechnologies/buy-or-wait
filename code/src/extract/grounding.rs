//! Literal-grounding rejection (PLAN.md §2.10/§3): a number a model claims to have read
//! from a message must actually appear in that message's text, or the fact is rejected
//! before it ever reaches the ledger. Images are grounded by arithmetic reconciliation
//! instead (`extract::images::reconciles`), since a receipt figure has no separate source
//! text to check against.

/// True if `amount` appears in `text` as a number, tolerant of thousands separators,
/// currency-code prefixes ("IDR 42750000"), and decimal formatting ("1,037.52" / "1037.52").
pub fn amount_grounded(amount: f64, text: &str) -> bool {
    let normalized: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    [format_plain(amount), format_grouped(amount)]
        .iter()
        .any(|c| normalized.contains(c.as_str()))
}

fn format_plain(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        let s = format!("{v:.2}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

fn format_grouped(v: f64) -> String {
    let plain = format_plain(v);
    let (int_part, frac_part) = match plain.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (plain.as_str(), None),
    };
    let neg = int_part.starts_with('-');
    let digits = int_part.trim_start_matches('-');
    let mut grouped = String::new();
    for (i, ch) in digits.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    let grouped: String = grouped.chars().rev().collect();
    let mut out = if neg { format!("-{grouped}") } else { grouped };
    if let Some(f) = frac_part {
        out.push('.');
        out.push_str(f);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grounds_plain_and_grouped_amounts() {
        assert!(amount_grounded(42750000.0, "Gaji bulanan Anda naik menjadi IDR 42750000."));
        assert!(amount_grounded(1037.52, "Your temporary monthly pay is EUR 1,037.52."));
        assert!(!amount_grounded(9999.0, "Your temporary monthly pay is EUR 1037.52."));
    }
}
