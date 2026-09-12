//! One-off tool (owner: extraction): report how many of `dataset/messages.csv`'s 215
//! messages the deterministic skeleton parser (`extract::messages::parse_known_skeleton`)
//! recognizes, vs. how many still need the model path. Run from `code/` with
//! `cargo run --bin coverage`.

use std::collections::BTreeMap;
use std::path::Path;

use buyorwait::extract::messages::parse_known_skeleton;
use buyorwait::model;

fn main() -> anyhow::Result<()> {
    let dataset_dir = Path::new("../dataset");
    let messages = model::load_messages(dataset_dir.join("messages.csv"))?;

    let mut covered = 0usize;
    let mut uncovered_ids: Vec<String> = Vec::new();
    let mut record_type_counts: BTreeMap<String, usize> = BTreeMap::new();

    for message in &messages {
        match parse_known_skeleton(&message.message_text, message.sent_at.date_naive()) {
            Some(records) => {
                covered += 1;
                if records.is_empty() {
                    *record_type_counts.entry("(recognized, zero records)".to_string()).or_default() += 1;
                }
                for r in &records {
                    *record_type_counts.entry(format!("{:?}", r.record_type)).or_default() += 1;
                }
            }
            None => uncovered_ids.push(message.message_id.clone()),
        }
    }

    println!("coverage: {covered}/{} messages parsed deterministically", messages.len());
    println!("\nby record_type (recognized skeletons only):");
    for (k, v) in &record_type_counts {
        println!("  {v:4}  {k}");
    }
    println!("\nuncovered ({}) — need the model path:", uncovered_ids.len());
    for id in &uncovered_ids {
        print!("{id} ");
    }
    println!();

    Ok(())
}
