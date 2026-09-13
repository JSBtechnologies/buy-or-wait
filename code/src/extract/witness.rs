//! Witness checks for an image-derived figure (image_accuracy_plan.md §2, owner: ml-engineer
//! for Phase A). Two independent reads agreeing on a normalized figure is necessary but not
//! sufficient (analyst audit #276: two reads of the same model can share the same misread) --
//! a figure `F` is only trusted once some INDEPENDENT arithmetic or textual identity on the
//! page itself proves it, and no other final-labeled figure on the page contradicts it.
//!
//! Two rules the gate must never violate (user directive, board `decision.accuracy_first`):
//! - a detailed breakdown that does NOT sum to `F` is a note, never a rejection -- pages are
//!   often cut off (image_11's Professional Fees section has no Subtotal row, unlike every
//!   other section on the same bill) -- so `find_witness` only ever looks for a PASSING
//!   identity and never fails the gate on a non-matching one;
//! - a final printed amount (Total, Grand Total, Total paid, Amount Payable, Balance/Balance
//!   Due, Net Pay) IS authoritative, so `final_label_contradicts` treats disagreement among
//!   those fields as a hard contradiction. `amount_paid` is deliberately excluded from that
//!   set: it legitimately differs from the target in a cash-tendered-with-change receipt or a
//!   still-unpaid pending/scheduled bill (image_11: `amount_paid = 0`, target = 3,650).

use crate::extract::images::ImageFigures;

fn approx_eq(a: f64, b: f64, tolerance: f64) -> bool {
    (a - b).abs() <= tolerance
}

/// English (+ Indonesian currency-name) number words to a value, tolerant of Indian lakh/crore
/// scale words and a trailing paise/cents/sen subunit clause (image_accuracy_plan.md §1 "Amount
/// in words" trap, images 01/05/06/08/09/10/16). Returns `None` when no recognizable number
/// word is found at all -- never a guess at a partially-parsed phrase.
pub fn words_to_number(raw: &str) -> Option<f64> {
    let cleaned: String = raw
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphabetic() { c } else { ' ' })
        .collect();
    let tokens: Vec<&str> = cleaned.split_whitespace().collect();
    if tokens.is_empty() {
        return None;
    }

    const SUBUNIT_KEYWORDS: &[&str] = &["paise", "paisa", "cents", "cent", "sen"];
    if let Some(kw_idx) = tokens.iter().position(|t| SUBUNIT_KEYWORDS.contains(t)) {
        let mut frac_start = 0;
        for i in (0..kw_idx).rev() {
            if tokens[i] == "and" {
                frac_start = i + 1;
                break;
            }
        }
        let frac_tokens = filter_stop_words(&tokens[frac_start..kw_idx]);
        let main_tokens = filter_stop_words(&tokens[..frac_start]);
        let main_value = parse_integer_words(&main_tokens).unwrap_or(0.0);
        let frac_value = parse_integer_words(&frac_tokens).map(|v| v / 100.0).unwrap_or(0.0);
        if main_tokens.is_empty() && frac_tokens.is_empty() {
            return None;
        }
        return Some(main_value + frac_value);
    }

    let main_tokens = filter_stop_words(&tokens);
    parse_integer_words(&main_tokens)
}

const STOP_WORDS: &[&str] = &[
    "rupees", "rupee", "dollars", "dollar", "rupiah", "rupiahs", "idr", "inr", "usd", "only",
    "of", "exactly", "and",
];

fn filter_stop_words<'a>(tokens: &[&'a str]) -> Vec<&'a str> {
    tokens.iter().filter(|t| !STOP_WORDS.contains(t)).copied().collect()
}

const ONES_AND_TEENS: &[(&str, f64)] = &[
    ("zero", 0.0), ("one", 1.0), ("two", 2.0), ("three", 3.0), ("four", 4.0), ("five", 5.0),
    ("six", 6.0), ("seven", 7.0), ("eight", 8.0), ("nine", 9.0), ("ten", 10.0),
    ("eleven", 11.0), ("twelve", 12.0), ("thirteen", 13.0), ("fourteen", 14.0),
    ("fifteen", 15.0), ("sixteen", 16.0), ("seventeen", 17.0), ("eighteen", 18.0),
    ("nineteen", 19.0),
];
const TENS: &[(&str, f64)] = &[
    ("twenty", 20.0), ("thirty", 30.0), ("forty", 40.0), ("fifty", 50.0), ("sixty", 60.0),
    ("seventy", 70.0), ("eighty", 80.0), ("ninety", 90.0),
];
/// `hundred` multiplies the current segment in place; every other scale word closes the
/// segment out into `total` and starts a fresh one (standard long-form number-word grammar).
const SCALES: &[(&str, f64)] = &[
    ("hundred", 100.0), ("thousand", 1_000.0), ("lakh", 100_000.0), ("lac", 100_000.0),
    ("crore", 10_000_000.0), ("million", 1_000_000.0),
];

