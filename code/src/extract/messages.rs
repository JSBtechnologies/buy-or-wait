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
use crate::extract::grounding::amount_grounded;
use crate::extract::model_config::{CandidateConfig, DecodingConfig};
use crate::extract::normalize;
use crate::extract::parse_json_reply;
use crate::extract::prompts::PromptSet;
use crate::hf::{ContentPart, HfClient, ModelCall};
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
    /// Stream selector for `income_ended` only (RULES S6.2 A4, lead-approved additive field
    /// on `Fact::IncomeEnded`): identifies WHICH income stream ends when a user has more
    /// than one in the same category, e.g. "Second household income". `None` ends every
    /// stream of the category — never set this to the message's stated remaining amount or
    /// any other field; it is a description substring, not a number.
    #[serde(default)]
    pub description_hint: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct MessageRecordsReply {
    message_id: String,
    #[serde(default)]
    records: Vec<MessageRecord>,
}

/// Delegates to the shared `normalize::parse_date` (a strict superset of the old ISO-only
/// parse: dashes/spaces/month-names/unambiguous slash dates also accepted, never fewer) so
/// date parsing never drifts between the message and image evidence paths.
fn parse_date(s: &str) -> Option<NaiveDate> {
    normalize::parse_date(s)
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

/// Analyst audit #301 (G4, CRITICAL): a regex-captured amount that fails to parse must
/// become `None`, never a silent `0.0` -- a blank/unparseable amount is not the same fact as
/// a stated zero, and every caller here already treats `None` correctly (e.g. `SalaryChange`
/// with `(None, Some(date))` becomes `IncomeDateMoved` instead of a fabricated
/// `IncomeAmountChange` of 0). Delegates to the shared `normalize::parse_amount` so this
/// stays consistent with the image path's number parsing.
fn amt(s: &str) -> Option<f64> {
    normalize::parse_amount(s, None, false)
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
        description_hint: None,
        note: None,
    }
}

