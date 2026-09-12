use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use buyorwait::engine::session::Session;
use buyorwait::engine::types::{PaymentOption, RateTable, RequestSpec};
use buyorwait::engine::Rules;
use buyorwait::evaluation::{Finding, ForecastSeries, InvariantViolation, Invariants};
use buyorwait::extract::{messages::deterministic_evidence, retrieval};
use buyorwait::model;
use buyorwait::store::cache::DiskCache;
use buyorwait::store::processed::ProcessedStore;

fn main() -> anyhow::Result<ExitCode> {
    let args: Vec<String> = env::args().collect();

    // `buyorwait verify <args>` delegates straight to the verifier's own CLI (validate/score/
    // selftest) and exits with its code, without touching the main pipeline below.
    if args.get(1).map(String::as_str) == Some("verify") {
        let code = buyorwait::evaluation::cli(&args[2..])?;
        return Ok(ExitCode::from(code.clamp(0, 255) as u8));
    }

    let mut cold = false;
    let mut requests_path = PathBuf::from("../dataset/requests.csv");
    let mut out_path = PathBuf::from("../output.csv");
    let mut it = args[1..].iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--cold" => cold = true,
            "--requests" => {
                requests_path = it
                    .next()
                    .map(PathBuf::from)
                    .ok_or_else(|| anyhow::anyhow!("--requests needs a value"))?;
            }
            "--out" => {
                out_path = it
                    .next()
                    .map(PathBuf::from)
                    .ok_or_else(|| anyhow::anyhow!("--out needs a value"))?;
            }
            other => anyhow::bail!("unexpected argument {other:?}"),
        }
    }

    let dataset_dir = Path::new("../dataset");
    let profiles = model::load_financial_profiles(dataset_dir.join("financial_profiles.csv"))?;
    let events = model::load_financial_events(dataset_dir.join("financial_events.csv"))?;
    let rates = model::load_exchange_rates(dataset_dir.join("exchange_rates.csv"))?;
    let requests = model::load_requests(&requests_path)?;
    let payment_options =
        model::load_request_payment_options(dataset_dir.join("request_payment_options.csv"))?;
    let messages = model::load_messages(dataset_dir.join("messages.csv"))?;
    let images = model::load_images(dataset_dir.join("images.csv"))?;

    eprintln!(
        "loaded {} profiles, {} events, {} rates, {} requests, {} payment options, {} messages, {} images (cold={})",
        profiles.len(),
        events.len(),
        rates.len(),
        requests.len(),
        payment_options.len(),
        messages.len(),
        images.len(),
        cold
    );

    // Processed-data store (PLAN.md §2.11): model-call cache plus persisted preprocessing
    // outputs. `--cold` wipes both so the run starts from empty and the usage report
    // reflects real calls; a warm rerun reuses them and must produce byte-identical output.
    let store_root = Path::new("store");
    let model_cache = if cold {
        DiskCache::open_cold(store_root.join("cache"))?
    } else {
        DiskCache::open(store_root.join("cache"))?
    };
    let processed_store = if cold {
        ProcessedStore::open_cold(store_root.join("processed"))?
    } else {
        ProcessedStore::open(store_root.join("processed"))?
    };
    processed_store.save("_meta", "last_run", &serde_json::json!({ "cold": cold }))?;

    // TODO(extraction/ml-engineer): once the bake-off picks a VLM/LLM, run image and
    // free-text message extraction through `model_cache` here too. For now every request
    // is decided from the ledger plus `extract::messages::deterministic_evidence` (the
    // zero-token skeleton parser) only -- the baseline build has no model calls.
    let rates = Arc::new(RateTable::from_model(&rates));
    let invariants = Invariants::load(dataset_dir, &requests_path)?;

    let mut rows = Vec::with_capacity(requests.len());
    let mut violations = 0usize;
    let mut engine_errors = 0usize;
    for request in &requests {
        let row = match decide_one(
            request,
            &profiles,
            &events,
            &messages,
            &payment_options,
            rates.clone(),
            &invariants,
        ) {
            Ok((row, Ok(warnings))) => {
                for w in warnings {
                    eprintln!("WARN {w}");
                }
                row
            }
            Ok((row, Err(violation))) => {
                violations += 1;
                eprintln!("INVARIANT_VIOLATION {} {violation}", request.request_id);
                row
            }
            Err(e) => {
                engine_errors += 1;
                eprintln!("ENGINE_ERROR {} {e:#}", request.request_id);
                fallback_row(&request.request_id, &e.to_string())
            }
        };
        rows.push(row);
    }
    eprintln!(
        "decided {} requests: {} invariant violations, {} engine errors",
        rows.len(),
        violations,
        engine_errors
    );

    let mut writer = csv::Writer::from_path(&out_path)?;
    for row in &rows {
        writer.serialize(row)?;
    }
    writer.flush()?;

    fs::create_dir_all("evaluation")?;
    fs::write(
        "evaluation/usage_report.md",
        "# Usage report\n\nPending: populated by ml-engineer from the final full-dataset run.\n",
    )?;

    eprintln!("model cache stats: {:?}", model_cache.stats());

    Ok(ExitCode::SUCCESS)
}

