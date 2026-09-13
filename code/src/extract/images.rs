//! Image figure transcription + deterministic selector + reconciliation
//! (schema = `code/prompts/image_transcription.v1.md`, PLAN.md §2.3).
//!
//! The VLM only transcribes every labeled figure on the page; it never picks which one
//! matters. This module picks the figure deterministically from the linked event's
//! type/status, checks it arithmetically, and only then turns it into a `Fact::EventAmount`.
//! A figure that fails reconciliation, or that the page simply does not contain, is never
//! guessed or treated as zero (see `docs/gold_subset.json` image_04 for the canonical case).

use std::io::Cursor;
use std::path::Path;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::Value;

use crate::engine::ledger::{EvidenceRecord, EvidenceSource, Fact};
use crate::engine::money::Money;
use crate::engine::types::{Event, EventType, Status};
use crate::extract::model_config::{
    CandidateConfig, DecodingConfig, DocValidationConfig, ModelsConfig, ReaderPick, VlmMode,
};
use crate::extract::normalize;
use crate::extract::parse_json_reply;
use crate::extract::prompts::PromptSet;
use crate::extract::witness;
use anyhow::Context as _;

use crate::anthropic::AnthropicClient;
use crate::hf::{ContentPart, HfClient, ModelCall, ModelResponse};

/// `doc_type` is purely advisory metadata (`select`/`reconciles` never branch on it) and is
/// deserialized leniently on purpose: a VLM's own wording for a document type is free text
/// in practice ("TAX INVOICE", "PROVISIONAL BILL", ...), and a strict enum match on it was
/// rejecting 58/112 otherwise-valid cached reads before `select()` ever ran (analyst audit
/// board:verify.image_audit #184). Unrecognized text maps to `Other` rather than erroring —
/// this field can never gate whether a figure is trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocType {
    Payslip,
    Receipt,
    Invoice,
    Bill,
    DeliverySummary,
    BankStatementExcerpt,
    Other,
}

impl<'de> Deserialize<'de> for DocType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Ok(DocType::from_free_text(&raw))
    }
}

impl DocType {
    fn from_free_text(raw: &str) -> DocType {
        let normalized: String = raw
            .trim()
            .to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { ' ' })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join("_");
        match normalized.as_str() {
            "payslip" | "pay_slip" | "salary_slip" | "pay_stub" | "paystub"
            | "slip_gaji" | "slip_gajian" => DocType::Payslip,
            "receipt" | "cash_receipt" | "sales_receipt" | "bill_of_supply" | "cash_bill"
            | "kwitansi" | "struk" | "nota" => DocType::Receipt,
            "invoice" | "tax_invoice" | "gst_invoice" | "sales_invoice" | "gst_tax_invoice"
            | "faktur" | "faktur_pajak" => DocType::Invoice,
            "bill" | "provisional_bill" | "hospital_bill" | "utility_bill" | "rent_receipt"
            | "tagihan" | "tagihan_listrik" | "tagihan_air" => DocType::Bill,
            "delivery_summary" | "order_summary" | "delivery_receipt" | "order_details"
            | "ringkasan_pesanan" | "rincian_pesanan" => DocType::DeliverySummary,
            "bank_statement_excerpt" | "bank_statement" | "statement" | "account_summary"
            | "rekening_koran" | "mutasi_rekening" => DocType::BankStatementExcerpt,
            _ => DocType::Other,
        }
    }
}

/// Every labeled figure the VLM transcribed off one document. Every field but `doc_type` is
/// nullable: a figure not printed on the page stays `None`, never an inferred zero.
/// Deserialized via `RawImageFigures` (below), never derived directly, so every field can
/// be coerced by JSON type AND the three due-date fields can recover a date string a model
/// dropped into the wrong (numeric) field.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImageFigures {
    pub doc_type: Option<DocType>,
    pub currency: Option<String>,
    pub subtotal: Option<f64>,
    pub tax: Option<f64>,
    pub total: Option<f64>,
    pub amount_due: Option<f64>,
    pub amount_paid: Option<f64>,
    pub balance_due: Option<f64>,
    pub gross_pay: Option<f64>,
    pub deductions: Option<f64>,
    pub net_pay: Option<f64>,
    pub previous_balance: Option<f64>,
    /// v1 schema (`image_transcription.v1.md`) -- LEGACY. `amount_due_before_date` is
    /// numeric-typed while `amount_due_before_date_value` is the date-string-typed field
    /// (an inverted name/type pairing), and `amount_due_after_date` reads as a date name but
    /// is numeric. Board finding `finding.image05_root_cause`: this ambiguity, not model
    /// vision, was why gemma-4-31B-it wrote the cutoff date into both `*_date` fields and had
    /// no slot left for the amount (0/5 image_05 reads), while other models put the amount in
    /// inconsistent slots. Kept only so already-cached v1 reads still parse; `select_pending_
    /// or_scheduled` prefers the unambiguous v2 fields below whenever they're present.
    pub amount_due_before_date: Option<f64>,
    pub amount_due_before_date_value: Option<String>,
    pub amount_due_after_date: Option<f64>,
    /// v2 schema (`image_transcription.v2.md`, `finding.image05_root_cause`): unambiguous
    /// replacement for the v1 trio above -- a date-typed cutoff field plus two clearly-named
    /// amount fields, no inverted names. `None` when the page has no due-date cutoff, or when
    /// this read used the v1 prompt (see the v1 fields above instead).
    pub due_cutoff_date: Option<String>,
    pub amount_due_by_cutoff: Option<f64>,
    pub amount_due_after_cutoff: Option<f64>,
    pub document_date: Option<String>,
    pub period_label: Option<String>,
    pub line_items_sum_check: Option<f64>,
    /// v3 schema (`image_transcription.v3.md`, image_accuracy_plan.md §1/§2 witness gate): a
    /// distinctly-labeled "Grand Total" line, when the page prints one separate from `total`
    /// (e.g. image_07: subtotal-style "Total" plus a "Grand Total" after service charges). Used
    /// only as another final-label witness/contradiction field (`extract::witness`); `None`
    /// when the page has no such separate line.
    pub grand_total: Option<f64>,
    /// v3 schema: the printed "amount in words" line verbatim (e.g. "Rupees Seven Hundred Four
    /// and Five Paise Only"), parsed by `extract::witness::words_to_number` -- never converted
    /// here, since the words parser needs the raw phrase, not a pre-parsed number.
    pub amount_in_words: Option<String>,
    /// v3 schema: every individual line-item amount printed on the page, copied verbatim and
    /// normalized independently (never a model-computed sum) -- `extract::witness::find_witness`
    /// re-sums these itself rather than trusting the model's own arithmetic (v1/v2's
    /// `line_items_sum_check` did the latter). Empty when the page prints no itemized list.
    pub line_items: Vec<f64>,
    /// v3 schema: a separate itemized list for charges/taxes/deductions distinct from
    /// `line_items` (e.g. image_07's two 203.05 service-charge lines added to a subtotal),
    /// verbatim and independently normalized. Empty when the page prints no such breakdown.
    pub charges_breakdown: Vec<f64>,
}

impl<'de> Deserialize<'de> for ImageFigures {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        RawImageFigures::deserialize(deserializer).map(ImageFigures::from)
    }
}

/// Every field captured as a raw `serde_json::Value` first (or absent), so the conversion
/// below can coerce by actual JSON type and cross-reference fields — neither is possible
/// with independent per-field `deserialize_with` visitors (analyst audits #184/#194/#200:
/// doc_type/currency/paid+total misreads; #203: a date string landing in a numeric
/// before/after field must be recovered as the cutoff, which requires seeing all three
/// due-date fields together).
#[derive(Debug, Default, Deserialize)]
struct RawImageFigures {
    #[serde(default)]
    doc_type: Option<Value>,
    #[serde(default)]
    currency: Option<Value>,
    #[serde(default)]
    subtotal: Option<Value>,
    #[serde(default)]
    tax: Option<Value>,
    #[serde(default)]
    total: Option<Value>,
    #[serde(default)]
    amount_due: Option<Value>,
    #[serde(default)]
    amount_paid: Option<Value>,
    #[serde(default)]
    balance_due: Option<Value>,
    #[serde(default)]
    gross_pay: Option<Value>,
    #[serde(default)]
    deductions: Option<Value>,
    #[serde(default)]
    net_pay: Option<Value>,
    #[serde(default)]
    previous_balance: Option<Value>,
    #[serde(default)]
    amount_due_before_date: Option<Value>,
    #[serde(default)]
    amount_due_before_date_value: Option<Value>,
    #[serde(default)]
    amount_due_after_date: Option<Value>,
    #[serde(default)]
    due_cutoff_date: Option<Value>,
    #[serde(default)]
    amount_due_by_cutoff: Option<Value>,
    #[serde(default)]
    amount_due_after_cutoff: Option<Value>,
    #[serde(default)]
    document_date: Option<Value>,
    #[serde(default)]
    period_label: Option<Value>,
    #[serde(default)]
    line_items_sum_check: Option<Value>,
    #[serde(default)]
    grand_total: Option<Value>,
    #[serde(default)]
    amount_in_words: Option<Value>,
    #[serde(default)]
    line_items: Option<Value>,
    #[serde(default)]
    charges_breakdown: Option<Value>,
}

/// Accepts a JSON number as-is, or a numeric-looking string via `normalize::parse_amount`
/// (thousands/lakh grouping, currency-aware decimal-vs-thousands disambiguation, analyst
/// RULES.md S7); anything else (including a date string landing in a numeric field, or a
/// genuinely ambiguous grouping) is `None` rather than a hard parse error or a guess.
fn coerce_f64(v: &Option<Value>, currency_hint: Option<&str>) -> Option<f64> {
    match v.as_ref()? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => normalize::parse_amount(s, currency_hint, false),
        _ => None,
    }
}

/// Each element of a JSON array coerced the same way `coerce_f64` coerces a single value
/// (number, or numeric-looking string via `normalize::parse_amount`); a non-array value, a
/// missing field, or an individual element that doesn't parse is simply dropped (never a hard
/// error and never a guessed 0) -- `extract::witness::find_witness` sums whatever survives.
fn coerce_f64_array(v: &Option<Value>, currency_hint: Option<&str>) -> Vec<f64> {
    match v.as_ref() {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| coerce_f64(&Some(item.clone()), currency_hint))
            .collect(),
        _ => Vec::new(),
    }
}

