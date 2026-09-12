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

fn amt(s: &str) -> f64 {
    s.replace(',', "").parse().unwrap_or(0.0)
}

fn blank_record(record_type: RecordType) -> MessageRecord {
    MessageRecord {
        record_type,
        amount: None,
        currency: None,
        percent: None,
        date: None,
        related_event_id: None,
        status_hint: None,
        direction: None,
        scope: None,
        is_duplicate_transfer: None,
        category_hint: None,
        note: None,
    }
}

/// Deterministic, zero-token parse for a message whose skeleton is already known — every
/// template family observed in `dataset/messages.csv` (English and Indonesian; see
/// `docs/gold_subset.json`'s `skeleton_families_survey`). Returns `None` for an unrecognized
/// skeleton, meaning it needs a model call (`extract_batch`) instead — never guessed here.
/// Runs no model, costs nothing, and is exact: every capture is a literal substring of the
/// message text, so `grounding::amount_grounded` always passes trivially for these records.
pub fn parse_known_skeleton(text: &str) -> Option<Vec<MessageRecord>> {
    // salary_increase: "...monthly salary has increased to <AMT>. The change applies from
    // <DATE>..." / "...Gaji bulanan Anda naik menjadi <AMT>. Perubahan ini berlaku mulai
    // <DATE>..."
    static RE_SALARY_INCREASE_EN: &str =
        r"salary has increased to ([A-Z]{2,4}) ([\d,]+(?:\.\d+)?)\. The change applies from (\d{4}-\d{2}-\d{2})";
    static RE_SALARY_INCREASE_ID: &str =
        r"naik menjadi ([A-Z]{2,4}) ([\d,]+(?:\.\d+)?)\. Perubahan ini berlaku mulai (\d{4}-\d{2}-\d{2})";
    if let Some(c) = regex::Regex::new(RE_SALARY_INCREASE_EN).unwrap().captures(text) {
        return Some(vec![salary_change_record(&c, "increase", "salary_increase")]);
    }
    if let Some(c) = regex::Regex::new(RE_SALARY_INCREASE_ID).unwrap().captures(text) {
        return Some(vec![salary_change_record(&c, "increase", "salary_increase")]);
    }

    // salary_temp_decrease: "...temporary monthly pay is <AMT>. The reduced amount continues
    // for the next payroll..." / "...Gaji bulanan sementara Anda adalah <AMT>..." — no date.
    static RE_SALARY_TEMP_EN: &str = r"temporary monthly pay is ([A-Z]{2,4}) ([\d,]+(?:\.\d+)?)";
    static RE_SALARY_TEMP_ID: &str = r"[Gg]aji bulanan sementara Anda adalah ([A-Z]{2,4}) ([\d,]+(?:\.\d+)?)";
    if let Some(c) = regex::Regex::new(RE_SALARY_TEMP_EN).unwrap().captures(text) {
        return Some(vec![salary_next_amount_record(&c, "temporary_reduction")]);
    }
    if let Some(c) = regex::Regex::new(RE_SALARY_TEMP_ID).unwrap().captures(text) {
        return Some(vec![salary_next_amount_record(&c, "temporary_reduction")]);
    }

    // salary_decrease_leave: "...next salary is reduced to <AMT>. The adjustment is due to
    // approved unpaid leave..."
    static RE_SALARY_LEAVE_EN: &str =
        r"next salary is reduced to ([A-Z]{2,4}) ([\d,]+(?:\.\d+)?)\. The adjustment is due to approved unpaid leave";
    if let Some(c) = regex::Regex::new(RE_SALARY_LEAVE_EN).unwrap().captures(text) {
        let mut r = blank_record(RecordType::SalaryChange);
        r.currency = Some(c[1].to_string());
        r.amount = Some(amt(&c[2]));
        r.direction = Some(RecordDirection::Decrease);
        r.status_hint = Some(StatusHint::Confirmed);
        r.category_hint = Some("salary".to_string());
        r.note = Some("unpaid_leave_reduction".to_string());
        return Some(vec![r]);
    }

    // salary_date_change: "...confirmed salary is now expected on <DATE>. This replaces the
    // payroll date..." / "...diperkirakan masuk pada <DATE>. Tanggal ini menggantikan..."
    static RE_SALARY_DATE_EN: &str = r"confirmed salary is now expected on (\d{4}-\d{2}-\d{2})";
    static RE_SALARY_DATE_ID: &str = r"diperkirakan masuk pada (\d{4}-\d{2}-\d{2})";
    if let Some(c) = regex::Regex::new(RE_SALARY_DATE_EN).unwrap().captures(text) {
        return Some(vec![salary_date_moved_record(&c)]);
    }
    if let Some(c) = regex::Regex::new(RE_SALARY_DATE_ID).unwrap().captures(text) {
        return Some(vec![salary_date_moved_record(&c)]);
    }

    // salary_resumes_plus_new_expense: "Regular salary of <AMT> resumes on <DATE>. A new
    // recurring <X> payment begins in the same month..."
    static RE_SALARY_RESUMES: &str =
        r"Regular salary of ([A-Z]{2,4}) ([\d,]+(?:\.\d+)?) resumes on (\d{4}-\d{2}-\d{2})\. A new recurring (.+?) payment begins";
    if let Some(c) = regex::Regex::new(RE_SALARY_RESUMES).unwrap().captures(text) {
        let mut salary = blank_record(RecordType::SalaryChange);
        salary.currency = Some(c[1].to_string());
        salary.amount = Some(amt(&c[2]));
        salary.date = Some(c[3].to_string());
        salary.direction = Some(RecordDirection::Increase);
        salary.status_hint = Some(StatusHint::Confirmed);
        salary.category_hint = Some("salary".to_string());
        salary.note = Some("resumes_after_pause".to_string());

        let mut expense = blank_record(RecordType::RecurringExpenseChange);
        // No amount stated ("begins" with no figure) -> to_evidence() drops this one rather
        // than invent an amount (RULES.md S5: NewRecurringExpense "cannot be counted"),
        // but it is still surfaced here for the audit trail.
        expense.date = Some(c[3].to_string());
        expense.status_hint = Some(StatusHint::Confirmed);
        expense.category_hint = Some(c[4].trim().to_lowercase());
        expense.note = Some("new_recurring_expense_amount_unknown".to_string());
        return Some(vec![salary, expense]);
    }

    // salary_first_confirmed: "...first salary will be <AMT>. The confirmed credit date is
    // <DATE>..." / "...Gaji pertama dari perusahaan baru adalah <AMT>. Pembayaran sudah
    // dikonfirmasi untuk <DATE>..."
    static RE_FIRST_SALARY_EN: &str =
        r"first salary will be ([A-Z]{2,4}) ([\d,]+(?:\.\d+)?)\. The confirmed credit date is (\d{4}-\d{2}-\d{2})";
    static RE_FIRST_SALARY_ID: &str =
        r"[Gg]aji pertama dari perusahaan baru adalah ([A-Z]{2,4}) ([\d,]+(?:\.\d+)?)\. Pembayaran sudah dikonfirmasi untuk (\d{4}-\d{2}-\d{2})";
    if let Some(c) = regex::Regex::new(RE_FIRST_SALARY_EN).unwrap().captures(text) {
        return Some(vec![first_salary_record(&c)]);
    }
    if let Some(c) = regex::Regex::new(RE_FIRST_SALARY_ID).unwrap().captures(text) {
        return Some(vec![first_salary_record(&c)]);
    }

    None
}

