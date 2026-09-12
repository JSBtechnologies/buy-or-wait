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
use crate::engine::money::Money;
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
/// the record carries no ledger-relevant fact (informational, rejected instruction), when
/// the event-level fact gate rejects its target event id (`event_fact_gate`, verifier #38),
/// or when a required field for its `Fact` shape is missing (never guessed).
pub fn to_evidence(
    message: &Message,
    idx: usize,
    record: &MessageRecord,
    home_currency: &str,
) -> Option<EvidenceRecord> {
    let record_id = format!("{}#{idx}", message.message_id);
    let observed_at = message.sent_at.naive_utc();
    // board decision `event_fact_gate` (verifier #38, endorsed by lead): an event-level fact
    // may only target the event `messages.csv` itself links via `related_event_id`. A
    // model's own `related_event_id` claim is never sufficient on its own, and one that
    // contradicts the CSV link is treated as a hallucination — the whole record is dropped
    // rather than guessing which event it meant. This is why messages like 13/23/41/135/
    // 202/213 (own-account-transfer, no CSV link) must yield zero ledger facts.
    let event_id = match (&message.related_event_id, &record.related_event_id) {
        (Some(csv_id), None) => Some(csv_id.clone()),
        (Some(csv_id), Some(claimed)) if claimed == csv_id => Some(csv_id.clone()),
        _ => None,
    };
    let currency = || record.currency.clone().unwrap_or_else(|| home_currency.to_string());
    let category = |default: &str| record.category_hint.clone().unwrap_or_else(|| default.to_string());

    let fact = match record.record_type {
        RecordType::SalaryChange | RecordType::SalaryFirstConfirmed => {
            match (record.amount, record.date.as_deref()) {
                (Some(a), Some(d)) => Fact::IncomeAmountChange {
                    category: category("salary"),
                    amount: Money::from_f64(a),
                    currency: currency(),
                    effective: parse_date(d)?,
                },
                (Some(a), None) => Fact::NextIncomeAmount {
                    category: category("salary"),
                    amount: Money::from_f64(a),
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
                amount: Money::from_f64(amount),
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
            amount: record.amount.map(Money::from_f64),
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
                        amount: record.amount.map(Money::from_f64),
                        date: record.date.as_deref().and_then(parse_date),
                    },
                    _ => Fact::EventAmended {
                        event_id,
                        amount: record.amount.map(Money::from_f64),
                        date: record.date.as_deref().and_then(parse_date),
                    },
                }
            }
        }
        RecordType::RecurringExpenseChange => {
            // engine#32/dd2402e: Fact::ExpenseAmountChange takes exactly one of amount/
            // percent, never a guessed category. amount wins if a model somehow sends
            // both (an absolute figure is more precise than a percent of an unknown base).
            let category = record.category_hint.clone()?;
            let (amount, percent) = match (record.amount, record.percent) {
                (Some(a), _) => (Some(Money::from_f64(a)), None),
                (None, Some(p)) => (None, Some(p)),
                (None, None) => return None,
            };
            Fact::ExpenseAmountChange {
                category,
                amount,
                percent,
                currency: record.currency.clone(),
                // No explicit effective date (e.g. "the next rent payment"): anchor on the
                // message's own sent_at date and let the recurrence detector find the next
                // stream occurrence on/after it, rather than guessing a calendar date here.
                effective: record
                    .date
                    .as_deref()
                    .and_then(parse_date)
                    .unwrap_or_else(|| observed_at.date()),
            }
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

    /// Rent +12% renewal messages (12/51/175, lead: engine#32/dd2402e) map to
    /// `Fact::ExpenseAmountChange` with `percent` set and `amount` left `None` — the model
    /// only ever states a percentage for this template, never an absolute new rent figure.
    #[test]
    fn recurring_expense_change_maps_to_expense_amount_change_by_percent() {
        let message = Message {
            message_id: "message_12".to_string(),
            user_id: "user_16".to_string(),
            request_id: Some("request_16".to_string()),
            related_event_id: None,
            sent_at: "2023-08-01T09:30:00Z".parse().unwrap(),
            source_type: "service_provider".to_string(),
            message_text: "StayLedger wanted to let you know about a change on your account. The renewed lease increases monthly rent by 12%. The new amount will be used for the next rent payment. Case ref SER-0012.".to_string(),
        };
        let record = MessageRecord {
            record_type: RecordType::RecurringExpenseChange,
            amount: None,
            currency: None,
            percent: Some(12.0),
            date: None,
            related_event_id: None,
            status_hint: Some(StatusHint::Confirmed),
            direction: Some(RecordDirection::Increase),
            scope: None,
            is_duplicate_transfer: None,
            category_hint: Some("rent".to_string()),
            note: Some("rent_up_12pct_next_payment_apply_to_existing_stream_amount".to_string()),
        };
        let evidence = to_evidence(&message, 0, &record, "INR").expect("expected a fact");
        match evidence.fact {
            Fact::ExpenseAmountChange { category, amount, percent, currency, effective } => {
                assert_eq!(category, "rent");
                assert_eq!(amount, None);
                assert_eq!(percent, Some(12.0));
                assert_eq!(currency, None);
                assert_eq!(effective, NaiveDate::from_ymd_opt(2023, 8, 1).unwrap());
            }
            other => panic!("expected ExpenseAmountChange, got {other:?}"),
        }
    }

    /// A record with neither an absolute amount nor a percent, or no category, is dropped
    /// rather than guessed.
    #[test]
    fn recurring_expense_change_without_amount_or_category_is_dropped() {
        let message = Message {
            message_id: "message_x".to_string(),
            user_id: "user_1".to_string(),
            request_id: None,
            related_event_id: None,
            sent_at: "2023-08-01T09:30:00Z".parse().unwrap(),
            source_type: "service_provider".to_string(),
            message_text: "irrelevant".to_string(),
        };
        let base = MessageRecord {
            record_type: RecordType::RecurringExpenseChange,
            amount: None,
            currency: None,
            percent: None,
            date: None,
            related_event_id: None,
            status_hint: None,
            direction: None,
            scope: None,
            is_duplicate_transfer: None,
            category_hint: Some("rent".to_string()),
            note: None,
        };
        assert!(to_evidence(&message, 0, &base, "INR").is_none()); // no amount/percent

        let mut no_category = base.clone();
        no_category.percent = Some(12.0);
        no_category.category_hint = None;
        assert!(to_evidence(&message, 0, &no_category, "INR").is_none()); // no category
    }

    #[test]
    fn skeleton_masks_numbers_dates_and_org_names() {
        let a = skeleton("Hi, Northstar Labs payroll here. Your monthly salary has increased to USD 2988. The change applies from 2026-07-15.");
        let b = skeleton("Hi, Greenfield Foods payroll here. Your monthly salary has increased to USD 1500. The change applies from 2025-03-01.");
        assert_eq!(a, b);
    }

    fn own_account_transfer_message(message_id: &str, text: &str) -> Message {
        Message {
            message_id: message_id.to_string(),
            user_id: "user_18".to_string(),
            request_id: Some("request_18".to_string()),
            related_event_id: None, // messages.csv leaves this blank for all 6 cases
            sent_at: "2026-07-01T09:30:00Z".parse().unwrap(),
            source_type: "bank".to_string(),
            message_text: text.to_string(),
        }
    }

    /// board decision `event_fact_gate` (verifier #38): own-account-transfer messages
    /// 13/23/41/135/202/213 have no `related_event_id` in messages.csv. Even if the model
    /// still claims `is_duplicate_transfer: true` (and, worse, hallucinates an event id),
    /// no event-level fact may be emitted — never guess which event it meant.
    #[test]
    fn own_account_transfer_without_csv_event_link_yields_no_fact() {
        let cases = [
            ("message_13", "There\u{2019}s an update from Summit Bank on your recent account activity. The matching debit and credit came from a transfer between your two accounts. Both accounts are registered under the same account holder. Both entries will remain visible in your transaction history. Txn ref BAN-0013."),
            ("message_23", "Cedar Bank has reviewed the transaction on your account. The matching debit and credit came from a transfer between your two accounts. Both entries will remain visible in your transaction history. Txn ref BAN-0023."),
            ("message_41", "Here\u{2019}s the latest transaction update from Summit Bank. The matching debit and credit came from a transfer between your two accounts. Both accounts are registered under the same account holder. Both entries will remain visible in your transaction history. Txn ref BAN-0041."),
            ("message_135", "Cedar Bank has new information about one of your transactions. The matching debit and credit came from a transfer between your two accounts. Both accounts are registered under the same account holder. Both entries will remain visible in your transaction history. Txn ref BAN-0135."),
            ("message_202", "Hi, Summit Bank here. The matching debit and credit came from a transfer between your two accounts. Both accounts are registered under the same account holder. Both entries will remain visible in your transaction history. Txn ref BAN-0202."),
            ("message_213", "Harbor Bank telah meninjau transaksi pada rekening Anda. Debit dan kredit dengan jumlah yang sama berasal dari transfer antara dua rekening Anda. Kedua rekening terdaftar atas nama pemilik yang sama. Kedua transaksi akan tetap terlihat dalam riwayat rekening Anda. Ref transaksi BAN-0213."),
        ];
        for (message_id, text) in cases {
            let message = own_account_transfer_message(message_id, text);
            // No claimed event id at all: gate drops it.
            let honest = MessageRecord {
                record_type: RecordType::EventAmendment,
                amount: None,
                currency: None,
                percent: None,
                date: None,
                related_event_id: None,
                status_hint: None,
                direction: None,
                scope: None,
                is_duplicate_transfer: Some(true),
                category_hint: None,
                note: Some("own_account_transfer_exclude_one_leg_from_cash_flow".into()),
            };
            assert!(
                to_evidence(&message, 0, &honest, "INR").is_none(),
                "{message_id}: no CSV related_event_id must yield zero facts"
            );

            // Model hallucinates an event id despite no CSV link: still must be dropped.
            let mut hallucinated = honest.clone();
            hallucinated.related_event_id = Some("event_9999".into());
            assert!(
                to_evidence(&message, 0, &hallucinated, "INR").is_none(),
                "{message_id}: a model-claimed event id must never substitute for the CSV link"
            );
        }
    }

    /// A model-claimed event id that CONTRADICTS the CSV link is also dropped, not trusted.
    #[test]
    fn contradicting_claimed_event_id_is_rejected() {
        let mut message = own_account_transfer_message("message_106", "dispute text");
        message.related_event_id = Some("event_12709".into());
        let record = MessageRecord {
            record_type: RecordType::EventAmendment,
            amount: None,
            currency: None,
            percent: None,
            date: None,
            related_event_id: Some("event_0001".into()), // does not match the CSV link
            status_hint: Some(StatusHint::Pending),
            direction: None,
            scope: None,
            is_duplicate_transfer: None,
            category_hint: None,
            note: None,
        };
        assert!(to_evidence(&message, 0, &record, "EUR").is_none());
    }

    /// The matching, non-contradicting case still works (sanity check for the gate).
    #[test]
    fn matching_claimed_event_id_is_accepted() {
        let mut message = own_account_transfer_message("message_106", "dispute text");
        message.related_event_id = Some("event_12709".into());
        let record = MessageRecord {
            record_type: RecordType::EventAmendment,
            amount: None,
            currency: None,
            percent: None,
            date: None,
            related_event_id: Some("event_12709".into()),
            status_hint: Some(StatusHint::Pending),
            direction: None,
            scope: None,
            is_duplicate_transfer: None,
            category_hint: None,
            note: None,
        };
        assert!(to_evidence(&message, 0, &record, "EUR").is_some());
    }
}