/// One user's session, bound and decided for one request (PLAN.md §2.5: one session per
/// user, evidence and `decide` never take a user id). Evidence is the deterministic,
/// zero-token skeleton parser only -- no model calls in this baseline build.
fn decide_one(
    request: &model::Request,
    profiles: &[model::FinancialProfile],
    events: &[model::FinancialEvent],
    messages: &[model::Message],
    payment_options: &[model::RequestPaymentOption],
    rates: Arc<RateTable>,
    invariants: &Invariants,
) -> anyhow::Result<(model::OutputRow, Result<Vec<Finding>, InvariantViolation>)> {
    let mut session = Session::from_model(&request.user_id, profiles, events, rates, Rules::default())?;

    let evidence = retrieval::for_user(&request.user_id, request.request_date, messages, &[]);
    let home_currency = session.profile().home_currency.clone();
    let facts = deterministic_evidence(&evidence.messages, &home_currency);
    session.apply_evidence(facts);

    let spec = RequestSpec::from_model(request);
    let options: Vec<PaymentOption> = payment_options
        .iter()
        .filter(|o| o.request_id == request.request_id)
        .map(PaymentOption::from_model)
        .collect::<anyhow::Result<_>>()?;

    let decision = session.decide(&request.request_id, request.request_date, &spec, &options)?;

    let baseline = decision.baseline_series();
    let baseline_low = decision.baseline_low_series();
    let with_changes = decision.with_changes_series();
    let with_changes_low = decision.with_changes_low_series();
    let forecast = ForecastSeries {
        start: request.request_date,
        minimum: decision.minimum_f64(),
        baseline: &baseline,
        baseline_low: Some(&baseline_low),
        with_changes: with_changes.as_deref(),
        with_changes_low: with_changes_low.as_deref(),
    };
    let outcome = invariants.assert_row(&decision.row, &forecast);
    Ok((decision.row, outcome))
}

/// Written only when the engine itself errors (e.g. a malformed upstream row) -- safe by
/// construction (no payment recommended) and loud (the error is in the explanation and on
/// stderr), so the run still produces the required one row per request.
fn fallback_row(request_id: &str, error: &str) -> model::OutputRow {
    model::OutputRow {
        request_id: request_id.to_string(),
        amount_safe_to_pay: "0".to_string(),
        affordability_status: "not_affordable".to_string(),
        recommended_payment_method: "not_recommended".to_string(),
        payment_plan: "none".to_string(),
        earliest_date_for_full_payment: String::new(),
        spending_changes_needed: "none".to_string(),
        decision_explanation: format!("Engine error, treated as not affordable pending a fix: {error}"),
    }
}