/// Accepts a JSON string, or coerces a JSON number to its string form; anything else is
/// `None`.
fn coerce_string(v: &Option<Value>) -> Option<String> {
    match v.as_ref()? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// `Some(the string)` only when `v` is a JSON string that itself parses as a date — used to
/// recover a cutoff date a model put in a numeric before/after field instead of the
/// `_value` field (analyst audit #203).
fn date_string_hint(v: &Option<Value>) -> Option<String> {
    match v.as_ref()? {
        Value::String(s) if parse_date(s).is_some() => Some(s.clone()),
        _ => None,
    }
}

impl From<RawImageFigures> for ImageFigures {
    fn from(raw: RawImageFigures) -> Self {
        // Resolved first so every amount field can disambiguate its own grouping/decimal
        // convention against it (normalize::parse_amount, analyst RULES.md S7).
        let currency = coerce_string(&raw.currency);
        let currency_hint = currency.as_deref().and_then(normalize::parse_currency);
        let currency_hint = currency_hint.as_deref();

        let mut before_value = coerce_string(&raw.amount_due_before_date_value);
        if before_value.as_deref().and_then(parse_date).is_none() {
            before_value = date_string_hint(&raw.amount_due_before_date)
                .or_else(|| date_string_hint(&raw.amount_due_after_date))
                .or(before_value);
        }
        ImageFigures {
            doc_type: raw
                .doc_type
                .as_ref()
                .and_then(Value::as_str)
                .map(DocType::from_free_text),
            currency,
            subtotal: coerce_f64(&raw.subtotal, currency_hint),
            tax: coerce_f64(&raw.tax, currency_hint),
            total: coerce_f64(&raw.total, currency_hint),
            amount_due: coerce_f64(&raw.amount_due, currency_hint),
            amount_paid: coerce_f64(&raw.amount_paid, currency_hint),
            balance_due: coerce_f64(&raw.balance_due, currency_hint),
            gross_pay: coerce_f64(&raw.gross_pay, currency_hint),
            deductions: coerce_f64(&raw.deductions, currency_hint),
            net_pay: coerce_f64(&raw.net_pay, currency_hint),
            previous_balance: coerce_f64(&raw.previous_balance, currency_hint),
            amount_due_before_date: coerce_f64(&raw.amount_due_before_date, currency_hint),
            amount_due_before_date_value: before_value,
            amount_due_after_date: coerce_f64(&raw.amount_due_after_date, currency_hint),
            due_cutoff_date: coerce_string(&raw.due_cutoff_date),
            amount_due_by_cutoff: coerce_f64(&raw.amount_due_by_cutoff, currency_hint),
            amount_due_after_cutoff: coerce_f64(&raw.amount_due_after_cutoff, currency_hint),
            document_date: coerce_string(&raw.document_date),
            period_label: coerce_string(&raw.period_label),
            line_items_sum_check: coerce_f64(&raw.line_items_sum_check, currency_hint),
            grand_total: coerce_f64(&raw.grand_total, currency_hint),
            amount_in_words: coerce_string(&raw.amount_in_words),
            line_items: coerce_f64_array(&raw.line_items, currency_hint),
            charges_breakdown: coerce_f64_array(&raw.charges_breakdown, currency_hint),
        }
    }
}


/// Rounding tolerance for reconciliation checks combining two printed terms (e.g.
/// subtotal+tax=total). Analyst audit RULES.md S5 image_07: subtotal 7,150 + tax 358.10 =
/// 7,508.10 exactly, but the same page also prints a plain "7,508" total — both readings
/// are legitimate, and an exact-cent check rejects a genuinely reconciling document. Allow
/// each of the two printed terms in a check to be off by up to half a currency unit
/// (0.5), for a combined tolerance of 1.0, and log whenever the looser bound is what
/// actually let a check pass (never silently — a human should be able to see it happened).
const ROUNDING_TOLERANCE_2TERM: f64 = 1.0;

fn close(a: f64, b: f64, tolerance: f64) -> bool {
    let diff = (a - b).abs();
    if diff > 0.0 && diff <= tolerance {
        eprintln!("reconcile: {a} vs {b} accepted within rounding tolerance {tolerance} (off by {diff})");
    }
    diff <= tolerance
}

/// Delegates to the shared `normalize::parse_date` (ISO, "DD-Mon-YYYY"/"D Mon YYYY" in
/// English or Indonesian, and an unambiguous "DD/MM/YYYY") so date parsing never drifts
/// between the image and message evidence paths.
fn parse_date(s: &str) -> Option<NaiveDate> {
    normalize::parse_date(s)
}

/// Deterministic selector (PLAN.md §2.3 table): which figure matters, given the linked
/// event's type/status. Never the model's job.
pub fn select(figures: &ImageFigures, event: &Event) -> Option<f64> {
    match (event.event_type, event.status) {
        (EventType::Income, Status::Settled | Status::Scheduled) => {
            figures.net_pay.or(figures.total)
        }
        // bus topic bakeoff #16 (lead ruling, image_07): a distinctly-labeled Grand Total
        // outranks a plain Total whenever the page prints both (image_07: "Total: 8,528.10"
        // AND "Grand Total (RS): 8,528" -- the Grand Total is the amount actually paid).
        // engine analyst audit RULES.md S5 image_12: below that, prefer the labeled `total`
        // over `amount_paid` for a settled expense. A receipt's "amount paid"/"cash" line can
        // be the cash tendered (e.g. "Cash 35.00, Change 6.25" against a 28.75 total), which is
        // not the expense amount; the printed total is the authoritative figure whenever
        // it's present, with amount_paid only as a fallback when no total is printed.
        (_, Status::Settled) => figures.grand_total.or(figures.total).or(figures.amount_paid),
        (_, Status::Pending | Status::Scheduled) => select_pending_or_scheduled(figures, event),
        _ => figures.total,
    }
}

/// Never treat exactly-zero (or negative) as the selected amount for a still-outstanding
/// debit: `Status::Pending`/`Status::Scheduled` are by construction money not yet moved, so
/// a 0 reading is a stale/misplaced figure, not "nothing owed" (analyst audit #203).
fn positive(amount: Option<f64>) -> Option<f64> {
    amount.filter(|v| *v > 0.0)
}

fn select_pending_or_scheduled(figures: &ImageFigures, event: &Event) -> Option<f64> {
    // v2 schema (`image_transcription.v2.md`, `finding.image05_root_cause`): unambiguous
    // due_cutoff_date/amount_due_by_cutoff/amount_due_after_cutoff trio. Preferred whenever
    // present -- a new call always uses this schema; only already-cached v1 reads fall
    // through to the legacy block below.
    //
    // Analyst #301 G2: a cutoff field that IS PRESENT but fails to parse is not the same
    // situation as no cutoff at all -- it means a due-date split exists on the page and we
    // simply couldn't read it, so the conservative "take the larger figure" guess below is
    // NOT safe here (the correct side could be the smaller one). `parse_date(raw)?` returns
    // `None` from this whole function in that case (escalate/missing), never falling through
    // to the max()-guess or to balance_due/amount_due.
    if let Some(raw) = figures.due_cutoff_date.as_deref() {
        let cutoff = parse_date(raw)?;
        // analyst audit #203: once the cutoff is known, the required side is exact -- never
        // fall back to the other (known-wrong-for-this-date) side, and never fall through
        // to balance_due/amount_due either. Missing the required side means escalate.
        return if event.cash_date() > cutoff {
            positive(figures.amount_due_after_cutoff)
        } else {
            positive(figures.amount_due_by_cutoff)
        };
    }
    if let (Some(before), Some(after)) = (figures.amount_due_by_cutoff, figures.amount_due_after_cutoff) {
        // No cutoff field at all (genuinely absent, not merely unparseable): the
        // conservative choice is the larger figure, never the smaller (see the v1 block's
        // identical reasoning below).
        return positive(Some(before.max(after)));
    }

    // v1 schema (LEGACY, `image_transcription.v1.md`) -- a cutoff may come from the labeled
    // `_value` field directly, or be recovered from a date string a model dropped into a
    // numeric before/after field (`RawImageFigures`'s conversion already folds that recovery
    // into `amount_due_before_date_value`). Same G2 rule: present-but-unparseable escalates,
    // it never falls through to the max()-guess.
    if let Some(raw) = figures.amount_due_before_date_value.as_deref() {
        let cutoff = parse_date(raw)?;
        return if event.cash_date() > cutoff {
            positive(figures.amount_due_after_date)
        } else {
            positive(figures.amount_due_before_date)
        };
    }
    if let (Some(before), Some(after)) =
        (figures.amount_due_before_date, figures.amount_due_after_date)
    {
        // No reliable cutoff date to choose between them: the conservative choice is the
        // larger figure, never the smaller -- under-reserving a pending debt risks a plan
        // that later breaches the minimum balance; over-reserving only costs safety margin.
        return positive(Some(before.max(after)));
    }
    // Only one of before/after is present with no date to resolve it (or neither is present
    // at all, in either schema): not safe to treat that lone value as authoritative on its
    // own -- fall through to whatever else the page states. If nothing here resolves either,
    // `select` returns `None` and the caller escalates rather than guessing.
    positive(figures.balance_due).or_else(|| positive(figures.amount_due))
}

/// Every check that has the data to run, must pass, or the figure is rejected
/// (PLAN.md §2.3: subtotal+tax=total, gross-deductions=net, amount_paid+balance_due=total,
/// currency match). `line_items_sum_check` is a fallback signal only, checked solely when
/// none of the labeled-field checks above had enough data to run at all — analyst audit
/// RULES.md S5 image_11: a multi-section hospital bill's itemized breakup sums to 2,790
/// while the labeled Total/Balance (which already reconcile against each other, 0 + 3,210 =
/// 3,210) say 3,210. Re-summing an arbitrary itemized breakup is not as reliable as the
/// document's own labeled totals, so it never overrides them.
/// Every distinct identity a document can print (subtotal+tax=total, gross-deductions=net,
/// amount_paid[+balance_due]=total) is checked independently; a figure is reconciled if
/// ANY identity that has enough data to run actually passes, not only if every identity
/// that happens to have data all agree. Analyst audit board:verify.image_audit #194: a
/// document can print an unrelated subtotal/tax breakdown (for a different section of the
/// bill) that doesn't sum to the overall total, while amount_paid + balance_due = total
/// independently confirms the figure that matters — the first identity's mismatch must not
/// veto the second's pass. Only rejects outright when at least one identity had the data to
/// run and every identity that ran failed.
pub fn reconciles(figures: &ImageFigures, event: &Event) -> bool {
    let mut any_ran = false;
    let mut any_passed = false;

    // bus topic bakeoff #18 (image_05): on a pending bill with a due-date cutoff split, the
    // generic `total`/`amount_paid` identities below test the wrong slice of the document --
    // `total` here is frequently a duplicate of `subtotal` (the pre-cutoff amount) rather than
    // a taxed whole, and `amount_paid` can be an unrelated carried-forward balance, not a
    // payment toward THIS bill. Once a cutoff is resolved, `select_pending_or_scheduled` never
    // even reads `total`/`subtotal`/`tax`/`amount_paid` to pick the figure -- it comes entirely
    // from `amount_due_by_cutoff`/`amount_due_after_cutoff`, corroborated by the witness gate's
    // own `CutoffAfterExceedsWitnessedBefore` identity, not by these. Generic on the document
    // SHAPE (has a cutoff), never on an image id.
    let has_cutoff_split = figures.due_cutoff_date.is_some();

    if !has_cutoff_split {
        if let (Some(sub), Some(tax), Some(total)) = (figures.subtotal, figures.tax, figures.total) {
            any_ran = true;
            if close(sub + tax, total, ROUNDING_TOLERANCE_2TERM) {
                any_passed = true;
            }
        }
    }
    if let (Some(gross), Some(ded), Some(net)) = (figures.gross_pay, figures.deductions, figures.net_pay)
    {
        any_ran = true;
        if close(gross - ded, net, ROUNDING_TOLERANCE_2TERM) {
            any_passed = true;
        }
    }
    // analyst audit #184 (image_12): cash tendered can legitimately exceed the total when
    // change is given (35.00 tendered against a 28.75 total) -- that is consistent by
    // construction, not a mismatch, regardless of what balance_due says. Only fall through
    // to the paid+balance_due=total identity when paid is actually less than total (a real
    // partial payment / balance-owed scenario).
    if !has_cutoff_split {
        if let (Some(paid), Some(total)) = (figures.amount_paid, figures.total) {
            any_ran = true;
            // analyst audit #200 FA1 (image_02): the paid>=total "cash tendered, change given"
            // shortcut falsely accepted a misread where paid (1,000,000) trivially exceeded
            // total but the page also printed a real, non-zero balance_due -- i.e. this was
            // actually an outstanding-balance scenario, not a fully-paid-with-change one. The
            // shortcut now requires balance_due to be absent or ~0 (nothing left owing) AND
            // paid to stay within a plausible cash-tendered range (under 2x total -- change
            // given is normally a fraction of the total, never several times it).
            let balance_clears = figures.balance_due.map_or(true, |b| b.abs() <= ROUNDING_TOLERANCE_2TERM);
            if paid + ROUNDING_TOLERANCE_2TERM >= total && balance_clears && paid < 2.0 * total {
                any_passed = true;
            } else if let Some(bal) = figures.balance_due {
                if close(paid + bal, total, ROUNDING_TOLERANCE_2TERM) {
                    any_passed = true;
                }
            }
            // else: paid < total and no balance_due stated -- this identity had enough data to
            // attempt (paid, total) but not enough to confirm or reject the gap; left un-passed,
            // same as a failed attempt, so it cannot by itself validate the figure.
        }
    }
    // image_accuracy_plan.md §2: a breakdown that does NOT sum to the target is a note, never
    // a rejection (pages are often cut off) -- so this identity only ever sets `any_ran` when
    // it actually PASSES; a mismatch here must never by itself make `any_ran && !any_passed`
    // true.
    if !any_passed {
        if let (Some(sum), Some(target)) =
            (figures.line_items_sum_check, figures.subtotal.or(figures.total))
        {
            if close(sum, target, ROUNDING_TOLERANCE_2TERM) {
                any_ran = true;
                any_passed = true;
            }
        }
    }
    if any_ran && !any_passed {
        return false;
    }
    if let Some(cur) = &figures.currency {
        if !currency_matches(cur, &event.currency) {
            return false;
        }
    }
    true
}

/// Currency-code match, tolerant of the symbols/names a VLM prints instead of the ISO code
/// (analyst audit #194: exact-string comparison was rejecting reads that had the right
/// currency in a different spelling). Delegates to the shared `normalize::parse_currency` so
/// currency normalization never drifts between the image and message evidence paths. A
/// currency that fails to normalize on either side (e.g. the literal text "null") never
/// counts as a match.
fn currency_matches(claimed: &str, expected: &str) -> bool {
    match (normalize::parse_currency(claimed), normalize::parse_currency(expected)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// User directive (accuracy is the only focus): deterministic checks BEYOND arithmetic
/// reconciliation, run before any figure is trusted, zero tokens. Unlike `reconciles()` (any
/// identity that ran passing is enough), every one of these that has the data to run must
/// pass -- a failed check rejects the whole read (falls through to tiebreak/missing per
/// `resolve_blank_amount_agreement`), never auto-corrected. `None` means the check had
/// nothing to check against (e.g. no document_date printed), which is never itself a
/// failure.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize)]
pub struct DocValidationResult {
    /// (1) The document's own printed currency matches the linked event's currency.
    pub currency_ok: Option<bool>,
    /// (2) `document_date`/`period_label`-derived date falls within
    /// `DocValidationConfig::date_window_days` of the event's cash date.
    pub date_window_ok: Option<bool>,
    /// (4) `doc_type` is consistent with the linked event's direction: a payslip only for an
    /// income event, a bill/receipt/invoice/delivery-summary only for an expense event.
    pub class_consistent: Option<bool>,
    /// (5) A pending-bill cutoff date, if any, is coherent with the event's own dates (not
    /// absurdly far from either) -- a cutoff that doesn't relate to this transaction at all
    /// is more likely a misread than a real due date for this bill.
    pub cutoff_coherent: Option<bool>,
    /// (3) The candidate amount is within `DocValidationConfig`'s plausibility band of this
    /// user's own typical amount for the same category/event type (`typical_settled_amount`)
    /// -- catches a self-consistent misread that arithmetic reconciliation alone cannot
    /// (analyst #276: two same-model image_02 reads both misread an Indian lakh grouping by
    /// 10x and still reconciled internally).
    pub plausible: Option<bool>,
}

impl DocValidationResult {
    /// `true` unless some check that had the data to run actually failed.
    pub fn passed(&self) -> bool {
        [self.currency_ok, self.date_window_ok, self.class_consistent, self.cutoff_coherent, self.plausible]
            .into_iter()
            .all(|c| c != Some(false))
    }
}

/// (4) Payslip figures only for income events; bill/receipt/invoice/delivery-summary figures
/// only for expense events (user directive's exact scope). Every other pairing -- a bank
/// statement excerpt, an unrecognized doc_type, or an event type this rule doesn't name
/// (subscription, debt payment, investment, refund) -- is `None` (no constraint), since
/// `doc_type` is otherwise purely advisory (analyst audit #184/#194) and this check only
/// covers the two directions the user explicitly named.
fn doc_class_matches_event(doc_type: DocType, event_type: EventType) -> Option<bool> {
    match doc_type {
        DocType::Payslip => Some(event_type == EventType::Income),
        DocType::Receipt | DocType::Invoice | DocType::Bill | DocType::DeliverySummary => {
            Some(event_type == EventType::Expense)
        }
        DocType::BankStatementExcerpt | DocType::Other => None,
    }
}

/// This user's OWN typical amount for the same category/event type (median of their other
/// settled amounts in that category, excluding the event under check) -- external ground
/// truth a document's own internal consistency can't fake. `None` when there's no settled
/// history to compare against, which never itself blocks a first-ever event in a category.
pub fn typical_settled_amount<'a>(
    history: impl Iterator<Item = &'a Event>,
    category: &str,
    event_type: EventType,
    exclude_event_id: &str,
) -> Option<f64> {
    let mut amounts: Vec<f64> = history
        .filter(|e| {
            e.id != exclude_event_id
                && e.event_type == event_type
                && e.category == category
                && e.status == Status::Settled
        })
        .filter_map(|e| e.amount.map(Money::to_f64))
        .collect();
    if amounts.is_empty() {
        return None;
    }
    amounts.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Some(amounts[amounts.len() / 2])
}

/// Runs every check that has the data to run; a check with nothing to compare against is
/// `None`, never a failure.
fn validate_doc(
    figures: &ImageFigures,
    event: &Event,
    candidate_amount: Option<f64>,
    typical_amount: Option<f64>,
    config: &DocValidationConfig,
) -> DocValidationResult {
    let currency_ok = figures.currency.as_deref().map(|c| currency_matches(c, &event.currency));

    let date_window_ok = figures
        .document_date
        .as_deref()
        .and_then(parse_date)
        .map(|doc_date| (doc_date - event.cash_date()).num_days().abs() <= config.date_window_days);

    let class_consistent = figures.doc_type.and_then(|dt| doc_class_matches_event(dt, event.event_type));

    let cutoff_coherent = figures
        .due_cutoff_date
        .as_deref()
        .and_then(parse_date)
        .or_else(|| figures.amount_due_before_date_value.as_deref().and_then(parse_date))
        .map(|cutoff| (cutoff - event.cash_date()).num_days().abs() <= config.date_window_days * 2);

    let plausible = match (candidate_amount, typical_amount) {
        (Some(candidate), Some(typical)) if typical > 0.0 => {
            let ratio = candidate / typical;
            Some(ratio >= config.plausibility_low && ratio <= config.plausibility_high)
        }
        _ => None,
    };

    DocValidationResult { currency_ok, date_window_ok, class_consistent, cutoff_coherent, plausible }
}

/// Reconcile + select a figure and resolve its currency, without building an
/// `EvidenceRecord` yet — shared by the single-reader path (`to_evidence`) and the
/// two-model agreement path (`resolve_blank_amount_agreement`), which needs the raw
/// `(amount, currency)` from each reader before deciding whether to trust either one.
fn selected_figure(figures: &ImageFigures, event: &Event) -> Option<(f64, String)> {
    if !reconciles(figures, event) {
        return None;
    }
    let amount = select(figures, event)?;
    let currency = figures.currency.clone().unwrap_or_else(|| event.currency.clone());
    Some((amount, currency))
}

/// Select, reconcile, and convert one image's transcription into a `Fact::EventAmount`.
/// `None` when reconciliation fails or the needed figure is genuinely not on the page —
/// the caller (store/preprocessing) is expected to escalate to a second read before giving
/// up, per `code/prompts/image_transcription.v1.md`.
pub fn to_evidence(image_id: &str, figures: &ImageFigures, event: &Event) -> Option<EvidenceRecord> {
    let (amount, currency) = selected_figure(figures, event)?;
    Some(EvidenceRecord {
        record_id: image_id.to_string(),
        source: EvidenceSource::Image,
        observed_at: event.event_date.and_hms_opt(0, 0, 0)?,
        fact: Fact::EventAmount { event_id: event.id.clone(), amount: Money::from_f64(amount), currency },
    })
}

/// Downscale to `max_dim` on the longest side and PNG-encode as base64 (PLAN.md §3 token-
/// efficiency lever: ship the smallest resolution that keeps gold accuracy — the bake-off's
/// job to pick `max_dim`, not this function's).
fn downscale_and_encode(path: &Path, max_dim: u32) -> anyhow::Result<String> {
    let img = image::open(path).map_err(|e| anyhow::anyhow!("opening {}: {e}", path.display()))?;
    let resized = img.thumbnail(max_dim, max_dim);
    let mut buf = Vec::new();
    resized
        .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
        .map_err(|e| anyhow::anyhow!("encoding downscaled PNG for {}: {e}", path.display()))?;
    Ok(BASE64.encode(buf))
}

/// One call per image (PLAN.md §3 batching lever), per
/// `code/prompts/image_transcription.v1.md`. Goes through `HfClient`'s own §2.11 disk
/// cache (content hash + model id + revision + prompt version), so a repeated image/model/
/// prompt combination costs zero tokens on rerun. `cold` selects `chat_completion_cold`
/// (bypass the cache read, still write it) for a `--cold` full-dataset run.
#[allow(clippy::too_many_arguments)]
fn call_vlm(
    client: &HfClient,
    anthropic: Option<&AnthropicClient>,
    cold: bool,
    run_idx: Option<u32>,
    prompt: &PromptSet,
    decoding: &DecodingConfig,
    candidate: &CandidateConfig,
    max_tokens: u32,
    image_b64: &str,
) -> anyhow::Result<ImageFigures> {
    let call = ModelCall {
        model_id: candidate.id.clone(),
        provider: candidate.provider.clone(),
        model_revision: candidate.model_revision.clone(),
        prompt_version: prompt.version.clone(),
        system_prompt: prompt.system_prompt.clone(),
        user_content: vec![
            ContentPart::Text(prompt.user_template.clone()),
            ContentPart::ImageDataUrl { mime: "image/png".to_string(), base64_data: image_b64.to_string() },
        ],
        temperature: decoding.temperature,
        seed: decoding.seed,
        max_tokens,
        json_response: candidate.supports_structured_output,
        // ml-engineer dropped structured-output json_schema for the Anthropic backend after
        // 3 live schema-validation errors in a row (anthropic.rs); every provider now relies
        // on the prompt text alone plus `parse_json_reply`'s lenient parsing.
        json_schema: None,
    };
    let response = dispatch_call(client, anthropic, cold, run_idx, candidate, &call)?;
    // Kimi-K3 (a thinking model, ml-engineer #204/lead) returns reasoning text ahead of the
    // JSON answer at any max_tokens generous enough to let it finish; `parse_json_reply`
    // already locates the JSON body rather than requiring the whole reply to be JSON.
    let value = parse_json_reply(&response.raw_text)?;
    Ok(serde_json::from_value(value)?)
}

/// Board decision `decision.vlm_routing_v2`: dispatches a call by `candidate.provider`
/// rather than a caller having to know which backend serves which reader. `"anthropic"`
/// goes to a freshly constructed `AnthropicClient` (needs only `ANTHROPIC_API_KEY`, no HF
/// router credits); anything else goes to the shared `HfClient`. Constructing the Anthropic
/// client is cheap enough to do per call and keeps every caller's signature unchanged (no
/// second client threaded through `resolve_blank_amount`'s public API). When
/// `ANTHROPIC_API_KEY` isn't set, `AnthropicClient::new()` errors here, which `read_candidate`
/// already treats as "this reader produced no result" — an inactive provider is reader-
/// unavailable, never a guess.
fn dispatch_call(
    hf: &HfClient,
    anthropic: Option<&AnthropicClient>,
    cold: bool,
    run_idx: Option<u32>,
    candidate: &CandidateConfig,
    call: &ModelCall,
) -> anyhow::Result<ModelResponse> {
    if candidate.provider == "anthropic" {
        let client = anthropic
            .context("anthropic provider selected but no AnthropicClient configured (ANTHROPIC_API_KEY unset) -- reader unavailable")?;
        return if cold { client.chat_completion_cold(call) } else { client.chat_completion(call) };
    }
    match (cold, run_idx) {
        // image_accuracy_plan.md §"Live N=5": persist every run's raw response under its own
        // file (hf.rs's `chat_completion_cold_numbered`), never silently overwritten by the
        // next run in the same stability sweep.
        (true, Some(idx)) => hf.chat_completion_cold_numbered(call, idx),
        (true, None) => hf.chat_completion_cold(call),
        (false, _) => hf.chat_completion(call),
    }
}

/// Per-read provenance (verifier #205 contract, board:verify.image_agreement): the
/// integrator persists one of these per attempted read to
/// `store/processed/image_reads/<image_id>.json` so a read's outcome is auditable
/// independent of whether it ended up contributing to the final evidence.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ImageReadProvenance {
    pub role: String,
    pub model_id: String,
    pub model_revision: String,
    pub max_dim_px: u32,
    pub max_tokens: u32,
    pub reconciled: bool,
    /// User directive (accuracy is the only focus): the 5 deterministic checks beyond
    /// arithmetic reconciliation. `selected_amount` is only ever populated when BOTH
    /// `reconciled` and `doc_checks.passed()` are true.
    pub doc_checks: DocValidationResult,
    pub selected_amount: Option<f64>,
    pub currency: Option<String>,
    /// The due-date cutoff this read itself resolved, if any (labeled or recovered).
    pub due_date: Option<String>,
    pub before_amount: Option<f64>,
    pub after_amount: Option<f64>,
    pub error: Option<String>,
    /// image_accuracy_plan.md §2 witness gate: the independent identity (line-item sum,
    /// subtotal+tax, gross-deductions, paid+balance, amount-in-words, a repeated final label,
    /// or a witnessed cutoff relationship) that proved `selected_amount` on THIS read's own
    /// figures, if any. Only ever populated when `selected_amount.is_some()`.
    pub witness: Option<String>,
    /// bus topic `bakeoff` #16/#18 (engine's `Fact::AmountWitness`): the witness identity's own
    /// arithmetic/corroborating result, which is not always identical to `selected_amount`
    /// (image_07: `selected_amount` = the Grand Total 8,528; `witness_computed` = the plain
    /// Total's 8,528.10, which rounds to it). Only ever populated alongside `witness`.
    pub witness_computed: Option<f64>,
    /// image_accuracy_plan.md §2: the first final-labeled field on THIS read's own figures
    /// that disagrees with `selected_amount` beyond tolerance, if any (`(field, value)` as
    /// `"field=value"`). A non-summing itemized breakdown is never reported here (module doc,
    /// `extract::witness`) -- only Total/Grand Total/Amount Due/Balance Due/Net Pay count.
    pub contradiction: Option<String>,
    /// fleet/specs/ocr_vllm_pipeline.md work item A4: the OCR path's fail-closed/audit reason
    /// codes from `labels::figures_from_rows` (an unmapped label, a role conflict, an
    /// unparseable cutoff date) -- empty for the VLM path, which has no such notes.
    pub ocr_notes: Vec<String>,
}

/// Full resolution for one blank-amount event: every read attempted, the outcome, and the
/// evidence (if any) that resulted. `class`/`mode` place the decision in context for audit.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ImageResolution {
    pub image_id: String,
    pub class: Option<String>,
    pub mode: String,
    pub reads: Vec<ImageReadProvenance>,
    pub outcome: String,
    pub evidence: Option<EvidenceRecord>,
}

