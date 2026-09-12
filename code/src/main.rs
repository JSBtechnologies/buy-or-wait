use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use buyorwait::engine::session::Session;
use buyorwait::engine::types::{Event, PaymentOption, RateTable, RequestSpec};
use buyorwait::engine::Rules;
use buyorwait::evaluation::{Finding, ForecastSeries, InvariantViolation, Invariants};
use buyorwait::extract::messages::{deterministic_evidence, llm_evidence};
use buyorwait::extract::model_config::ModelsConfig;
use buyorwait::extract::prompts::{self, PromptSet};
use buyorwait::extract::{images, retrieval};
use buyorwait::hf::{self, HfClient};
use buyorwait::model;
use buyorwait::store::cache::DiskCache;
use buyorwait::store::processed::ProcessedStore;
use serde::Deserialize;

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

    // Which models (if any) to call live this run. With `[selected]` absent from
    // config/models.toml (the state before the user picks, PLAN.md Phase 2d),
    // vlm_primary()/llm_primary() are both None and the whole model path below stays
    // inactive -- this run is then behaviorally identical to the deterministic-evidence-only
    // baseline (PLAN.md Phase 3: byte-identical output is required either way).
    let models_config = ModelsConfig::load(Path::new("config/models.toml"))?;
    let use_models = models_config.vlm_primary().is_some() || models_config.llm_primary().is_some();

    // ml-engineer's HfClient owns the actual HF router calls and their own on-disk cache
    // (PLAN.md §2.11); `--cold` wipes it the same way as `store/cache` and `store/processed`
    // above, so a cold run never reads a prior run's cached responses, and a later warm
    // rerun reuses whatever this run writes (determinism + cache-hit-rate check, §3 Phase 3).
    // Only constructed when a model is actually selected, so HF_TOKEN is never required to
    // run the baseline.
    let model_cache_dir = store_root.join("model_cache");
    if cold && model_cache_dir.exists() {
        std::fs::remove_dir_all(&model_cache_dir)?;
    }
    let hf_client: Option<HfClient> = if use_models {
        match HfClient::with_cache_dir(&model_cache_dir) {
            Ok(client) => Some(client),
            Err(e) => {
                eprintln!("model selected in config/models.toml but no live calls this run ({e:#})");
                None
            }
        }
    } else {
        None
    };
    let image_prompt = if models_config.vlm_primary().is_some() && hf_client.is_some() {
        Some(prompts::load(Path::new("prompts/image_transcription.v1.md"), "User prompt template")?)
    } else {
        None
    };
    let message_prompt = if models_config.llm_primary().is_some() && hf_client.is_some() {
        Some(prompts::load(Path::new("prompts/message_extraction.v1.md"), "User prompt template")?)
    } else {
        None
    };
    let model_ctx = ModelContext {
        config: &models_config,
        client: hf_client.as_ref(),
        cold,
        image_prompt: image_prompt.as_ref(),
        message_prompt: message_prompt.as_ref(),
    };

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
            &images,
            &payment_options,
            rates.clone(),
            &invariants,
            dataset_dir,
            &model_ctx,
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

    // Every call `hf_client` made this run (empty until extraction wires a live call site
    // into `decide_one`); `write_usage_report` still renders every required section with
    // zeros (PLAN.md §6.5) so signoff's usage-report check passes on a 0-call run.
    let pricing = load_pricing(Path::new("config/models.toml"))?;
    let usage_records: Vec<hf::Usage> = hf_client.as_ref().map(HfClient::usage_records).unwrap_or_default();
    hf::write_usage_report(
        Path::new("evaluation/usage_report.md"),
        &usage_records,
        &pricing,
        requests.len(),
    )?;

    eprintln!("model cache stats: {:?}", model_cache.stats());

    Ok(ExitCode::SUCCESS)
}

/// The model path's shared, load-once state (PLAN.md §2.11: one client, one prompt load
/// per file, reused across every request). Every field stays `None` when no model is
/// selected in `config/models.toml`, which keeps the whole model path inactive.
struct ModelContext<'a> {
    config: &'a ModelsConfig,
    client: Option<&'a HfClient>,
    cold: bool,
    image_prompt: Option<&'a PromptSet>,
    message_prompt: Option<&'a PromptSet>,
}