/// Deterministic, zero-token parse for a message whose skeleton is already known — every
/// template family observed in `dataset/messages.csv` (English and Indonesian; see
/// `docs/gold_subset.json`'s `skeleton_families_survey`). Returns `None` for an unrecognized
/// skeleton, meaning it needs a model call (`extract_batch`) instead — never guessed here.
/// Runs no model, costs nothing, and is exact: every capture is a literal substring of the
/// message text, so `grounding::amount_grounded` always passes trivially for these records.
/// `text` is the message body; `sent_at` is its `sent_at` date, needed only by families that
/// state no explicit date of their own (e.g. the unpaid-leave reduction, analyst RULES.md
/// S6.1: must anchor `IncomeAmountChange` from the next pay date, not just change the one
/// next occurrence).
pub fn parse_known_skeleton(text: &str, sent_at: NaiveDate) -> Option<Vec<MessageRecord>> {
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
    // approved unpaid leave..." No explicit effective date in the text, but analyst audit
    // (RULES.md S6.1, 10 messages: 06 44 87 102 115 132 140 148 155 195) found the settled
    // history tail for this family is always `[X, X, ~0.55X]` and the message restores X —
    // `NextIncomeAmount` (only the next occurrence) leaves every LATER month falling back to
    // the reduced history value, understating income for the whole horizon. This needs
    // `IncomeAmountChange` effective from the next pay date; since the message states no
    // date, anchor on `sent_at` and let the recurrence detector apply it from the stream's
    // next occurrence on/after that date (same anchoring convention as
    // `ExpenseAmountChange`'s `None`, just expressed as a concrete date because
    // `IncomeAmountChange::effective` is not optional).
    static RE_SALARY_LEAVE_EN: &str =
        r"next salary is reduced to ([A-Z]{2,4}) ([\d,]+(?:\.\d+)?)\. The adjustment is due to approved unpaid leave";
    if let Some(c) = regex::Regex::new(RE_SALARY_LEAVE_EN).unwrap().captures(text) {
        let mut r = blank_record(RecordType::SalaryChange);
        r.currency = Some(c[1].to_string());
        r.amount = amt(&c[2]);
        r.date = Some(sent_at.format("%Y-%m-%d").to_string());
        r.direction = Some(RecordDirection::Decrease);
        r.status_hint = Some(StatusHint::Confirmed);
        r.category_hint = Some("salary".to_string());
        r.note = Some("unpaid_leave_reduction_effective_next_pay".to_string());
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
        // engine#63/8cc70f3: "resumes" is Fact::IncomeStarts (re-anchors the monthly
        // cadence from first_date), same as a first salary — use SalaryFirstConfirmed
        // so to_evidence() routes it there instead of a plain amount change.
        let mut salary = blank_record(RecordType::SalaryFirstConfirmed);
        salary.currency = Some(c[1].to_string());
        salary.amount = amt(&c[2]);
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

    // A1 (analyst RULES.md S6.2): three more first-salary/resumes phrasings, all
    // IncomeStarts — the stream has no other way to seed itself for these users.
    static RE_FIRST_SALARY_EN2: &str =
        r"first salary from the new employer is ([A-Z]{2,4}) ([\d,]+(?:\.\d+)?)\. It is confirmed for (\d{4}-\d{2}-\d{2})";
    static RE_FIRST_SALARY_EN3: &str =
        r"first salary of ([A-Z]{2,4}) ([\d,]+(?:\.\d+)?) is scheduled for (\d{4}-\d{2}-\d{2})\. Payroll has approved the payment and sent it for processing";
    static RE_FIRST_SALARY_ID2: &str =
        r"Gaji pertama Anda sebesar ([A-Z]{2,4}) ([\d,]+(?:\.\d+)?) dijadwalkan pada (\d{4}-\d{2}-\d{2})";
    static RE_FIRST_SALARY_ID3: &str =
        r"Gaji pertama Anda sebesar ([A-Z]{2,4}) ([\d,]+(?:\.\d+)?)\. Tanggal kredit yang dikonfirmasi adalah (\d{4}-\d{2}-\d{2})";
    for re in [RE_FIRST_SALARY_EN2, RE_FIRST_SALARY_EN3, RE_FIRST_SALARY_ID2, RE_FIRST_SALARY_ID3] {
        if let Some(c) = regex::Regex::new(re).unwrap().captures(text) {
            return Some(vec![first_salary_record(&c)]);
        }
    }

    // A2 (S6.2): seasonal contract ended -> IncomeEnded{scope: seasonal_contract}.
    static RE_CONTRACT_ENDED_EN: &str =
        r"current seasonal contract has ended\. No off-season income or renewal has been confirmed";
    static RE_CONTRACT_ENDED_ID: &str =
        r"Kontrak musiman saat ini telah berakhir\. Belum ada pendapatan di luar musim atau perpanjangan kontrak yang dikonfirmasi";
    if regex::Regex::new(RE_CONTRACT_ENDED_EN).unwrap().is_match(text)
        || regex::Regex::new(RE_CONTRACT_ENDED_ID).unwrap().is_match(text)
    {
        let mut r = blank_record(RecordType::IncomeEnded);
        r.scope = Some(Scope::SeasonalContract);
        r.status_hint = Some(StatusHint::Ended);
        r.category_hint = Some("salary".to_string());
        r.note = Some("no_confirmed_renewal_do_not_project".to_string());
        return Some(vec![r]);
    }

    // A3 (S6.2): full employment ended -> IncomeEnded{scope: full_employment}.
    static RE_EMPLOYMENT_ENDED_EN: &str =
        r"Your employment has ended\. There are no regular salary payments scheduled after the final settlement";
    static RE_EMPLOYMENT_ENDED_ID: &str = r"Hubungan kerja Anda telah berakhir\. Tidak ada pembayaran gaji rutin yang dijadwalkan setelah penyelesaian akhir";
    if regex::Regex::new(RE_EMPLOYMENT_ENDED_EN).unwrap().is_match(text)
        || regex::Regex::new(RE_EMPLOYMENT_ENDED_ID).unwrap().is_match(text)
    {
        let mut r = blank_record(RecordType::IncomeEnded);
        r.scope = Some(Scope::FullEmployment);
        r.status_hint = Some(StatusHint::Ended);
        r.category_hint = Some("salary".to_string());
        r.note = Some("stop_projecting_this_salary_stream".to_string());
        return Some(vec![r]);
    }

    // A4 (S6.2, household-partial income end): engine a3ee0f8 added
    // `Fact::IncomeEnded.description` so this can target ONE of two same-category streams.
    // Analyst audit: same shape as user_13 ("Second household income" stream stopped for a
    // missed occurrence) — the selector is that literal description across all 7 users
    // (30 37 119 180 187 · 42 203). The message's stated "remaining confirmed monthly
    // salary" is deliberately NOT applied here (RULES: it doesn't equal the primary
    // stream's settled amount; keep settled history, conflict rule 3).
    static RE_HOUSEHOLD_PARTIAL_EN: &str = r"One household employment record has ended\. The remaining confirmed monthly salary is [A-Z]{2,4} [\d,]+(?:\.\d+)?";
    static RE_HOUSEHOLD_PARTIAL_ID: &str = r"Salah satu sumber pendapatan kerja rumah tangga telah berakhir\. Sisa gaji bulanan yang dikonfirmasi adalah [A-Z]{2,4} [\d,]+(?:\.\d+)?";
    if regex::Regex::new(RE_HOUSEHOLD_PARTIAL_EN).unwrap().is_match(text)
        || regex::Regex::new(RE_HOUSEHOLD_PARTIAL_ID).unwrap().is_match(text)
    {
        let mut r = blank_record(RecordType::IncomeEnded);
        r.status_hint = Some(StatusHint::Ended);
        r.category_hint = Some("salary".to_string());
        r.description_hint = Some("Second household income".to_string());
        r.note = Some("household_partial_end_never_apply_stated_remaining_amount".to_string());
        return Some(vec![r]);
    }

    // A6 (S6.2, board decision.A6_invoice, user-approved via bus #105): an approved invoice
    // payment with an expected settlement date IS counted — a confirmed one-time credit on
    // its settlement date (Fact::OneTimeFlow via RecordType::OneTimeAdjustment, already
    // wired). "The other submitted invoices are still awaiting approval" is filler and
    // never becomes a fact.
    static RE_INVOICE_EN: &str = r"client approved an invoice payment of ([A-Z]{2,4}) ([\d,]+(?:\.\d+)?)\. Settlement is expected on (\d{4}-\d{2}-\d{2})";
    static RE_INVOICE_ID: &str = r"[Kk]lien menyetujui pembayaran faktur sebesar ([A-Z]{2,4}) ([\d,]+(?:\.\d+)?)\. Penyelesaian diperkirakan pada (\d{4}-\d{2}-\d{2})";
    for re in [RE_INVOICE_EN, RE_INVOICE_ID] {
        if let Some(c) = regex::Regex::new(re).unwrap().captures(text) {
            let mut r = blank_record(RecordType::OneTimeAdjustment);
            r.currency = Some(c[1].to_string());
            r.amount = amt(&c[2]);
            r.date = Some(c[3].to_string());
            r.status_hint = Some(StatusHint::Confirmed);
            r.category_hint = Some("invoice_income".to_string());
            r.note = Some("approved_invoice_credit_on_settlement_date".to_string());
            return Some(vec![r]);
        }
    }

    // A5 (S6.2): rent renewal +P% -> ExpenseAmountChange{percent}. Two English phrasings
    // share the same anchor clause; effective stays None (next occurrence after sent_at).
    static RE_RENT_PCT_EN: &str = r"increases monthly rent by (\d+)%";
    static RE_RENT_PCT_ID: &str = r"menaikkan biaya sewa bulanan sebesar (\d+)%";
    for re in [RE_RENT_PCT_EN, RE_RENT_PCT_ID] {
        if let Some(c) = regex::Regex::new(re).unwrap().captures(text) {
            let mut r = blank_record(RecordType::RecurringExpenseChange);
            r.percent = amt(&c[1]);
            r.status_hint = Some(StatusHint::Confirmed);
            r.direction = Some(RecordDirection::Increase);
            r.category_hint = Some("rent".to_string());
            r.note = Some("rent_up_next_payment_apply_to_existing_stream_amount".to_string());
            return Some(vec![r]);
        }
    }

    // A7 (S6.2): "confirmed base salary is X, commission for open deals still pending" —
    // X is consistently ~5/3 of the already-settled base (analyst: applying it as a salary
    // change moves E further from the label). Emit ONLY the commission Unconfirmed record;
    // never touch the salary side for this family.
    static RE_BASE_PLUS_COMMISSION_EN: &str =
        r"confirmed base salary is [A-Z]{2,4} [\d,]+(?:\.\d+)?\. The commission shown for open deals is still pending approval";
    static RE_BASE_PLUS_COMMISSION_ID: &str = r"Gaji pokok yang dikonfirmasi adalah [A-Z]{2,4} [\d,]+(?:\.\d+)?\. Komisi dari transaksi yang masih berjalan belum disetujui";
    if regex::Regex::new(RE_BASE_PLUS_COMMISSION_EN).unwrap().is_match(text)
        || regex::Regex::new(RE_BASE_PLUS_COMMISSION_ID).unwrap().is_match(text)
    {
        let mut r = blank_record(RecordType::PendingUnconfirmedCredit);
        r.category_hint = Some("salary".to_string());
        r.note = Some("commission_open_deals_do_not_count_keep_settled_base".to_string());
        return Some(vec![r]);
    }

    // A8 (S6.2): quarterly bonus pending performance review -> Unconfirmed.
    static RE_BONUS_EN: &str = r"quarterly bonus is still subject to the final performance review";
    static RE_BONUS_ID: &str = r"Bonus kuartalan Anda masih menunggu hasil akhir penilaian kinerja";
    if regex::Regex::new(RE_BONUS_EN).unwrap().is_match(text)
        || regex::Regex::new(RE_BONUS_ID).unwrap().is_match(text)
    {
        return Some(vec![unconfirmed_record("bonus_unconfirmed_do_not_count")]);
    }

    // A9 (S6.2): gig-platform payout still pending (QuickCrew/TaskLoop/RideGrid/WorkDash/
    // ShiftPay/TaskSprint/...) -> Unconfirmed; gig income is never projected.
    static RE_GIG_PAYOUT_EN: &str = r"payout is still pending\. The weekly earnings shown in the \S+ app can change until the payout is closed";
    static RE_GIG_PAYOUT_ID: &str = r"masih tertunda\. Penghasilan mingguan di aplikasi \S+ masih dapat berubah sampai pembayaran diselesaikan";
    if regex::Regex::new(RE_GIG_PAYOUT_EN).unwrap().is_match(text)
        || regex::Regex::new(RE_GIG_PAYOUT_ID).unwrap().is_match(text)
    {
        return Some(vec![unconfirmed_record("gig_payout_pending_do_not_count")]);
    }

    // A10 (S6.2): prize verified/processing, domestic refund initiated, foreign-currency
    // refund still processing -> Unconfirmed; none of these ever count until settled.
    static RE_PRIZE_PROCESSING_EN: &str =
        r"prize claim has been verified and is still in payment processing";
    static RE_PRIZE_PROCESSING_ID: &str = r"[Kk]laim hadiah Anda sudah diverifikasi dan masih dalam proses pembayaran";
    static RE_REFUND_INITIATED_EN: &str =
        r"refund has been initiated but has not reached your account yet";
    static RE_FX_REFUND_PROCESSING_EN: &str = r"foreign-currency refund is still processing";
    static RE_FX_REFUND_PROCESSING_ID: &str =
        r"Tagihan dikenakan dalam mata uang asing|Pengembalian dana sudah diproses, tetapi belum masuk ke rekening Anda";
    for re in [
        RE_PRIZE_PROCESSING_EN,
        RE_PRIZE_PROCESSING_ID,
        RE_REFUND_INITIATED_EN,
        RE_FX_REFUND_PROCESSING_EN,
        RE_FX_REFUND_PROCESSING_ID,
    ] {
        if regex::Regex::new(re).unwrap().is_match(text) {
            return Some(vec![unconfirmed_record("pending_credit_do_not_count")]);
        }
    }

    // N1 (S6.2): "regular salary for the next payroll is X, same payroll includes a
    // one-time arrears adjustment of Y" — X equals the already-settled salary and Y is
    // already a settled row before rd; emitting OneTimeFlow for Y would double-count.
    // Recognized, zero records (never emit the arrears flow for this family).
    static RE_ARREARS_SAME_PAYROLL_EN: &str = r"one-time arrears adjustment of [A-Z]{2,4} [\d,]+(?:\.\d+)?\. Your next payslip will show the regular pay and any one-off adjustment separately";
    static RE_ARREARS_SAME_PAYROLL_ID: &str = r"penyesuaian tunggakan satu kali sebesar [A-Z]{2,4} [\d,]+(?:\.\d+)?\. Slip gaji berikutnya akan menampilkan gaji rutin dan penyesuaian satu kali secara terpisah";
    if regex::Regex::new(RE_ARREARS_SAME_PAYROLL_EN).unwrap().is_match(text)
        || regex::Regex::new(RE_ARREARS_SAME_PAYROLL_ID).unwrap().is_match(text)
    {
        return Some(Vec::new());
    }

    // N2 (S6.2): "salary of X is confirmed for D, receiving bank will convert using the
    // settlement-date rate" — X equals the settled foreign-salary stream; FX is already
    // handled at the settlement date (RULES S2.2). Recognized, zero records.
    static RE_SALARY_FX_EN: &str =
        r"salary of [A-Z]{2,4} [\d,]+(?:\.\d+)?\ is confirmed for \d{4}-\d{2}-\d{2}\. The receiving bank will convert it using the rate applied on the settlement date";
    static RE_SALARY_FX_ID: &str = r"Gaji sebesar [A-Z]{2,4} [\d,]+(?:\.\d+)? dikonfirmasi untuk \d{4}-\d{2}-\d{2}\. Bank penerima akan mengonversinya dengan kurs pada tanggal penyelesaian";
    if regex::Regex::new(RE_SALARY_FX_EN).unwrap().is_match(text)
        || regex::Regex::new(RE_SALARY_FX_ID).unwrap().is_match(text)
    {
        return Some(Vec::new());
    }

    // N3 (S6.2): "previous debit attempt failed, another debit will be attempted" — the
    // linked scheduled retry row already exists and counts on its own; no extra debit.
    static RE_FAILED_RETRY_EN: &str =
        r"previous debit attempt failed\. The bill is still outstanding and another debit will be attempted";
    if regex::Regex::new(RE_FAILED_RETRY_EN).unwrap().is_match(text) {
        return Some(Vec::new());
    }

    // N4 (S6.2, board decision.dup_charges): dispute open, no reversal posted — the pending
    // debit must stay reserved, never be treated as a duplicate to drop.
    static RE_DISPUTE_OPEN_EN: &str =
        r"extra card charge is still being investigated\. A reversal has not been posted to the account yet";
    static RE_DISPUTE_OPEN_ID: &str = r"Sengketa masih terbuka dan dana pembalikan belum tercatat";
    if regex::Regex::new(RE_DISPUTE_OPEN_EN).unwrap().is_match(text)
        || regex::Regex::new(RE_DISPUTE_OPEN_ID).unwrap().is_match(text)
    {
        return Some(Vec::new());
    }

    // N5 (S6.2): own-account-transfer messages with no matching debit/credit pair in that
    // user's events — a distractor. Recognized, zero records (also enforced independently
    // by `event_fact_gate` if a record were ever built for one of these).
    static RE_OWN_TRANSFER_EN: &str = r"matching debit and credit came from a transfer between your two accounts";
    static RE_OWN_TRANSFER_ID: &str =
        r"Debit dan kredit dengan jumlah yang sama berasal dari transfer antara dua rekening Anda";
    if regex::Regex::new(RE_OWN_TRANSFER_EN).unwrap().is_match(text)
        || regex::Regex::new(RE_OWN_TRANSFER_ID).unwrap().is_match(text)
    {
        return Some(Vec::new());
    }

    // N6 (S6.2): two-card-minimums reminder — no card rows in these users' events; a
    // distractor with no forecast effect.
    static RE_TWO_CARD_MINIMUMS_EN: &str =
        r"minimum payments due on two separate card accounts this month";
    if regex::Regex::new(RE_TWO_CARD_MINIMUMS_EN).unwrap().is_match(text) {
        return Some(Vec::new());
    }

    // N7 (S6.2): prize proceeds settled / investment sale proceeds settled / employer
    // reimbursement "not your regular salary" — historical settled one-offs, already
    // excluded from stream detection by RULES S2.1/S3.4. Recognized, zero records.
    static RE_PRIZE_SETTLED_EN: &str = r"prize proceeds have reached your account after withholding";
    static RE_INVESTMENT_SETTLED_EN: &str = r"proceeds from your investment sale have settled in the cash account\. The sale order is complete and there are no remaining proceeds pending";
    static RE_INVESTMENT_SETTLED_ID: &str = r"Hasil penjualan investasi Anda sudah masuk ke rekening tunai\. Perintah penjualan sudah selesai dan tidak ada hasil penjualan yang masih tertunda";
    static RE_REIMBURSEMENT_EN: &str = r"reimbursement for your earlier work expense\. The claim is now closed and no additional reimbursement is scheduled";
    static RE_REIMBURSEMENT_ID: &str = r"penggantian atas biaya kerja Anda sebelumnya\. Klaim sudah ditutup dan tidak ada penggantian tambahan yang dijadwalkan";
    for re in [
        RE_PRIZE_SETTLED_EN,
        RE_INVESTMENT_SETTLED_EN,
        RE_INVESTMENT_SETTLED_ID,
        RE_REIMBURSEMENT_EN,
        RE_REIMBURSEMENT_ID,
    ] {
        if regex::Regex::new(re).unwrap().is_match(text) {
            return Some(Vec::new());
        }
    }

    // N8 (S6.2): portfolio/investment value up or down, unrealized — non-cash, already
    // excluded by the base cash rules. Recognized, zero records.
    static RE_PORTFOLIO_UP_EN: &str = r"portfolio.s displayed market value has increased substantially";
    static RE_PORTFOLIO_DOWN_EN: &str =
        r"displayed value of the investment has fallen\. The holding has not been sold and there has been no cash transaction";
    static RE_PORTFOLIO_DOWN_ID: &str =
        r"Nilai investasi yang ditampilkan telah turun\. Investasi tersebut belum dijual dan tidak ada transaksi tunai";
    for re in [RE_PORTFOLIO_UP_EN, RE_PORTFOLIO_DOWN_EN, RE_PORTFOLIO_DOWN_ID] {
        if regex::Regex::new(re).unwrap().is_match(text) {
            return Some(Vec::new());
        }
    }

    // N9 (S6.2): foreign-currency bill, final home-currency amount set at settlement —
    // RULES S2.2 already applies the settlement-date rate; no separate fact needed.
    static RE_FX_BILL_EN: &str =
        r"bill was charged in a foreign currency\. Your bank will confirm the final home-currency amount when the transaction settles";
    static RE_FX_BILL_ID: &str = r"Tagihan dikenakan dalam mata uang asing\. Bank Anda akan mengonfirmasi jumlah akhir dalam mata uang utama saat transaksi selesai";
    if regex::Regex::new(RE_FX_BILL_EN).unwrap().is_match(text)
        || regex::Regex::new(RE_FX_BILL_ID).unwrap().is_match(text)
    {
        return Some(Vec::new());
    }

    // N10 (S6.2): advance-fee prize scam — injection-guard canary, already tested for
    // messages 67/142. RejectedInstruction, never a fact.
    static RE_PRIZE_SCAM_EN: &str =
        r"selected for a cash prize\. Pay the release charge today to receive the funds immediately";
    static RE_PRIZE_SCAM_ID: &str =
        r"terpilih untuk menerima hadiah uang tunai\. Bayar biaya pencairan hari ini agar dana segera diterima";
    if regex::Regex::new(RE_PRIZE_SCAM_EN).unwrap().is_match(text)
        || regex::Regex::new(RE_PRIZE_SCAM_ID).unwrap().is_match(text)
    {
        let mut r = blank_record(RecordType::RejectedInstruction);
        r.note = Some("advance_fee_prize_scam_no_verifiable_fact".to_string());
        return Some(vec![r]);
    }

    // N11 (S6.2): receipt pointers ("payment received on D, receipt has the final
    // amount/paid in CUR on D") — the amount comes from the linked image, never the
    // message text. Recognized, zero records.
    static RE_RECEIPT_POINTER_EN: &str =
        r"payment was received on \d{1,2} \w+ \d{4}\. The receipt has the final \S+ amount and the original due date";
    static RE_RECEIPT_POINTER_EN2: &str =
        r"confirmed that the .+ order was paid in \S+ on \d{1,2} \w+ \d{4}\. The receipt has the final amount";
    if regex::Regex::new(RE_RECEIPT_POINTER_EN).unwrap().is_match(text)
        || regex::Regex::new(RE_RECEIPT_POINTER_EN2).unwrap().is_match(text)
    {
        return Some(Vec::new());
    }

    // N12 (S6.2): message_02 payslip-composition note (user_03) — no amount/date stated;
    // the fact is in image_01. Recognized, zero records (kept minimal rather than a
    // null-filled SalaryChange, since to_evidence() would drop that anyway).
    static RE_PAYSLIP_COMPOSITION_ID: &str = r"Gaji rutin untuk penggajian berikutnya sudah dikonfirmasi\. Slip gaji berikutnya akan menampilkan gaji rutin dan penyesuaian satu kali secara terpisah";
    if regex::Regex::new(RE_PAYSLIP_COMPOSITION_ID).unwrap().is_match(text) {
        return Some(Vec::new());
    }

    None
}

fn unconfirmed_record(note: &str) -> MessageRecord {
    let mut r = blank_record(RecordType::PendingUnconfirmedCredit);
    r.status_hint = Some(StatusHint::Pending);
    r.note = Some(note.to_string());
    r
}

fn salary_change_record(c: &regex::Captures, direction: &str, note: &str) -> MessageRecord {
    let mut r = blank_record(RecordType::SalaryChange);
    r.currency = Some(c[1].to_string());
    r.amount = amt(&c[2]);
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
    r.amount = amt(&c[2]);
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
    r.amount = amt(&c[2]);
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
        let Some(records) = parse_known_skeleton(&message.message_text, message.sent_at.date_naive())
        else {
            continue;
        };
        for (idx, record) in records.iter().enumerate() {
            if let Some(evidence) = to_evidence(message, idx, record, home_currency) {
                out.push(evidence);
            }
        }
    }
    out
}

