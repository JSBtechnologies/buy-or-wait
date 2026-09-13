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
    "of", "exactly", "and", "indian", "us",
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
    /// User ruling `ruling.total_or_witnessed_sum` point 2 (image_06): no total field was
    /// readable at all, but the sum of printed line items/charges is independently
    /// corroborated by an amount-in-words value (within rounding).
    LineItemSumWitnessedByWords,
    /// Same fallback, corroborated instead by another printed final-label-family field
    /// equal to the sum (never the sum's own field -- that would be self-witnessed).
    LineItemSumWitnessedByLabel,
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
            WitnessKind::LineItemSumWitnessedByWords => "line_item_sum_witnessed_by_words",
            WitnessKind::LineItemSumWitnessedByLabel => "line_item_sum_witnessed_by_label",
        }
    }
}

/// Which final-labeled fields are trustworthy corroboration/contradiction for `target`,
/// depending on how `extract::images::select` chose it (bus topic `bakeoff` #16/#18, images
/// 02/05). Computed by the caller from the event's status and whether this read resolved a
/// due-date cutoff -- never from an image id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalLabelScope {
    /// `target` is meant to be the document's WHOLE final amount (a settled expense/income, or
    /// the default `_ => total` selection) -- every final-labeled field applies normally.
    Whole,
    /// `target` is a REMAINING-owed `balance_due`/`amount_due` on a pending/scheduled bill with
    /// no due-date cutoff (image_02) -- `total`/`grand_total`/`net_pay` legitimately differ (a
    /// partial payment already made) and neither corroborate nor contradict it.
    RemainingOwed,
    /// `target` is a due-date-cutoff-resolved before/after amount (image_05) -- NONE of the
    /// generic final-labeled fields are trustworthy here (observed: `total` duplicated from the
    /// pre-cutoff subtotal, `balance_due`/`amount_due` an unrelated carried-forward figure);
    /// only the cutoff identity itself (`WitnessKind::CutoffAfterExceedsWitnessedBefore`)
    /// corroborates, and nothing here can contradict it.
    CutoffResolved,
}

/// Every final-labeled field this dataset's documents print, filtered to what `scope` says is
/// trustworthy for `target` (Total, Grand Total, Total paid/Amount Payable/Balance/Balance Due
/// -> `amount_due`/`balance_due`, Net Pay). Deliberately excludes `amount_paid` (see module doc)
/// and `previous_balance` (a carry-forward figure, not this document's own final amount).
fn final_label_fields(figures: &ImageFigures, scope: FinalLabelScope) -> Vec<(&'static str, Option<f64>)> {
    match scope {
        FinalLabelScope::CutoffResolved => vec![],
        FinalLabelScope::RemainingOwed => {
            vec![("amount_due", figures.amount_due), ("balance_due", figures.balance_due)]
        }
        FinalLabelScope::Whole => vec![
            ("total", figures.total),
            ("grand_total", figures.grand_total),
            ("amount_due", figures.amount_due),
            ("balance_due", figures.balance_due),
            ("net_pay", figures.net_pay),
        ],
    }
}

/// `final_label_fields` PLUS `subtotal`/`amount_paid`, for POSITIVE repeated-label matching
/// only -- `final_label_contradicts` never calls this, so a disagreeing subtotal or amount
/// paid still never blocks `target` (module doc on `IGNORED_LABELS`/`AMOUNT_PAID_KEYWORDS` in
/// `extract::labels`, and image_11: `amount_paid = 0` legitimately differs from `target =
/// 3,650`). A printed subtotal or amount actually paid that happens to EQUAL `target` is
/// genuine independent corroboration (user ruling `ruling.total_or_witnessed_sum` point 4:
/// "Item Total" == "Total paid" for image_13 with no delivery fee, "Net Amount" == "Cash Paid"
/// for image_03).
///
/// `subtotal` is excluded when `target` IS the bare subtotal (`target_is_bare_subtotal`) --
/// otherwise a cropped page's `total` duplicated straight from `subtotal` (image_04) would
/// count as "2 matches" against itself (subtotal + the duplicate), exactly the degenerate case
/// that guard exists to block.
fn repeat_witness_family_fields(
    figures: &ImageFigures,
    target: f64,
    tolerance: f64,
    scope: FinalLabelScope,
) -> Vec<(&'static str, Option<f64>)> {
    let mut fields = final_label_fields(figures, scope);
    if !target_is_bare_subtotal(figures, target, tolerance, scope) {
        fields.push(("subtotal", figures.subtotal));
    }
    fields.push(("amount_paid", figures.amount_paid));
    fields
}

