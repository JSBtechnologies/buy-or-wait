use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use buyorwait::engine::ledger::Fact;
use buyorwait::engine::session::Session;
use buyorwait::engine::types::{Event, PaymentOption, RateTable, RequestSpec};
use buyorwait::engine::Rules;
use buyorwait::evaluation::{Finding, ForecastSeries, InvariantViolation, Invariants};
use buyorwait::extract::intake;
use buyorwait::extract::messages::{deterministic_evidence, llm_evidence};
use buyorwait::extract::model_config::ModelsConfig;
use buyorwait::extract::ocr::{OcrClient, OcrConfig};
use buyorwait::extract::prompts::{self, PromptSet};
use buyorwait::extract::{images, retrieval};
use buyorwait::hf::{self, HfClient};
use buyorwait::model;
use buyorwait::store::cache::DiskCache;
use buyorwait::store::processed::ProcessedStore;
use chrono::NaiveDate;
use serde::Deserialize;

fn main() -> anyhow::Result<ExitCode> {
    load_dotenv();
    let args: Vec<String> = env::args().collect();

    // `buyorwait verify <args>` delegates straight to the verifier's own CLI (validate/score/
    // selftest) and exits with its code, without touching the main pipeline below.
    if args.get(1).map(String::as_str) == Some("verify") {
        let code = buyorwait::evaluation::cli(&args[2..])?;
        return Ok(ExitCode::from(code.clamp(0, 255) as u8));
    }

    // `buyorwait ask --user <id> --text "..."` is the interactive mode of PLAN.md §2.6: the
    // request arrives as free text instead of `requests.csv` columns, so it goes through
    // `extract::intake::parse_request_text` to build the same `RequestSpec` batch mode builds
    // from columns. Not part of the graded batch pipeline above (never touches `output.csv`),
    // and never runs with `[selected]` absent since it has no non-model path to a `RequestSpec`.
    if args.get(1).map(String::as_str) == Some("ask") {
        return run_ask(&args[2..]);
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

    // OCR ingestion (fleet/specs/ocr_vllm_pipeline.md, work item B): users upload receipts
    // before asking questions, so every image in dataset/images.csv is OCR'd up front here
    // and cached at store/ocr/<image_id>/page_<n>.md; requests then read that cache (once
    // extraction/ml-engineer publish the resolve-from-cache entry point in extract::images --
    // not wired yet, see below). `--cold` forces re-OCR, same as the other caches above.
    // Skipped (not a hard error) when OCR isn't configured this run, e.g. `OCR_BASE_URL`
    // unset while `blocker.ocr_endpoint` is still open -- the batch pipeline stays runnable
    // without it.
    let ocr_cache_dir = store_root.join("ocr");
    let mut ocr_results: HashMap<String, buyorwait::extract::ocr::OcrResult> = HashMap::new();
    let mut ocr_usage: Vec<buyorwait::extract::ocr::OcrUsage> = Vec::new();
    match OcrConfig::from_env() {
        Ok(ocr_config) => match OcrClient::new(ocr_config, ocr_cache_dir) {
            Ok(ocr_client) => {
                let mut ocr_ok = 0usize;
                let mut ocr_err = 0usize;
                for image in &images {
                    let image_path = dataset_dir.join("media/images").join(format!("{}.png", image.image_id));
                    match ocr_client.ingest(&image.image_id, &image_path, cold) {
                        Ok(result) => {
                            ocr_ok += 1;
                            ocr_usage.extend(result.pages.iter().map(|p| p.usage.clone()));
                            ocr_results.insert(image.image_id.clone(), result);
                        }
                        Err(e) => {
                            ocr_err += 1;
                            eprintln!("ocr: {} failed: {e:#}", image.image_id);
                        }
                    }
                }
                eprintln!("ocr: ingested {ocr_ok} images ({ocr_err} failed)");
            }
            Err(e) => eprintln!("ocr: configured but client init failed, skipping ingest ({e:#})"),
        },
        Err(e) => eprintln!("ocr: not configured this run, skipping ingest ({e:#})"),
    }

    // Which models (if any) to call live this run. With `[selected]` absent from
    // config/models.toml (the state before the user picks, PLAN.md Phase 2d),
    // llm_primary() is None and the message-extraction model path below stays inactive --
    // this run is then behaviorally identical to the deterministic-evidence-only baseline
    // (PLAN.md Phase 3: byte-identical output is required either way). The HF VLM image path
    // and the Anthropic tiebreak were removed (user cleanup, board:cleanup.remove_vlm_anthropic):
    // blank-amount images resolve from the OCR path only (`resolve_blank_amount_ocr` above).
    let models_config = ModelsConfig::load(Path::new("config/models.toml"))?;
    let use_models = models_config.llm_primary().is_some();

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
    let message_prompt = if models_config.llm_primary().is_some() && hf_client.is_some() {
        Some(prompts::load(Path::new("prompts/message_extraction.v1.md"), "User prompt template")?)
    } else {
        None
    };
    let model_ctx = ModelContext {
        config: &models_config,
        client: hf_client.as_ref(),
        cold,
        message_prompt: message_prompt.as_ref(),
        ocr_results: &ocr_results,
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
            &model_ctx,
            &processed_store,
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
    let mut pricing = load_pricing(Path::new("config/models.toml"))?;
    let mut usage_records: Vec<hf::Usage> = hf_client.as_ref().map(HfClient::usage_records).unwrap_or_default();
    // AGENTS.md §6.5: the report must summarize the FINAL run's providers/models, calls,
    // tokens, and cost -- OCR is a real model path, not a footnote, so its calls become
    // ordinary `hf::Usage` records (one per page, matching how HF calls are counted) and a
    // derived per-token `Pricing` entry, rather than a separate section the Overview/
    // per-model table ignore. The self-hosted H100 is billed by wall time, not per token, so
    // this rate is back-derived from that wall-time cost purely so the existing per-token
    // cost math lands on the same total -- `append_ocr_cost_note` states the real assumption.
    const OCR_MODEL_ID: &str = "baidu/Unlimited-OCR";
    const OCR_PROVIDER: &str = "self-hosted vLLM (RunPod H100)";
    const H100_HOURLY_USD: f64 = 2.69;
    let ocr_wall_seconds: f64 = ocr_usage.iter().map(|u| u.seconds).sum();
    let ocr_cost_usd = (ocr_wall_seconds / 3600.0) * H100_HOURLY_USD;
    let ocr_total_tokens: u64 = ocr_usage.iter().map(|u| u.prompt_tokens + u.completion_tokens).sum();
    if ocr_total_tokens > 0 {
        let rate_per_m = ocr_cost_usd * 1_000_000.0 / ocr_total_tokens as f64;
        pricing.insert(OCR_MODEL_ID.to_string(), hf::Pricing { input_per_m: rate_per_m, output_per_m: rate_per_m });
    }
    usage_records.extend(ocr_usage.iter().map(|u| hf::Usage {
        model_id: OCR_MODEL_ID.to_string(),
        provider: OCR_PROVIDER.to_string(),
        prompt_tokens: u.prompt_tokens,
        completion_tokens: u.completion_tokens,
        latency_ms: (u.seconds * 1000.0) as u64,
        cache_hit: u.cache_hit,
    }));
    hf::write_usage_report(
        Path::new("evaluation/usage_report.md"),
        &usage_records,
        &pricing,
        requests.len(),
    )?;
    append_ocr_cost_note(Path::new("evaluation/usage_report.md"), H100_HOURLY_USD, ocr_wall_seconds, ocr_cost_usd)?;

    eprintln!("model cache stats: {:?}", model_cache.stats());

    Ok(ExitCode::SUCCESS)
}

/// Interactive-mode entry point (PLAN.md §2.6): `buyorwait ask --user <id> --text "..." [--date
/// YYYY-MM-DD]`. Requires an active `[selected]` `llm_primary` in `config/models.toml` --
/// unlike batch mode, free text has no columns to fall back to, so with no model selected this
/// prints a clear message instead of guessing a field.
fn run_ask(args: &[String]) -> anyhow::Result<ExitCode> {
    let mut user_id: Option<String> = None;
    let mut text: Option<String> = None;
    let mut date: Option<NaiveDate> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--user" => {
                user_id = Some(
                    it.next()
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("--user needs a value"))?,
                );
            }
            "--text" => {
                text = Some(
                    it.next()
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("--text needs a value"))?,
                );
            }
            "--date" => {
                let s = it
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--date needs a value"))?;
                date = Some(
                    NaiveDate::parse_from_str(s, "%Y-%m-%d")
                        .map_err(|e| anyhow::anyhow!("--date {s:?}: {e}"))?,
                );
            }
            other => anyhow::bail!("unexpected argument {other:?} (usage: ask --user <id> --text \"...\" [--date YYYY-MM-DD])"),
        }
    }
    let user_id = user_id.ok_or_else(|| anyhow::anyhow!("ask needs --user <id>"))?;
    let text = text.ok_or_else(|| anyhow::anyhow!("ask needs --text \"...\""))?;
    let request_date = date.unwrap_or_else(|| chrono::Local::now().date_naive());

    let dataset_dir = Path::new("../dataset");
    let profiles = model::load_financial_profiles(dataset_dir.join("financial_profiles.csv"))?;
    let events = model::load_financial_events(dataset_dir.join("financial_events.csv"))?;
    let rates = model::load_exchange_rates(dataset_dir.join("exchange_rates.csv"))?;
    let rates = Arc::new(RateTable::from_model(&rates));

    let models_config = ModelsConfig::load(Path::new("config/models.toml"))?;
    let Some(llm_primary) = models_config.llm_primary() else {
        anyhow::bail!(
            "interactive mode needs an active `[selected]` llm_primary in config/models.toml; \
             none is configured yet (see PLAN.md §2.6 and board decision.selected_activation_question) -- \
             batch mode (`cargo run --release`) is unaffected, it never needs a model for intake"
        );
    };
    let client = HfClient::new()?;
    let prompt = prompts::load(
        Path::new("prompts/request_text_extraction.v1.md"),
        "User prompt template",
    )?;

    let Some(spec) = intake::parse_request_text(&client, false, &prompt, &models_config.decoding, llm_primary, &text)?
    else {
        eprintln!(
            "could not ground amount, deadline, and type in the request text -- \
             refusing to guess a decision-affecting default"
        );
        return Ok(ExitCode::FAILURE);
    };

    let session = Session::from_model(&user_id, &profiles, &events, rates, Rules::default())?;
    // No `request_payment_options.csv` row exists for an ad hoc text request, so only the
    // full-payment/wait/spending-change candidates (PLAN.md §2.8) are considered; no
    // installment option can be offered.
    let decision = session.decide("ask", request_date, &spec, &[])?;
    let row = decision.row;
    println!("amount_safe_to_pay: {}", row.amount_safe_to_pay);
    println!("affordability_status: {}", row.affordability_status);
    println!("recommended_payment_method: {}", row.recommended_payment_method);
    println!("payment_plan: {}", row.payment_plan);
    println!("earliest_date_for_full_payment: {}", row.earliest_date_for_full_payment);
    println!("spending_changes_needed: {}", row.spending_changes_needed);
    println!("decision_explanation: {}", row.decision_explanation);

    Ok(ExitCode::SUCCESS)
}