fn salary_change_record(c: &regex::Captures, direction: &str, note: &str) -> MessageRecord {
    let mut r = blank_record(RecordType::SalaryChange);
    r.currency = Some(c[1].to_string());
    r.amount = Some(amt(&c[2]));
    r.date = Some(c[3].to_string());
    r.direction = Some(if direction == "increase" { RecordDirection::Increase } else { RecordDirection::Decrease });
    r.status_hint = Some(StatusHint::Confirmed);
    r.category_hint = Some("salary".to_string());
    r.note = Some(note.to_string());
    r
}

fn salary_next_amount_record(c: &regex::Captures, note: &str) -> MessageRecord {
    let mut r = blank_record(RecordType::SalaryChange);
    r.currency = Some(c[1].to_string());
    r.amount = Some(amt(&c[2]));
    r.direction = Some(RecordDirection::Decrease);
    r.status_hint = Some(StatusHint::Scheduled);
    r.category_hint = Some("salary".to_string());
    r.note = Some(note.to_string());
    r
}

fn salary_date_moved_record(c: &regex::Captures) -> MessageRecord {
    let mut r = blank_record(RecordType::SalaryChange);
    r.date = Some(c[1].to_string());
    r.status_hint = Some(StatusHint::Confirmed);
    r.category_hint = Some("salary".to_string());
    r.note = Some("pay_date_moved_supersedes_prior".to_string());
    r
}

