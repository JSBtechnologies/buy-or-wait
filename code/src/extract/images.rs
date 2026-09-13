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
use crate::extract::model_config::{CandidateConfig, DecodingConfig, ModelsConfig, ReaderPick, VlmMode};
use crate::extract::parse_json_reply;
use crate::extract::prompts::PromptSet;
use crate::hf::{ContentPart, HfClient, ModelCall};

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
            "payslip" | "pay_slip" | "salary_slip" | "pay_stub" | "paystub" => DocType::Payslip,
            "receipt" | "cash_receipt" | "sales_receipt" | "bill_of_supply" | "cash_bill" => {
                DocType::Receipt
            }
            "invoice" | "tax_invoice" | "gst_invoice" | "sales_invoice" | "gst_tax_invoice" => {
                DocType::Invoice
            }
            "bill" | "provisional_bill" | "hospital_bill" | "utility_bill" | "rent_receipt" => {
                DocType::Bill
            }
            "delivery_summary" | "order_summary" | "delivery_receipt" | "order_details" => {
                DocType::DeliverySummary
            }
            "bank_statement_excerpt" | "bank_statement" | "statement" | "account_summary" => {
                DocType::BankStatementExcerpt
            }
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
    pub amount_due_before_date: Option<f64>,
    pub amount_due_before_date_value: Option<String>,
    pub amount_due_after_date: Option<f64>,
    pub document_date: Option<String>,
    pub period_label: Option<String>,
    pub line_items_sum_check: Option<f64>,
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
    document_date: Option<Value>,
    #[serde(default)]
    period_label: Option<Value>,
    #[serde(default)]
    line_items_sum_check: Option<Value>,
}

/// Accepts a JSON number or a numeric-looking string; anything else (including a date
/// string landing in a numeric field) is `None` rather than a hard parse error.
fn coerce_f64(v: &Option<Value>) -> Option<f64> {
    match v.as_ref()? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().replace(',', "").parse::<f64>().ok(),
        _ => None,
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
            currency: coerce_string(&raw.currency),
            subtotal: coerce_f64(&raw.subtotal),
            tax: coerce_f64(&raw.tax),
            total: coerce_f64(&raw.total),
            amount_due: coerce_f64(&raw.amount_due),
            amount_paid: coerce_f64(&raw.amount_paid),
            balance_due: coerce_f64(&raw.balance_due),
            gross_pay: coerce_f64(&raw.gross_pay),
            deductions: coerce_f64(&raw.deductions),
            net_pay: coerce_f64(&raw.net_pay),
            previous_balance: coerce_f64(&raw.previous_balance),
            amount_due_before_date: coerce_f64(&raw.amount_due_before_date),
            amount_due_before_date_value: before_value,
            amount_due_after_date: coerce_f64(&raw.amount_due_after_date),
            document_date: coerce_string(&raw.document_date),
            period_label: coerce_string(&raw.period_label),
            line_items_sum_check: coerce_f64(&raw.line_items_sum_check),
        }
    }
}


/// Rounding tolerance for reconciliation checks combining two printed terms (e.g.
/// subtotal+tax=total). Analyst audit RULES.md S5 image_07: subtotal 8,122 + tax 406.10 =
/// 8,528.10 exactly, but the same page also prints a plain "8,528" total — both readings
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

fn parse_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