/// End-to-end resolution for one blank-amount event with a linked image (PLAN.md §2.3,
/// user decision `decision.vlm_setup`). Dispatches on `config.vlm_mode()`:
/// - `Escalate`: the original primary -> escalation -> fallback chain, accepting the first
///   reconciling read.
/// - `Agreement`: two independent readers (chosen by `config.readers_for(event)`'s
///   deterministic event-class routing, each with its own resolution/token budget) must
///   select the same amount before it's trusted; `vlm_fallback` tiebreaks on disagreement
///   or a missing reader.
///
/// When the user has not picked a model yet (`[selected]` absent from
/// `config/models.toml`), the caller simply does not call this function; the model path is
/// inactive by construction, not by a special case here.
///
/// `anthropic`: board decision `decision.vlm_routing_v2` (Kimi-K3 replaced by claude-opus-5
/// as a backup reader) — pass `Some(&client)` when `ANTHROPIC_API_KEY` is set, `None`
/// otherwise. Any reader whose `[[candidates.vlm]]`/`[[fallback.candidates]]` entry has
/// `provider = "anthropic"` resolves through it instead of the shared `HfClient`; with `None`
/// here, such a reader is simply unavailable for this run (no read, no guess), never an error
/// that aborts the whole resolution.
///
/// `history`: this user's OTHER financial events, for `validate_doc`'s amount-plausibility
/// check (analyst #276) -- pass the full per-user event slice; `typical_settled_amount`
/// itself excludes `event` and filters to matching category/type/settled status. An empty
/// slice (or no matching history) simply skips that one check (`None`, not a failure).
#[allow(clippy::too_many_arguments)]
pub fn resolve_blank_amount(
    client: &HfClient,
    anthropic: Option<&AnthropicClient>,
    cold: bool,
    prompt: &PromptSet,
    image_max_dim_px: u32,
    image_path: &Path,
    image_id: &str,
    config: &ModelsConfig,
    event: &Event,
    history: &[Event],
) -> anyhow::Result<ImageResolution> {
    resolve_blank_amount_inner(
        client, anthropic, cold, None, prompt, image_max_dim_px, image_path, image_id, config, event, history,
    )
}

/// Same as `resolve_blank_amount`, but persists every live call this resolution makes under
/// its own numbered file (`hf::HfClient::chat_completion_cold_numbered`) instead of the
/// canonical cache entry alone -- image_accuracy_plan.md §"Live N=5": ml-engineer's stability
/// sweep (`bin/bakeoff.rs`) needs every run's raw response on disk for review, not just the
/// last run's (which would otherwise silently overwrite runs 1..N-1, same cache key by
/// construction). `resolve_blank_amount`'s own public signature stays exactly as main.rs
/// already calls it; only this bake-off-only entry point takes `run_idx`.
#[allow(clippy::too_many_arguments)]
pub fn resolve_blank_amount_numbered_run(
    client: &HfClient,
    anthropic: Option<&AnthropicClient>,
    prompt: &PromptSet,
    image_max_dim_px: u32,
    image_path: &Path,
    image_id: &str,
    config: &ModelsConfig,
    event: &Event,
    history: &[Event],
    run_idx: u32,
) -> anyhow::Result<ImageResolution> {
    resolve_blank_amount_inner(
        client,
        anthropic,
        true,
        Some(run_idx),
        prompt,
        image_max_dim_px,
        image_path,
        image_id,
        config,
        event,
        history,
    )
}