/// True when `target` is exactly the page's own `subtotal` (bus topic `bakeoff` #6, lead
/// ruling, image_04): a line-item sum only ever proves the SUBTOTAL by construction -- it is
/// never by itself proof of a genuinely distinct final total. A cropped or otherwise
/// incomplete page can have the model duplicate its subtotal into the `total` field with
/// nothing left to distinguish the two (image_04: `total = subtotal = 2,854`, the real total
/// -- a partially-visible delivery fee below the crop -- never read). Generic on the VALUE
/// relationship, never on an image id: whenever `target` and `subtotal` coincide, an item-sum
/// witness contributes nothing, regardless of which image it came from.
///
/// Only applies to `FinalLabelScope::Whole` (bus topic `bakeoff` #18, image_05): a pending/
/// scheduled bill's before-cutoff amount IS legitimately the page's own subtotal by
/// construction (the amount owed before any late fee applies) -- that is the intended witness
/// (image_accuracy_plan.md §3: "580.65+16.00+107.40 proves 704.05"), not the image_04
/// duplicated-into-`total` failure mode this guard exists for.
fn target_is_bare_subtotal(figures: &ImageFigures, target: f64, tolerance: f64, scope: FinalLabelScope) -> bool {
    scope == FinalLabelScope::Whole && figures.subtotal.is_some_and(|s| approx_eq(s, target, tolerance))
}

/// At least one independent identity that proves `target`, beyond the bare fact that two reads
/// picked the same number. Only ever returns a PASSING identity (see module doc: a non-summing
/// breakdown is never itself checked here as a failure, it simply contributes no witness). A
/// subtotal or item bill is never itself promoted to the event amount (module doc,
/// `target_is_bare_subtotal`) -- an item-sum witness only counts when it proves a genuine
/// final-labeled figure distinct from the bare subtotal.
///
/// Returns `(kind, computed)`: `computed` is the identity's own arithmetic/corroborating result
/// (bus topic `bakeoff` #16/#18, engine's `Fact::AmountWitness`) -- usually within `tolerance`
/// of `target` but not always identical to it (image_07: `target` = the Grand Total 8,528;
/// `computed` = the plain Total's 8,528.10, which rounds to it). Never itself a contradiction
/// (`final_label_contradicts` already tolerates the same rounding gap): a printed Total that
/// rounds to a distinct Grand Total is exactly what `RepeatedFinalLabel` treats as corroboration.
pub fn find_witness(
    figures: &ImageFigures,
    target: f64,
    tolerance: f64,
    scope: FinalLabelScope,
) -> Option<(WitnessKind, f64)> {
    if !figures.line_items.is_empty() && !target_is_bare_subtotal(figures, target, tolerance, scope) {
        let sum: f64 = figures.line_items.iter().sum();
        if approx_eq(sum, target, tolerance) {
            return Some((WitnessKind::LineItemSum, sum));
        }
    }
    if let Some(subtotal) = figures.subtotal {
        if !figures.charges_breakdown.is_empty() {
            let charges: f64 = figures.charges_breakdown.iter().sum();
            let computed = subtotal + charges;
            if approx_eq(computed, target, tolerance) {
                return Some((WitnessKind::SubtotalPlusCharges, computed));
            }
        }
    }
    if let (Some(sub), Some(tax)) = (figures.subtotal, figures.tax) {
        let computed = sub + tax;
        if approx_eq(computed, target, tolerance) {
            return Some((WitnessKind::SubtotalPlusTax, computed));
        }
    }
    if let (Some(gross), Some(ded)) = (figures.gross_pay, figures.deductions) {
        let computed = gross - ded;
        if approx_eq(computed, target, tolerance) {
            return Some((WitnessKind::GrossMinusDeductions, computed));
        }
    }
    // Both `paid + balance = total` and `total - paid = balance` are the same underlying
    // identity, proving whichever side is the target -- guarded to `paid > tolerance` so a
    // still-unpaid document (`amount_paid = 0`) never trivially "proves" its own balance/total
    // field is equal to itself; that degenerate case is `RepeatedFinalLabel`'s job instead
    // (image_accuracy_plan.md §3 image_11: amount_paid = 0, target = 3,650).
    if let (Some(paid), Some(balance)) = (figures.amount_paid, figures.balance_due) {
        let computed = paid + balance;
        if paid > tolerance && approx_eq(computed, target, tolerance) {
            return Some((WitnessKind::PaidPlusBalance, computed));
        }
    }
    if let (Some(total), Some(paid)) = (figures.total, figures.amount_paid) {
        let computed = total - paid;
        if paid > tolerance && approx_eq(computed, target, tolerance) {
            return Some((WitnessKind::TotalMinusPaid, computed));
        }
    }
    if let Some(words) = figures.amount_in_words.as_deref() {
        if let Some(n) = words_to_number(words) {
            if approx_eq(n, target, tolerance) {
                return Some((WitnessKind::AmountInWords, n));
            }
        }
    }
    // The same final amount repeated under a second final label (e.g. image_11: Total Bill
    // Amount = Amount Payable = Balance, all 3,650; image_07: Total 8,528.10 rounds to Grand
    // Total 8,528; image_03: Net Amount = Cash Paid; image_13: Total paid = Item Total, no
    // delivery fee) -- needs at least two DISTINCT final-label-FAMILY fields to actually equal
    // the target, not just one. `computed` surfaces a genuinely different corroborating value
    // when one exists, else the corroborating value is identical to `target`.
    let matches: Vec<f64> = repeat_witness_family_fields(figures, target, tolerance, scope)
        .into_iter()
        .filter_map(|(_, v)| v)
        .filter(|v| approx_eq(*v, target, tolerance))
        .collect();
    if matches.len() >= 2 {
        let computed = matches.iter().find(|v| !approx_eq(**v, target, 1e-9)).copied().unwrap_or(target);
        return Some((WitnessKind::RepeatedFinalLabel, computed));
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
            && find_witness(figures, before, tolerance, scope).is_some()
        {
            return Some((WitnessKind::CutoffAfterExceedsWitnessedBefore, after));
        }
    }
    None
}

