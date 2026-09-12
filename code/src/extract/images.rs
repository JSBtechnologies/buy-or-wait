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

use crate::engine::ledger::{EvidenceRecord, EvidenceSource, Fact};
use crate::engine::money::Money;
use crate::engine::types::{Event, EventType, Status};
use crate::extract::model_config::{CandidateConfig, DecodingConfig};
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
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ImageFigures {
    pub doc_type: Option<DocType>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub currency: Option<String>,
    #[serde(default, deserialize_with = "lenient_f64")]
    pub subtotal: Option<f64>,
    #[serde(default, deserialize_with = "lenient_f64")]
    pub tax: Option<f64>,
    #[serde(default, deserialize_with = "lenient_f64")]
    pub total: Option<f64>,
    #[serde(default, deserialize_with = "lenient_f64")]
    pub amount_due: Option<f64>,
    #[serde(default, deserialize_with = "lenient_f64")]
    pub amount_paid: Option<f64>,
    #[serde(default, deserialize_with = "lenient_f64")]
    pub balance_due: Option<f64>,
    #[serde(default, deserialize_with = "lenient_f64")]
    pub gross_pay: Option<f64>,
    #[serde(default, deserialize_with = "lenient_f64")]
    pub deductions: Option<f64>,
    #[serde(default, deserialize_with = "lenient_f64")]
    pub net_pay: Option<f64>,
    #[serde(default, deserialize_with = "lenient_f64")]
    pub previous_balance: Option<f64>,
    // analyst audit board:verify.image05_shapes: cached reads for image_05 put a date
    // string where a number was expected on this trio (or vice versa) roughly as often as
    // not -- strict typing on any one of the three failed the WHOLE ImageFigures parse
    // instead of leaving just that field unusable. `lenient_f64`/`lenient_string` accept
    // whichever JSON type actually showed up and coerce it (or give up to `None`), never a
    // hard error.
    #[serde(default, deserialize_with = "lenient_f64")]
    pub amount_due_before_date: Option<f64>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub amount_due_before_date_value: Option<String>,
    #[serde(default, deserialize_with = "lenient_f64")]
    pub amount_due_after_date: Option<f64>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub document_date: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub period_label: Option<String>,
    #[serde(default, deserialize_with = "lenient_f64")]
    pub line_items_sum_check: Option<f64>,
}

/// Accepts a JSON number or a numeric-looking string; anything else (including a date
/// string landing in a numeric field) is `None` rather than a hard parse error.
fn lenient_f64<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|v| match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().replace(',', "").parse::<f64>().ok(),
        _ => None,
    }))
}