/// The model path's shared, load-once state (PLAN.md §2.11: one client, one prompt load
/// per file, reused across every request). Every field stays `None` when no model is
/// selected in `config/models.toml`, which keeps the whole model path inactive.
struct ModelContext<'a> {
    config: &'a ModelsConfig,
    client: Option<&'a HfClient>,
    cold: bool,
    message_prompt: Option<&'a PromptSet>,
    /// OCR results cached at ingestion (fleet/specs/ocr_vllm_pipeline.md), keyed by
    /// `image_id`. Populated independent of `[selected]` -- OCR is the default image path,
    /// not gated behind a chosen VLM -- and empty (never absent) when OCR isn't configured
    /// this run.
    ocr_results: &'a HashMap<String, buyorwait::extract::ocr::OcrResult>,
}

/// One user's session, bound and decided for one request (PLAN.md §2.5: one session per
/// user, evidence and `decide` never take a user id). Evidence is the deterministic,
/// zero-token skeleton parser, plus an OCR-cache read for each of this user's blank-amount
/// events and (only when `model_ctx` has a selected LLM) an LLM read for messages the
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
    model_ctx: &ModelContext,
    processed_store: &ProcessedStore,
) -> anyhow::Result<(model::OutputRow, Result<Vec<Finding>, InvariantViolation>)> {
    let mut session = Session::from_model(&request.user_id, profiles, events, rates, Rules::default())?;

    let evidence = retrieval::for_user(&request.user_id, request.request_date, messages, &[]);
    let home_currency = session.profile().home_currency.clone();
    let mut facts = deterministic_evidence(&evidence.messages, &home_currency);

    // Blank-amount image resolution (fleet/specs/ocr_vllm_pipeline.md work item B2; VLM/
    // Anthropic path removed, board:cleanup.remove_vlm_anthropic): OCR, cached at ingestion
    // and keyed by image_id, is the only image path -- a deterministic reader + witness
    // gate, no live call here.
    for event in events.iter().filter(|e| e.user_id == request.user_id && e.amount.is_none()) {
        let Some(image) = images.iter().find(|i| i.related_event_id == event.event_id) else {
            continue;
        };
        let typed_event = match Event::from_model(event) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("image: skipping {}: {e:#}", event.event_id);
                continue;
            }
        };

        let Some(ocr_result) = model_ctx.ocr_results.get(&image.image_id) else { continue };
        let resolution = images::resolve_blank_amount_ocr(ocr_result, &image.image_id, &typed_event);
        let accepted_amount = resolution.evidence.as_ref().and_then(|e| match &e.fact {
            Fact::EventAmount { amount, .. } => Some(amount.to_f64()),
            _ => None,
        });
        // Blocker #205 (verifier board:verify.image_agreement): persist every attempted
        // read's provenance, independent of whether it contributed to the final evidence, so
        // the agreement outcome is auditable. Runtime store only (code/store/, gitignored),
        // never shipped.
        let reads: Vec<serde_json::Value> = resolution
            .reads
            .iter()
            .map(|r| {
                let cutoff = (r.due_date.is_some() || r.before_amount.is_some() || r.after_amount.is_some())
                    .then(|| {
                        serde_json::json!({
                            "due_date": r.due_date,
                            "before_amount": r.before_amount,
                            "after_amount": r.after_amount,
                        })
                    });
                serde_json::json!({
                    "role": r.role,
                    "model_id": r.model_id,
                    "model_revision": r.model_revision,
                    "max_dim_px": r.max_dim_px,
                    "reconciled": r.reconciled,
                    "selected_amount": r.selected_amount,
                    "cutoff": cutoff,
                    "witness": r.witness,
                    "ocr_notes": r.ocr_notes,
                })
            })
            .collect();
        let provenance = serde_json::json!({
            "image_id": image.image_id,
            "event_id": typed_event.id,
            "class": resolution.class.clone().unwrap_or_default(),
            "mode": resolution.mode,
            "reads": reads,
            "outcome": resolution.outcome,
            "accepted_amount": accepted_amount,
        });
        if let Err(e) = processed_store.save("image_reads", &image.image_id, &provenance) {
            eprintln!("image: failed to persist provenance for {}: {e:#}", image.image_id);
        }
        if let Some(record) = resolution.evidence {
            facts.push(record);
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

    // Blocker #177 (verifier): persist exactly what gets applied -- deterministic plus
    // model-path records, after grounding -- so signoff can diff it against an independent
    // regeneration. Runtime store only (code/store/, gitignored), never shipped.
    processed_store.save("evidence", &request.request_id, &facts)?;
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

/// Loads `KEY=VALUE` lines from a `.env` file into the process environment (e.g.
/// `OCR_BASE_URL`, per the lead's ruling on bus `integrate`), without overriding a variable
/// already set -- an explicit `OCR_BASE_URL=... cargo run` still wins. Checks `.env` (the
/// usual cwd when running from `code/`) then `../.env` (repo root). Never required: silently
/// does nothing if neither exists. Hand-rolled rather than a `dotenv` crate dependency -- a
/// handful of lines, no new Cargo.toml surface.
fn load_dotenv() {
    for path in [Path::new(".env"), Path::new("../.env")] {
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else { continue };
            let key = key.trim();
            let value = value.trim().trim_matches('"').trim_matches('\'');
            if env::var_os(key).is_none() {
                env::set_var(key, value);
            }
        }
        return;
    }
}