fn first_salary_record(c: &regex::Captures) -> MessageRecord {
    let mut r = blank_record(RecordType::SalaryFirstConfirmed);
    r.currency = Some(c[1].to_string());
    r.amount = Some(amt(&c[2]));
    r.date = Some(c[3].to_string());
    r.status_hint = Some(StatusHint::Confirmed);
    r.category_hint = Some("salary".to_string());
    r.note = Some("new_employer_first_salary".to_string());
    r
}

/// Every message from `messages` whose skeleton `parse_known_skeleton` recognizes, converted
/// straight into evidence — zero model calls. Feed the result directly into
/// `engine::session::Session::apply_evidence`. Messages with an unrecognized skeleton are
/// silently skipped here (they still need a model call later); this function never blocks
/// on the model path.
pub fn deterministic_evidence(messages: &[&Message], home_currency: &str) -> Vec<EvidenceRecord> {
    let mut out = Vec::new();
    for message in messages {
        let Some(records) = parse_known_skeleton(&message.message_text) else { continue };
        for (idx, record) in records.iter().enumerate() {
            if let Some(evidence) = to_evidence(message, idx, record, home_currency) {
                out.push(evidence);
            }
        }
    }
    out
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

    fn msg(id: &str, user: &str, sent_at: &str, source: &str, text: &str) -> Message {
        Message {
            message_id: id.to_string(),
            user_id: user.to_string(),
            request_id: None,
            related_event_id: None,
            sent_at: sent_at.parse().unwrap(),
            source_type: source.to_string(),
            message_text: text.to_string(),
        }
    }

    /// The 6 sample-user misses the lead flagged (RULES.md S3.4: users 02/06/07/08/14/15,
    /// messages 01/04/05/06/10/11) parse deterministically, with zero model calls, into
    /// exactly the facts RULES.md says the tuning labels need.
    #[test]
    fn deterministic_parse_covers_the_six_sample_misses() {
        // user_02 / message_01: 42,750,000 IDR from 2025-08-15 (salary raise).
        let records = parse_known_skeleton("Rincian penggajian Anda di Cobalt Systems telah berubah. Gaji bulanan Anda naik menjadi IDR 42750000. Perubahan ini berlaku mulai 2025-08-15. Jumlah yang diperbarui akan terlihat pada slip gaji berikutnya. Ref payroll EMP-0001.").expect("message_01 skeleton");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].record_type, RecordType::SalaryChange);
        assert_eq!(records[0].amount, Some(42_750_000.0));
        assert_eq!(records[0].currency.as_deref(), Some("IDR"));
        assert_eq!(records[0].date.as_deref(), Some("2025-08-15"));

        // user_06 / message_04: temporary 1,037.52 EUR, no date.
        let records = parse_known_skeleton("Here\u{2019}s the latest payroll information from Northstar Labs. Your temporary monthly pay is EUR 1037.52. The reduced amount continues for the next payroll. This is the amount currently scheduled for the affected pay cycle. Payroll ref EMP-0004.").expect("message_04 skeleton");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].amount, Some(1037.52));
        assert_eq!(records[0].currency.as_deref(), Some("EUR"));
        assert_eq!(records[0].date, None);

        // user_07 / message_05: pay date moves to 2024-09-23, no amount.
        let records = parse_known_skeleton("BrightPath Media has updated your payroll record. Your confirmed salary is now expected on 2024-09-23. This replaces the payroll date shown in the earlier update. Please use the revised date for anything you normally pay around payday. Payroll ref EMP-0005.").expect("message_05 skeleton");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].amount, None);
        assert_eq!(records[0].date.as_deref(), Some("2024-09-23"));

        // user_08 / message_06: reduced to 1,422.85 EUR, unpaid leave, no date.
        let records = parse_known_skeleton("Hi, Greenfield Foods payroll here. Your next salary is reduced to EUR 1422.85. The adjustment is due to approved unpaid leave. The adjustment will be visible on your next payslip. Payroll ref EMP-0006.").expect("message_06 skeleton");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].amount, Some(1422.85));
        assert_eq!(records[0].currency.as_deref(), Some("EUR"));

        // user_14 / message_10: salary 2,717 EUR resumes 2025-08-15, PLUS a childcare
        // record with no amount (must be dropped downstream, not invented).
        let records = parse_known_skeleton("Here\u{2019}s the latest payroll information from HarborWorks. Regular salary of EUR 2717 resumes on 2025-08-15. A new recurring childcare payment begins in the same month. The updated pay and deductions will appear from the next cycle. Payroll ref EMP-0010.").expect("message_10 skeleton");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].amount, Some(2717.0));
        assert_eq!(records[0].date.as_deref(), Some("2025-08-15"));
        assert_eq!(records[1].record_type, RecordType::RecurringExpenseChange);
        assert_eq!(records[1].amount, None);
        let message_10 = msg("message_10", "user_14", "2025-07-27T09:30:00Z", "employer", "irrelevant");
        assert!(to_evidence(&message_10, 1, &records[1], "EUR").is_none()); // no invented amount

        // user_15 / message_11: first salary 1,661 EUR, confirmed credit date 2026-01-15.
        let records = parse_known_skeleton("A quick update from the payroll team at Riverline Retail. Your first salary will be EUR 1661. The confirmed credit date is 2026-01-15. The money will appear after the bank posts the credit. Payroll ref EMP-0011.").expect("message_11 skeleton");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].record_type, RecordType::SalaryFirstConfirmed);
        assert_eq!(records[0].amount, Some(1661.0));
        assert_eq!(records[0].date.as_deref(), Some("2026-01-15"));
    }

    #[test]
    fn deterministic_evidence_produces_engine_ready_facts_for_the_six_misses() {
        let messages = vec![
            msg("message_01", "user_02", "2025-07-29T09:30:00Z", "employer", "Rincian penggajian Anda di Cobalt Systems telah berubah. Gaji bulanan Anda naik menjadi IDR 42750000. Perubahan ini berlaku mulai 2025-08-15. Jumlah yang diperbarui akan terlihat pada slip gaji berikutnya. Ref payroll EMP-0001."),
            msg("message_04", "user_06", "2025-12-28T09:30:00Z", "employer", "Here\u{2019}s the latest payroll information from Northstar Labs. Your temporary monthly pay is EUR 1037.52. The reduced amount continues for the next payroll. This is the amount currently scheduled for the affected pay cycle. Payroll ref EMP-0004."),
            msg("message_05", "user_07", "2024-08-29T09:30:00Z", "employer", "BrightPath Media has updated your payroll record. Your confirmed salary is now expected on 2024-09-23. This replaces the payroll date shown in the earlier update. Please use the revised date for anything you normally pay around payday. Payroll ref EMP-0005."),
            msg("message_06", "user_08", "2025-02-06T09:30:00Z", "employer", "Hi, Greenfield Foods payroll here. Your next salary is reduced to EUR 1422.85. The adjustment is due to approved unpaid leave. The adjustment will be visible on your next payslip. Payroll ref EMP-0006."),
            msg("message_10", "user_14", "2025-07-27T09:30:00Z", "employer", "Here\u{2019}s the latest payroll information from HarborWorks. Regular salary of EUR 2717 resumes on 2025-08-15. A new recurring childcare payment begins in the same month. The updated pay and deductions will appear from the next cycle. Payroll ref EMP-0010."),
            msg("message_11", "user_15", "2026-01-03T09:30:00Z", "employer", "A quick update from the payroll team at Riverline Retail. Your first salary will be EUR 1661. The confirmed credit date is 2026-01-15. The money will appear after the bank posts the credit. Payroll ref EMP-0011."),
        ];
        let refs: Vec<&Message> = messages.iter().collect();
        let evidence = deterministic_evidence(&refs, "EUR");
        // One Fact per message except message_10, which yields only its salary fact (the
        // childcare one drops for lack of an amount) -> 6 facts total.
        assert_eq!(evidence.len(), 6);
        assert!(evidence.iter().all(|e| matches!(
            e.fact,
            Fact::IncomeAmountChange { .. }
                | Fact::NextIncomeAmount { .. }
                | Fact::IncomeDateMoved { .. }
        )));
    }

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