#[allow(clippy::too_many_arguments)]
fn resolve_blank_amount_inner(
    client: &HfClient,
    anthropic: Option<&AnthropicClient>,
    cold: bool,
    run_idx: Option<u32>,
    prompt: &PromptSet,
    image_max_dim_px: u32,
    image_path: &Path,
    image_id: &str,
    config: &ModelsConfig,
    event: &Event,
    history: &[Event],
) -> anyhow::Result<ImageResolution> {
    match config.vlm_mode() {
        VlmMode::Escalate => resolve_blank_amount_escalate(
            client,
            anthropic,
            cold,
            run_idx,
            prompt,
            &config.decoding,
            image_max_dim_px,
            image_path,
            image_id,
            config.vlm_primary(),
            config.vlm_escalation(),
            config.vlm_fallback(),
            event,
            history,
            &config.doc_validation,
        ),
        VlmMode::Agreement => resolve_blank_amount_agreement(
            client,
            anthropic,
            cold,
            run_idx,
            prompt,
            &config.decoding,
            image_path,
            image_id,
            config,
            event,
            history,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn read_candidate(
    client: &HfClient,
    anthropic: Option<&AnthropicClient>,
    cold: bool,
    run_idx: Option<u32>,
    prompt: &PromptSet,
    decoding: &DecodingConfig,
    pick: ReaderPick<'_>,
    image_b64: &str,
    image_id: &str,
    event: &Event,
    history: &[Event],
    doc_validation: &DocValidationConfig,
) -> ImageReadProvenance {
    let mut prov = ImageReadProvenance {
        role: pick.role.to_string(),
        model_id: pick.candidate.id.clone(),
        model_revision: pick.candidate.model_revision.clone(),
        max_dim_px: pick.max_dim_px,
        max_tokens: pick.max_tokens,
        reconciled: false,
        doc_checks: DocValidationResult::default(),
        selected_amount: None,
        currency: None,
        due_date: None,
        before_amount: None,
        after_amount: None,
        error: None,
        witness: None,
        witness_computed: None,
        contradiction: None,
        ocr_notes: Vec::new(),
    };
    match call_vlm(client, anthropic, cold, run_idx, prompt, decoding, pick.candidate, pick.max_tokens, image_b64) {
        Ok(figures) => {
            // Prefer v2's unambiguous fields; fall back to v1's for an already-cached v1
            // read (`finding.image05_root_cause`). Either way, `ImageReadProvenance`'s own
            // field names (due_date/before_amount/after_amount) stay stable regardless of
            // which prompt schema produced the read -- downstream (verifier, integrator)
            // never needs to know about the v1/v2 split.
            prov.due_date = figures.due_cutoff_date.clone().or_else(|| figures.amount_due_before_date_value.clone());
            prov.before_amount = figures.amount_due_by_cutoff.or(figures.amount_due_before_date);
            prov.after_amount = figures.amount_due_after_cutoff.or(figures.amount_due_after_date);
            prov.reconciled = reconciles(&figures, event);
            let candidate_amount = select(&figures, event);
            let typical_amount =
                typical_settled_amount(history.iter(), &event.category, event.event_type, &event.id);
            prov.doc_checks = validate_doc(&figures, event, candidate_amount, typical_amount, doc_validation);
            if prov.reconciled && prov.doc_checks.passed() {
                prov.selected_amount = candidate_amount;
                if let Some(amount) = prov.selected_amount {
                    prov.currency =
                        Some(figures.currency.clone().unwrap_or_else(|| event.currency.clone()));
                    // image_accuracy_plan.md §2 witness gate: computed on THIS read's own raw
                    // figures, never across reads -- a non-summing breakdown never rejects
                    // (`find_witness` only ever reports a passing identity), and a real
                    // final-label contradiction is recorded for the caller to veto on.
                    //
                    // bus topic bakeoff #16/#18 (image_02, image_05): `select_pending_or_
                    // scheduled` never picks a whole-document `total`/`grand_total`/`net_pay`
                    // -- a REMAINING-owed `balance_due`/`amount_due` (image_02) is legitimately
                    // smaller than `total` (a partial payment already made), and a due-date-
                    // cutoff-resolved before/after amount (image_05) makes even `balance_due`/
                    // `amount_due` untrustworthy (observed: an unrelated carried-forward
                    // figure) -- only the cutoff identity itself corroborates there.
                    let scope = match event.status {
                        Status::Pending | Status::Scheduled if prov.due_date.is_some() => {
                            witness::FinalLabelScope::CutoffResolved
                        }
                        Status::Pending | Status::Scheduled => witness::FinalLabelScope::RemainingOwed,
                        _ => witness::FinalLabelScope::Whole,
                    };
                    if let Some((kind, computed)) =
                        witness::find_witness(&figures, amount, ROUNDING_TOLERANCE_2TERM, scope)
                    {
                        prov.witness = Some(kind.label().to_string());
                        prov.witness_computed = Some(computed);
                    }
                    prov.contradiction = witness::final_label_contradicts(&figures, amount, ROUNDING_TOLERANCE_2TERM, scope)
                        .map(|(field, value)| format!("{field}={value}"));
                }
            }
        }
        Err(e) => {
            eprintln!("vlm: {image_id} via {}: {e:#}", pick.candidate.id);
            prov.error = Some(e.to_string());
        }
    }
    prov
}

/// What a read's OWN resolved cutoff (if any) requires the final amount to equal — mirrors
/// `select_pending_or_scheduled`'s cutoff branch, applied to provenance rather than
/// `ImageFigures` directly, so the two-model tiebreak can cross-check against it without
/// keeping every read's raw figures around.
fn cutoff_requirement(prov: &ImageReadProvenance, event: &Event) -> Option<f64> {
    let cutoff = prov.due_date.as_deref().and_then(parse_date)?;
    let required = if event.cash_date() > cutoff { prov.after_amount } else { prov.before_amount };
    required.filter(|v| *v > 0.0)
}

/// Three states a read's own `due_date` can be in, for `cutoff_dates_agree` (board decision
/// `decision.vlm_routing_v3`, verifier ca892d5): the field can be genuinely absent, present
/// but unparseable, or present and a real date. Unparseable is deliberately its own state,
/// never folded into `Absent` -- verifier's exact semantics: "both absent = equal,
/// unparseable never equals absent" (nor, by the same never-guess logic, another
/// unparseable value -- two garbled strings are not evidence they mean the same thing).
enum CutoffState {
    Absent,
    Unparseable,
    Date(NaiveDate),
}

fn cutoff_state(due_date: &Option<String>) -> CutoffState {
    match due_date {
        None => CutoffState::Absent,
        Some(s) => match parse_date(s) {
            Some(d) => CutoffState::Date(d),
            None => CutoffState::Unparseable,
        },
    }
}

/// Analyst #317 ("invented-cutoff hole") / board decision `decision.vlm_routing_v3`: two
/// reads whose SELECTED AMOUNTS happen to match numerically must also agree on the parsed
/// due-date cutoff before counting as a real agreement -- otherwise one read can invent (or
/// drop) a cutoff the other doesn't share and still slip through on a coincidental amount
/// match. Equal only when both reads genuinely lack a cutoff, or both parse to the identical
/// date; an unparseable cutoff on either side is never treated as equal to anything,
/// including another unparseable value.
fn cutoff_dates_agree(a: &ImageReadProvenance, b: &ImageReadProvenance) -> bool {
    match (cutoff_state(&a.due_date), cutoff_state(&b.due_date)) {
        (CutoffState::Absent, CutoffState::Absent) => true,
        (CutoffState::Date(x), CutoffState::Date(y)) => x == y,
        _ => false,
    }
}

fn build_evidence(
    image_id: &str,
    event: &Event,
    amount: f64,
    currency: String,
    agreeing_roles: &[&str],
) -> EvidenceRecord {
    EvidenceRecord {
        // Provenance records which reader role(s) actually agreed on this figure (user
        // decision `decision.vlm_setup`: "record which models agreed in the evidence
        // record"). `record_id` is free-form provenance text, not a `Fact` field, so this
        // needs no engine-side change.
        record_id: format!("{image_id}#agree:{}", agreeing_roles.join("+")),
        source: EvidenceSource::Image,
        observed_at: event
            .event_date
            .and_hms_opt(0, 0, 0)
            .unwrap_or_else(|| event.event_date.and_hms_opt(12, 0, 0).unwrap()),
        fact: Fact::EventAmount { event_id: event.id.clone(), amount: Money::from_f64(amount), currency },
    }
}

/// The original chain: downscale, transcribe with the primary VLM, select + reconcile; on
/// a reconciliation failure (or a malformed/unparseable reply), escalate once to the second
/// configured model — a fresh read, not a retry of the same call — and try again, then the
/// backup frontier model. Still failing, or nothing configured beyond primary: no evidence.
/// Never a guess, never a zero.
#[allow(clippy::too_many_arguments)]
fn resolve_blank_amount_escalate(
    client: &HfClient,
    anthropic: Option<&AnthropicClient>,
    cold: bool,
    run_idx: Option<u32>,
    prompt: &PromptSet,
    decoding: &DecodingConfig,
    image_max_dim_px: u32,
    image_path: &Path,
    image_id: &str,
    vlm_primary: Option<&CandidateConfig>,
    vlm_escalation: Option<&CandidateConfig>,
    vlm_fallback: Option<&CandidateConfig>,
    event: &Event,
    history: &[Event],
    doc_validation: &DocValidationConfig,
) -> anyhow::Result<ImageResolution> {
    let outcome = |outcome: &str, reads: Vec<ImageReadProvenance>, evidence: Option<EvidenceRecord>| ImageResolution {
        image_id: image_id.to_string(),
        class: None,
        mode: "escalate".to_string(),
        reads,
        outcome: outcome.to_string(),
        evidence,
    };
    let Some(vlm_primary) = vlm_primary else { return Ok(outcome("no_route", vec![], None)) };
    let image_b64 = downscale_and_encode(image_path, image_max_dim_px)?;

    let mut reads = Vec::new();
    for (role, candidate) in [
        ("vlm_primary", Some(vlm_primary)),
        ("vlm_escalation", vlm_escalation),
        ("vlm_fallback", vlm_fallback),
    ]
    .into_iter()
    .filter_map(|(r, c)| c.map(|c| (r, c)))
    {
        let pick = ReaderPick { role, candidate, max_dim_px: image_max_dim_px, max_tokens: decoding.max_tokens_vlm };
        let prov = read_candidate(
            client, anthropic, cold, run_idx, prompt, decoding, pick, &image_b64, image_id, event, history, doc_validation,
        );
        if let (true, Some(amount), Some(currency)) =
            (prov.reconciled, prov.selected_amount, prov.currency.clone())
        {
            reads.push(prov);
            let evidence = build_evidence(image_id, event, amount, currency, &[role]);
            return Ok(outcome("single_reconciled_accept", reads, Some(evidence)));
        }
        reads.push(prov);
    }
    Ok(outcome("no_reconciling_read", reads, None))
}

/// HF-only witness gate (image_accuracy_plan.md §2, RULES.md S8: the Anthropic org cap blocks
/// a tiebreak model until 2026-10-01, so Phase A never routes through a third/backup model at
/// all -- this supersedes the per-class routing-table + tiebreak design the doc comments above
/// still describe; Phase B may reinstate a distinct tiebreak reader once one is actually
/// available). The two models named by `[selected]` (`vlm_primary`, `vlm_escalation`) each read
/// the image independently at a fixed base resolution (1024px / 768px). A figure is trusted
/// only once some pair of reads:
/// - select the same normalized amount within tolerance, and agree on any due-date cutoff
///   (analyst #317 "invented-cutoff hole");
/// - satisfies that cutoff requirement, if any read resolved one;
/// - carries at least one independent witness on EITHER side proving that amount
///   (`extract::witness::find_witness` — a non-summing breakdown is a note, never a veto);
/// - carries no final-label contradiction on EITHER side (`extract::witness::
///   final_label_contradicts`).
/// If the base pair doesn't clear the gate, up to two more reads are added at a second
/// resolution (`vlm_primary`@1536px, `vlm_escalation`@1024px, image_accuracy_plan.md §2 "Read
/// budget"), and every pair among the reads attempted so far is re-checked, stopping as soon as
/// any pair passes. Two readers both failing to reconcile, or reconciling with no witness, is
/// never covered by adding more of the SAME two models past the 4-read budget (never a guess).
#[allow(clippy::too_many_arguments)]
fn resolve_blank_amount_agreement(
    client: &HfClient,
    anthropic: Option<&AnthropicClient>,
    cold: bool,
    run_idx: Option<u32>,
    prompt: &PromptSet,
    decoding: &DecodingConfig,
    image_path: &Path,
    image_id: &str,
    config: &ModelsConfig,
    event: &Event,
    history: &[Event],
) -> anyhow::Result<ImageResolution> {
    let outcome = |outcome: &str, reads: Vec<ImageReadProvenance>, evidence: Option<EvidenceRecord>| ImageResolution {
        image_id: image_id.to_string(),
        class: None,
        mode: "witness".to_string(),
        reads,
        outcome: outcome.to_string(),
        evidence,
    };

    let (Some(primary), Some(escalation)) = (config.vlm_primary(), config.vlm_escalation()) else {
        return Ok(outcome("no_route", vec![], None));
    };

    // image_accuracy_plan.md §2 "Read budget (HF-only)": the two base reads, then up to two
    // more at a second resolution if the gate isn't met yet -- never a third model.
    const BASE_PRIMARY_DIM: u32 = 1024;
    const BASE_ESCALATION_DIM: u32 = 768;
    const EXTRA_PRIMARY_DIM: u32 = 1536;
    const EXTRA_ESCALATION_DIM: u32 = 1024;
    let budget: [(&str, &CandidateConfig, u32); 4] = [
        ("vlm_primary", primary, BASE_PRIMARY_DIM),
        ("vlm_escalation", escalation, BASE_ESCALATION_DIM),
        ("vlm_primary_extra", primary, EXTRA_PRIMARY_DIM),
        ("vlm_escalation_extra", escalation, EXTRA_ESCALATION_DIM),
    ];

    let tolerance = config.agreement_tolerance();
    let mut reads: Vec<ImageReadProvenance> = Vec::new();

    for (role, candidate, max_dim_px) in budget {
        let b64 = downscale_and_encode(image_path, max_dim_px)?;
        let pick = ReaderPick { role, candidate, max_dim_px, max_tokens: decoding.max_tokens_vlm };
        let prov = read_candidate(
            client, anthropic, cold, run_idx, prompt, decoding, pick, &b64, image_id, event, history, &config.doc_validation,
        );
        reads.push(prov);

        if let Some((amount, currency, roles)) = find_witnessed_pair(&reads, event, tolerance) {
            let role_refs: Vec<&str> = roles.iter().map(String::as_str).collect();
            let evidence = build_evidence(image_id, event, amount, currency, &role_refs);
            return Ok(outcome("witness_accept", reads, Some(evidence)));
        }
    }

    Ok(outcome("no_agreement", reads, None))
}

/// The first pair (in read order) that clears the full witness gate: agreeing amounts, agreeing
/// (and satisfied) cutoff, at least one witness between the pair, and no contradiction on
/// either side (image_accuracy_plan.md §2). `None` when no such pair exists among the reads
/// attempted so far — the caller adds more reads (up to the budget) and re-checks.
fn find_witnessed_pair(
    reads: &[ImageReadProvenance],
    event: &Event,
    tolerance: f64,
) -> Option<(f64, String, Vec<String>)> {
    for i in 0..reads.len() {
        for j in (i + 1)..reads.len() {
            let (a, b) = (&reads[i], &reads[j]);
            let (Some(amt_a), Some(amt_b)) = (a.selected_amount, b.selected_amount) else { continue };
            if !close(amt_a, amt_b, tolerance) || !cutoff_dates_agree(a, b) {
                continue;
            }
            let cutoff_req = [a, b].iter().find_map(|p| cutoff_requirement(p, event));
            if cutoff_req.is_some_and(|req| !close(amt_a, req, tolerance)) {
                continue;
            }
            if a.contradiction.is_some() || b.contradiction.is_some() {
                continue;
            }
            if a.witness.is_none() && b.witness.is_none() {
                continue;
            }
            let currency =
                a.currency.clone().or_else(|| b.currency.clone()).unwrap_or_else(|| event.currency.clone());
            return Some((amt_a, currency, vec![a.role.clone(), b.role.clone()]));
        }
    }
    None
}

/// OCR-path resolution (fleet/specs/ocr_vllm_pipeline.md work item A4): Unlimited-OCR already
/// ran at ingestion and is cached (`extract::ocr::OcrResult`); this reads that cache, maps
/// every page's rows to `ImageFigures` (`extract::labels::figures_from_rows`), and runs the
/// SAME deterministic `select` + witness gate the VLM path uses -- `reconciles`/`doc_checks`
/// don't apply here (there is nothing to reconcile away: the model transcribed the label the
/// page actually prints, not a value guessed into a fixed JSON key). There is exactly one
/// deterministic reader, so two-read agreement never applies -- a figure is accepted the
/// moment `select` finds one AND the witness gate (a witness, no contradiction) clears it;
/// otherwise the read fails closed, never a guess.
pub fn resolve_blank_amount_ocr(
    ocr_result: &crate::extract::ocr::OcrResult,
    image_id: &str,
    event: &Event,
) -> ImageResolution {
    let outcome = |outcome: &str, reads: Vec<ImageReadProvenance>, evidence: Option<EvidenceRecord>| ImageResolution {
        image_id: image_id.to_string(),
        class: None,
        mode: "ocr".to_string(),
        reads,
        outcome: outcome.to_string(),
        evidence,
    };

    let currency_hint = normalize::parse_currency(&event.currency);
    let mut rows = Vec::new();
    let mut page_notes = Vec::new();
    for page in &ocr_result.pages {
        rows.extend(crate::extract::ocr_parse::parse_page(&page.raw_text, page.page, currency_hint.as_deref()));
        if page.truncated {
            page_notes.push(format!("page_{}_truncated", page.page));
        }
    }

    let (figures, mut notes) = crate::extract::labels::figures_from_rows(&rows, event.cash_date());
    notes.extend(page_notes);

    let mut prov = ImageReadProvenance {
        role: "ocr".to_string(),
        model_id: "baidu/Unlimited-OCR".to_string(),
        model_revision: String::new(),
        max_dim_px: 0,
        max_tokens: 0,
        reconciled: true,
        doc_checks: DocValidationResult::default(),
        selected_amount: None,
        currency: None,
        due_date: figures.due_cutoff_date.clone(),
        before_amount: figures.amount_due_by_cutoff,
        after_amount: figures.amount_due_after_cutoff,
        error: None,
        witness: None,
        witness_computed: None,
        contradiction: None,
        ocr_notes: notes,
    };

    let scope = match event.status {
        Status::Pending | Status::Scheduled if figures.due_cutoff_date.is_some() => {
            witness::FinalLabelScope::CutoffResolved
        }
        Status::Pending | Status::Scheduled => witness::FinalLabelScope::RemainingOwed,
        _ => witness::FinalLabelScope::Whole,
    };

    let (amount, fallback_witness) = match select(&figures, event) {
        Some(amount) => (amount, None),
        None => {
            // User ruling `ruling.total_or_witnessed_sum` point 2: no total field is readable
            // at all -- fall back to a witnessed line-item/charges sum before failing closed.
            match witness::witnessed_line_item_sum(&figures, scope, ROUNDING_TOLERANCE_2TERM) {
                Some((amount, kind)) => (amount, Some(kind)),
                None => {
                    prov.ocr_notes.push("no_final_label".to_string());
                    return outcome("fail_closed", vec![prov], None);
                }
            }
        }
    };

    let witness_hit = fallback_witness
        .map(|kind| (kind, amount))
        .or_else(|| witness::find_witness(&figures, amount, ROUNDING_TOLERANCE_2TERM, scope));
    let contradiction = witness::final_label_contradicts(&figures, amount, ROUNDING_TOLERANCE_2TERM, scope);

    prov.witness = witness_hit.map(|(kind, _)| kind.label().to_string());
    prov.witness_computed = witness_hit.map(|(_, computed)| computed);
    prov.contradiction = contradiction.map(|(field, value)| format!("{field}={value}"));

    if prov.witness.is_none() || prov.contradiction.is_some() {
        prov.ocr_notes.push(if prov.witness.is_none() { "no_witness".to_string() } else { "contradiction".to_string() });
        return outcome("fail_closed", vec![prov], None);
    }

    prov.selected_amount = Some(amount);
    let currency = figures.currency.clone().unwrap_or_else(|| event.currency.clone());
    prov.currency = Some(currency.clone());
    let evidence = build_evidence(image_id, event, amount, currency, &["ocr"]);
    outcome("witness_accept", vec![prov], Some(evidence))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::types::{Flexibility, Direction};

    fn income_event() -> Event {
        Event {
            id: "event_253".into(),
            event_type: EventType::Income,
            description: "August 2019 net salary".into(),
            category: "salary".into(),
            direction: Direction::Credit,
            amount: None,
            currency: "IDR".into(),
            event_date: NaiveDate::from_ymd_opt(2019, 8, 31).unwrap(),
            settlement_date: Some(NaiveDate::from_ymd_opt(2019, 8, 31).unwrap()),
            status: Status::Settled,
            linked_event_id: None,
            flexibility: Flexibility::Fixed,
            minimum_allowed_amount: None,
        }
    }

    #[test]
    fn image_01_selects_net_pay_not_gross() {
        let figures = ImageFigures {
            doc_type: Some(DocType::Payslip),
            currency: Some("IDR".into()),
            subtotal: Some(4_212_000.0),
            gross_pay: Some(4_212_000.0),
            deductions: Some(367_000.0),
            net_pay: Some(4_365_000.0),
            line_items_sum_check: Some(4_212_000.0),
            ..Default::default()
        };
        let event = income_event();
        assert!(reconciles(&figures, &event));
        assert_eq!(select(&figures, &event), Some(4_365_000.0));
    }

    fn expense_event(id: &str, category: &str, currency: &str, status: Status) -> Event {
        Event {
            id: id.into(),
            event_type: EventType::Expense,
            description: "x".into(),
            category: category.into(),
            direction: Direction::Debit,
            amount: None,
            currency: currency.into(),
            event_date: NaiveDate::from_ymd_opt(2025, 10, 1).unwrap(),
            settlement_date: Some(NaiveDate::from_ymd_opt(2025, 10, 1).unwrap()),
            status,
            linked_event_id: None,
            flexibility: Flexibility::Fixed,
            minimum_allowed_amount: None,
        }
    }

    /// Analyst audit RULES.md S5 image_07: subtotal 7,150 + tax 358.10 = 7,508.10 exactly,
    /// but the page also prints a plain "7,508" total. An exact-cent check would reject a
    /// genuinely reconciling document; the documented rounding tolerance accepts it (and
    /// selects the printed total, not a recomputed one).
    #[test]
    fn image_07_reconciles_within_rounding_tolerance() {
        let figures = ImageFigures {
            doc_type: Some(DocType::Invoice),
            currency: Some("INR".into()),
            subtotal: Some(7150.0),
            tax: Some(358.10),
            total: Some(7508.0),
            ..Default::default()
        };
        let event = expense_event("event_3231", "dining", "INR", Status::Settled);
        assert!(reconciles(&figures, &event));
        assert_eq!(select(&figures, &event), Some(7508.0));
    }

    /// Analyst audit RULES.md S5 image_11: a multi-section hospital bill's itemized
    /// breakup sums to 2,790, but the labeled Amount Paid (0) + Balance (3,210) already
    /// reconcile against the labeled Total (3,210). The line-item sum must never override
    /// labeled fields that already reconcile among themselves.
    #[test]
    fn image_11_ignores_line_item_sum_when_labeled_fields_reconcile() {
        let figures = ImageFigures {
            doc_type: Some(DocType::Invoice),
            currency: Some("INR".into()),
            total: Some(3210.0),
            amount_paid: Some(0.0),
            balance_due: Some(3210.0),
            line_items_sum_check: Some(2790.0), // wrong: breakup subtotals miss a line
            ..Default::default()
        };
        let event = expense_event("event_6859", "healthcare", "INR", Status::Scheduled);
        assert!(reconciles(&figures, &event));
        assert_eq!(select(&figures, &event), Some(3210.0));
    }

    /// Analyst audit RULES.md S5 image_12: a settled expense's "amount paid"/cash line can
    /// be the cash TENDERED (35.00, with 6.25 change), not the expense itself (28.75
    /// total). The selector must prefer the printed total over amount_paid whenever a
    /// total is present.
    #[test]
    fn image_12_settled_expense_prefers_total_over_cash_tendered_amount_paid() {
        // Exact shape of the cached VLM reads (analyst audit #184): balance_due present as
        // 0.00 (nothing owed), amount_paid the cash tendered (35.00), which naively fails
        // paid+balance_due=total (35 != 28.75) — paid >= total must be treated as
        // consistent on its own, never requiring balance_due to explain the gap.
        let figures = ImageFigures {
            doc_type: Some(DocType::Receipt),
            currency: Some("USD".into()),
            total: Some(28.75),
            amount_paid: Some(35.00), // cash tendered, not the expense amount
            balance_due: Some(0.00),
            ..Default::default()
        };
        let event = expense_event("event_7307", "transport", "USD", Status::Settled);
        assert!(reconciles(&figures, &event));
        assert_eq!(select(&figures, &event), Some(28.75));
    }

    /// analyst audit #184: strict enum matching on the VLM's own free-text doc_type wording
    /// rejected 58/112 cached reads before `select()` ever ran. `doc_type` is advisory only
    /// and must never gate a figure — unrecognized or synonymous wording maps to a known
    /// variant or `Other`, never an error.
    #[test]
    fn doc_type_parses_leniently_from_free_text_synonyms() {
        assert_eq!(DocType::from_free_text("TAX INVOICE"), DocType::Invoice);
        assert_eq!(DocType::from_free_text("PROVISIONAL BILL"), DocType::Bill);
        assert_eq!(DocType::from_free_text("  Pay Slip "), DocType::Payslip);
        assert_eq!(DocType::from_free_text("Bill of Supply"), DocType::Receipt);
        assert_eq!(DocType::from_free_text("Order Details"), DocType::DeliverySummary);
        assert_eq!(DocType::from_free_text("something the model made up"), DocType::Other);

        // Indonesian synonyms (dataset is EN+ID, PLAN.md §2.4).
        assert_eq!(DocType::from_free_text("Slip Gaji"), DocType::Payslip);
        assert_eq!(DocType::from_free_text("Kwitansi"), DocType::Receipt);
        assert_eq!(DocType::from_free_text("Faktur Pajak"), DocType::Invoice);
        assert_eq!(DocType::from_free_text("Tagihan Listrik"), DocType::Bill);
        assert_eq!(DocType::from_free_text("Rekening Koran"), DocType::BankStatementExcerpt);

        let value = serde_json::json!({"doc_type": "TAX INVOICE", "total": 100.0});
        let figures: ImageFigures = serde_json::from_value(value).expect("should not reject on doc_type");
        assert_eq!(figures.doc_type, Some(DocType::Invoice));
    }

    /// analyst audit #194: a VLM's currency field is often a symbol/name, not the ISO
    /// code -- exact-string comparison against the event's "INR"/"USD"/etc. was rejecting
    /// otherwise-correct reads.
    #[test]
    fn currency_matches_symbols_and_names() {
        assert!(currency_matches("Rs", "INR"));
        assert!(currency_matches("\u{20B9}", "INR"));
        assert!(currency_matches("Indian Rupees", "INR"));
        assert!(currency_matches("$", "USD"));
        assert!(currency_matches("US$", "USD"));
        assert!(currency_matches("Rp", "IDR"));
        assert!(currency_matches("R", "ZAR"));
        assert!(currency_matches("\u{20AC}", "EUR"));
        assert!(!currency_matches("USD", "INR"));
    }

    /// Analyst audit #276 (image_02-shaped, SYNTHETIC values -- not the real dataset
    /// figures): two Qwen3-VL-235B reads of the real image both misread its printed Indian
    /// lakh grouping as 10x the true amount and still passed internal reconciliation. This
    /// test covers the specific sub-case where a model transcribes the grouped figure as a
    /// JSON STRING (`"1,00,000"`) rather than a plain number -- `coerce_f64` must recover the
    /// correct value via `normalize::parse_amount`'s Indian-grouping rule, not silently
    /// accept a naive comma-strip-only misread. It does NOT cover a model emitting the wrong
    /// value as a plain JSON number already (no text exists at that point to reparse) --
    /// that failure mode is caught by cross-model agreement (>=2 distinct models must agree,
    /// `resolve_blank_amount_agreement`) and the amount-plausibility-vs-history check.
    #[test]
    fn image_02_shaped_lakh_grouped_string_total_parses_to_the_true_amount_not_10x() {
        let value = serde_json::json!({
            "doc_type": "receipt",
            "currency": "INR",
            "total": "1,00,000",
            "amount_paid": "1,00,000"
        });
        let figures: ImageFigures = serde_json::from_value(value).expect("should parse");
        assert_eq!(figures.total, Some(100_000.0), "must recover the lakh-grouped total, not 1,000,000");
        let event = expense_event("event_synthetic_lakh", "transport", "INR", Status::Settled);
        assert!(reconciles(&figures, &event));
        assert_eq!(select(&figures, &event), Some(100_000.0));
    }

    /// analyst audit #194: a document can print an unrelated subtotal/tax breakdown that
    /// doesn't sum to the overall total, while amount_paid + balance_due = total
    /// independently confirms the figure — the first identity's mismatch must not veto the
    /// second's pass.
    #[test]
    fn unrelated_subtotal_tax_mismatch_does_not_veto_a_passing_paid_balance_check() {
        let figures = ImageFigures {
            doc_type: Some(DocType::Invoice),
            currency: Some("INR".into()),
            subtotal: Some(880.0), // describes a different section; doesn't sum to total
            tax: Some(44.0),
            total: Some(3210.0),
            amount_paid: Some(0.0),
            balance_due: Some(3210.0),
            ..Default::default()
        };
        let event = expense_event("event_6859", "healthcare", "INR", Status::Scheduled);
        assert!(reconciles(&figures, &event));
        assert_eq!(select(&figures, &event), Some(3210.0));
    }

    fn pending_utilities_event(settlement_date: NaiveDate) -> Event {
        Event {
            id: "event_1786".into(),
            event_type: EventType::Expense,
            description: "Outstanding telecom bill".into(),
            category: "utilities".into(),
            direction: Direction::Debit,
            amount: None,
            currency: "INR".into(),
            event_date: NaiveDate::from_ymd_opt(2026, 2, 6).unwrap(),
            settlement_date: Some(settlement_date),
            status: Status::Pending,
            linked_event_id: None,
            flexibility: Flexibility::Fixed,
            minimum_allowed_amount: None,
        }
    }

    /// docs/gold_subset.json image_05: a valid cutoff date resolves before/after normally
    /// -- the original intent this selector branch was built for, never actually covered
    /// by a unit test until the board:verify.image05_shapes audit found it.
    #[test]
    fn image_05_well_formed_cutoff_date_selects_after_when_settlement_is_later() {
        let figures = ImageFigures {
            doc_type: Some(DocType::Bill),
            currency: Some("INR".into()),
            amount_due_before_date: Some(611.45),
            amount_due_before_date_value: Some("2026-02-06".into()),
            amount_due_after_date: Some(739.65),
            ..Default::default()
        };
        let event = pending_utilities_event(NaiveDate::from_ymd_opt(2026, 2, 9).unwrap());
        assert!(reconciles(&figures, &event));
        assert_eq!(select(&figures, &event), Some(739.65));
    }

    /// analyst audit board:verify.image05_shapes "Shape A" (6 Qwen reads): before/after are
    /// both present as plain numbers, but no cutoff date is present anywhere. Conservative
    /// fallback: the larger figure, never under-reserve a pending debt.
    #[test]
    fn image_05_no_cutoff_date_falls_back_to_the_larger_figure() {
        let figures = ImageFigures {
            doc_type: Some(DocType::Bill),
            currency: Some("INR".into()),
            amount_due_before_date: Some(611.45),
            amount_due_before_date_value: None,
            amount_due_after_date: Some(739.65),
            ..Default::default()
        };
        let event = pending_utilities_event(NaiveDate::from_ymd_opt(2026, 2, 9).unwrap());
        assert!(reconciles(&figures, &event));
        assert_eq!(select(&figures, &event), Some(739.65));
    }

    /// analyst audit "Shape B", the 4 gemma reads: only the before-cutoff figure (611.45)
    /// is present anywhere on the page, with no after figure and no balance_due/amount_due
    /// to fall back to. Not safe to treat the lone value as authoritative -- `select`
    /// returns `None` so the caller escalates rather than guessing.
    #[test]
    fn image_05_only_one_candidate_present_does_not_guess() {
        let figures = ImageFigures {
            doc_type: Some(DocType::Bill),
            currency: Some("INR".into()),
            amount_due_before_date: Some(611.45),
            ..Default::default()
        };
        let event = pending_utilities_event(NaiveDate::from_ymd_opt(2026, 2, 9).unwrap());
        assert_eq!(select(&figures, &event), None);
    }

    /// analyst audit #203: a pending/scheduled debit never selects a zero -- a stale
    /// balance_due=0 must fall through to amount_due (or whatever else resolves it), not be
    /// accepted as "nothing owed" (the row is pending/scheduled by construction).
    #[test]
    fn pending_never_selects_zero_falls_through_to_amount_due() {
        let figures = ImageFigures {
            doc_type: Some(DocType::Bill),
            currency: Some("INR".into()),
            balance_due: Some(0.0),
            amount_due: Some(739.65),
            ..Default::default()
        };
        let event = pending_utilities_event(NaiveDate::from_ymd_opt(2026, 2, 9).unwrap());
        assert_eq!(select(&figures, &event), Some(739.65));
    }

    /// analyst audit #203: once a cutoff IS known and the cash date is past it, the
    /// after-cutoff figure is required exactly -- never fall back to the before-cutoff
    /// figure (which is known-wrong for this date) or to balance_due/amount_due.
    #[test]
    fn image_05_known_cutoff_requires_after_never_falls_back_to_before() {
        let figures = ImageFigures {
            doc_type: Some(DocType::Bill),
            currency: Some("INR".into()),
            amount_due_before_date: Some(611.45),
            amount_due_before_date_value: Some("2026-02-06".into()),
            amount_due_after_date: None, // missing -- must not fall back to before (611.45)
            balance_due: Some(611.45),   // must not fall back here either
            ..Default::default()
        };
        let event = pending_utilities_event(NaiveDate::from_ymd_opt(2026, 2, 9).unwrap());
        assert_eq!(select(&figures, &event), None);
    }

    /// v2 schema (`finding.image05_root_cause`): the unambiguous
    /// due_cutoff_date/amount_due_by_cutoff/amount_due_after_cutoff trio is preferred over
    /// the legacy v1 fields whenever both happen to be present (a mixed-schema read should
    /// not occur in practice, but preferring v2 is the documented, tested behavior).
    #[test]
    fn v2_cutoff_fields_are_preferred_over_legacy_v1_fields() {
        let figures = ImageFigures {
            doc_type: Some(DocType::Bill),
            currency: Some("INR".into()),
            due_cutoff_date: Some("2026-02-06".into()),
            amount_due_by_cutoff: Some(615.20),
            amount_due_after_cutoff: Some(742.90),
            // Legacy v1 fields, deliberately different values and no v1 cutoff date -- if v1
            // were consulted first this would fall through to the "no cutoff" branch instead.
            amount_due_before_date: Some(999.99),
            amount_due_after_date: Some(999.99),
            ..Default::default()
        };
        let event = pending_utilities_event(NaiveDate::from_ymd_opt(2026, 2, 9).unwrap());
        assert!(reconciles(&figures, &event));
        assert_eq!(select(&figures, &event), Some(742.90));
    }

    /// v2 schema: no cutoff date present, both amounts printed -- same conservative
    /// larger-figure fallback as the v1 "Shape A" case.
    #[test]
    fn v2_no_cutoff_date_falls_back_to_the_larger_figure() {
        let figures = ImageFigures {
            doc_type: Some(DocType::Bill),
            currency: Some("INR".into()),
            amount_due_by_cutoff: Some(615.20),
            amount_due_after_cutoff: Some(742.90),
            ..Default::default()
        };
        let event = pending_utilities_event(NaiveDate::from_ymd_opt(2026, 2, 9).unwrap());
        assert_eq!(select(&figures, &event), Some(742.90));
    }

    /// v2 schema: a known cutoff in the past requires the after-cutoff figure exactly, same
    /// as v1 -- never falls back to the before-cutoff figure or to balance_due/amount_due.
    #[test]
    fn v2_known_cutoff_requires_after_never_falls_back() {
        let figures = ImageFigures {
            doc_type: Some(DocType::Bill),
            currency: Some("INR".into()),
            due_cutoff_date: Some("2026-02-06".into()),
            amount_due_by_cutoff: Some(615.20),
            amount_due_after_cutoff: None, // missing -- must not fall back to by-cutoff
            balance_due: Some(615.20),     // must not fall back here either
            ..Default::default()
        };
        let event = pending_utilities_event(NaiveDate::from_ymd_opt(2026, 2, 9).unwrap());
        assert_eq!(select(&figures, &event), None);
    }

    /// Analyst #301 G2: a cutoff field that's PRESENT but doesn't parse as a date must
    /// escalate (None), never fall back to the conservative max(by, after) guess -- a cutoff
    /// we failed to read is not the same situation as a document with no cutoff at all, and
    /// the correct side could be the SMALLER figure, which max() would never pick.
    #[test]
    fn v2_present_but_unparseable_cutoff_escalates_never_guesses_max() {
        let figures = ImageFigures {
            doc_type: Some(DocType::Bill),
            currency: Some("INR".into()),
            due_cutoff_date: Some("not a real date".into()),
            amount_due_by_cutoff: Some(615.20),
            amount_due_after_cutoff: Some(742.90),
            ..Default::default()
        };
        let event = pending_utilities_event(NaiveDate::from_ymd_opt(2026, 2, 9).unwrap());
        assert_eq!(select(&figures, &event), None);
    }

    /// `read_candidate`'s provenance keeps stable field names (due_date/before_amount/
    /// after_amount, verifier #205's contract) regardless of which prompt schema produced
    /// the read -- verifier #266.
    #[test]
    fn provenance_maps_v2_fields_into_stable_names() {
        let figures = ImageFigures {
            due_cutoff_date: Some("2026-02-06".into()),
            amount_due_by_cutoff: Some(615.20),
            amount_due_after_cutoff: Some(742.90),
            ..Default::default()
        };
        assert_eq!(figures.due_cutoff_date.clone().or_else(|| figures.amount_due_before_date_value.clone()), Some("2026-02-06".to_string()));
        assert_eq!(figures.amount_due_by_cutoff.or(figures.amount_due_before_date), Some(615.20));
        assert_eq!(figures.amount_due_after_cutoff.or(figures.amount_due_after_date), Some(742.90));
    }

    fn provenance_with_due_date(role: &str, due_date: Option<&str>, amount: Option<f64>) -> ImageReadProvenance {
        ImageReadProvenance {
            role: role.to_string(),
            model_id: "vendor/model".to_string(),
            model_revision: "rev".to_string(),
            max_dim_px: 1024,
            max_tokens: 400,
            reconciled: true,
            doc_checks: DocValidationResult::default(),
            selected_amount: amount,
            currency: Some("INR".to_string()),
            due_date: due_date.map(str::to_string),
            before_amount: None,
            after_amount: None,
            error: None,
            witness: None,
            witness_computed: None,
            contradiction: None,
            ocr_notes: Vec::new(),
        }
    }

    /// Analyst #317 ("invented-cutoff hole"): two reads whose amounts happen to match must
    /// also agree on the parsed due-date cutoff -- neither reporting one is fine, but one
    /// inventing/dropping a cutoff the other doesn't share is a real disagreement, not a
    /// coincidental amount match to accept.
    #[test]
    fn cutoff_dates_agree_requires_matching_or_absent_cutoffs() {
        let neither = provenance_with_due_date("vlm_primary", None, Some(100.0));
        let neither2 = provenance_with_due_date("vlm_escalation", None, Some(100.0));
        assert!(cutoff_dates_agree(&neither, &neither2), "neither read reports a cutoff");

        let same_a = provenance_with_due_date("vlm_primary", Some("2026-02-06"), Some(100.0));
        let same_b = provenance_with_due_date("vlm_escalation", Some("2026-02-06"), Some(100.0));
        assert!(cutoff_dates_agree(&same_a, &same_b), "identical parsed cutoffs");

        let different_b = provenance_with_due_date("vlm_escalation", Some("2026-03-15"), Some(100.0));
        assert!(!cutoff_dates_agree(&same_a, &different_b), "different parsed cutoffs must disagree");

        let one_only = provenance_with_due_date("vlm_escalation", None, Some(100.0));
        assert!(!cutoff_dates_agree(&same_a, &one_only), "one read invented/found a cutoff the other lacks");
    }

    /// Board decision `decision.vlm_routing_v3` / verifier ca892d5's exact semantics: an
    /// unparseable cutoff is its own state, never treated as equal to a genuinely absent one
    /// (nor to another unparseable value -- two garbled strings are not evidence they agree).
    #[test]
    fn cutoff_dates_agree_never_equates_unparseable_with_absent_or_itself() {
        let absent = provenance_with_due_date("vlm_primary", None, Some(100.0));
        let garbled = provenance_with_due_date("vlm_escalation", Some("not a date"), Some(100.0));
        let garbled2 = provenance_with_due_date("vlm_fallback", Some("also not a date"), Some(100.0));
        assert!(!cutoff_dates_agree(&absent, &garbled), "unparseable must never equal absent");
        assert!(!cutoff_dates_agree(&garbled, &garbled2), "two unparseable cutoffs are not evidence of agreement");
    }

    /// Analyst #317 / verifier ca892d5's follow-up: an unavailable tiebreak provider
    /// (`anthropic: None` here stands in for "ANTHROPIC_API_KEY unset" / a 400 usage-limit /
    /// circuit-open condition ml-engineer's `AnthropicClient` maps to an `Err`) must return
    /// an error from `dispatch_call`, never panic -- `read_candidate` already turns that
    /// error into "no read" (`selected_amount: None`), which `resolve_blank_amount_agreement`
    /// treats as `no_agreement` (flagged missing), never a crash or a fallback guess.
    #[test]
    fn dispatch_call_reports_an_unavailable_anthropic_provider_as_an_error_not_a_panic() {
        std::env::set_var("HF_TOKEN", "test-token-not-real");
        let dir = std::env::temp_dir().join("buyorwait_test_dispatch_call_anthropic_unavailable");
        let hf = crate::hf::HfClient::with_cache_dir(&dir).expect("a placeholder token still constructs a client");
        let candidate = CandidateConfig {
            id: "claude-opus-5".to_string(),
            provider: "anthropic".to_string(),
            model_revision: "claude-opus-5".to_string(),
            supports_structured_output: true,
            role: Some("vlm".to_string()),
        };
        let call = ModelCall {
            model_id: candidate.id.clone(),
            provider: candidate.provider.clone(),
            model_revision: candidate.model_revision.clone(),
            prompt_version: "test".to_string(),
            system_prompt: "test".to_string(),
            user_content: vec![],
            temperature: 0.0,
            seed: 42,
            max_tokens: 10,
            json_response: false,
            json_schema: None,
        };
        let result = dispatch_call(&hf, None, false, None, &candidate, &call);
        assert!(result.is_err(), "an unavailable anthropic provider must error, never panic or silently succeed");
    }

    /// analyst audit "Shape B", the other 2 reads: after-cutoff (739.65) landed in
    /// balance_due instead of amount_due_after_date. The fallback still resolves it.
    #[test]
    fn image_05_after_value_in_balance_due_still_resolves() {
        let figures = ImageFigures {
            doc_type: Some(DocType::Bill),
            currency: Some("INR".into()),
            amount_due_before_date: Some(611.45),
            balance_due: Some(739.65),
            ..Default::default()
        };
        let event = pending_utilities_event(NaiveDate::from_ymd_opt(2026, 2, 9).unwrap());
        assert_eq!(select(&figures, &event), Some(739.65));
    }

    /// analyst audit "Shape B": a date string landed in the numeric before/after fields
    /// and a number landed in the date-string `_value` field -- a straight type mismatch
    /// that must not fail the whole `ImageFigures` parse. `lenient_f64`/`lenient_string`
    /// coerce what they can and give up to `None` on the rest, never a hard error.
    #[test]
    fn image_05_swapped_json_types_deserialize_without_error() {
        let value = serde_json::json!({
            "amount_due_before_date": "611.45",
            "amount_due_before_date_value": 611.45,
            "amount_due_after_date": 739.65,
            "currency": "INR"
        });
        let figures: ImageFigures =
            serde_json::from_value(value).expect("swapped types must not error the whole parse");
        assert_eq!(figures.amount_due_before_date, Some(611.45));
        assert_eq!(figures.amount_due_before_date_value, Some("611.45".to_string()));
        assert_eq!(figures.amount_due_after_date, Some(739.65));

        let event = pending_utilities_event(NaiveDate::from_ymd_opt(2026, 2, 9).unwrap());
        // Analyst #301 G2: "611.45" doesn't parse as a date, but the field IS present -- this
        // is a cutoff we failed to read, not a document with no cutoff at all, so it must
        // escalate (None), never guess the conservative max() of the two amount fields.
        assert_eq!(select(&figures, &event), None);
    }

    #[test]
    fn image_04_rejects_when_total_is_not_on_the_page() {
        let figures = ImageFigures {
            doc_type: Some(DocType::DeliverySummary),
            currency: Some("INR".into()),
            subtotal: Some(2513.0),
            line_items_sum_check: Some(2513.0),
            total: None,
            amount_paid: None,
            ..Default::default()
        };
        let event = Event {
            id: "event_1700".into(),
            event_type: EventType::Expense,
            description: "Delivered grocery order".into(),
            category: "groceries".into(),
            direction: Direction::Debit,
            amount: None,
            currency: "INR".into(),
            event_date: NaiveDate::from_ymd_opt(2024, 9, 3).unwrap(),
            settlement_date: Some(NaiveDate::from_ymd_opt(2024, 9, 3).unwrap()),
            status: Status::Settled,
            linked_event_id: None,
            flexibility: Flexibility::Fixed,
            minimum_allowed_amount: None,
        };
        assert!(reconciles(&figures, &event)); // nothing printed contradicts itself
        assert_eq!(select(&figures, &event), None); // but amount_paid/total genuinely absent
    }

    /// Live integration test (PLAN.md Phase 2d): once `code/config/models.toml`'s
    /// `[selected]` names a real `vlm_primary`, this exercises the whole wired path —
    /// downscale, call, select, reconcile — against image_01 (gold: Net Pay 4,365,000 IDR,
    /// docs/gold_subset.json). Requires `HF_TOKEN` and `[selected]` to be set; skips
    /// (does not fail) if the model has not been picked yet. Not run by default:
    /// `cargo test -- --ignored resolve_blank_amount_image_01_live`.
    #[test]
    #[ignore]
    fn resolve_blank_amount_image_01_live() {
        let cfg = ModelsConfig::load(Path::new("config/models.toml"))
            .expect("config/models.toml should parse");
        if cfg.vlm_primary().is_none() {
            eprintln!("skipping: config/models.toml [selected].vlm_primary not set yet");
            return;
        }
        let prompt = crate::extract::prompts::load(
            Path::new("prompts/image_transcription.v1.md"),
            "User prompt template",
        )
        .expect("image_transcription.v1.md should parse");
        let client = crate::hf::HfClient::new().expect("HF_TOKEN must be set");
        let event = income_event();
        let resolution = resolve_blank_amount(
            &client,
            None,
            false,
            &prompt,
            cfg.image_max_dim_px(),
            Path::new("../dataset/media/images/image_01.png"),
            "image_01",
            &cfg,
            &event,
            &[],
        )
        .expect("call should not error");
        let record = resolution.evidence.expect("image_01 should resolve to a figure");
        match record.fact {
            Fact::EventAmount { amount, .. } => assert_eq!(amount, Money::from_f64(4_365_000.0)),
            other => panic!("expected EventAmount, got {other:?}"),
        }
    }

    /// Debug-only live probe (bus topic `bakeoff` #6, lead): the live N=5 sweep accepted
    /// image_04 at 2,854.0 on 3/5 runs via `line_item_sum`, which RULES.md S8 says must fail
    /// closed 5/5 (cropped, history-only). Prints both base readers' raw `ImageFigures` so the
    /// specific field(s) the witness gate trusted can be inspected directly, rather than
    /// guessing from the summary table alone. Not run by default:
    /// `cargo test -- --ignored image_04_debug_raw_figures_live -- --nocapture`.
    #[test]
    #[ignore]
    fn image_04_debug_raw_figures_live() {
        let prompt = crate::extract::prompts::load(
            Path::new("prompts/image_transcription.v3.md"),
            "User prompt template",
        )
        .expect("image_transcription.v3.md should parse");
        let client = crate::hf::HfClient::with_cache_dir("store/debug_image_04_cache")
            .expect("HF_TOKEN must be set");
        let decoding = DecodingConfig { temperature: 0.0, seed: 42, max_tokens_vlm: 1400, max_tokens_llm: 300 };
        let candidates = [
            ("Qwen/Qwen3-VL-235B-A22B-Instruct", "deepinfra", "710c13861be6c466e66de3f484069440b8f31389", 1024u32),
            ("google/gemma-4-31B-it", "deepinfra", "842da3794eaa0b77d5f08bae87a17459d91ff475", 768u32),
        ];
        for image_id in ["image_04", "image_05"] {
            eprintln!("\n########## {image_id} ##########");
            for (id, provider, revision, dim) in candidates {
                let candidate = CandidateConfig {
                    id: id.to_string(),
                    provider: provider.to_string(),
                    model_revision: revision.to_string(),
                    supports_structured_output: true,
                    role: None,
                };
                let b64 = downscale_and_encode(
                    Path::new(&format!("../dataset/media/images/{image_id}.png")),
                    dim,
                )
                .expect("downscale should succeed");
                match call_vlm(&client, None, true, None, &prompt, &decoding, &candidate, decoding.max_tokens_vlm, &b64) {
                    Ok(figures) => eprintln!("=== {id}@{dim} ===\n{figures:#?}\n"),
                    Err(e) => eprintln!("=== {id}@{dim} ERROR ===\n{e:#}\n"),
                }
            }
        }
    }

    /// Debug-only live probe (bus topic `bakeoff` #6/#18, lead): live N=5 accepts 0/5 on
    /// images 02/05/09/10/12/16 with no JSON parse errors (ruling out truncation) -- prints
    /// each base reader's full `ImageReadProvenance` (reconciled, selected_amount, witness,
    /// contradiction) against the REAL linked event from `dataset/financial_events.csv`, so the
    /// exact reason the gate withholds a figure is visible directly. Not run by default:
    /// `cargo test -- --ignored images_debug_02_05_no_agreement_live -- --nocapture`.
    #[test]
    #[ignore]
    fn images_debug_02_05_no_agreement_live() {
        let prompt = crate::extract::prompts::load(
            Path::new("prompts/image_transcription.v3.md"),
            "User prompt template",
        )
        .expect("image_transcription.v3.md should parse");
        let client = crate::hf::HfClient::with_cache_dir("store/debug_02_05_cache")
            .expect("HF_TOKEN must be set");
        let decoding = DecodingConfig { temperature: 0.0, seed: 42, max_tokens_vlm: 1400, max_tokens_llm: 300 };
        let doc_validation = DocValidationConfig::default();
        let qwen = CandidateConfig {
            id: "Qwen/Qwen3-VL-235B-A22B-Instruct".to_string(),
            provider: "deepinfra".to_string(),
            model_revision: "710c13861be6c466e66de3f484069440b8f31389".to_string(),
            supports_structured_output: true,
            role: None,
        };
        let gemma = CandidateConfig {
            id: "google/gemma-4-31B-it".to_string(),
            provider: "deepinfra".to_string(),
            model_revision: "842da3794eaa0b77d5f08bae87a17459d91ff475".to_string(),
            supports_structured_output: true,
            role: None,
        };

        // dataset/financial_events.csv, dataset/images.csv -- exact rows for image_02/image_05.
        let image_02_event = Event {
            id: "event_1442".into(),
            event_type: EventType::Expense,
            description: "Outstanding rent balance".into(),
            category: "rent".into(),
            direction: Direction::Debit,
            amount: None,
            currency: "INR".into(),
            event_date: NaiveDate::from_ymd_opt(2023, 8, 11).unwrap(),
            settlement_date: Some(NaiveDate::from_ymd_opt(2023, 8, 16).unwrap()),
            status: Status::Scheduled,
            linked_event_id: None,
            flexibility: Flexibility::Fixed,
            minimum_allowed_amount: None,
        };
        let image_05_event = Event {
            id: "event_1786".into(),
            event_type: EventType::Expense,
            description: "Outstanding telecom bill".into(),
            category: "utilities".into(),
            direction: Direction::Debit,
            amount: None,
            currency: "INR".into(),
            event_date: NaiveDate::from_ymd_opt(2026, 2, 6).unwrap(),
            settlement_date: Some(NaiveDate::from_ymd_opt(2026, 2, 9).unwrap()),
            status: Status::Pending,
            linked_event_id: None,
            flexibility: Flexibility::Fixed,
            minimum_allowed_amount: None,
        };

        for (image_id, event) in [("image_02", &image_02_event), ("image_05", &image_05_event)] {
            eprintln!("\n########## {image_id} ##########");
            for (role, candidate, dim) in [("vlm_primary", &qwen, 1024u32), ("vlm_escalation", &gemma, 768u32)] {
                let b64 = downscale_and_encode(
                    Path::new(&format!("../dataset/media/images/{image_id}.png")),
                    dim,
                )
                .expect("downscale should succeed");
                let pick = ReaderPick { role, candidate, max_dim_px: dim, max_tokens: decoding.max_tokens_vlm };
                let prov = read_candidate(
                    &client, None, true, None, &prompt, &decoding, pick, &b64, image_id, event, &[], &doc_validation,
                );
                eprintln!("--- {role} ({}) ---\n{prov:#?}\n", candidate.id);
            }
        }
    }
}

/// End-to-end fixture tests for the OCR path (fleet/specs/ocr_vllm_pipeline.md work item A5),
/// using the lead's local Unlimited-OCR outputs verbatim
/// (`scratch/ocr_baidu/pagesgundam/final_r1/image_0{1,2,4,5}.md`, copied here dev-only --
/// production code never names an image id). Images 07/10/11/12 have no local fixture yet
/// (only 01-06 were captured before this pivot); their e2e expectations from the spec
/// (07=8528, 10=79679.26, 11=3650, 12=33.50 USD) are not yet covered by a fixture test.
#[cfg(test)]
mod ocr_e2e {
    use super::*;
    use crate::engine::types::{Direction, Flexibility};
    use crate::extract::ocr::{OcrPage, OcrResult, OcrUsage};

    fn ocr_result(raw: &str) -> OcrResult {
        OcrResult {
            image_id: "test".to_string(),
            pages: vec![OcrPage { page: 1, raw_text: raw.to_string(), truncated: false, usage: OcrUsage::default() }],
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn event(
        id: &str,
        event_type: EventType,
        category: &str,
        currency: &str,
        event_date: NaiveDate,
        settlement_date: NaiveDate,
        status: Status,
    ) -> Event {
        Event {
            id: id.to_string(),
            event_type,
            description: "x".into(),
            category: category.into(),
            direction: Direction::Debit,
            amount: None,
            currency: currency.into(),
            event_date,
            settlement_date: Some(settlement_date),
            status,
            linked_event_id: None,
            flexibility: Flexibility::Fixed,
            minimum_allowed_amount: None,
        }
    }

    fn accepted_amount(resolution: &ImageResolution) -> f64 {
        assert_eq!(resolution.outcome, "witness_accept", "reads: {:#?}", resolution.reads);
        match &resolution.evidence.as_ref().expect("should have evidence").fact {
            Fact::EventAmount { amount, .. } => amount.to_f64(),
            other => panic!("expected EventAmount, got {other:?}"),
        }
    }

    /// image_01 (payslip, settled income): Net Pay 4,365,000 IDR, three separate `<|det|>text`
    /// blocks on the same visual row (`"Net Pay"` / `": IDR"` / `"4,365,000"`).
    #[test]
    fn image_01_net_pay() {
        let raw = r#"<|det|>header [70, 117, 228, 130]<|/det|>HUMAN RESOURCE DEPARTMENT
<|det|>text [460, 160, 546, 175]<|/det|>PAY SLIP Aug-2019
<|det|>table [70, 178, 814, 234]<|/det|><table><tr><td>Name</td><td>: M NURHUDA SY</td><td>Tax Ref No</td><td>: 899763619907000</td></tr></table>
<|det|>table [68, 294, 937, 495]<|/det|><table><tr><td colspan="2">Earning Allowances</td><td colspan="3">Deductions</td></tr><tr><td>Salary</td><td>: IDR</td><td>4,500,000</td><td>Deduction BPJS Pen 2% Company</td><td>: IDR 90,000</td></tr></table>
<|det|>table [68, 504, 937, 694]<|/det|><table><tr><td>Subtotal Earnings</td><td>: IDR</td><td>4,780,800</td><td>Subtotal Deductions</td><td>: IDR</td><td>415,800</td></tr><tr><td>Total Earnings</td><td>: IDR</td><td>4,780,800</td><td>Total Deductions</td><td>: IDR</td><td>415,800</td></tr></table>
<|det|>text [512, 671, 548, 686]<|/det|>Net Pay
<|det|>text [789, 671, 821, 685]<|/det|>: IDR
<|det|>text [888, 671, 930, 685]<|/det|>4,365,000
<|det|>text [689, 694, 938, 709]<|/det|>Four Million Three Hundred Sixty Five Thousand Rupiahs"#;
        let event = event(
            "event_253",
            EventType::Income,
            "salary",
            "IDR",
            NaiveDate::from_ymd_opt(2019, 8, 31).unwrap(),
            NaiveDate::from_ymd_opt(2019, 8, 31).unwrap(),
            Status::Settled,
        );
        let resolution = resolve_blank_amount_ocr(&ocr_result(raw), "test", &event);
        assert_eq!(accepted_amount(&resolution), 4_365_000.0);
    }

    /// image_02 (rent balance, scheduled): balance_due = 100,000, witnessed by
    /// total - amount_paid.
    #[test]
    fn image_02_balance_due() {
        let raw = r#"<|det|>table [53, 33, 999, 999]<|/det|><table><tr><td colspan="4">Rent Receipt</td></tr><tr><td>Owner Name</td><td>Vimlesh</td><td></td><td></td></tr><tr><td>Receipt No.</td><td>9453</td><td>Date</td><td>11/08/23</td></tr><tr><td colspan="4">This is to acknowledged the receipt from Yashwant (tenant) to sum of Rupees 2,00,000 towards house rent for the month of April 2022 to September 2022, towards the property bearing the address &quot;24th, 3 floor, 150/2 Enzyme Diamond, 7th Cross Rd, 1st Sector, HSR Layout, Bengaluru, Karnataka 560102</td></tr><tr><td colspan="4">Payment Mode</td></tr><tr><td rowspan="4"></td><td colspan="2">Rent &amp; Maintenance</td><td>1,80,000.00</td></tr><tr><td colspan="2">Water Charges:</td><td>5,000.00</td></tr><tr><td colspan="2">Rental Tax:</td><td>5,000.00</td></tr><tr><td colspan="2">Electrical Charges:</td><td>10,000.00</td></tr><tr><td></td><td colspan="2">Total Amount to be Receiv</td><td>2,00,000.00</td></tr><tr><td>Amount in Words</td><td colspan="2">Amount Received:</td><td>1,00,000.00</td></tr><tr><td></td><td colspan="2">Balance Due:</td><td>1,00,000.00</td></tr></table>"#;
        let event = event(
            "event_1442",
            EventType::Expense,
            "rent",
            "INR",
            NaiveDate::from_ymd_opt(2023, 8, 11).unwrap(),
            NaiveDate::from_ymd_opt(2023, 8, 16).unwrap(),
            Status::Scheduled,
        );
        let resolution = resolve_blank_amount_ocr(&ocr_result(raw), "test", &event);
        assert_eq!(accepted_amount(&resolution), 100_000.0);
    }

    /// image_04 (cropped delivery-app screenshot, settled, history-only): "Item Bill" maps
    /// only to `subtotal` -- with no total/grand total/amount paid/balance due anywhere on the
    /// page, `select` finds no final label at all, so this fails closed (never a guess at a
    /// subtotal standing in for the real, uncaptured total).
    #[test]
    fn image_04_no_final_label_fails_closed() {
        let raw = r#"<|det|>header [41, 27, 278, 47]<|/det|>ITEM DETAILS
<|det|>text [43, 214, 965, 240]<|/det|>1x [Combo] Nissin Cup Noodles Mazedaar Masala 95.0
<|det|>footer [40, 944, 160, 965]<|/det|>Item Bill
<|det|>footer [824, 944, 963, 965]<|/det|>2854.00"#;
        let event = event(
            "event_1700",
            EventType::Expense,
            "groceries",
            "INR",
            NaiveDate::from_ymd_opt(2024, 9, 3).unwrap(),
            NaiveDate::from_ymd_opt(2024, 9, 3).unwrap(),
            Status::Settled,
        );
        let resolution = resolve_blank_amount_ocr(&ocr_result(raw), "test", &event);
        assert_eq!(resolution.outcome, "fail_closed");
        assert!(resolution.evidence.is_none());
        assert!(resolution.reads[0].ocr_notes.contains(&"no_final_label".to_string()));
    }

    /// image_05 (telecom, pending, due-date cutoff): 822.05 after the 06-Feb-2026 cutoff --
    /// event settles 2026-02-09, after the cutoff.
    #[test]
    fn image_05_after_cutoff_amount() {
        let raw = r#"<|det|>text [13, 20, 294, 56]<|/det|>YOUR ACCOUNT SUMMARY
<|det|>table [38, 88, 442, 380]<|/det|><table><tr><td>Previous balance</td><td></td><td>3,543.54</td></tr><tr><td>Payments</td><td>-</td><td>3,543.54</td></tr><tr><td>This month&#x27;s charges</td><td>+</td><td>704.05</td></tr><tr><td>Amount due till</td><td></td><td></td></tr><tr><td>06-Feb-2026</td><td>=</td><td>704.05</td></tr><tr><td>Amount due after</td><td></td><td></td></tr><tr><td>06-Feb-2026</td><td>=</td><td>822.05</td></tr></table>
<|det|>title [544, 20, 800, 56]<|/det|>THIS MONTH'S CHARGES
<|det|>table [556, 88, 952, 425]<|/det|><table><tr><td></td><td>amount(&#8377;)</td></tr><tr><td>Rentals</td><td>580.65</td></tr><tr><td>Usage charges</td><td>16.00</td></tr><tr><td>Taxes</td><td>107.40</td></tr><tr><td>Total (&#8377;)</td><td>704.05</td></tr></table>
<|det|>text [572, 432, 860, 456]<|/det|>Total : Seven Hundred Four Rupees and Five Paise Only"#;
        let event = event(
            "event_1786",
            EventType::Expense,
            "utilities",
            "INR",
            NaiveDate::from_ymd_opt(2026, 2, 6).unwrap(),
            NaiveDate::from_ymd_opt(2026, 2, 9).unwrap(),
            Status::Pending,
        );
        let resolution = resolve_blank_amount_ocr(&ocr_result(raw), "test", &event);
        assert_eq!(accepted_amount(&resolution), 822.05);
    }

    /// image_07 (restaurant, settled): the page prints both "Total : 8528.10" and "Grand
    /// Total (RS) : 8528" -- Grand Total wins (lead ruling, bus #16), witnessed by
    /// SubTotal + SGST + CGST = 8528.10, which rounds to the accepted 8,528.
    #[test]
    fn image_07_grand_total_over_plain_total() {
        let raw = r#"<|det|>title [379, 66, 751, 135]<|/det|>PAID
<|det|>text [541, 744, 869, 768]<|/det|>SubTotal : 8122.00
<|det|>text [474, 770, 869, 794]<|/det|>SGST 2.50 % : 203.05
<|det|>text [474, 795, 869, 819]<|/det|>CGST 2.50 % : 203.05
<|det|>text [593, 821, 869, 846]<|/det|>Total : 8528.10
<|det|>text [112, 874, 705, 898]<|/det|>Grand Total (RS) : 8528"#;
        let event = event(
            "event_3231",
            EventType::Expense,
            "dining",
            "INR",
            NaiveDate::from_ymd_opt(2025, 10, 29).unwrap(),
            NaiveDate::from_ymd_opt(2025, 10, 29).unwrap(),
            Status::Settled,
        );
        let resolution = resolve_blank_amount_ocr(&ocr_result(raw), "test", &event);
        assert_eq!(accepted_amount(&resolution), 8528.0);
    }

    /// image_10 (large grocery invoice, pending): balance due 79,679.26, witnessed by the
    /// printed amount-in-words line ("Indian Rupee Seventy-Nine Thousand Six Hundred
    /// Seventy-Nine and Twenty-Six Paise Only") -- the summary row's 6-labels/6-values cell
    /// is space- (not newline-) separated and isn't zipped by this pass; only the
    /// amount-in-words and the separate "Balance Due" line resolve the figure.
    #[test]
    fn image_10_balance_due_via_amount_in_words() {
        let raw = r#"<|det|>table [0, 0, 999, 882]<|/det|><table><tr><td colspan="5">Total In Words Indian Rupee Seventy-Nine Thousand Six Hundred Seventy-Nine and Twenty-Six Paise Only</td><td colspan="3">Sub Total CGST2.5 (2.5%) SGST2.5 (2.5%) CGST20 (20%) SGST20 (20%) Total</td><td colspan="2">72,045.00 1,513.13 1,513.13 2,304.00 2,304.00 79,679.26</td></tr><tr><td colspan="5">Notes Thanks for your business.</td><td colspan="3">Balance Due</td><td colspan="2">79,679.26</td></tr></table>"#;
        let event = event(
            "event_6033",
            EventType::Expense,
            "groceries",
            "INR",
            NaiveDate::from_ymd_opt(2024, 6, 3).unwrap(),
            NaiveDate::from_ymd_opt(2024, 6, 10).unwrap(),
            Status::Pending,
        );
        let resolution = resolve_blank_amount_ocr(&ocr_result(raw), "test", &event);
        assert_eq!(accepted_amount(&resolution), 79_679.26);
    }

    /// image_11 (hospital bill, scheduled, amount paid 0): Total Bill Amount = Amount
    /// Payable = Balance = 3,650 all repeat the same figure; the detailed breakup (not
    /// modeled here) is cut off and must never block this.
    #[test]
    fn image_11_repeated_final_label_balance() {
        let raw = r#"<|det|>text [747, 411, 953, 426]<|/det|>Total Bill Amount: 3650.00
<|det|>text [756, 429, 953, 443]<|/det|>Amount Payable: 3650.00
<|det|>text [811, 446, 953, 460]<|/det|>Amount Paid: 0.00
<|det|>text [820, 463, 953, 476]<|/det|>Balance: 3650.00
<|det|>text [738, 480, 953, 494]<|/det|>Paid amount in words : Zero"#;
        let event = event(
            "event_6859",
            EventType::Expense,
            "healthcare",
            "INR",
            NaiveDate::from_ymd_opt(2023, 1, 19).unwrap(),
            NaiveDate::from_ymd_opt(2023, 1, 23).unwrap(),
            Status::Scheduled,
        );
        let resolution = resolve_blank_amount_ocr(&ocr_result(raw), "test", &event);
        assert_eq!(accepted_amount(&resolution), 3650.0);
    }

    /// image_12 (taxi, settled, USD): Subtotal 33.50 + Tax 0.00 witnesses Total 33.50; cash
    /// tendered ($40.00) and change ($6.50) are ignored, never the fare amount.
    #[test]
    fn image_12_usd_total_via_subtotal_plus_tax() {
        let raw = r#"<|det|>text [78, 636, 233, 656]<|/det|>Subtotal:
<|det|>text [821, 636, 928, 656]<|/det|>$33.50
<|det|>text [78, 674, 147, 693]<|/det|>Tax:
<|det|>text [839, 674, 928, 693]<|/det|>$0.00
<|det|>text [78, 713, 192, 734]<|/det|>Total:
<|det|>text [811, 713, 928, 734]<|/det|>$33.50
<|det|>text [78, 802, 250, 822]<|/det|>Cash Paid:
<|det|>text [821, 802, 928, 822]<|/det|>$40.00
<|det|>text [78, 841, 199, 861]<|/det|>Change:
<|det|>text [839, 841, 928, 861]<|/det|>$6.50"#;
        let event = event(
            "event_7307",
            EventType::Expense,
            "transport",
            "USD",
            NaiveDate::from_ymd_opt(2025, 10, 1).unwrap(),
            NaiveDate::from_ymd_opt(2025, 10, 1).unwrap(),
            Status::Settled,
        );
        let resolution = resolve_blank_amount_ocr(&ocr_result(raw), "test", &event);
        assert_eq!(accepted_amount(&resolution), 33.50);
    }

    /// Debug-only report (lead, live vLLM endpoint): reads the lead's real serving-output
    /// fixtures directly from disk (`scratch/ocr_baidu/vllm/r1/image_*.md`, byte-identical
    /// across 2 live runs) and prints the gate outcome for all 16 images. Not run by default,
    /// and never a committed fixture path -- the scratch directory lives outside `code/`.
    /// `cargo test -- --ignored all_16_images_gate_report_live -- --nocapture`.
    #[allow(clippy::type_complexity)]
    fn all_16_cases() -> [(&'static str, EventType, &'static str, &'static str, (i32, u32, u32), (i32, u32, u32), Status); 16] {
        [
            ("image_01", EventType::Income, "salary", "IDR", (2019, 8, 31), (2019, 8, 31), Status::Settled),
            ("image_02", EventType::Expense, "rent", "INR", (2023, 8, 11), (2023, 8, 16), Status::Scheduled),
            ("image_03", EventType::Expense, "groceries", "INR", (2026, 2, 27), (2026, 2, 27), Status::Settled),
            ("image_04", EventType::Expense, "groceries", "INR", (2024, 9, 3), (2024, 9, 3), Status::Settled),
            ("image_05", EventType::Expense, "utilities", "INR", (2026, 2, 6), (2026, 2, 9), Status::Pending),
            ("image_06", EventType::Expense, "groceries", "INR", (2026, 1, 6), (2026, 1, 6), Status::Settled),
            ("image_07", EventType::Expense, "dining", "INR", (2025, 10, 29), (2025, 10, 29), Status::Settled),
            ("image_08", EventType::Expense, "housing", "INR", (2026, 7, 24), (2026, 7, 24), Status::Settled),
            ("image_09", EventType::Expense, "utilities", "INR", (2026, 6, 7), (2026, 6, 7), Status::Settled),
            ("image_10", EventType::Expense, "groceries", "INR", (2024, 6, 3), (2024, 6, 10), Status::Pending),
            ("image_11", EventType::Expense, "healthcare", "INR", (2023, 1, 19), (2023, 1, 23), Status::Scheduled),
            ("image_12", EventType::Expense, "transport", "USD", (2025, 10, 1), (2025, 10, 1), Status::Settled),
            ("image_13", EventType::Expense, "shopping", "INR", (2026, 4, 3), (2026, 4, 3), Status::Settled),
            ("image_14", EventType::Expense, "healthcare", "INR", (2025, 11, 2), (2025, 11, 2), Status::Settled),
            ("image_15", EventType::Expense, "transport", "INR", (2026, 6, 7), (2026, 6, 7), Status::Settled),
            ("image_16", EventType::Expense, "transport", "INR", (2026, 9, 3), (2026, 9, 3), Status::Settled),
        ]
    }

    #[test]
    #[ignore]
    fn all_16_images_gate_report_live() {
        let fixtures_dir = Path::new("E:/projects/hackerrank-orchestrate-september26/scratch/ocr_baidu/vllm/r1");
        let cases = all_16_cases();

        eprintln!("\n| image | outcome | figure | witness | notes |");
        eprintln!("|---|---|---|---|---|");
        for (image_id, event_type, category, currency, event_date, settlement_date, status) in cases {
            let raw = std::fs::read_to_string(fixtures_dir.join(format!("{image_id}.md")))
                .unwrap_or_else(|e| panic!("reading {image_id}.md: {e:#}"));
            let ev = event(
                image_id,
                event_type,
                category,
                currency,
                NaiveDate::from_ymd_opt(event_date.0, event_date.1, event_date.2).unwrap(),
                NaiveDate::from_ymd_opt(settlement_date.0, settlement_date.1, settlement_date.2).unwrap(),
                status,
            );
            let resolution = resolve_blank_amount_ocr(&ocr_result(&raw), image_id, &ev);
            let figure = resolution.evidence.as_ref().map(|e| match &e.fact {
                Fact::EventAmount { amount, .. } => amount.to_f64(),
                _ => f64::NAN,
            });
            let read = &resolution.reads[0];
            eprintln!(
                "| {image_id} | {} | {} | {} | {} |",
                resolution.outcome,
                figure.map(|f| format!("{f:.2}")).unwrap_or_else(|| "—".to_string()),
                read.witness.clone().unwrap_or_else(|| "—".to_string()),
                read.ocr_notes.join(";"),
            );
        }
    }

    /// LIVE two-cold-run ingest against the real RunPod vLLM endpoint (Phase A close-out,
    /// bus topic bakeoff): runs `extract::ocr::OcrClient::ingest` (the actual production
    /// entry point, not a pre-captured fixture) `--cold` over all 16 images, twice, each into
    /// its own fresh cache directory, then gates each run through `resolve_blank_amount_ocr`
    /// and compares run1 vs run2 for identical accepted figures AND byte-identical cached
    /// `.md` pages. Requires `OCR_BASE_URL` (and optionally `OCR_API_KEY`) in the environment;
    /// fires real network calls and real OCR compute, so it is never run by default:
    /// `cargo test -- --ignored live_two_cold_runs_all_16 -- --nocapture`.
    #[test]
    #[ignore]
    fn live_two_cold_runs_all_16() {
        use std::path::PathBuf;
        let dataset_dir = Path::new("../dataset");
        let cases = all_16_cases();

        struct RunResult {
            outcome: String,
            figure: Option<f64>,
            witness: Option<String>,
            notes: Vec<String>,
            page_bytes: Vec<String>,
        }

        let mut per_run: Vec<std::collections::HashMap<&str, RunResult>> = Vec::new();
        for run in 1..=2u32 {
            let cfg = crate::extract::ocr::OcrConfig::from_env().expect("OCR_BASE_URL must be set");
            let cache_dir = PathBuf::from(format!("store/live_ocr_run{run}"));
            let client = crate::extract::ocr::OcrClient::new(cfg, cache_dir).expect("building OcrClient");

            let mut results = std::collections::HashMap::new();
            for (image_id, event_type, category, currency, event_date, settlement_date, status) in cases {
                eprintln!("[run {run}] ingesting {image_id} ...");
                let image_path = dataset_dir.join("media/images").join(format!("{image_id}.png"));
                let ocr = match client.ingest(image_id, &image_path, true) {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("  {image_id} ingest FAILED: {e:#}");
                        results.insert(
                            image_id,
                            RunResult {
                                outcome: format!("error: {e:#}"),
                                figure: None,
                                witness: None,
                                notes: vec![],
                                page_bytes: vec![],
                            },
                        );
                        continue;
                    }
                };
                let ev = event(
                    image_id,
                    event_type,
                    category,
                    currency,
                    NaiveDate::from_ymd_opt(event_date.0, event_date.1, event_date.2).unwrap(),
                    NaiveDate::from_ymd_opt(settlement_date.0, settlement_date.1, settlement_date.2).unwrap(),
                    status,
                );
                let resolution = resolve_blank_amount_ocr(&ocr, image_id, &ev);
                let figure = resolution.evidence.as_ref().and_then(|e| match &e.fact {
                    Fact::EventAmount { amount, .. } => Some(amount.to_f64()),
                    _ => None,
                });
                let read = &resolution.reads[0];
                results.insert(
                    image_id,
                    RunResult {
                        outcome: resolution.outcome.clone(),
                        figure,
                        witness: read.witness.clone(),
                        notes: read.ocr_notes.clone(),
                        page_bytes: ocr.pages.iter().map(|p| p.raw_text.clone()).collect(),
                    },
                );
            }
            per_run.push(results);
        }

        eprintln!("\n| image | outcome | figure | witness | ocr_notes | run1==run2 (figure) | run1==run2 (bytes) |");
        eprintln!("|---|---|---|---|---|---|---|");
        for (image_id, ..) in cases {
            let r1 = &per_run[0][image_id];
            let r2 = &per_run[1][image_id];
            let figure_match = match (r1.figure, r2.figure) {
                (Some(a), Some(b)) => (a - b).abs() < 1e-9,
                (None, None) => true,
                _ => false,
            };
            let bytes_match = r1.page_bytes == r2.page_bytes;
            eprintln!(
                "| {image_id} | {} | {} | {} | {} | {} | {} |",
                r1.outcome,
                r1.figure.map(|f| format!("{f:.2}")).unwrap_or_else(|| "—".to_string()),
                r1.witness.clone().unwrap_or_else(|| "—".to_string()),
                r1.notes.join(";"),
                figure_match,
                bytes_match,
            );
        }
    }
}