fn word_value(tok: &str) -> Option<f64> {
    ONES_AND_TEENS.iter().chain(TENS).find(|(w, _)| *w == tok).map(|(_, v)| *v)
}

fn scale_value(tok: &str) -> Option<f64> {
    SCALES.iter().find(|(w, _)| *w == tok).map(|(_, v)| *v)
}

/// An unrecognized token anywhere in the phrase rejects the whole parse (`None`) rather than
/// silently dropping it and guessing a partial number.
fn parse_integer_words(tokens: &[&str]) -> Option<f64> {
    if tokens.is_empty() {
        return None;
    }
    let mut total = 0.0;
    let mut segment = 0.0;
    for tok in tokens {
        if let Some(v) = word_value(tok) {
            segment += v;
        } else if let Some(scale) = scale_value(tok) {
            let base = if segment == 0.0 { 1.0 } else { segment };
            if scale == 100.0 {
                segment = base * scale;
            } else {
                total += base * scale;
                segment = 0.0;
            }
        } else {
            return None;
        }
    }
    Some(total + segment)
}

/// The kind of witness that proved a figure, for provenance/audit (`docs/bakeoff.md`'s per
/// image × run table) -- never surfaced in `decision_explanation` beyond a plain label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WitnessKind {
    LineItemSum,
    SubtotalPlusCharges,
    SubtotalPlusTax,
    GrossMinusDeductions,
    PaidPlusBalance,
    TotalMinusPaid,
    AmountInWords,
    RepeatedFinalLabel,
    CutoffAfterExceedsWitnessedBefore,
}

impl WitnessKind {
    pub fn label(self) -> &'static str {
        match self {
            WitnessKind::LineItemSum => "line_item_sum",
            WitnessKind::SubtotalPlusCharges => "subtotal_plus_charges",
            WitnessKind::SubtotalPlusTax => "subtotal_plus_tax",
            WitnessKind::GrossMinusDeductions => "gross_minus_deductions",
            WitnessKind::PaidPlusBalance => "paid_plus_balance",
            WitnessKind::TotalMinusPaid => "total_minus_paid",
            WitnessKind::AmountInWords => "amount_in_words",
            WitnessKind::RepeatedFinalLabel => "repeated_final_label",
            WitnessKind::CutoffAfterExceedsWitnessedBefore => "cutoff_after_exceeds_witnessed_before",
        }
    }
}