/// `Some((field name, its value))` for the first final-labeled field that disagrees with
/// `target` beyond tolerance -- a real contradiction, per module doc. `None` means every
/// final-labeled field present either agrees with `target` or wasn't printed at all.
/// `scope`: see `final_label_fields`/`FinalLabelScope`.
pub fn final_label_contradicts(
    figures: &ImageFigures,
    target: f64,
    tolerance: f64,
    scope: FinalLabelScope,
) -> Option<(&'static str, f64)> {
    final_label_fields(figures, scope)
        .into_iter()
        .find_map(|(name, v)| v.filter(|v| !approx_eq(*v, target, tolerance)).map(|v| (name, v)))
}

/// User ruling `ruling.total_or_witnessed_sum` point 2: when `extract::images::select` finds
/// NO final-label field readable at all (image_06 -- no total/grand total/amount paid/balance
/// due/amount due anywhere on the page), fall back to the sum of whatever line items/charges
/// were printed, accepted ONLY when an INDEPENDENT witness corroborates that sum -- never
/// self-witnessed (the sum can't prove itself). The accepted value is the WITNESS's own
/// figure, not necessarily the raw sum (image_06: words say 1,995, the items sum to 1,994.99 --
/// accept 1,995, the printed witness). Tries `amount_in_words` first, then any
/// `repeat_witness_family_fields` value that happens to equal the sum.
pub fn witnessed_line_item_sum(figures: &ImageFigures, scope: FinalLabelScope, tolerance: f64) -> Option<(f64, WitnessKind)> {
    if figures.line_items.is_empty() && figures.charges_breakdown.is_empty() {
        return None;
    }
    let sum: f64 = figures.line_items.iter().sum::<f64>() + figures.charges_breakdown.iter().sum::<f64>();

    if let Some(words) = figures.amount_in_words.as_deref() {
        if let Some(words_value) = words_to_number(words) {
            if approx_eq(words_value, sum, tolerance) {
                return Some((words_value, WitnessKind::LineItemSumWitnessedByWords));
            }
        }
    }
    repeat_witness_family_fields(figures, sum, tolerance, scope)
        .into_iter()
        .find_map(|(_, v)| v.filter(|v| approx_eq(*v, sum, tolerance)))
        .map(|v| (v, WitnessKind::LineItemSumWitnessedByLabel))
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
        assert_eq!(find_witness(&figures, 100_000.0, WITNESS_TOLERANCE, FinalLabelScope::Whole), Some((WitnessKind::TotalMinusPaid, 100_000.0)));
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
        assert_eq!(find_witness(&figures, 2854.0, WITNESS_TOLERANCE, FinalLabelScope::Whole), None);
        assert!(target_is_bare_subtotal(&figures, 2854.0, WITNESS_TOLERANCE, FinalLabelScope::Whole));
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
        assert_eq!(find_witness(&figures, 2870.0, WITNESS_TOLERANCE, FinalLabelScope::Whole), Some((WitnessKind::LineItemSum, 2870.0)));
    }

    /// image_accuracy_plan.md §3 image_07: round(8,122 + 203.05 + 203.05) = round(8,528.10) ~=
    /// 8,528 within the combined 2-term tolerance.
    #[test]
    fn line_item_sum_witnesses_the_grand_total_within_rounding_tolerance() {
        let figures = figures_with(|f| {
            f.line_items = vec![8122.0, 203.05, 203.05];
        });
        let (kind, computed) = find_witness(&figures, 8528.0, WITNESS_TOLERANCE, FinalLabelScope::Whole).expect("should witness");
        assert_eq!(kind, WitnessKind::LineItemSum);
        assert!((computed - 8528.10).abs() < 1e-6);
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
        assert_eq!(find_witness(&figures, 3650.0, WITNESS_TOLERANCE, FinalLabelScope::Whole), Some((WitnessKind::RepeatedFinalLabel, 3650.0)));
        assert_eq!(final_label_contradicts(&figures, 3650.0, WITNESS_TOLERANCE, FinalLabelScope::Whole), None);
    }

    /// image_accuracy_plan.md §3 image_12: 28.50 + 5.00 witnesses 33.50; cash tendered (40.00)
    /// and change (6.50) are ignored -- they are not final-labeled fields.
    #[test]
    fn line_item_sum_witnesses_a_settled_receipt_total() {
        let figures = figures_with(|f| {
            f.line_items = vec![28.50, 5.00];
        });
        assert_eq!(find_witness(&figures, 33.50, WITNESS_TOLERANCE, FinalLabelScope::Whole), Some((WitnessKind::LineItemSum, 33.50)));
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
            find_witness(&figures, 822.05, WITNESS_TOLERANCE, FinalLabelScope::CutoffResolved),
            Some((WitnessKind::CutoffAfterExceedsWitnessedBefore, 822.05))
        );
    }

    /// A final-labeled figure that disagrees with the candidate target is a real contradiction.
    #[test]
    fn a_disagreeing_final_label_is_a_contradiction() {
        let figures = figures_with(|f| {
            f.total = Some(9999.0);
        });
        assert_eq!(
            final_label_contradicts(&figures, 3650.0, WITNESS_TOLERANCE, FinalLabelScope::Whole),
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
        assert_eq!(final_label_contradicts(&figures, 3650.0, WITNESS_TOLERANCE, FinalLabelScope::Whole), None);
    }

    /// A non-summing line-item breakdown alone (no other identity, no final-label match) simply
    /// yields no witness -- it must never be mistaken for a contradiction either.
    #[test]
    fn a_non_summing_breakdown_alone_yields_no_witness_and_no_contradiction() {
        let figures = figures_with(|f| {
            f.line_items = vec![500.0];
        });
        assert_eq!(find_witness(&figures, 3650.0, WITNESS_TOLERANCE, FinalLabelScope::Whole), None);
        assert_eq!(final_label_contradicts(&figures, 3650.0, WITNESS_TOLERANCE, FinalLabelScope::Whole), None);
    }

    /// bus topic `bakeoff` #16 (lead ruling, image_02): a REMAINING-owed balance_due naturally
    /// differs from `total` (a partial payment already made) -- `total` disagreeing with it is
    /// not a contradiction under `FinalLabelScope::RemainingOwed`, and `total_minus_paid` still
    /// witnesses the balance directly.
    #[test]
    fn total_never_contradicts_a_pending_scheduled_balance_due() {
        let figures = figures_with(|f| {
            f.total = Some(200_000.0);
            f.amount_paid = Some(100_000.0);
            f.balance_due = Some(100_000.0);
            f.amount_due = Some(100_000.0);
        });
        assert_eq!(
            final_label_contradicts(&figures, 100_000.0, WITNESS_TOLERANCE, FinalLabelScope::RemainingOwed),
            None
        );
        let (kind, _) = find_witness(&figures, 100_000.0, WITNESS_TOLERANCE, FinalLabelScope::RemainingOwed)
            .expect("should witness");
        assert!(matches!(kind, WitnessKind::TotalMinusPaid | WitnessKind::RepeatedFinalLabel));
    }

    /// bus topic `bakeoff` #18 (image_05): the after-cutoff amount naturally differs from a
    /// `total` field the model duplicated from the pre-cutoff subtotal, and `balance_due`/
    /// `amount_due` may be an unrelated carried-forward figure -- neither contradicts under
    /// `FinalLabelScope::CutoffResolved`.
    #[test]
    fn total_never_contradicts_a_resolved_cutoff_amount() {
        let figures = figures_with(|f| {
            f.subtotal = Some(704.05);
            f.tax = Some(107.4);
            f.total = Some(704.05); // duplicated from subtotal, unrelated to the after-cutoff figure
            f.balance_due = Some(0.0); // an unrelated carried-forward figure, not the after-cutoff amount
            f.amount_due_by_cutoff = Some(704.05);
            f.amount_due_after_cutoff = Some(822.05);
            f.line_items = vec![580.65, 16.0, 107.4];
        });
        assert_eq!(
            final_label_contradicts(&figures, 822.05, WITNESS_TOLERANCE, FinalLabelScope::CutoffResolved),
            None
        );
        assert_eq!(
            find_witness(&figures, 822.05, WITNESS_TOLERANCE, FinalLabelScope::CutoffResolved),
            Some((WitnessKind::CutoffAfterExceedsWitnessedBefore, 822.05))
        );
    }
}
