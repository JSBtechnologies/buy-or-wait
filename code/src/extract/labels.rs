//! Deterministic keyword -> role mapping from OCR rows to `ImageFigures`
//! (fleet/specs/ocr_vllm_pipeline.md work item A3). Every role is a fixed keyword list, never
//! a model decision -- the OCR model already read the label and value correctly (the lead's
//! audit); this module's only job is mapping a printed label to the right `ImageFigures`
//! field, the same job `extract::images`'s deterministic `select` does one level up.

use chrono::NaiveDate;

use crate::extract::images::ImageFigures;
use crate::extract::normalize;
use crate::extract::ocr_parse::LabeledValue;
use crate::extract::witness;

/// Labels that never contribute a figure at all (fleet/specs/ocr_vllm_pipeline.md A3):
/// cash tendered/change are not the expense amount (analyst audit #184, image_12), "payments"
/// on an account-summary table is a running-ledger line rather than this bill's own paid
/// amount, and a bare "due date" (no till/by/before/after qualifier) is not a resolvable
/// cutoff (module doc below).
const IGNORED_LABELS: &[&str] = &["cash paid", "cash", "tendered", "change", "payments", "due date"];

const GRAND_TOTAL_KEYWORDS: &[&str] = &["grand total"];
const TOTAL_KEYWORDS: &[&str] = &[
    "total",
    "total amount",
    "total bill amount",
    "total amount to be received",
    "net amount",
    "total paid",
    "total amount received",
];
const AMOUNT_PAID_KEYWORDS: &[&str] = &["amount received", "total paid", "total amount received", "amount paid"];
const BALANCE_DUE_KEYWORDS: &[&str] = &["balance due", "balance"];
const AMOUNT_DUE_KEYWORDS: &[&str] = &["amount payable", "amount due"];
const SUBTOTAL_KEYWORDS: &[&str] = &["sub total", "subtotal", "item bill", "item total", "taxable value"];
const TAX_KEYWORDS: &[&str] = &["cgst", "sgst", "igst", "ugst", "gst", "tax", "taxes", "cess", "vat"];
const GROSS_PAY_KEYWORDS: &[&str] = &["total earnings"];
const DEDUCTIONS_KEYWORDS: &[&str] = &["total deductions"];
const NET_PAY_KEYWORDS: &[&str] = &["net pay"];
const PREVIOUS_BALANCE_KEYWORDS: &[&str] = &["previous balance"];

const CUTOFF_BEFORE_PHRASES: &[&str] = &["due till", "due by", "due before"];
const CUTOFF_AFTER_PHRASES: &[&str] = &["due after"];