/// Deterministic selector (PLAN.md §2.3 table): which figure matters, given the linked
/// event's type/status. Never the model's job.
pub fn select(figures: &ImageFigures, event: &Event) -> Option<f64> {
    match (event.event_type, event.status) {
        (EventType::Income, Status::Settled | Status::Scheduled) => {
            figures.net_pay.or(figures.total)
        }
        // engine analyst audit RULES.md S5 image_12: prefer the labeled `total` over
        // `amount_paid` for a settled expense. A receipt's "amount paid"/"cash" line can be
        // the cash tendered (e.g. "Cash 40.00, Change 6.50" against a 33.50 total), which is
        // not the expense amount; the printed total is the authoritative figure whenever
        // it's present, with amount_paid only as a fallback when no total is printed.
        (_, Status::Settled) => figures.total.or(figures.amount_paid),
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
    // A cutoff may come from the labeled `_value` field directly, or be recovered from a
    // date string a model dropped into a numeric before/after field (`RawImageFigures`'s
    // conversion already folds that recovery into `amount_due_before_date_value`).
    if let Some(cutoff) = figures.amount_due_before_date_value.as_deref().and_then(parse_date) {
        // analyst audit #203: once the cutoff is known, the required side is exact -- never
        // fall back to the other (known-wrong-for-this-date) side, and never fall through
        // to balance_due/amount_due either. Missing the required side means escalate.
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
    // at all): not safe to treat that lone value as authoritative on its own -- fall through
    // to whatever else the page states. If nothing here resolves either, `select` returns
    // `None` and the caller escalates rather than guessing.
    positive(figures.balance_due).or_else(|| positive(figures.amount_due))
}

/// Every check that has the data to run, must pass, or the figure is rejected
/// (PLAN.md §2.3: subtotal+tax=total, gross-deductions=net, amount_paid+balance_due=total,
/// currency match). `line_items_sum_check` is a fallback signal only, checked solely when
/// none of the labeled-field checks above had enough data to run at all — analyst audit
/// RULES.md S5 image_11: a multi-section hospital bill's itemized breakup sums to 3,150
/// while the labeled Total/Balance (which already reconcile against each other, 0 + 3,650 =
/// 3,650) say 3,650. Re-summing an arbitrary itemized breakup is not as reliable as the
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

    if let (Some(sub), Some(tax), Some(total)) = (figures.subtotal, figures.tax, figures.total) {
        any_ran = true;
        if close(sub + tax, total, ROUNDING_TOLERANCE_2TERM) {
            any_passed = true;
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
    // change is given (40.00 tendered against a 33.50 total) -- that is consistent by
    // construction, not a mismatch, regardless of what balance_due says. Only fall through
    // to the paid+balance_due=total identity when paid is actually less than total (a real
    // partial payment / balance-owed scenario).
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
    if !any_passed {
        if let (Some(sum), Some(target)) =
            (figures.line_items_sum_check, figures.subtotal.or(figures.total))
        {
            any_ran = true;
            if close(sum, target, ROUNDING_TOLERANCE_2TERM) {
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
/// currency in a different spelling). Limited to the dataset's five currencies (PLAN.md:
/// INR, ZAR, IDR, USD, EUR); anything else falls back to a cleaned exact-string comparison
/// rather than guessing a mapping.
fn currency_matches(claimed: &str, expected: &str) -> bool {
    normalize_currency(claimed) == normalize_currency(expected)
}

fn normalize_currency(raw: &str) -> String {
    let cleaned = raw.trim().trim_end_matches('.').to_uppercase();
    match cleaned.as_str() {
        "INR" | "RS" | "RUPEES" | "RUPEE" | "INDIAN RUPEE" | "INDIAN RUPEES" | "\u{20B9}" => {
            "INR".to_string()
        }
        "USD" | "US$" | "$" | "US DOLLAR" | "US DOLLARS" | "DOLLAR" | "DOLLARS" => {
            "USD".to_string()
        }
        "EUR" | "\u{20AC}" | "EURO" | "EUROS" => "EUR".to_string(),
        "IDR" | "RP" | "RUPIAH" | "INDONESIAN RUPIAH" => "IDR".to_string(),
        "ZAR" | "R" | "RAND" | "SOUTH AFRICAN RAND" => "ZAR".to_string(),
        _ => cleaned,
    }
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
fn call_vlm(
    client: &HfClient,
    cold: bool,
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
    };
    let response =
        if cold { client.chat_completion_cold(&call)? } else { client.chat_completion(&call)? };
    // Kimi-K3 (a thinking model, ml-engineer #204/lead) returns reasoning text ahead of the
    // JSON answer at any max_tokens generous enough to let it finish; `parse_json_reply`
    // already locates the JSON body rather than requiring the whole reply to be JSON.
    let value = parse_json_reply(&response.raw_text)?;
    Ok(serde_json::from_value(value)?)
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
    pub selected_amount: Option<f64>,
    pub currency: Option<String>,
    /// The due-date cutoff this read itself resolved, if any (labeled or recovered).
    pub due_date: Option<String>,
    pub before_amount: Option<f64>,
    pub after_amount: Option<f64>,
    pub error: Option<String>,
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
pub fn resolve_blank_amount(
    client: &HfClient,
    cold: bool,
    prompt: &PromptSet,
    image_max_dim_px: u32,
    image_path: &Path,
    image_id: &str,
    config: &ModelsConfig,
    event: &Event,
) -> anyhow::Result<ImageResolution> {
    match config.vlm_mode() {
        VlmMode::Escalate => resolve_blank_amount_escalate(
            client,
            cold,
            prompt,
            &config.decoding,
            image_max_dim_px,
            image_path,
            image_id,
            config.vlm_primary(),
            config.vlm_escalation(),
            config.vlm_fallback(),
            event,
        ),
        VlmMode::Agreement => {
            resolve_blank_amount_agreement(client, cold, prompt, &config.decoding, image_path, image_id, config, event)
        }
    }
}

fn read_candidate(
    client: &HfClient,
    cold: bool,
    prompt: &PromptSet,
    decoding: &DecodingConfig,
    pick: ReaderPick<'_>,
    image_b64: &str,
    image_id: &str,
    event: &Event,
) -> ImageReadProvenance {
    let mut prov = ImageReadProvenance {
        role: pick.role.to_string(),
        model_id: pick.candidate.id.clone(),
        model_revision: pick.candidate.model_revision.clone(),
        max_dim_px: pick.max_dim_px,
        max_tokens: pick.max_tokens,
        reconciled: false,
        selected_amount: None,
        currency: None,
        due_date: None,
        before_amount: None,
        after_amount: None,
        error: None,
    };
    match call_vlm(client, cold, prompt, decoding, pick.candidate, pick.max_tokens, image_b64) {
        Ok(figures) => {
            prov.due_date = figures.amount_due_before_date_value.clone();
            prov.before_amount = figures.amount_due_before_date;
            prov.after_amount = figures.amount_due_after_date;
            prov.reconciled = reconciles(&figures, event);
            if prov.reconciled {
                prov.selected_amount = select(&figures, event);
                if prov.selected_amount.is_some() {
                    prov.currency =
                        Some(figures.currency.clone().unwrap_or_else(|| event.currency.clone()));
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
    cold: bool,
    prompt: &PromptSet,
    decoding: &DecodingConfig,
    image_max_dim_px: u32,
    image_path: &Path,
    image_id: &str,
    vlm_primary: Option<&CandidateConfig>,
    vlm_escalation: Option<&CandidateConfig>,
    vlm_fallback: Option<&CandidateConfig>,
    event: &Event,
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
        let prov = read_candidate(client, cold, prompt, decoding, pick, &image_b64, image_id, event);
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

/// Two-model agreement (user decision `decision.vlm_setup`, board:verify.image_agree_preaudit
/// + board:verify.image_agree_audit): `config.readers_for(event)` routes this event's class
/// (income/payslip, pending/scheduled bill, or settled expense/receipt — from
/// `event_type`/`status`/`category`, never the model's own `doc_type`) to a pair of reader
/// roles, each with its own resolution/token budget. Both read the image independently; if
/// their selected amounts agree (within the documented rounding tolerance), that figure is
/// trusted. On disagreement, or when one reader is missing/unreconciled, the class's own
/// `tiebreak` reader (user decision `decision.tiebreak_distinct`) is called — a class's
/// `tiebreak` role is enforced distinct from both its primary `readers` at
/// `ModelsConfig::load()` (hard error), so there is no runtime self-tiebreak case to guard
/// here: calling it is always a genuinely independent third read (analyst audit #214/#219 —
/// the image_05 false accept was exactly a self-tiebreak, Kimi matching its own cached
/// answer 5/5). A tiebreak match must ALSO satisfy any due-date cutoff any of the three reads
/// resolved (analyst audit #203) — matching a stale pre-cutoff figure numerically is not
/// enough. Two readers both failing to reconcile is never covered by a lone tiebreak read
/// (analyst audit #214: "(None,None) also accepts one read" was a bug, not a feature —
/// two-model agreement never trusts exactly one model).
fn resolve_blank_amount_agreement(
    client: &HfClient,
    cold: bool,
    prompt: &PromptSet,
    decoding: &DecodingConfig,
    image_path: &Path,
    image_id: &str,
    config: &ModelsConfig,
    event: &Event,
) -> anyhow::Result<ImageResolution> {
    let class = config.classify_event(event).to_string();
    let outcome = |outcome: &str, reads: Vec<ImageReadProvenance>, evidence: Option<EvidenceRecord>| ImageResolution {
        image_id: image_id.to_string(),
        class: Some(class.clone()),
        mode: "agreement".to_string(),
        reads,
        outcome: outcome.to_string(),
        evidence,
    };

    let Some((pick_a, pick_b)) = config.readers_for(event) else {
        return Ok(outcome("no_route", vec![], None));
    };

    let b64_a = downscale_and_encode(image_path, pick_a.max_dim_px)?;
    let b64_b = if pick_b.max_dim_px == pick_a.max_dim_px {
        b64_a.clone()
    } else {
        downscale_and_encode(image_path, pick_b.max_dim_px)?
    };
    let prov_a = read_candidate(client, cold, prompt, decoding, pick_a, &b64_a, image_id, event);
    let prov_b = read_candidate(client, cold, prompt, decoding, pick_b, &b64_b, image_id, event);
    let mut reads = vec![prov_a.clone(), prov_b.clone()];

    if let (Some(amt_a), Some(amt_b)) = (prov_a.selected_amount, prov_b.selected_amount) {
        if close(amt_a, amt_b, ROUNDING_TOLERANCE_2TERM) {
            let currency = prov_a.currency.clone().unwrap_or_else(|| event.currency.clone());
            let evidence = build_evidence(image_id, event, amt_a, currency, &[&prov_a.role, &prov_b.role]);
            return Ok(outcome("agree", reads, Some(evidence)));
        }
    }

    let Some(tb_pick) = config.tiebreak_for(event) else {
        return Ok(outcome("no_agreement", reads, None));
    };
    let b64_fb = if tb_pick.max_dim_px == pick_a.max_dim_px {
        b64_a.clone()
    } else if tb_pick.max_dim_px == pick_b.max_dim_px {
        b64_b.clone()
    } else {
        downscale_and_encode(image_path, tb_pick.max_dim_px)?
    };
    let prov_fb = read_candidate(client, cold, prompt, decoding, tb_pick, &b64_fb, image_id, event);
    reads.push(prov_fb.clone());

    let Some(amt_fb) = prov_fb.selected_amount else {
        return Ok(outcome("no_agreement", reads, None));
    };

    let cutoff_requirement =
        [&prov_a, &prov_b, &prov_fb].iter().find_map(|p| cutoff_requirement(p, event));
    let satisfies_cutoff =
        |amount: f64| cutoff_requirement.is_none_or(|req| close(amount, req, ROUNDING_TOLERANCE_2TERM));

    let matched_role = if prov_a.selected_amount.is_some_and(|a| close(amt_fb, a, ROUNDING_TOLERANCE_2TERM)) {
        Some(prov_a.role.clone())
    } else if prov_b.selected_amount.is_some_and(|b| close(amt_fb, b, ROUNDING_TOLERANCE_2TERM)) {
        Some(prov_b.role.clone())
    } else {
        None
    };

    match matched_role {
        Some(role) if satisfies_cutoff(amt_fb) => {
            let currency = prov_fb.currency.clone().unwrap_or_else(|| event.currency.clone());
            let evidence = build_evidence(image_id, event, amt_fb, currency, &[&role, &prov_fb.role]);
            Ok(outcome("tiebreak_accept", reads, Some(evidence)))
        }
        _ => Ok(outcome("no_agreement", reads, None)),
    }
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
            subtotal: Some(4_780_800.0),
            gross_pay: Some(4_780_800.0),
            deductions: Some(415_800.0),
            net_pay: Some(4_365_000.0),
            line_items_sum_check: Some(4_780_800.0),
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

    /// Analyst audit RULES.md S5 image_07: subtotal 8,122 + tax 406.10 = 8,528.10 exactly,
    /// but the page also prints a plain "8,528" total. An exact-cent check would reject a
    /// genuinely reconciling document; the documented rounding tolerance accepts it (and
    /// selects the printed total, not a recomputed one).
    #[test]
    fn image_07_reconciles_within_rounding_tolerance() {
        let figures = ImageFigures {
            doc_type: Some(DocType::Invoice),
            currency: Some("INR".into()),
            subtotal: Some(8122.0),
            tax: Some(406.10),
            total: Some(8528.0),
            ..Default::default()
        };
        let event = expense_event("event_3231", "dining", "INR", Status::Settled);
        assert!(reconciles(&figures, &event));
        assert_eq!(select(&figures, &event), Some(8528.0));
    }

    /// Analyst audit RULES.md S5 image_11: a multi-section hospital bill's itemized
    /// breakup sums to 3,150, but the labeled Amount Paid (0) + Balance (3,650) already
    /// reconcile against the labeled Total (3,650). The line-item sum must never override
    /// labeled fields that already reconcile among themselves.
    #[test]
    fn image_11_ignores_line_item_sum_when_labeled_fields_reconcile() {
        let figures = ImageFigures {
            doc_type: Some(DocType::Invoice),
            currency: Some("INR".into()),
            total: Some(3650.0),
            amount_paid: Some(0.0),
            balance_due: Some(3650.0),
            line_items_sum_check: Some(3150.0), // wrong: breakup subtotals miss a line
            ..Default::default()
        };
        let event = expense_event("event_6859", "healthcare", "INR", Status::Scheduled);
        assert!(reconciles(&figures, &event));
        assert_eq!(select(&figures, &event), Some(3650.0));
    }

    /// Analyst audit RULES.md S5 image_12: a settled expense's "amount paid"/cash line can
    /// be the cash TENDERED (40.00, with 6.50 change), not the expense itself (33.50
    /// total). The selector must prefer the printed total over amount_paid whenever a
    /// total is present.
    #[test]
    fn image_12_settled_expense_prefers_total_over_cash_tendered_amount_paid() {
        // Exact shape of the cached VLM reads (analyst audit #184): balance_due present as
        // 0.00 (nothing owed), amount_paid the cash tendered (40.00), which naively fails
        // paid+balance_due=total (40 != 33.50) — paid >= total must be treated as
        // consistent on its own, never requiring balance_due to explain the gap.
        let figures = ImageFigures {
            doc_type: Some(DocType::Receipt),
            currency: Some("USD".into()),
            total: Some(33.50),
            amount_paid: Some(40.00), // cash tendered, not the expense amount
            balance_due: Some(0.00),
            ..Default::default()
        };
        let event = expense_event("event_7307", "transport", "USD", Status::Settled);
        assert!(reconciles(&figures, &event));
        assert_eq!(select(&figures, &event), Some(33.50));
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

    /// analyst audit #194: a document can print an unrelated subtotal/tax breakdown that
    /// doesn't sum to the overall total, while amount_paid + balance_due = total
    /// independently confirms the figure — the first identity's mismatch must not veto the
    /// second's pass.
    #[test]
    fn unrelated_subtotal_tax_mismatch_does_not_veto_a_passing_paid_balance_check() {
        let figures = ImageFigures {
            doc_type: Some(DocType::Invoice),
            currency: Some("INR".into()),
            subtotal: Some(1000.0), // describes a different section; doesn't sum to total
            tax: Some(50.0),
            total: Some(3650.0),
            amount_paid: Some(0.0),
            balance_due: Some(3650.0),
            ..Default::default()
        };
        let event = expense_event("event_6859", "healthcare", "INR", Status::Scheduled);
        assert!(reconciles(&figures, &event));
        assert_eq!(select(&figures, &event), Some(3650.0));
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
            amount_due_before_date: Some(704.05),
            amount_due_before_date_value: Some("2026-02-06".into()),
            amount_due_after_date: Some(822.05),
            ..Default::default()
        };
        let event = pending_utilities_event(NaiveDate::from_ymd_opt(2026, 2, 9).unwrap());
        assert!(reconciles(&figures, &event));
        assert_eq!(select(&figures, &event), Some(822.05));
    }

    /// analyst audit board:verify.image05_shapes "Shape A" (6 Qwen reads): before/after are
    /// both present as plain numbers, but no cutoff date is present anywhere. Conservative
    /// fallback: the larger figure, never under-reserve a pending debt.
    #[test]
    fn image_05_no_cutoff_date_falls_back_to_the_larger_figure() {
        let figures = ImageFigures {
            doc_type: Some(DocType::Bill),
            currency: Some("INR".into()),
            amount_due_before_date: Some(704.05),
            amount_due_before_date_value: None,
            amount_due_after_date: Some(822.05),
            ..Default::default()
        };
        let event = pending_utilities_event(NaiveDate::from_ymd_opt(2026, 2, 9).unwrap());
        assert!(reconciles(&figures, &event));
        assert_eq!(select(&figures, &event), Some(822.05));
    }

    /// analyst audit "Shape B", the 4 gemma reads: only the before-cutoff figure (704.05)
    /// is present anywhere on the page, with no after figure and no balance_due/amount_due
    /// to fall back to. Not safe to treat the lone value as authoritative -- `select`
    /// returns `None` so the caller escalates rather than guessing.
    #[test]
    fn image_05_only_one_candidate_present_does_not_guess() {
        let figures = ImageFigures {
            doc_type: Some(DocType::Bill),
            currency: Some("INR".into()),
            amount_due_before_date: Some(704.05),
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
            amount_due: Some(822.05),
            ..Default::default()
        };
        let event = pending_utilities_event(NaiveDate::from_ymd_opt(2026, 2, 9).unwrap());
        assert_eq!(select(&figures, &event), Some(822.05));
    }

    /// analyst audit #203: once a cutoff IS known and the cash date is past it, the
    /// after-cutoff figure is required exactly -- never fall back to the before-cutoff
    /// figure (which is known-wrong for this date) or to balance_due/amount_due.
    #[test]
    fn image_05_known_cutoff_requires_after_never_falls_back_to_before() {
        let figures = ImageFigures {
            doc_type: Some(DocType::Bill),
            currency: Some("INR".into()),
            amount_due_before_date: Some(704.05),
            amount_due_before_date_value: Some("2026-02-06".into()),
            amount_due_after_date: None, // missing -- must not fall back to before (704.05)
            balance_due: Some(704.05),   // must not fall back here either
            ..Default::default()
        };
        let event = pending_utilities_event(NaiveDate::from_ymd_opt(2026, 2, 9).unwrap());
        assert_eq!(select(&figures, &event), None);
    }

    /// analyst audit "Shape B", the other 2 reads: after-cutoff (822.05) landed in
    /// balance_due instead of amount_due_after_date. The fallback still resolves it.
    #[test]
    fn image_05_after_value_in_balance_due_still_resolves() {
        let figures = ImageFigures {
            doc_type: Some(DocType::Bill),
            currency: Some("INR".into()),
            amount_due_before_date: Some(704.05),
            balance_due: Some(822.05),
            ..Default::default()
        };
        let event = pending_utilities_event(NaiveDate::from_ymd_opt(2026, 2, 9).unwrap());
        assert_eq!(select(&figures, &event), Some(822.05));
    }

    /// analyst audit "Shape B": a date string landed in the numeric before/after fields
    /// and a number landed in the date-string `_value` field -- a straight type mismatch
    /// that must not fail the whole `ImageFigures` parse. `lenient_f64`/`lenient_string`
    /// coerce what they can and give up to `None` on the rest, never a hard error.
    #[test]
    fn image_05_swapped_json_types_deserialize_without_error() {
        let value = serde_json::json!({
            "amount_due_before_date": "704.05",
            "amount_due_before_date_value": 704.05,
            "amount_due_after_date": 822.05,
            "currency": "INR"
        });
        let figures: ImageFigures =
            serde_json::from_value(value).expect("swapped types must not error the whole parse");
        assert_eq!(figures.amount_due_before_date, Some(704.05));
        assert_eq!(figures.amount_due_before_date_value, Some("704.05".to_string()));
        assert_eq!(figures.amount_due_after_date, Some(822.05));

        let event = pending_utilities_event(NaiveDate::from_ymd_opt(2026, 2, 9).unwrap());
        // "704.05" does not parse as a date, so no cutoff resolves -> conservative max().
        assert_eq!(select(&figures, &event), Some(822.05));
    }

    #[test]
    fn image_04_rejects_when_total_is_not_on_the_page() {
        let figures = ImageFigures {
            doc_type: Some(DocType::DeliverySummary),
            currency: Some("INR".into()),
            subtotal: Some(2854.0),
            line_items_sum_check: Some(2854.0),
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
            false,
            &prompt,
            cfg.image_max_dim_px(),
            Path::new("../dataset/media/images/image_01.png"),
            "image_01",
            &cfg,
            &event,
        )
        .expect("call should not error");
        let record = resolution.evidence.expect("image_01 should resolve to a figure");
        match record.fact {
            Fact::EventAmount { amount, .. } => assert_eq!(amount, Money::from_f64(4_365_000.0)),
            other => panic!("expected EventAmount, got {other:?}"),
        }
    }
}