/// Appends a one-line note to `evaluation/usage_report.md` stating the OCR cost assumption
/// (fleet/specs/ocr_vllm_pipeline.md work item B4). The Overview and per-model breakdown
/// above already carry OCR's real calls/tokens/cost as an ordinary `hf::Usage`/`Pricing`
/// entry (main(), AGENTS.md §6.5) -- this note exists only because that per-token `Pricing`
/// number is itself back-derived from a wall-time assumption, and the assumption needs to be
/// visible, not just its result. **Stated explicitly**: $2.69/hr, a commonly quoted RunPod
/// H100 SXM secure-cloud on-demand rate as of this run -- adjust the caller's
/// `H100_HOURLY_USD` if the actual pod's rate differs.
fn append_ocr_cost_note(path: &Path, hourly_usd: f64, wall_seconds: f64, cost_usd: f64) -> anyhow::Result<()> {
    let note = format!(
        "\n_OCR cost assumption: baidu/Unlimited-OCR (self-hosted vLLM, RunPod H100) is billed \
         by wall time, not per token -- ${cost_usd:.6} = {wall_seconds:.2}s wall time \u{d7} \
         ${hourly_usd:.2}/hr, shown above via a per-token rate back-derived to match; verify \
         against the actual RunPod rate._\n"
    );
    let mut file = std::fs::OpenOptions::new().append(true).open(path)?;
    use std::io::Write;
    file.write_all(note.as_bytes())?;
    Ok(())
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
    for c in config.candidates.llm {
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
