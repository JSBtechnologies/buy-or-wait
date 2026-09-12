//! Typed message records (PLAN.md §2.4, §2.5; schema = `code/prompts/message_extraction.v1.md`).
//! One message maps to 0..n records. Every field except `record_type` is optional: a model
//! that claims a value for a field with no basis in the text should emit `null` there, and
//! an outright injection attempt lands in `record_type: rejected_instruction`, which this
//! module never converts into a `Fact` — there is no field for an instruction to reach the
//! engine through (PLAN.md §2.5).

use chrono::NaiveDate;
use serde::Deserialize;
use std::collections::HashMap;

use crate::engine::ledger::{EvidenceRecord, EvidenceSource, Fact};
use crate::engine::money::Cents;
use crate::engine::types::Direction;
use crate::extract::{parse_json_reply, ModelClient};
use crate::model::Message;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordType {
    SalaryChange,
    SalaryFirstConfirmed,
    IncomeEnded,
    OneTimeAdjustment,
    RecurringExpenseChange,
    PendingUnconfirmedCredit,
    EventAmendment,
    InvestmentUnrealizedChange,
    NoActionableFact,
    RejectedInstruction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusHint {
    Confirmed,
    Pending,
    Processing,
    Settled,
    Scheduled,
    Cancelled,
    Failed,
    Ended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordDirection {
    Increase,
    Decrease,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    FullEmployment,
    SeasonalContract,
    HouseholdPartial,
}

/// Fixed-field record deserialized from a model reply. Extra JSON fields the model emits
/// are silently dropped (serde's default, non-`deny_unknown_fields` behavior); missing
/// optional fields default to `None`.
#[derive(Debug, Clone, Deserialize)]
pub struct MessageRecord {
    pub record_type: RecordType,
    #[serde(default)]
    pub amount: Option<f64>,
    #[serde(default)]
    pub currency: Option<String>,
    #[serde(default)]
    pub percent: Option<f64>,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub related_event_id: Option<String>,
    #[serde(default)]
    pub status_hint: Option<StatusHint>,
    #[serde(default)]
    pub direction: Option<RecordDirection>,
    #[serde(default)]
    pub scope: Option<Scope>,
    #[serde(default)]
    pub is_duplicate_transfer: Option<bool>,
    #[serde(default)]
    pub category_hint: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct MessageRecordsReply {
    message_id: String,
    #[serde(default)]
    records: Vec<MessageRecord>,
}

fn parse_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

/// Mask numbers/dates/percentages and multi-word proper-noun runs into a skeleton, so
/// structurally identical messages (same template, different employer/amount/date) share
/// one model call and one cached parse (PLAN.md §3 batching lever, §2.11 caching). See
/// `docs/gold_subset.json` `skeleton_families_survey` for the family counts this is based on.
pub fn skeleton(text: &str) -> String {
    let num = regex::Regex::new(r"\d[\d,.\-]*").unwrap();
    let mut t = num.replace_all(text, "<N>").into_owned();
    let pct = regex::Regex::new(r"<N>%").unwrap();
    t = pct.replace_all(&t, "<PCT>").into_owned();
    let names = regex::Regex::new(r"\b[A-Z][a-zA-Z]+(?:\s[A-Z][a-zA-Z]+){1,2}\b").unwrap();
    names.replace_all(&t, "<ORG>").into_owned()
}

/// Batch every message in `batch` (already filtered to relevant, unresolved-skeleton
/// messages for one user) into a single model call, per
/// `code/prompts/message_extraction.v1.md`.
pub fn extract_batch(
    client: &dyn ModelClient,
    system_prompt: &str,
    batch: &[&Message],
) -> anyhow::Result<(HashMap<String, Vec<MessageRecord>>, u32, u32)> {
    let block: String = batch
        .iter()
        .map(|m| format!("[{}] {}\n", m.message_id, m.message_text))
        .collect();
    let user_prompt = format!(
        "Extract typed records from each of the following messages. Respond with the JSON array only.\n\n{block}"
    );
    let response = client.complete(system_prompt, &user_prompt, &[])?;
    let value = parse_json_reply(&response.text)?;
    let replies: Vec<MessageRecordsReply> = serde_json::from_value(value)?;
    let by_id = replies
        .into_iter()
        .map(|r| (r.message_id, r.records))
        .collect();
    Ok((by_id, response.prompt_tokens, response.completion_tokens))
}

/// Convert one validated record into the engine's evidence contract. Returns `None` when
/// the record carries no ledger-relevant fact (informational, rejected instruction) or when
/// the fact it describes has no matching `Fact` variant yet (never guessed into the wrong
/// shape — see the `RecurringExpenseChange` arm).
pub fn to_evidence(
    message: &Message,
    idx: usize,
    record: &MessageRecord,
    home_currency: &str,
) -> Option<EvidenceRecord> {
    let record_id = format!("{}#{idx}", message.message_id);
    let observed_at = message.sent_at.naive_utc();
    let event_id = record
        .related_event_id
        .clone()
        .or_else(|| message.related_event_id.clone());
    let currency = || record.currency.clone().unwrap_or_else(|| home_currency.to_string());
    let category = |default: &str| record.category_hint.clone().unwrap_or_else(|| default.to_string());

    let fact = match record.record_type {
        RecordType::SalaryChange | RecordType::SalaryFirstConfirmed => {
            match (record.amount, record.date.as_deref()) {
                (Some(a), Some(d)) => Fact::IncomeAmountChange {
                    category: category("salary"),
                    amount: Cents::from_f64(a),
                    currency: currency(),
                    effective: parse_date(d)?,
                },
                (Some(a), None) => Fact::NextIncomeAmount {
                    category: category("salary"),
                    amount: Cents::from_f64(a),
                    currency: currency(),
                    date: None,
                },
                (None, Some(d)) => Fact::IncomeDateMoved {
                    category: category("salary"),
                    new_date: parse_date(d)?,
                },
                (None, None) => return None,
            }
        }
        RecordType::IncomeEnded => Fact::IncomeEnded {
            category: category("salary"),
            effective: record
                .date
                .as_deref()
                .and_then(parse_date)
                .unwrap_or_else(|| observed_at.date()),
        },
        RecordType::OneTimeAdjustment => {
            let amount = record.amount?;
            Fact::OneTimeFlow {
                // Every gold example of this record type is a salary-linked arrears credit;
                // a debit one-off has no observed template yet. Revisit if the bake-off or
                // full-dataset run surfaces a debit case (PLAN.md §2.4).
                direction: Direction::Credit,
                category: category("salary"),
                amount: Cents::from_f64(amount),
                currency: currency(),
                date: record
                    .date
                    .as_deref()
                    .and_then(parse_date)
                    .unwrap_or_else(|| observed_at.date()),
            }
        }
        RecordType::PendingUnconfirmedCredit => Fact::Unconfirmed {
            category: category("windfall"),
            amount: record.amount.map(Cents::from_f64),
            currency: record.currency.clone(),
        },
        RecordType::EventAmendment => {
            let event_id = event_id?;
            if record.is_duplicate_transfer == Some(true) {
                Fact::OwnAccountTransfer { event_id }
            } else {
                match record.status_hint {
                    Some(StatusHint::Cancelled) => Fact::EventCancelled { event_id },
                    Some(StatusHint::Settled) => Fact::EventSettled {
                        event_id,
                        amount: record.amount.map(Cents::from_f64),
                        date: record.date.as_deref().and_then(parse_date),
                    },
                    _ => Fact::EventAmended {
                        event_id,
                        amount: record.amount.map(Cents::from_f64),
                        date: record.date.as_deref().and_then(parse_date),
                    },
                }
            }
        }
        RecordType::RecurringExpenseChange => {
            // No `Fact` variant expresses "an EXISTING recurring expense changes by amount
            // or percent" yet (only `NewRecurringExpense`, which requires a concrete amount
            // + first_date). Raised on bus topic `blocker` (owner: engine). Never guess a
            // Fact for this record type until a variant lands.
            return None;
        }
        RecordType::InvestmentUnrealizedChange => {
            // `Status::Unrealized` rows are already `Excluded(NonCash)` by the base cash
            // rules regardless of evidence (PLAN.md §2.1) — this message is pure
            // corroboration, kept as a no-op amendment for the audit trail.
            let event_id = event_id?;
            Fact::EventAmended { event_id, amount: None, date: None }
        }
        RecordType::NoActionableFact | RecordType::RejectedInstruction => return None,
    };

    Some(EvidenceRecord {
        record_id,
        source: EvidenceSource::Message { source_type: message.source_type.clone() },
        observed_at,
        fact,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skeleton_masks_numbers_dates_and_org_names() {
        let a = skeleton("Hi, Northstar Labs payroll here. Your monthly salary has increased to USD 2988. The change applies from 2026-07-15.");
        let b = skeleton("Hi, Greenfield Foods payroll here. Your monthly salary has increased to USD 1500. The change applies from 2025-03-01.");
        assert_eq!(a, b);
    }
}
