//! Image figure transcription + deterministic selector + reconciliation
//! (schema = `code/prompts/image_transcription.v1.md`, PLAN.md §2.3).
//!
//! The VLM only transcribes every labeled figure on the page; it never picks which one
//! matters. This module picks the figure deterministically from the linked event's
//! type/status, checks it arithmetically, and only then turns it into a `Fact::EventAmount`.
//! A figure that fails reconciliation, or that the page simply does not contain, is never
//! guessed or treated as zero (see `docs/gold_subset.json` image_04 for the canonical case).

use chrono::NaiveDate;
use serde::Deserialize;

use crate::engine::ledger::{EvidenceRecord, EvidenceSource, Fact};
use crate::engine::money::Cents;
use crate::engine::types::{Event, EventType, Status};
use crate::extract::{parse_json_reply, ModelClient};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocType {
    Payslip,
    Receipt,
    Invoice,
    Bill,
    DeliverySummary,
    BankStatementExcerpt,
    Other,
}

/// Every labeled figure the VLM transcribed off one document. Every field but `doc_type` is
/// nullable: a figure not printed on the page stays `None`, never an inferred zero.
#[derive(Debug, Clone, Default, Deserialize)]
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

const TOL: f64 = 0.01;

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= TOL
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
        (_, Status::Settled) => figures.amount_paid.or(figures.total),
        (_, Status::Pending | Status::Scheduled) => {
            if let (Some(before), Some(before_val), Some(after)) = (
                figures.amount_due_before_date,
                figures
                    .amount_due_before_date_value
                    .as_deref()
                    .and_then(parse_date),
                figures.amount_due_after_date,
            ) {
                if event.cash_date() > before_val {
                    return Some(after);
                }
                return Some(before);
            }
            figures.balance_due.or(figures.amount_due)
        }
        _ => figures.total,
    }
}

/// Every check that has the data to run, must pass, or the figure is rejected
/// (PLAN.md §2.3: subtotal+tax=total, gross-deductions=net, currency match, and the VLM's
/// own line-item sum against whatever it lines up with).
pub fn reconciles(figures: &ImageFigures, event: &Event) -> bool {
    if let (Some(sub), Some(tax), Some(total)) = (figures.subtotal, figures.tax, figures.total) {
        if !close(sub + tax, total) {
            return false;
        }
    }
    if let (Some(gross), Some(ded), Some(net)) = (figures.gross_pay, figures.deductions, figures.net_pay)
    {
        if !close(gross - ded, net) {
            return false;
        }
    }
    if let (Some(paid), Some(bal), Some(total)) = (figures.amount_paid, figures.balance_due, figures.total)
    {
        if !close(paid + bal, total) {
            return false;
        }
    }
    if let (Some(sum), Some(target)) = (figures.line_items_sum_check, figures.subtotal.or(figures.total))
    {
        if !close(sum, target) {
            return false;
        }
    }
    if let Some(cur) = &figures.currency {
        if cur != &event.currency {
            return false;
        }
    }
    true
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
            amount: Cents::from_f64(amount),
            currency: figures.currency.clone().unwrap_or_else(|| event.currency.clone()),
        },
    })
}

/// One call per image (PLAN.md §3 batching lever), per
/// `code/prompts/image_transcription.v1.md`. Returns the transcription plus token counts
/// for the usage report.
pub fn extract_figures(
    client: &dyn ModelClient,
    system_prompt: &str,
    image_bytes: Vec<u8>,
) -> anyhow::Result<(ImageFigures, u32, u32)> {
    let user_prompt = "Transcribe every labeled figure on this document. Respond with the JSON object only.";
    let response = client.complete(system_prompt, user_prompt, &[image_bytes])?;
    let value = parse_json_reply(&response.text)?;
    let figures: ImageFigures = serde_json::from_value(value)?;
    Ok((figures, response.prompt_tokens, response.completion_tokens))
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
}
