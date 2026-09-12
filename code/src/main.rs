use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

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

    // TODO(extraction): build the per-request evidence index; run VLM/LLM extraction
    // through `model_cache`, persisting typed image figures / message records via
    // `processed_store`.
    // TODO(engine): reconstruct per-user ledgers (persisted via `processed_store`), detect
    // recurrence, forecast, search+rank plans for each request.
    // TODO(evaluation): assert invariants, score against sample_requests.csv, replay.

    let mut writer = csv::Writer::from_path(&out_path)?;
    for request in &requests {
        writer.serialize(model::OutputRow {
            request_id: request.request_id.clone(),
            ..Default::default()
        })?;
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
