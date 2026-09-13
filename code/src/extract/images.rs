//! Image figure transcription (Unlimited-OCR + deterministic label mapping,
//! `extract::ocr`/`extract::ocr_parse`/`extract::labels`) + deterministic selector +
//! witness gate (PLAN.md §2.3, fleet/specs/ocr_vllm_pipeline.md).
//!
//! The OCR model only transcribes every labeled figure on the page; it never picks which
//! one matters. This module picks the figure deterministically from the linked event's
//! type/status and only trusts it once the witness gate (`extract::witness`) clears it. A
//! figure the witness gate rejects, or that the page simply does not contain, is never
//! guessed or treated as zero (see `docs/gold_subset.json` image_04 for the canonical case).

use chrono::NaiveDate;
use serde::Deserialize;

use crate::engine::ledger::{EvidenceRecord, EvidenceSource, Fact};
use crate::engine::money::Money;
use crate::engine::types::{Event, EventType, Status};
use crate::extract::normalize;
use crate::extract::witness;

/// `doc_type` is purely advisory metadata (`select` never branches on it) and is
/// deserialized leniently on purpose: a document's own wording for its type is free text
/// in practice ("TAX INVOICE", "PROVISIONAL BILL", ...), and a strict enum match on it was
/// rejecting many otherwise-valid reads before `select()` ever ran (analyst audit
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

/// Every labeled figure recovered off one document (`extract::labels::figures_from_rows`,
/// fed by `extract::ocr_parse`'s OCR-row parse). Every field but `doc_type` is nullable: a
/// figure not printed on the page stays `None`, never an inferred zero.
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

/// Rounding tolerance for reconciliation checks combining two printed terms (e.g.
/// subtotal+tax=total). Analyst audit RULES.md S5 image_07: subtotal 7,150 + tax 358.10 =
/// 7,508.10 exactly, but the same page also prints a plain "7,508" total — both readings
/// are legitimate, and an exact-cent check rejects a genuinely reconciling document. Allow
/// each of the two printed terms in a check to be off by up to half a currency unit
/// (0.5), for a combined tolerance of 1.0, and log whenever the looser bound is what
/// actually let a check pass (never silently — a human should be able to see it happened).
const ROUNDING_TOLERANCE_2TERM: f64 = 1.0;

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
    // numeric before/after field. Same G2 rule: present-but-unparseable escalates, it never
    // falls through to the max()-guess.
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


/// Legacy per-read validation checks from the retired multi-model VLM path (cleanup
/// `cleanup.remove_vlm_anthropic`). Kept only so `ImageReadProvenance`'s serialized shape
/// stays stable for the verifier/integrator -- the OCR path (`resolve_blank_amount_ocr`)
/// always fills this with `Default::default()` (every field `None`, so `passed()` is
/// trivially `true`); nothing here is computed anymore.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize)]
pub struct DocValidationResult {
    pub currency_ok: Option<bool>,
    pub date_window_ok: Option<bool>,
    pub class_consistent: Option<bool>,
    pub cutoff_coherent: Option<bool>,
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
    /// unparseable cutoff date).
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

/// OCR-path resolution (fleet/specs/ocr_vllm_pipeline.md work item A4; the only image-read
/// path since cleanup `cleanup.remove_vlm_anthropic`): Unlimited-OCR already ran at ingestion
/// and is cached (`extract::ocr::OcrResult`); this reads that cache, maps every page's rows
/// to `ImageFigures` (`extract::labels::figures_from_rows`), and runs the deterministic
/// `select` + witness gate (`extract::witness`). There is exactly one deterministic reader,
/// so multi-model agreement never applies -- a figure is accepted the moment `select` (or the
/// ruled witnessed-sum fallback) finds one AND the witness gate (a witness, or a printed
/// total, with no contradiction) clears it; otherwise the read fails closed, never a guess.
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

    let selected = select(&figures, event);
    if selected.is_none() {
        prov.ocr_notes.push("no_final_label".to_string());
    }

    // User ruling `ruling.total_trumps_all` (refines `ruling.lone_printed_total`): the exact
    // field `select()` already resolves for this event's status/scope (the printed total
    // family for a settled read, the printed still-owed amount -- balance due or the dated
    // after-cutoff figure -- for a pending/scheduled one; `select_pending_or_scheduled`'s own
    // logic is that exception, unchanged here) is accepted outright: it beats a computed sum,
    // an amount-in-words reading, a missing witness, and any other final-labeled field that
    // happens to disagree (a breakdown/contradiction never rejects it). The one carve-out is a
    // bare-subtotal duplicate (`target_is_bare_subtotal`, `ruling.total_or_witnessed_sum`
    // image_04: the real total was never printed at all, cut off below the crop) -- that is
    // never a genuine printed total to begin with, so it still needs independent corroboration.
    let printed_total = selected.filter(|&amount| {
        !witness::target_is_bare_subtotal(&figures, amount, ROUNDING_TOLERANCE_2TERM, scope)
    });

    let (amount, witness_hit, printed_total_accept) = match printed_total {
        Some(amount) => {
            // Still surface a real corroborating identity as the witness label when one
            // exists -- purely informational now, never required for acceptance.
            let hit = witness::find_witness(&figures, amount, ROUNDING_TOLERANCE_2TERM, scope);
            (amount, hit, true)
        }
        None => {
            // No printed total at all (or only a bare-subtotal duplicate) -- a value only
            // stands with independent corroboration: `ruling.total_or_witnessed_sum`'s
            // witnessed line-item/charges sum fallback, never self-witnessed.
            let primary_witness = selected.and_then(|amount| {
                witness::find_witness(&figures, amount, ROUNDING_TOLERANCE_2TERM, scope).map(|hit| (amount, hit))
            });
            match primary_witness {
                Some((amount, hit)) => (amount, Some(hit), false),
                None => match witness::witnessed_line_item_sum(&figures, scope, ROUNDING_TOLERANCE_2TERM) {
                    Some((sum_amount, kind)) => (sum_amount, Some((kind, sum_amount)), false),
                    None => {
                        return outcome("fail_closed", vec![prov], None);
                    }
                },
            }
        }
    };

    let contradiction = witness::final_label_contradicts(&figures, amount, ROUNDING_TOLERANCE_2TERM, scope);

    prov.witness = witness_hit
        .map(|(kind, _)| kind.label().to_string())
        .or_else(|| printed_total_accept.then(|| "printed_final_label_only".to_string()));
    prov.witness_computed = witness_hit.map(|(_, computed)| computed);
    prov.contradiction = contradiction.map(|(field, value)| format!("{field}={value}"));

    if !printed_total_accept && (prov.witness.is_none() || prov.contradiction.is_some()) {
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
mod ocr_e2e {
    use super::*;
    use std::path::Path;
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