/// One user's session, bound and decided for one request (PLAN.md §2.5: one session per
/// user, evidence and `decide` never take a user id). Evidence is the deterministic,
/// zero-token skeleton parser, plus (only when `model_ctx` has a selected model) a VLM
/// read for each of this user's blank-amount events and an LLM read for messages the
/// skeleton parser doesn't recognize.
#[allow(clippy::too_many_arguments)]
fn decide_one(
    request: &model::Request,
    profiles: &[model::FinancialProfile],
    events: &[model::FinancialEvent],
    messages: &[model::Message],
    images: &[model::Image],
    payment_options: &[model::RequestPaymentOption],
    rates: Arc<RateTable>,
    invariants: &Invariants,
    dataset_dir: &Path,
    model_ctx: &ModelContext,
) -> anyhow::Result<(model::OutputRow, Result<Vec<Finding>, InvariantViolation>)> {
    let mut session = Session::from_model(&request.user_id, profiles, events, rates, Rules::default())?;

    let evidence = retrieval::for_user(&request.user_id, request.request_date, messages, &[]);
    let home_currency = session.profile().home_currency.clone();
    let mut facts = deterministic_evidence(&evidence.messages, &home_currency);

    if let (Some(client), Some(prompt), Some(vlm_primary)) =
        (model_ctx.client, model_ctx.image_prompt, model_ctx.config.vlm_primary())
    {
        let vlm_escalation = model_ctx.config.vlm_escalation();
        let vlm_fallback = model_ctx.config.vlm_fallback();
        let image_max_dim_px = model_ctx.config.image_max_dim_px();
        for event in events.iter().filter(|e| e.user_id == request.user_id && e.amount.is_none()) {
            let Some(image) = images.iter().find(|i| i.related_event_id == event.event_id) else {
                continue;
            };
            let image_path = dataset_dir.join("media/images").join(format!("{}.png", image.image_id));
            let typed_event = match Event::from_model(event) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("vlm: skipping {}: {e:#}", event.event_id);
                    continue;
                }
            };
            match images::resolve_blank_amount(
                client,
                model_ctx.cold,
                prompt,
                &model_ctx.config.decoding,
                image_max_dim_px,
                &image_path,
                &image.image_id,
                vlm_primary,
                vlm_escalation,
                vlm_fallback,
                &typed_event,
            ) {
                Ok(Some(record)) => facts.push(record),
                Ok(None) => {}
                Err(e) => eprintln!("vlm: {} failed: {e:#}", image.image_id),
            }
        }
    }

    if let (Some(client), Some(prompt), Some(llm_primary)) =
        (model_ctx.client, model_ctx.message_prompt, model_ctx.config.llm_primary())
    {
        match llm_evidence(
            client,
            model_ctx.cold,
            prompt,
            &model_ctx.config.decoding,
            llm_primary,
            model_ctx.config.llm_fallback(),
            &evidence.messages,
            &home_currency,
        ) {
            Ok(records) => facts.extend(records),
            Err(e) => eprintln!("llm: {} failed: {e:#}", request.user_id),
        }
    }

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

/// Reads `config/models.toml`'s candidate pricing into the map `hf::write_usage_report`
/// expects, keyed by `model_id`. `hf.rs` deliberately has no TOML dependency of its own
/// (its own doc comment: "the caller reads models.toml and builds this map"), so that
/// caller is here.
fn load_pricing(path: &Path) -> anyhow::Result<HashMap<String, hf::Pricing>> {
    #[derive(Deserialize)]
    struct ModelsConfig {
        candidates: CandidatesConfig,
    }
    #[derive(Deserialize)]
    struct CandidatesConfig {
        vlm: Vec<CandidateConfig>,
        llm: Vec<CandidateConfig>,
    }
    #[derive(Deserialize)]
    struct CandidateConfig {
        id: String,
        pricing_usd_per_m_tokens: PricingConfig,
    }
    #[derive(Deserialize)]
    struct PricingConfig {
        input: f64,
        output: f64,
    }

    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", path.display()))?;
    let config: ModelsConfig = toml::from_str(&text)
        .map_err(|e| anyhow::anyhow!("failed to parse {}: {e}", path.display()))?;
    let mut pricing = HashMap::new();
    for c in config.candidates.vlm.into_iter().chain(config.candidates.llm) {
        pricing.insert(
            c.id,
            hf::Pricing {
                input_per_m: c.pricing_usd_per_m_tokens.input,
                output_per_m: c.pricing_usd_per_m_tokens.output,
            },
        );
    }
    Ok(pricing)
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