/// Every final-labeled field this dataset's documents print (Total, Grand Total, Total
/// paid/Amount Payable/Balance/Balance Due -> `amount_due`/`balance_due`, Net Pay). Deliberately
/// excludes `amount_paid` (see module doc) and `previous_balance` (a carry-forward figure, not
/// this document's own final amount).
fn final_label_fields(figures: &ImageFigures) -> [(&'static str, Option<f64>); 5] {
    [
        ("total", figures.total),
        ("grand_total", figures.grand_total),
        ("amount_due", figures.amount_due),
        ("balance_due", figures.balance_due),
        ("net_pay", figures.net_pay),
    ]
}

/// True when `target` is exactly the page's own `subtotal` (bus topic `bakeoff` #6, lead
/// ruling, image_04): a line-item sum only ever proves the SUBTOTAL by construction -- it is
/// never by itself proof of a genuinely distinct final total. A cropped or otherwise
/// incomplete page can have the model duplicate its subtotal into the `total` field with
/// nothing left to distinguish the two (image_04: `total = subtotal = 2,854`, the real total
/// -- a partially-visible delivery fee below the crop -- never read). Generic on the VALUE
/// relationship, never on an image id: whenever `target` and `subtotal` coincide, an item-sum
/// witness contributes nothing, regardless of which image it came from.
fn target_is_bare_subtotal(figures: &ImageFigures, target: f64, tolerance: f64) -> bool {
    figures.subtotal.is_some_and(|s| approx_eq(s, target, tolerance))
}

/// At least one independent identity that proves `target`, beyond the bare fact that two reads
/// picked the same number. Only ever returns a PASSING identity (see module doc: a non-summing
/// breakdown is never itself checked here as a failure, it simply contributes no witness). A
/// subtotal or item bill is never itself promoted to the event amount (module doc,
/// `target_is_bare_subtotal`) -- an item-sum witness only counts when it proves a genuine
/// final-labeled figure distinct from the bare subtotal.
pub fn find_witness(figures: &ImageFigures, target: f64, tolerance: f64) -> Option<WitnessKind> {
    if !figures.line_items.is_empty() && !target_is_bare_subtotal(figures, target, tolerance) {
        let sum: f64 = figures.line_items.iter().sum();
        if approx_eq(sum, target, tolerance) {
            return Some(WitnessKind::LineItemSum);
        }
    }
    if let Some(subtotal) = figures.subtotal {
        if !figures.charges_breakdown.is_empty() {
            let charges: f64 = figures.charges_breakdown.iter().sum();
            if approx_eq(subtotal + charges, target, tolerance) {
                return Some(WitnessKind::SubtotalPlusCharges);
            }
        }
    }
    if let (Some(sub), Some(tax)) = (figures.subtotal, figures.tax) {
        if approx_eq(sub + tax, target, tolerance) {
            return Some(WitnessKind::SubtotalPlusTax);
        }
    }
    if let (Some(gross), Some(ded)) = (figures.gross_pay, figures.deductions) {
        if approx_eq(gross - ded, target, tolerance) {
            return Some(WitnessKind::GrossMinusDeductions);
        }
    }
    // Both `paid + balance = total` and `total - paid = balance` are the same underlying
    // identity, proving whichever side is the target -- guarded to `paid > tolerance` so a
    // still-unpaid document (`amount_paid = 0`) never trivially "proves" its own balance/total
    // field is equal to itself; that degenerate case is `RepeatedFinalLabel`'s job instead
    // (image_accuracy_plan.md §3 image_11: amount_paid = 0, target = 3,650).
    if let (Some(paid), Some(balance)) = (figures.amount_paid, figures.balance_due) {
        if paid > tolerance && approx_eq(paid + balance, target, tolerance) {
            return Some(WitnessKind::PaidPlusBalance);
        }
    }
    if let (Some(total), Some(paid)) = (figures.total, figures.amount_paid) {
        if paid > tolerance && approx_eq(total - paid, target, tolerance) {
            return Some(WitnessKind::TotalMinusPaid);
        }
    }
    if let Some(words) = figures.amount_in_words.as_deref() {
        if let Some(n) = words_to_number(words) {
            if approx_eq(n, target, tolerance) {
                return Some(WitnessKind::AmountInWords);
            }
        }
    }
    // The same final amount repeated under a second final label (e.g. image_11: Total Bill
    // Amount = Amount Payable = Balance, all 3,650) -- needs at least two DISTINCT final-label
    // fields to actually equal the target, not just one.
    let repeats = final_label_fields(figures)
        .into_iter()
        .filter_map(|(_, v)| v)
        .filter(|v| approx_eq(*v, target, tolerance))
        .count();
    if repeats >= 2 {
        return Some(WitnessKind::RepeatedFinalLabel);
    }
    // image_accuracy_plan.md §3 image_05: the after-cutoff figure is proven by exceeding an
    // independently witnessed before-cutoff figure (a late fee is a knowable positive delta),
    // not by its own sum/words identity.
    if let (Some(before), Some(after)) = (
        figures.amount_due_by_cutoff.or(figures.amount_due_before_date),
        figures.amount_due_after_cutoff.or(figures.amount_due_after_date),
    ) {
        if approx_eq(after, target, tolerance)
            && after > before
            && find_witness(figures, before, tolerance).is_some()
        {
            return Some(WitnessKind::CutoffAfterExceedsWitnessedBefore);
        }
    }
    None
}

/// `Some((field name, its value))` for the first final-labeled field that disagrees with
/// `target` beyond tolerance -- a real contradiction, per module doc. `None` means every
/// final-labeled field present either agrees with `target` or wasn't printed at all.
pub fn final_label_contradicts(figures: &ImageFigures, target: f64, tolerance: f64) -> Option<(&'static str, f64)> {
    final_label_fields(figures)
        .into_iter()
        .find_map(|(name, v)| v.filter(|v| !approx_eq(*v, target, tolerance)).map(|v| (name, v)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rounding tolerance for a two-term identity (line items, subtotal+tax, gross-deductions,
    /// paid+balance) — matches `images::ROUNDING_TOLERANCE_2TERM`; test-only, since real
    /// callers (`extract::images`) always pass their own tolerance explicitly.
    const WITNESS_TOLERANCE: f64 = 1.0;

    #[test]
    fn words_to_number_parses_lakh_and_crore_scale_words() {
        assert_eq!(words_to_number("Rupees Two Lakh Only"), Some(200_000.0));
        // 1 crore (10,000,000) + 25,000 = 10,025,000.
        assert_eq!(words_to_number("One Crore Twenty Five Thousand"), Some(10_025_000.0));
    }

    #[test]
    fn words_to_number_parses_hundreds_with_and_without_and() {
        assert_eq!(words_to_number("Nine Thousand One Hundred and Twenty Four"), Some(9124.0));
        assert_eq!(words_to_number("Nine Thousand One Hundred Twenty Four"), Some(9124.0));
    }

    /// image_accuracy_plan.md §3 image_05 witness: the pre-cutoff figure is proven by the
    /// words, e.g. "Rupees Seven Hundred Four and Five Paise Only" -> 704.05.
    #[test]
    fn words_to_number_parses_a_paise_subunit_clause() {
        assert_eq!(words_to_number("Rupees Seven Hundred Four and Five Paise Only"), Some(704.05));
    }

    #[test]
    fn words_to_number_rejects_unrecognized_tokens() {
        assert_eq!(words_to_number("see attached schedule"), None);
        assert_eq!(words_to_number(""), None);
    }

    fn figures_with(f: impl FnOnce(&mut ImageFigures)) -> ImageFigures {
        let mut figures = ImageFigures::default();
        f(&mut figures);
        figures
    }

    /// image_accuracy_plan.md §3 image_02: 2,00,000 total minus 1,00,000 received witnesses the
    /// 100,000 balance due.
    #[test]
    fn total_minus_paid_witnesses_the_balance_due() {
        let figures = figures_with(|f| {
            f.total = Some(200_000.0);
            f.amount_paid = Some(100_000.0);
            f.balance_due = Some(100_000.0);
        });
        assert_eq!(find_witness(&figures, 100_000.0, WITNESS_TOLERANCE), Some(WitnessKind::TotalMinusPaid));
    }

    /// bus topic `bakeoff` #6 (lead ruling, image_04): a cropped page can duplicate its
    /// subtotal into `total`, with the line items summing to exactly that same value -- an
    /// item-sum witness must NOT confirm this, since it only ever proves the subtotal, never a
    /// genuinely distinct final total. With no other final-labeled field to corroborate it,
    /// `find_witness` must return `None` (fail closed), not `LineItemSum`.
    #[test]
    fn line_item_sum_never_promotes_a_bare_subtotal_to_the_event_amount() {
        let figures = figures_with(|f| {
            f.subtotal = Some(2854.0);
            f.total = Some(2854.0); // the model duplicated subtotal into total (cropped page)
            f.line_items = vec![95.0, 531.0, 0.0, 186.0, 122.0, 464.0, 184.0, 144.0, 75.0, 366.0, 190.0, 121.0, 376.0];
        });
        assert_eq!(find_witness(&figures, 2854.0, WITNESS_TOLERANCE), None);
        assert!(target_is_bare_subtotal(&figures, 2854.0, WITNESS_TOLERANCE));
    }

    /// When the line items themselves include a further charge beyond the subtotal (e.g. a
    /// delivery fee), their sum is genuinely distinct from the bare subtotal -- the item-sum
    /// witness still proves that larger, distinct target normally.
    #[test]
    fn line_item_sum_still_witnesses_a_target_distinct_from_the_subtotal() {
        let figures = figures_with(|f| {
            f.subtotal = Some(2854.0);
            f.total = Some(2870.0); // subtotal + a 16.0 delivery fee, genuinely distinct
            let mut items = vec![95.0, 531.0, 0.0, 186.0, 122.0, 464.0, 184.0, 144.0, 75.0, 366.0, 190.0, 121.0, 376.0];
            items.push(16.0); // the delivery fee, itself printed as a line item
            f.line_items = items;
        });
        assert_eq!(find_witness(&figures, 2870.0, WITNESS_TOLERANCE), Some(WitnessKind::LineItemSum));
    }

    /// image_accuracy_plan.md §3 image_07: round(8,122 + 203.05 + 203.05) = round(8,528.10) ~=
    /// 8,528 within the combined 2-term tolerance.
    #[test]
    fn line_item_sum_witnesses_the_grand_total_within_rounding_tolerance() {
        let figures = figures_with(|f| {
            f.line_items = vec![8122.0, 203.05, 203.05];
        });
        assert_eq!(find_witness(&figures, 8528.0, WITNESS_TOLERANCE), Some(WitnessKind::LineItemSum));
    }

    /// image_accuracy_plan.md §3 image_11: Total Bill Amount = Amount Payable = Balance = 3,650
    /// witnesses the figure even though the detailed Professional Fees breakup (likely cut off)
    /// does not sum to it -- and the non-summing breakup must never itself cause a rejection.
    #[test]
    fn repeated_final_label_witnesses_and_a_non_summing_breakdown_never_rejects() {
        let figures = figures_with(|f| {
            f.total = Some(3650.0);
            f.amount_due = Some(3650.0);
            f.balance_due = Some(3650.0);
            f.amount_paid = Some(0.0);
            // A breakdown that does NOT sum to 3,650 (likely cut off, per the plan) --
            // must not cause `find_witness` to reject; it just isn't itself the proof.
            f.line_items = vec![500.0];
        });
        assert_eq!(find_witness(&figures, 3650.0, WITNESS_TOLERANCE), Some(WitnessKind::RepeatedFinalLabel));
        assert_eq!(final_label_contradicts(&figures, 3650.0, WITNESS_TOLERANCE), None);
    }

    /// image_accuracy_plan.md §3 image_12: 28.50 + 5.00 witnesses 33.50; cash tendered (40.00)
    /// and change (6.50) are ignored -- they are not final-labeled fields.
    #[test]
    fn line_item_sum_witnesses_a_settled_receipt_total() {
        let figures = figures_with(|f| {
            f.line_items = vec![28.50, 5.00];
        });
        assert_eq!(find_witness(&figures, 33.50, WITNESS_TOLERANCE), Some(WitnessKind::LineItemSum));
    }

    /// image_accuracy_plan.md §3 image_05: 822.05 (after cutoff) is proven by exceeding the
    /// independently witnessed 704.05 (before cutoff, itself summed from 580.65+16.00+107.40).
    #[test]
    fn cutoff_after_amount_is_witnessed_by_exceeding_the_witnessed_before_amount() {
        let figures = figures_with(|f| {
            f.line_items = vec![580.65, 16.00, 107.40];
            f.amount_due_by_cutoff = Some(704.05);
            f.amount_due_after_cutoff = Some(822.05);
        });
        assert_eq!(
            find_witness(&figures, 822.05, WITNESS_TOLERANCE),
            Some(WitnessKind::CutoffAfterExceedsWitnessedBefore)
        );
    }

    /// A final-labeled figure that disagrees with the candidate target is a real contradiction.
    #[test]
    fn a_disagreeing_final_label_is_a_contradiction() {
        let figures = figures_with(|f| {
            f.total = Some(9999.0);
        });
        assert_eq!(
            final_label_contradicts(&figures, 3650.0, WITNESS_TOLERANCE),
            Some(("total", 9999.0))
        );
    }

    /// `amount_paid` disagreeing with the target (a still-unpaid pending/scheduled bill, or
    /// cash tendered with change) is never a contradiction -- it is deliberately excluded from
    /// the final-label set (module doc, image_11: amount_paid = 0, target = 3,650).
    #[test]
    fn amount_paid_disagreeing_with_the_target_is_never_a_contradiction() {
        let figures = figures_with(|f| {
            f.total = Some(3650.0);
            f.amount_paid = Some(0.0);
        });
        assert_eq!(final_label_contradicts(&figures, 3650.0, WITNESS_TOLERANCE), None);
    }

    /// A non-summing line-item breakdown alone (no other identity, no final-label match) simply
    /// yields no witness -- it must never be mistaken for a contradiction either.
    #[test]
    fn a_non_summing_breakdown_alone_yields_no_witness_and_no_contradiction() {
        let figures = figures_with(|f| {
            f.line_items = vec![500.0];
        });
        assert_eq!(find_witness(&figures, 3650.0, WITNESS_TOLERANCE), None);
        assert_eq!(final_label_contradicts(&figures, 3650.0, WITNESS_TOLERANCE), None);
    }
}