/// Accepts a JSON string, or coerces a JSON number to its string form (e.g. a number
/// landing in what should have been a date-string field); anything else is `None`.
fn lenient_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|v| match v {
        serde_json::Value::String(s) => Some(s),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }))
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
        (_, Status::Pending | Status::Scheduled) => {
            if let (Some(before), Some(after)) =
                (figures.amount_due_before_date, figures.amount_due_after_date)
            {
                // analyst audit board:verify.image05_shapes: several cached reads never
                // land a usable cutoff date in amount_due_before_date_value at all (it's
                // absent, or a stray number landed there instead of a date string).
                if let Some(cutoff) =
                    figures.amount_due_before_date_value.as_deref().and_then(parse_date)
                {
                    return Some(if event.cash_date() > cutoff { after } else { before });
                }
                // No reliable cutoff date to choose between them: the conservative choice
                // is the larger figure, never the smaller -- under-reserving a pending debt
                // risks a plan that later breaches the minimum balance; over-reserving only
                // costs safety margin, never correctness.
                return Some(before.max(after));
            }
            // Only one of before/after is present with no date to resolve it (or neither
            // is present at all): not safe to treat that lone value as authoritative on its
            // own -- fall through to whatever else the page states. If nothing here
            // resolves either, `select` returns `None` and the caller escalates rather than
            // guessing (never accepts an unresolved lone due-date figure as-is).
            figures.balance_due.or(figures.amount_due)
        }
        _ => figures.total,
    }
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
        if paid + ROUNDING_TOLERANCE_2TERM >= total {
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

/// Select, reconcile, and convert one image's transcription into a `Fact::EventAmount`.
/// `None` when reconciliation fails or the needed figure is genuinely not on the page —
/// the caller (store/preprocessing) is expected to escalate to a second read before giving
/// up, per `code/prompts/image_transcription.v1.md`.
pub fn to_evidence(image_id: &str, figures: &ImageFigures, event: &Event) -> Option<EvidenceRecord> {
    if !reconciles(figures, event) {
        return None;
    }
    let amount = select(figures, event)?;
    Some(EvidenceRecord {
        record_id: image_id.to_string(),
        source: EvidenceSource::Image,
        observed_at: event.event_date.and_hms_opt(0, 0, 0)?,
        fact: Fact::EventAmount {
            event_id: event.id.clone(),
            amount: Money::from_f64(amount),
            currency: figures.currency.clone().unwrap_or_else(|| event.currency.clone()),
        },
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
        max_tokens: decoding.max_tokens_vlm,
        json_response: candidate.supports_structured_output,
    };
    let response =
        if cold { client.chat_completion_cold(&call)? } else { client.chat_completion(&call)? };
    let value = parse_json_reply(&response.raw_text)?;
    Ok(serde_json::from_value(value)?)
}

/// End-to-end resolution for one blank-amount event with a linked image (PLAN.md §2.3):
/// downscale, transcribe with the primary VLM, select + reconcile; on a reconciliation
/// failure (or a malformed/unparseable reply), escalate once to the second configured
/// model — a fresh read, not a retry of the same call — and try again. Still failing, or no
/// escalation model configured: `None`. Never a guess, never a zero.
///
/// `vlm_primary`/`vlm_escalation` come from `ModelsConfig::vlm_primary()` /
/// `vlm_escalation()` (`code/config/models.toml`'s `[selected]` table, PLAN.md Phase 2d) —
/// when the user has not picked yet, the caller simply does not call this function; the
/// model path is inactive by construction, not by a special case here.
pub fn resolve_blank_amount(
    client: &HfClient,
    cold: bool,
    prompt: &PromptSet,
    decoding: &DecodingConfig,
    image_max_dim_px: u32,
    image_path: &Path,
    image_id: &str,
    vlm_primary: &CandidateConfig,
    vlm_escalation: Option<&CandidateConfig>,
    // User-requested backup frontier model (board decision, PLAN.md Phase 2d): tried after
    // `vlm_escalation` also fails to reconcile, or when an earlier candidate's call itself
    // errored (network/provider outage) rather than just producing a bad reconcile — both
    // cases already collapse to `attempt` returning `Ok(None)` below.
    vlm_fallback: Option<&CandidateConfig>,
    event: &Event,
) -> anyhow::Result<Option<EvidenceRecord>> {
    let image_b64 = downscale_and_encode(image_path, image_max_dim_px)?;

    let attempt = |candidate: &CandidateConfig| -> anyhow::Result<Option<EvidenceRecord>> {
        match call_vlm(client, cold, prompt, decoding, candidate, &image_b64) {
            Ok(figures) => Ok(to_evidence(image_id, &figures, event)),
            Err(e) => {
                eprintln!("vlm: {image_id} via {}: {e:#}", candidate.id);
                Ok(None)
            }
        }
    };

    for candidate in [Some(vlm_primary), vlm_escalation, vlm_fallback].into_iter().flatten() {
        if let Some(record) = attempt(candidate)? {
            return Ok(Some(record));
        }
    }
    Ok(None)
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
        let cfg = crate::extract::model_config::ModelsConfig::load(Path::new("config/models.toml"))
            .expect("config/models.toml should parse");
        let Some(vlm_primary) = cfg.vlm_primary() else {
            eprintln!("skipping: config/models.toml [selected].vlm_primary not set yet");
            return;
        };
        let prompt = crate::extract::prompts::load(
            Path::new("prompts/image_transcription.v1.md"),
            "User prompt template",
        )
        .expect("image_transcription.v1.md should parse");
        let client = crate::hf::HfClient::new().expect("HF_TOKEN must be set");
        let event = income_event();
        let record = resolve_blank_amount(
            &client,
            false,
            &prompt,
            &cfg.decoding,
            cfg.image_max_dim_px(),
            Path::new("../dataset/media/images/image_01.png"),
            "image_01",
            vlm_primary,
            cfg.vlm_escalation(),
            cfg.vlm_fallback(),
            &event,
        )
        .expect("call should not error")
        .expect("image_01 should resolve to a figure");
        match record.fact {
            Fact::EventAmount { amount, .. } => assert_eq!(amount, Money::from_f64(4_365_000.0)),
            other => panic!("expected EventAmount, got {other:?}"),
        }
    }
}