/// Batch every message in `batch` (already filtered to relevant, unresolved-skeleton
/// messages for one user — PLAN.md §3 batching lever: one call per user batch, never one
/// call per message) into a single LLM call, per `code/prompts/message_extraction.v1.md`.
/// Goes through `HfClient`'s own §2.11 disk cache (content hash + model id + revision +
/// prompt version).
pub fn extract_batch(
    client: &HfClient,
    cold: bool,
    prompt: &PromptSet,
    decoding: &DecodingConfig,
    candidate: &CandidateConfig,
    batch: &[&Message],
) -> anyhow::Result<HashMap<String, Vec<MessageRecord>>> {
    let block: String = batch
        .iter()
        .map(|m| format!("[{}] {}\n", m.message_id, m.message_text))
        .collect();
    let user_content = prompt.user_template.replace("{{MESSAGES_BLOCK}}", &block);
    let call = ModelCall {
        model_id: candidate.id.clone(),
        provider: candidate.provider.clone(),
        model_revision: candidate.model_revision.clone(),
        prompt_version: prompt.version.clone(),
        system_prompt: prompt.system_prompt.clone(),
        user_content: vec![ContentPart::Text(user_content)],
        temperature: decoding.temperature,
        seed: decoding.seed,
        max_tokens: decoding.max_tokens_llm,
        // ml-engineer's step-5 harness (blocker.msg86_json_array_mismatch): this call's
        // contract is a top-level JSON *array* (one entry per batched message), but
        // `response_format: json_object` requires a top-level *object* on every provider
        // that implements it -- SEA-LION and its DeepSeek fallback both collapsed a batch
        // to one bare `{message_id, records}` object instead of the array this parses
        // below. That breaks any batch, including production's single-message batch=1 call
        // for msg_86. Never force structured-output mode for this array-shaped contract;
        // the prompt's own "strict JSON only" instruction is what every other array-shaped
        // path here already relies on.
        json_response: false,
        json_schema: None,
    };
    let response =
        if cold { client.chat_completion_cold(&call)? } else { client.chat_completion(&call)? };
    let value = parse_json_reply(&response.raw_text)?;
    let replies: Vec<MessageRecordsReply> = serde_json::from_value(value)?;
    Ok(replies.into_iter().map(|r| (r.message_id, r.records)).collect())
}

