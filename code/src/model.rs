use std::path::Path;

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub struct FinancialProfile {
    pub user_id: String,
    pub home_currency: String,
    pub current_available_balance: f64,
    pub minimum_balance_to_keep: f64,
    pub financial_priorities: String,
    pub expense_categories_to_protect: String,
    pub expense_categories_user_is_willing_to_reduce: String,
    pub expense_categories_user_is_willing_to_stop: String,
    pub payment_methods_user_will_consider: String,
    pub max_installment_months: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FinancialEvent {
    pub event_id: String,
    pub user_id: String,
    pub event_type: String,
    pub description: String,
    pub category: String,
    pub direction: String,
    pub amount: Option<f64>,
    pub currency: String,
    pub event_date: NaiveDate,
    pub settlement_date: Option<NaiveDate>,
    pub status: String,
    pub linked_event_id: Option<String>,
    pub flexibility: String,
    pub minimum_allowed_amount: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExchangeRate {
    pub rate_date: NaiveDate,
    pub from_currency: String,
    pub to_currency: String,
    pub rate: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    pub request_id: String,
    pub user_id: String,
    pub request_date: NaiveDate,
    pub request_type: String,
    pub requested_amount: f64,
    pub desired_completion_date: NaiveDate,
    pub allows_partial_payment: bool,
    pub request_text: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SampleRequest {
    pub request_id: String,
    pub user_id: String,
    pub request_date: NaiveDate,
    pub request_type: String,
    pub requested_amount: f64,
    pub desired_completion_date: NaiveDate,
    pub allows_partial_payment: bool,
    pub request_text: String,
    pub amount_safe_to_pay: f64,
    pub affordability_status: String,
    pub recommended_payment_method: String,
    pub payment_plan: String,
    pub earliest_date_for_full_payment: Option<NaiveDate>,
    pub spending_changes_needed: String,
    pub decision_explanation: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RequestPaymentOption {
    pub payment_option_id: String,
    pub request_id: String,
    pub payment_method: String,
    pub payment_amount: f64,
    pub number_of_payments: u32,
    pub first_payment_date: NaiveDate,
    pub payment_frequency_days: Option<u32>,
    pub financing_fee: f64,
    pub total_payable_amount: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub message_id: String,
    pub user_id: String,
    pub request_id: Option<String>,
    pub related_event_id: Option<String>,
    pub sent_at: DateTime<Utc>,
    pub source_type: String,
    pub message_text: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Image {
    pub image_id: String,
    pub user_id: String,
    pub request_id: String,
    pub related_event_id: String,
}

/// One row of the required submission output (`output.csv`).
/// Fields are kept as display-ready strings: formatting/rounding is the
/// engine's responsibility, not this shared model.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OutputRow {
    pub request_id: String,
    pub amount_safe_to_pay: String,
    pub affordability_status: String,
    pub recommended_payment_method: String,
    pub payment_plan: String,
    pub earliest_date_for_full_payment: String,
    pub spending_changes_needed: String,
    pub decision_explanation: String,
}

fn load_csv<T: for<'de> Deserialize<'de>>(path: impl AsRef<Path>) -> anyhow::Result<Vec<T>> {
    let path = path.as_ref();
    let mut reader = csv::Reader::from_path(path)
        .map_err(|e| anyhow::anyhow!("failed to open {}: {e}", path.display()))?;
    let mut rows = Vec::new();
    for result in reader.deserialize() {
        let row: T =
            result.map_err(|e| anyhow::anyhow!("failed to parse {}: {e}", path.display()))?;
        rows.push(row);
    }
    Ok(rows)
}

pub fn load_financial_profiles(path: impl AsRef<Path>) -> anyhow::Result<Vec<FinancialProfile>> {
    load_csv(path)
}

pub fn load_financial_events(path: impl AsRef<Path>) -> anyhow::Result<Vec<FinancialEvent>> {
    load_csv(path)
}

pub fn load_exchange_rates(path: impl AsRef<Path>) -> anyhow::Result<Vec<ExchangeRate>> {
    load_csv(path)
}

pub fn load_requests(path: impl AsRef<Path>) -> anyhow::Result<Vec<Request>> {
    load_csv(path)
}

pub fn load_sample_requests(path: impl AsRef<Path>) -> anyhow::Result<Vec<SampleRequest>> {
    load_csv(path)
}

pub fn load_request_payment_options(
    path: impl AsRef<Path>,
) -> anyhow::Result<Vec<RequestPaymentOption>> {
    load_csv(path)
}

pub fn load_messages(path: impl AsRef<Path>) -> anyhow::Result<Vec<Message>> {
    load_csv(path)
}

pub fn load_images(path: impl AsRef<Path>) -> anyhow::Result<Vec<Image>> {
    load_csv(path)
}

pub fn load_output_template(path: impl AsRef<Path>) -> anyhow::Result<Vec<OutputRow>> {
    load_csv(path)
}