/// Case/punctuation-insensitive word tokens, so `"Subtotal Earnings"` never spuriously matches
/// the single-word keyword `"total"` (it tokenizes to `["subtotal", "earnings"]` -- `"total"`
/// is not one of those tokens, unlike a naive substring check on `"subtotal"`).
fn words(s: &str) -> Vec<String> {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// True when `keyword`'s own words appear as a contiguous run inside `label_words`.
fn contains_phrase(label_words: &[String], keyword: &str) -> bool {
    let kw = words(keyword);
    if kw.is_empty() || kw.len() > label_words.len() {
        return false;
    }
    label_words.windows(kw.len()).any(|w| w == kw.as_slice())
}

fn matches_any(label_words: &[String], keywords: &[&str]) -> bool {
    keywords.iter().any(|k| contains_phrase(label_words, k))
}

/// `Some(field name)` when `existing` disagrees with `new_value` beyond a currency-rounding
/// tolerance -- fleet/specs/ocr_vllm_pipeline.md A3 "role conflicts (different values): ...
/// else None + note": a second, disagreeing read of the same role is not resolvable
/// deterministically without knowing which section is "the summary", so the conservative
/// choice is to withhold the figure entirely (`None`) rather than silently keep the first or
/// the last one seen.
fn set_or_conflict(existing: &mut Option<f64>, new_value: f64, field: &str, notes: &mut Vec<String>) {
    match *existing {
        None => *existing = Some(new_value),
        Some(v) if (v - new_value).abs() <= 0.01 => {}
        Some(v) => {
            notes.push(format!("conflict:{field}={v}|{new_value}"));
            *existing = None;
        }
    }
}

enum CutoffDirection {
    Before,
    After,
}

fn cutoff_direction(label_words: &[String]) -> Option<CutoffDirection> {
    if matches_any(label_words, CUTOFF_BEFORE_PHRASES) {
        Some(CutoffDirection::Before)
    } else if matches_any(label_words, CUTOFF_AFTER_PHRASES) {
        Some(CutoffDirection::After)
    } else {
        None
    }
}

/// A date-shaped token or short trailing window of `label` (the cutoff date is typically the
/// LAST token(s), e.g. `"Amount due till 06-Feb-2026"` -- `extract::ocr_parse` concatenates a
/// label that wrapped onto the next printed line, cutoff date included). Tries the last 1, 2,
/// then 3 whitespace tokens (widest window last, so a clean single unambiguous token wins
/// first) via `normalize::parse_date_near` against `anchor_date`.
fn extract_cutoff_date(label: &str, anchor_date: NaiveDate) -> Option<NaiveDate> {
    let tokens: Vec<&str> = label.split_whitespace().collect();
    for window in 1..=3usize.min(tokens.len()) {
        let candidate = tokens[tokens.len() - window..].join(" ");
        if let Some(d) = normalize::parse_date_near(&candidate, anchor_date) {
            return Some(d);
        }
    }
    None
}

/// Builds `ImageFigures` from every `LabeledValue` OCR recovered on the page(s), plus a note
/// for every row that didn't map to a role (conflict, or genuinely unknown label) -- audit
/// trail for `ImageReadProvenance::ocr_notes` (`extract::images`).
pub fn figures_from_rows(rows: &[LabeledValue], anchor_date: NaiveDate) -> (ImageFigures, Vec<String>) {
    let mut figures = ImageFigures::default();
    let mut notes = Vec::new();
    let mut tax_sum: Option<f64> = None;
    let mut tax_ran = false;

    for row in rows {
        let label_words = words(&row.label);
        if label_words.is_empty() || IGNORED_LABELS.iter().any(|k| matches_any(&label_words, &[k])) {
            continue;
        }

        if let Some(direction) = cutoff_direction(&label_words) {
            let Some(date) = extract_cutoff_date(&row.label, anchor_date) else {
                notes.push(format!("cutoff_unparseable:{}", row.label));
                continue;
            };
            match &figures.due_cutoff_date {
                Some(existing) if *existing != date.to_string() => {
                    notes.push(format!("conflict:due_cutoff_date={existing}|{date}"));
                }
                _ => figures.due_cutoff_date = Some(date.to_string()),
            }
            if let Some(amount) = row.amount {
                match direction {
                    CutoffDirection::Before => {
                        set_or_conflict(&mut figures.amount_due_by_cutoff, amount, "amount_due_by_cutoff", &mut notes)
                    }
                    CutoffDirection::After => {
                        set_or_conflict(&mut figures.amount_due_after_cutoff, amount, "amount_due_after_cutoff", &mut notes)
                    }
                }
            }
            continue;
        }

        let Some(amount) = row.amount else {
            if figures.amount_in_words.is_none()
                && !matches_any(&label_words, &["paid amount in words"])
                && witness::words_to_number(&row.value_raw).is_some()
            {
                figures.amount_in_words = Some(row.value_raw.clone());
            } else {
                notes.push(format!("unmapped:{}", row.label));
            }
            continue;
        };

        // Some role keywords are multi-word phrases that themselves CONTAIN a shorter, more
        // generic keyword from a different role ("Grand Total"/"Total Earnings"/"Total
        // Deductions" all contain the bare word "total"; "Previous Balance" contains the bare
        // word "balance"). Those specific, more-informative roles are checked first and, when
        // they match, the generic role is skipped entirely for this row -- word-boundary
        // matching alone (`contains_phrase`) can't tell "an unrelated total" from "this same
        // total, more specifically labeled". `total`/`amount_paid` legitimately overlap on
        // purpose instead ("total paid"/"total amount received" are listed under BOTH roles,
        // fleet/specs/ocr_vllm_pipeline.md A3) -- both are populated from the same value.
        let is_grand_total = matches_any(&label_words, GRAND_TOTAL_KEYWORDS);
        let is_gross_pay = matches_any(&label_words, GROSS_PAY_KEYWORDS);
        let is_deductions = matches_any(&label_words, DEDUCTIONS_KEYWORDS);
        let is_previous_balance = matches_any(&label_words, PREVIOUS_BALANCE_KEYWORDS);

        let mut mapped = false;
        if is_grand_total {
            set_or_conflict(&mut figures.grand_total, amount, "grand_total", &mut notes);
            mapped = true;
        }
        if is_gross_pay {
            set_or_conflict(&mut figures.gross_pay, amount, "gross_pay", &mut notes);
            mapped = true;
        }
        if is_deductions {
            set_or_conflict(&mut figures.deductions, amount, "deductions", &mut notes);
            mapped = true;
        }
        if is_previous_balance {
            set_or_conflict(&mut figures.previous_balance, amount, "previous_balance", &mut notes);
            mapped = true;
        }
        if !is_grand_total && !is_gross_pay && !is_deductions && matches_any(&label_words, TOTAL_KEYWORDS) {
            set_or_conflict(&mut figures.total, amount, "total", &mut notes);
            mapped = true;
        }
        if matches_any(&label_words, AMOUNT_PAID_KEYWORDS) {
            set_or_conflict(&mut figures.amount_paid, amount, "amount_paid", &mut notes);
            mapped = true;
        }
        if !is_previous_balance && matches_any(&label_words, BALANCE_DUE_KEYWORDS) {
            set_or_conflict(&mut figures.balance_due, amount, "balance_due", &mut notes);
            mapped = true;
        }
        if matches_any(&label_words, AMOUNT_DUE_KEYWORDS) {
            set_or_conflict(&mut figures.amount_due, amount, "amount_due", &mut notes);
            mapped = true;
        }
        if matches_any(&label_words, SUBTOTAL_KEYWORDS) {
            set_or_conflict(&mut figures.subtotal, amount, "subtotal", &mut notes);
            mapped = true;
        }
        if matches_any(&label_words, TAX_KEYWORDS) {
            tax_sum = Some(tax_sum.unwrap_or(0.0) + amount);
            tax_ran = true;
            mapped = true;
        }
        if matches_any(&label_words, NET_PAY_KEYWORDS) {
            set_or_conflict(&mut figures.net_pay, amount, "net_pay", &mut notes);
            mapped = true;
        }
        if !mapped {
            notes.push(format!("unmapped:{}={}", row.label, amount));
        }
    }
    if tax_ran {
        figures.tax = tax_sum;
    }
    (figures, notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(label: &str, value_raw: &str, amount: Option<f64>) -> LabeledValue {
        LabeledValue { page: 1, label: label.to_string(), value_raw: value_raw.to_string(), amount, date: None }
    }

    fn anchor() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 2, 9).unwrap()
    }

    /// image_02 (fleet/specs/ocr_vllm_pipeline.md A5): balance_due = 100,000, witnessed by
    /// total - amount_paid.
    #[test]
    fn image_02_shape_maps_total_paid_and_balance_due() {
        let rows = vec![
            row("Total Amount to be Receiv", "2,00,000.00", Some(200_000.0)),
            row("Amount Received:", "1,00,000.00", Some(100_000.0)),
            row("Balance Due:", "1,00,000.00", Some(100_000.0)),
        ];
        let (figures, notes) = figures_from_rows(&rows, anchor());
        assert_eq!(figures.total, Some(200_000.0));
        assert_eq!(figures.amount_paid, Some(100_000.0));
        assert_eq!(figures.balance_due, Some(100_000.0));
        assert!(notes.is_empty(), "unexpected notes: {notes:?}");
    }

    /// image_05: a wrapped cutoff label combined by ocr_parse into one string; the date is
    /// recovered from the label itself, and amount from the value cell.
    #[test]
    fn cutoff_before_and_after_labels_populate_the_split_amounts() {
        let rows = vec![
            row("Amount due till 06-Feb-2026", "704.05", Some(704.05)),
            row("Amount due after 06-Feb-2026", "822.05", Some(822.05)),
        ];
        let (figures, notes) = figures_from_rows(&rows, anchor());
        assert_eq!(figures.due_cutoff_date, Some("2026-02-06".to_string()));
        assert_eq!(figures.amount_due_by_cutoff, Some(704.05));
        assert_eq!(figures.amount_due_after_cutoff, Some(822.05));
        assert!(notes.is_empty(), "unexpected notes: {notes:?}");
    }

    /// A bare "Due Date" (no till/by/before/after) never counts as a cutoff.
    #[test]
    fn a_bare_due_date_label_is_ignored_not_a_cutoff() {
        let rows = vec![row("Due Date", "15/02/2026", None)];
        let (figures, _notes) = figures_from_rows(&rows, anchor());
        assert_eq!(figures.due_cutoff_date, None);
    }

    /// Cash tendered/change are never the expense amount (image_12, analyst audit #184).
    #[test]
    fn cash_tendered_and_change_are_ignored() {
        let rows = vec![
            row("Cash", "40.00", Some(40.0)),
            row("Change", "6.50", Some(6.50)),
            row("Total", "33.50", Some(33.50)),
        ];
        let (figures, notes) = figures_from_rows(&rows, anchor());
        assert_eq!(figures.total, Some(33.50));
        assert!(notes.is_empty(), "unexpected notes: {notes:?}");
    }

    /// Multiple GST lines sum into a single `tax` figure.
    #[test]
    fn tax_lines_are_summed() {
        let rows = vec![row("CGST", "50.00", Some(50.0)), row("SGST", "50.00", Some(50.0))];
        let (figures, _notes) = figures_from_rows(&rows, anchor());
        assert_eq!(figures.tax, Some(100.0));
    }

    /// A conflicting second read of the same role withholds the figure and notes it, rather
    /// than guessing which value is right.
    #[test]
    fn conflicting_total_values_withhold_the_figure() {
        let rows = vec![row("Total", "100.00", Some(100.0)), row("Grand Total", "200.00", Some(200.0)), row("Total", "999.00", Some(999.0))];
        let (figures, notes) = figures_from_rows(&rows, anchor());
        assert_eq!(figures.total, None);
        assert_eq!(figures.grand_total, Some(200.0));
        assert!(notes.iter().any(|n| n.starts_with("conflict:total=")));
    }

    /// image_04: "Item Bill" is a `subtotal` synonym, and with no total/grand total/amount
    /// paid/balance due anywhere on the page, no final label exists at all (fail-closed
    /// upstream in `extract::images::select`, not this module's job to decide).
    #[test]
    fn item_bill_maps_to_subtotal_only_no_other_final_label() {
        let rows = vec![row("Item Bill", "2854.00", Some(2854.0))];
        let (figures, notes) = figures_from_rows(&rows, anchor());
        assert_eq!(figures.subtotal, Some(2854.0));
        assert_eq!(figures.total, None);
        assert_eq!(figures.grand_total, None);
        assert_eq!(figures.amount_paid, None);
        assert!(notes.is_empty(), "unexpected notes: {notes:?}");
    }

    /// A spelled-out amount value is recovered into `amount_in_words`, keyed off the VALUE
    /// shape (`witness::words_to_number`), not a fixed label keyword list.
    #[test]
    fn a_words_value_is_recovered_as_amount_in_words() {
        let rows = vec![row("Total", "Seven Hundred Four Rupees and Five Paise Only", None)];
        let (figures, notes) = figures_from_rows(&rows, anchor());
        assert_eq!(figures.amount_in_words.as_deref(), Some("Seven Hundred Four Rupees and Five Paise Only"));
        assert!(notes.is_empty(), "unexpected notes: {notes:?}");
    }

    /// `"Subtotal Earnings"` must never match the single-word `total` keyword (word-boundary
    /// matching, not substring) -- a payslip's subtotal is not this dataset's `subtotal` role
    /// concept either, but it must not silently become `total`.
    #[test]
    fn subtotal_earnings_never_matches_the_bare_total_keyword() {
        let rows = vec![row("Subtotal Earnings", "4,780,800", Some(4_780_800.0))];
        let (figures, _notes) = figures_from_rows(&rows, anchor());
        assert_eq!(figures.total, None);
        assert_eq!(figures.grand_total, None);
    }

    /// image_01: "Total Earnings"/"Total Deductions"/"Net Pay" map to gross_pay/deductions/
    /// net_pay respectively.
    #[test]
    fn payslip_labels_map_to_income_roles() {
        let rows = vec![
            row("Total Earnings", "4,780,800", Some(4_780_800.0)),
            row("Total Deductions", "415,800", Some(415_800.0)),
            row("Net Pay", "4,365,000", Some(4_365_000.0)),
        ];
        let (figures, notes) = figures_from_rows(&rows, anchor());
        assert_eq!(figures.gross_pay, Some(4_780_800.0));
        assert_eq!(figures.deductions, Some(415_800.0));
        assert_eq!(figures.net_pay, Some(4_365_000.0));
        assert!(notes.is_empty(), "unexpected notes: {notes:?}");
    }
}