/// Every message in `messages` whose skeleton `parse_known_skeleton` does not recognize
/// (e.g. message_86), run through the LLM path in one batch call and converted to
/// evidence. Rejects any record whose claimed amount is not literally present in its own
/// message text (`grounding::amount_grounded`) — the LLM path is the one place a
/// hallucinated number could otherwise slip through; the deterministic parser's captures
/// are already grounded by construction. Returns an empty vec with zero calls when every
/// message in `messages` is already deterministically covered.
pub fn llm_evidence(
    client: &HfClient,
    cold: bool,
    prompt: &PromptSet,
    decoding: &DecodingConfig,
    candidate: &CandidateConfig,
    // User-requested backup frontier model (board decision, PLAN.md Phase 2d): tried for
    // this same batch when `candidate`'s call errors or its reply doesn't parse into valid
    // records — a fresh call, not a retry of the failed one.
    fallback: Option<&CandidateConfig>,
    messages: &[&Message],
    home_currency: &str,
) -> anyhow::Result<Vec<EvidenceRecord>> {
    let unresolved: Vec<&Message> = messages
        .iter()
        .copied()
        .filter(|m| parse_known_skeleton(&m.message_text, m.sent_at.date_naive()).is_none())
        .collect();
    if unresolved.is_empty() {
        return Ok(Vec::new());
    }
    let by_id = match extract_batch(client, cold, prompt, decoding, candidate, &unresolved) {
        Ok(by_id) => by_id,
        Err(e) => {
            eprintln!("llm: batch via {} failed: {e:#}", candidate.id);
            let Some(fallback) = fallback else { return Ok(Vec::new()) };
            extract_batch(client, cold, prompt, decoding, fallback, &unresolved)?
        }
    };
    let mut out = Vec::new();
    for message in unresolved {
        let Some(records) = by_id.get(&message.message_id) else { continue };
        for (idx, record) in records.iter().enumerate() {
            if let Some(amount) = record.amount {
                if !amount_grounded(amount, &message.message_text) {
                    eprintln!(
                        "grounding: rejected {} record {idx}: claimed amount {amount} not found in source text",
                        message.message_id
                    );
                    continue;
                }
            }
            if let Some(evidence) = to_evidence(message, idx, record, home_currency) {
                out.push(evidence);
            }
        }
    }
    Ok(out)
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
        RecordType::SalaryChange => match (record.amount, record.date.as_deref()) {
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
        },
        // engine#63/8cc70f3: first salary at a new employer, or pay resuming after a
        // pause, is Fact::IncomeStarts — both amount and a first_date are required
        // (never guessed); a record missing either is dropped.
        RecordType::SalaryFirstConfirmed => Fact::IncomeStarts {
            category: category("salary"),
            amount: Money::from_f64(record.amount?),
            currency: currency(),
            first_date: parse_date(record.date.as_deref()?)?,
        },
        RecordType::IncomeEnded => Fact::IncomeEnded {
            category: category("salary"),
            effective: record
                .date
                .as_deref()
                .and_then(parse_date)
                .unwrap_or_else(|| observed_at.date()),
            // engine a3ee0f8 (RULES S6.2 A4, lead-approved additive field): None ends every
            // stream of the category (A2/A3 full/seasonal employment end); Some(desc) ends
            // only the matching secondary stream (A4 household-partial end) — never the
            // message's stated remaining amount.
            description: record.description_hint.clone(),
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
                // engine#63/8cc70f3: effective is Option<NaiveDate> now — None means "the
                // stream's next occurrence after the evidence was sent" (e.g. "the next
                // rent payment"), which is exactly what a message with no explicit date
                // means. Never invent a calendar date to fill this in.
                effective: record.date.as_deref().and_then(parse_date),
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
        let records = parse_known_skeleton("Rincian penggajian Anda di Cobalt Systems telah berubah. Gaji bulanan Anda naik menjadi IDR 42750000. Perubahan ini berlaku mulai 2025-08-15. Jumlah yang diperbarui akan terlihat pada slip gaji berikutnya. Ref payroll EMP-0001.", NaiveDate::from_ymd_opt(2025, 7, 29).unwrap()).expect("message_01 skeleton");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].record_type, RecordType::SalaryChange);
        assert_eq!(records[0].amount, Some(42_750_000.0));
        assert_eq!(records[0].currency.as_deref(), Some("IDR"));
        assert_eq!(records[0].date.as_deref(), Some("2025-08-15"));

        // user_06 / message_04: temporary 1,037.52 EUR, no date.
        let records = parse_known_skeleton("Here\u{2019}s the latest payroll information from Northstar Labs. Your temporary monthly pay is EUR 1037.52. The reduced amount continues for the next payroll. This is the amount currently scheduled for the affected pay cycle. Payroll ref EMP-0004.", NaiveDate::from_ymd_opt(2025, 12, 28).unwrap()).expect("message_04 skeleton");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].amount, Some(1037.52));
        assert_eq!(records[0].currency.as_deref(), Some("EUR"));
        assert_eq!(records[0].date, None);

        // user_07 / message_05: pay date moves to 2024-09-23, no amount.
        let records = parse_known_skeleton("BrightPath Media has updated your payroll record. Your confirmed salary is now expected on 2024-09-23. This replaces the payroll date shown in the earlier update. Please use the revised date for anything you normally pay around payday. Payroll ref EMP-0005.", NaiveDate::from_ymd_opt(2024, 8, 29).unwrap()).expect("message_05 skeleton");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].amount, None);
        assert_eq!(records[0].date.as_deref(), Some("2024-09-23"));

        // user_08 / message_06: reduced to 1,422.85 EUR, unpaid leave. No date stated, but
        // analyst RULES.md S6.1 requires IncomeAmountChange (not NextIncomeAmount) effective
        // from the next pay date, so the record is stamped with sent_at as that anchor.
        let records = parse_known_skeleton("Hi, Greenfield Foods payroll here. Your next salary is reduced to EUR 1422.85. The adjustment is due to approved unpaid leave. The adjustment will be visible on your next payslip. Payroll ref EMP-0006.", NaiveDate::from_ymd_opt(2025, 2, 6).unwrap()).expect("message_06 skeleton");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].amount, Some(1422.85));
        assert_eq!(records[0].currency.as_deref(), Some("EUR"));
        assert_eq!(records[0].date.as_deref(), Some("2025-02-06"));

        // user_14 / message_10: salary 2,717 EUR resumes 2025-08-15, PLUS a childcare
        // record with no amount (must be dropped downstream, not invented).
        let records = parse_known_skeleton("Here\u{2019}s the latest payroll information from HarborWorks. Regular salary of EUR 2717 resumes on 2025-08-15. A new recurring childcare payment begins in the same month. The updated pay and deductions will appear from the next cycle. Payroll ref EMP-0010.", NaiveDate::from_ymd_opt(2025, 7, 27).unwrap()).expect("message_10 skeleton");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].amount, Some(2717.0));
        assert_eq!(records[0].date.as_deref(), Some("2025-08-15"));
        assert_eq!(records[1].record_type, RecordType::RecurringExpenseChange);
        assert_eq!(records[1].amount, None);
        let message_10 = msg("message_10", "user_14", "2025-07-27T09:30:00Z", "employer", "irrelevant");
        assert!(to_evidence(&message_10, 1, &records[1], "EUR").is_none()); // no invented amount

        // user_15 / message_11: first salary 1,661 EUR, confirmed credit date 2026-01-15.
        let records = parse_known_skeleton("A quick update from the payroll team at Riverline Retail. Your first salary will be EUR 1661. The confirmed credit date is 2026-01-15. The money will appear after the bank posts the credit. Payroll ref EMP-0011.", NaiveDate::from_ymd_opt(2026, 1, 3).unwrap()).expect("message_11 skeleton");
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
                | Fact::IncomeStarts { .. } // message_11: first salary -> engine#63
        )));
    }

    /// A4 (analyst RULES.md S6.2, engine a3ee0f8): household-partial income-end messages
    /// (e.g. message_42/user_58) target only the secondary stream via
    /// `Fact::IncomeEnded.description`, and never apply the message's stated "remaining
    /// confirmed monthly salary" figure.
    #[test]
    fn household_partial_end_targets_secondary_stream_only() {
        let message = msg(
            "message_42",
            "user_58",
            "2024-11-29T09:30:00Z",
            "employer",
            "Rincian penggajian Anda di Cedar Health telah berubah. Salah satu sumber pendapatan kerja rumah tangga telah berakhir. Sisa gaji bulanan yang dikonfirmasi adalah IDR 25840000. Pendapatan yang sudah berakhir harus dikeluarkan dari perkiraan berikutnya. Ref payroll EMP-0042.",
        );
        let records = parse_known_skeleton(&message.message_text, message.sent_at.date_naive())
            .expect("message_42 skeleton");
        assert_eq!(records.len(), 1);
        let evidence = to_evidence(&message, 0, &records[0], "IDR").expect("expected a fact");
        match evidence.fact {
            Fact::IncomeEnded { category, description, .. } => {
                assert_eq!(category, "salary");
                assert_eq!(description.as_deref(), Some("Second household income"));
            }
            other => panic!("expected IncomeEnded, got {other:?}"),
        }
    }

    /// A6 (board decision.A6_invoice): an approved invoice payment IS counted as a
    /// `Fact::OneTimeFlow` credit on its settlement date.
    #[test]
    fn approved_invoice_payment_counts_as_one_time_credit() {
        let message = msg(
            "message_24",
            "user_34",
            "2024-11-23T09:30:00Z",
            "service_provider",
            "Hi, InvoiceLane here. The client approved an invoice payment of INR 196000. Settlement is expected on 2024-12-15; the other submitted invoices are still awaiting approval. Only invoices marked as confirmed should be included in the upcoming payout. Case ref SER-0024.",
        );
        let records = parse_known_skeleton(&message.message_text, message.sent_at.date_naive())
            .expect("message_24 skeleton");
        assert_eq!(records.len(), 1);
        let evidence = to_evidence(&message, 0, &records[0], "INR").expect("expected a fact");
        match evidence.fact {
            Fact::OneTimeFlow { direction, amount, date, .. } => {
                assert_eq!(direction, Direction::Credit);
                assert_eq!(amount, Money::from_f64(196000.0));
                assert_eq!(date, NaiveDate::from_ymd_opt(2024, 12, 15).unwrap());
            }
            other => panic!("expected OneTimeFlow, got {other:?}"),
        }
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
            description_hint: None,
            note: Some("rent_up_12pct_next_payment_apply_to_existing_stream_amount".to_string()),
        };
        let evidence = to_evidence(&message, 0, &record, "INR").expect("expected a fact");
        match evidence.fact {
            Fact::ExpenseAmountChange { category, amount, percent, currency, effective } => {
                assert_eq!(category, "rent");
                assert_eq!(amount, None);
                assert_eq!(percent, Some(12.0));
                assert_eq!(currency, None);
                // engine#63/8cc70f3: no explicit date -> None (next stream occurrence
                // after sent_at, computed by engine), not the message's own sent_at date.
                assert_eq!(effective, None);
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
            description_hint: None,
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
                description_hint: None,
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
            description_hint: None,
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
            description_hint: None,
            note: None,
        };
        assert!(to_evidence(&message, 0, &record, "EUR").is_some());
    }

    /// TEMPORARY duplicate of `tests/llm_generalization_fixtures.rs` (ml-engineer step 5,
    /// lead request 2026-09-12). This copy runs under `--lib` to verify
    /// `docs/llm_generalization_fixtures.json` is correct today; delete it once the
    /// integration test is confirmed to run the same check for real.
    #[test]
    fn llm_generalization_fixtures_are_internally_consistent() {
        use std::fs;
        let text = fs::read_to_string(std::path::Path::new("../docs/llm_generalization_fixtures.json"))
            .expect("docs/llm_generalization_fixtures.json should exist");
        let root: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        let fixtures = root["fixtures"].as_array().expect("fixtures array");
        assert!(fixtures.len() >= 18, "expected ~20 fixtures, found {}", fixtures.len());

        for fixture in fixtures {
            let id = fixture["id"].as_str().unwrap();
            let text = fixture["text"].as_str().unwrap();
            assert!(
                parse_known_skeleton(text, NaiveDate::from_ymd_opt(2027, 1, 1).unwrap()).is_none(),
                "{id} matched a known deterministic skeleton -- reword it"
            );

            let record: MessageRecord = serde_json::from_value(fixture["expected_llm_record"].clone())
                .unwrap_or_else(|e| panic!("{id}: expected_llm_record should deserialize: {e}"));
            let message = Message {
                message_id: "llm_gen_fixture".to_string(),
                user_id: "user_fixture".to_string(),
                request_id: None,
                related_event_id: None,
                sent_at: "2027-01-01T00:00:00Z".parse().unwrap(),
                source_type: "test_fixture".to_string(),
                message_text: text.to_string(),
            };
            let home_currency = record.currency.clone().unwrap_or_else(|| "USD".to_string());
            let evidence = to_evidence(&message, 0, &record, &home_currency);
            let expected_facts = fixture["expected_facts"].as_array().unwrap();

            if expected_facts.is_empty() {
                assert!(
                    evidence.is_none(),
                    "{id}: expected no fact ({}), got {:?}",
                    fixture["expected_no_fact_reason"].as_str().unwrap_or(""),
                    evidence.map(|e| e.fact)
                );
                continue;
            }
            let evidence = evidence.unwrap_or_else(|| panic!("{id}: expected a fact, got None"));
            let expected = &expected_facts[0];
            let kind = expected["fact"].as_str().unwrap();
            let amt = |key: &str| expected[key].as_f64().map(Money::from_f64);
            let date = |key: &str| {
                expected[key].as_str().and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
            };
            let txt = |key: &str| expected[key].as_str().map(str::to_string);

            match (kind, evidence.fact) {
                ("IncomeAmountChange", Fact::IncomeAmountChange { category, amount, currency, effective }) => {
                    assert_eq!(category, expected["category"].as_str().unwrap(), "{id} category");
                    assert_eq!(Some(amount), amt("amount"), "{id} amount");
                    assert_eq!(currency, expected["currency"].as_str().unwrap(), "{id} currency");
                    assert_eq!(Some(effective), date("effective"), "{id} effective");
                }
                ("IncomeDateMoved", Fact::IncomeDateMoved { category, new_date }) => {
                    assert_eq!(category, expected["category"].as_str().unwrap(), "{id} category");
                    assert_eq!(Some(new_date), date("new_date"), "{id} new_date");
                }
                ("IncomeEnded", Fact::IncomeEnded { category, effective, description }) => {
                    assert_eq!(category, expected["category"].as_str().unwrap(), "{id} category");
                    assert_eq!(Some(effective), date("effective"), "{id} effective");
                    assert_eq!(description, txt("description"), "{id} description");
                }
                ("IncomeStarts", Fact::IncomeStarts { category, amount, currency, first_date }) => {
                    assert_eq!(category, expected["category"].as_str().unwrap(), "{id} category");
                    assert_eq!(Some(amount), amt("amount"), "{id} amount");
                    assert_eq!(currency, expected["currency"].as_str().unwrap(), "{id} currency");
                    assert_eq!(Some(first_date), date("first_date"), "{id} first_date");
                }
                ("ExpenseAmountChange", Fact::ExpenseAmountChange { category, amount, percent, currency, effective }) => {
                    assert_eq!(category, expected["category"].as_str().unwrap(), "{id} category");
                    assert_eq!(amount, expected["amount"].as_f64().map(Money::from_f64), "{id} amount");
                    assert_eq!(percent, expected["percent"].as_f64(), "{id} percent");
                    assert_eq!(currency, txt("currency"), "{id} currency");
                    assert_eq!(effective, expected["effective"].as_str().and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()), "{id} effective");
                }
                ("OneTimeFlow", Fact::OneTimeFlow { direction, category, amount, currency, date: d }) => {
                    assert_eq!(direction, if expected["direction"] == "Credit" { Direction::Credit } else { Direction::Debit }, "{id} direction");
                    assert_eq!(category, expected["category"].as_str().unwrap(), "{id} category");
                    assert_eq!(Some(amount), amt("amount"), "{id} amount");
                    assert_eq!(currency, expected["currency"].as_str().unwrap(), "{id} currency");
                    assert_eq!(Some(d), date("date"), "{id} date");
                }
                ("Unconfirmed", Fact::Unconfirmed { category, amount, currency }) => {
                    assert_eq!(category, expected["category"].as_str().unwrap(), "{id} category");
                    assert_eq!(amount, expected["amount"].as_f64().map(Money::from_f64), "{id} amount");
                    assert_eq!(currency, txt("currency"), "{id} currency");
                }
                (kind, other) => panic!("{id}: expected {kind:?}, got {other:?}"),
            }
        }
    }
}
