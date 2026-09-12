use std::env;
use std::fs;
use std::path::Path;

use buyorwait::model;
use buyorwait::store::cache::DiskCache;
use buyorwait::store::processed::ProcessedStore;

fn main() -> anyhow::Result<()> {
    let cold = env::args().any(|a| a == "--cold");

    let dataset_dir = Path::new("../dataset");
    let profiles = model::load_financial_profiles(dataset_dir.join("financial_profiles.csv"))?;
    let events = model::load_financial_events(dataset_dir.join("financial_events.csv"))?;
    let rates = model::load_exchange_rates(dataset_dir.join("exchange_rates.csv"))?;
    let requests = model::load_requests(dataset_dir.join("requests.csv"))?;
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

    let mut writer = csv::Writer::from_path("../output.csv")?;
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

    Ok(())
}
