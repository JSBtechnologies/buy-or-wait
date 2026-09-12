//! Grounding audit of extraction's evidence records (PLAN.md §3 deterministic verification (c)/(d)).
//!
//! Reads the handoff files `store/evidence/<user_id>.json` as plain JSON (a serialized
//! `Vec<EvidenceRecord>`), so a schema tweak on the engine side does not break the audit.
//! Rules for message-derived records:
//! - every amount in a fact literally appears in the message text (any thousands style)
//! - every date appears in the text (ISO or "D Month YYYY", English or Indonesian) — warning only
//! - event-level facts only when the message's related_event_id equals the fact's event_id
//! - scam / own-account-distractor messages yield no counted facts
//! - announced-but-unconfirmed money (not approved, not credited, pending) is `Unconfirmed`, never counted
//! - no record from a message sent after the user's request date
//!
//! Image-derived `EventAmount` facts: currency equals the event's, amount within 0.25x–4x of the
//! median of that user's same-description rows (warning when no history).

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use super::contract::{Finding, Severity};
use super::data::{parse_cents, Cents, Dataset};

pub struct Message {
    pub message_id: String,
    pub user_id: String,
    pub related_event_id: String,
    pub sent_date: String,
    pub text: String,
}

pub fn load_messages(dataset_dir: &Path) -> Result<HashMap<String, Message>> {
    let mut rdr = csv::Reader::from_path(dataset_dir.join("messages.csv"))?;
    let h = rdr.headers()?.clone();
    let col = |name: &str| h.iter().position(|x| x.trim_start_matches('\u{feff}') == name).context(name.to_string());
    let (id, user, rel, sent, text) =
        (col("message_id")?, col("user_id")?, col("related_event_id")?, col("sent_at")?, col("message_text")?);
    let mut out = HashMap::new();
    for rec in rdr.records() {
        let rec = rec?;
        let m = Message {
            message_id: rec[id].to_string(),
            user_id: rec[user].to_string(),
            related_event_id: rec[rel].to_string(),
            sent_date: rec[sent].chars().take(10).collect(),
            text: rec[text].to_string(),
        };
        out.insert(m.message_id.clone(), m);
    }
    Ok(out)
}

const SCAM: [&str; 4] = ["release charge", "processing charge", "biaya pencairan", "biaya pemrosesan"];
const OWN_ACCOUNT: [&str; 3] = ["transfer between your two accounts", "antara dua rekening", "rekening Anda sendiri"];
const NOT_CONFIRMED: [&str; 10] = [
    "not been approved",
    "not approved",
    "have not been approved",
    "still pending",
    "not been credited",
    "not credited",
    "belum disetujui",
    "belum dikreditkan",
    "masih menunggu",
    "still subject to",
];
const COUNTED_MONEY: [&str; 5] = ["IncomeAmountChange", "NextIncomeAmount", "OneTimeFlow", "NewRecurringExpense", "ExpenseAmountChange"];
const EVENT_LEVEL: [&str; 6] = ["EventAmount", "EventCancelled", "EventSettled", "EventAmended", "DuplicateOf", "OwnAccountTransfer"];
const MONTHS_EN: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
const MONTHS_ID: [&str; 12] = ["Januari", "Februari", "Maret", "April", "Mei", "Juni", "Juli", "Agustus", "September", "Oktober", "November", "Desember"];

/// All amounts written in `text`, in cents, accepting `1,234.56`, `1.234,56`, `1234.5`, `1 234`.
pub fn amounts_in(text: &str) -> Vec<Cents> {
    let mut out = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len()
            && (chars[i].is_ascii_digit()
                || ((chars[i] == ',' || chars[i] == '.') && chars.get(i + 1).map(|c| c.is_ascii_digit()).unwrap_or(false)))
        {
            i += 1;
        }
        let tok: String = chars[start..i].iter().collect();
        // Try: separators as thousands (all of them), or the last separator as a decimal point.
        let digits: String = tok.chars().filter(|c| c.is_ascii_digit()).collect();
        if let Ok(c) = parse_cents(&digits) {
            out.push(c);
        }
        if let Some(pos) = tok.rfind([',', '.']) {
            let frac = &tok[pos + 1..];
            if frac.len() <= 2 {
                let int: String = tok[..pos].chars().filter(|c| c.is_ascii_digit()).collect();
                if let Ok(c) = parse_cents(&format!("{int}.{frac}")) {
                    out.push(c);
                }
            }
        }
    }
    out
}

fn date_in(text: &str, iso: &str) -> bool {
    if text.contains(iso) {
        return true;
    }
    let Ok(d) = chrono::NaiveDate::parse_from_str(iso, "%Y-%m-%d") else { return false };
    use chrono::Datelike;
    let m = d.month0() as usize;
    [MONTHS_EN[m], MONTHS_ID[m]].iter().any(|name| text.contains(&format!("{} {} {}", d.day(), name, d.year())))
}

fn walk<'a>(v: &'a Value, key: &str, out: &mut Vec<&'a Value>) {
    match v {
        Value::Object(map) => {
            for (k, x) in map {
                if k == key {
                    out.push(x);
                }
                walk(x, key, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| walk(x, key, out)),
        _ => {}
    }
}

/// Money serializes as its raw i64 at engine scale; `money_scale` converts to cents (10_000 → 100).
pub fn audit_records(
    ds: &Dataset,
    messages: &HashMap<String, Message>,
    user_id: &str,
    records: &Value,
    money_scale: i64,
) -> Vec<Finding> {
    let mut out = Vec::new();
    let Some(list) = records.as_array() else {
        out.push(Finding { request_id: user_id.into(), severity: Severity::Error, code: "EV0_not_array", detail: "evidence file is not a JSON array".into() });
        return out;
    };
    let request_date = ds.requests.iter().find(|r| r.user_id == user_id).map(|r| r.request_date.to_string());
    let mut push = |sev, code, detail: String| out.push(Finding { request_id: user_id.into(), severity: sev, code, detail });
    for rec in list {
        let rid = rec.get("record_id").and_then(Value::as_str).unwrap_or("?").to_string();
        let Some(fact) = rec.get("fact").and_then(Value::as_object).and_then(|m| m.iter().next()) else {
            push(Severity::Error, "EV0_no_fact", format!("{rid}: no fact"));
            continue;
        };
        let (kind, body) = (fact.0.as_str(), fact.1);
        let source_id = rid.split('#').next().unwrap_or("").to_string();
        let amounts: Vec<Cents> = {
            let mut vals = Vec::new();
            walk(body, "amount", &mut vals);
            vals.iter().filter_map(|v| v.as_i64()).map(|raw| raw * 100 / money_scale).collect()
        };
        let dates: Vec<String> = {
            let mut vals = Vec::new();
            for k in ["date", "effective", "new_date", "first_date"] {
                walk(body, k, &mut vals);
            }
            vals.iter().filter_map(|v| v.as_str().map(String::from)).collect()
        };
        let event_id = body.get("event_id").and_then(Value::as_str).map(String::from);

        if let Some(m) = messages.get(&source_id) {
            if m.user_id != user_id {
                push(Severity::Error, "EV1_other_user", format!("{rid}: message belongs to {}", m.user_id));
            }
            if let Some(rd) = &request_date {
                if m.sent_date.as_str() > rd.as_str() {
                    push(Severity::Error, "EV1_after_request", format!("{rid}: sent {} after request_date {rd}", m.sent_date));
                }
            }
            let in_text = amounts_in(&m.text);
            for a in &amounts {
                if !in_text.contains(a) {
                    push(Severity::Error, "EV2_amount_not_in_text", format!("{rid} {kind}: amount {} not written in {source_id}", super::data::fmt_cents_2dp(*a)));
                }
            }
            for d in &dates {
                if !date_in(&m.text, d) {
                    push(Severity::Warn, "EV3_date_not_in_text", format!("{rid} {kind}: date {d} not written in {source_id}"));
                }
            }
            if EVENT_LEVEL.contains(&kind) && (m.related_event_id.is_empty() || event_id.as_deref() != Some(m.related_event_id.as_str())) {
                push(
                    Severity::Error,
                    "EV4_event_fact_ungrounded",
                    format!("{rid} {kind} on {:?} but message related_event_id is {:?}", event_id, m.related_event_id),
                );
            }
            let lower = m.text.to_lowercase();
            if kind != "Unconfirmed" && SCAM.iter().any(|s| lower.contains(s)) {
                push(Severity::Error, "EV5_scam_fact", format!("{rid} {kind} from scam message {source_id}"));
            }
            if m.related_event_id.is_empty() && OWN_ACCOUNT.iter().any(|s| lower.contains(&s.to_lowercase())) {
                push(Severity::Error, "EV5_distractor_fact", format!("{rid} {kind} from own-account message {source_id} with no event row"));
            }
            if COUNTED_MONEY.contains(&kind) && NOT_CONFIRMED.iter().any(|s| lower.contains(s)) {
                push(Severity::Warn, "EV6_unconfirmed_counted", format!("{rid} {kind} from {source_id}, which says the money is not confirmed"));
            }
        } else if source_id.starts_with("image_") {
            if kind == "EventAmount" {
                let Some(ev) = event_id.as_ref().and_then(|id| ds.events.get(id)) else {
                    push(Severity::Error, "EV7_image_unknown_event", format!("{rid}: unknown event {event_id:?}"));
                    continue;
                };
                let cur = body.get("currency").and_then(Value::as_str).unwrap_or("");
                if cur != ev.currency {
                    push(Severity::Error, "EV7_image_currency", format!("{rid}: {cur} vs event {}", ev.currency));
                }
                let mut hist: Vec<Cents> = ds
                    .events_by_user
                    .get(&ev.user_id)
                    .into_iter()
                    .flatten()
                    .filter_map(|id| ds.events.get(id))
                    .filter(|o| o.event_id != ev.event_id && o.description == ev.description && o.category == ev.category)
                    .filter_map(|o| o.amount)
                    .collect();
                hist.sort();
                match (amounts.first(), hist.get(hist.len() / 2)) {
                    (Some(a), Some(med)) if *a * 4 < *med || *a > *med * 4 => push(
                        Severity::Warn,
                        "EV7_image_implausible",
                        format!("{rid}: {} vs history median {}", super::data::fmt_cents_2dp(*a), super::data::fmt_cents_2dp(*med)),
                    ),
                    (_, None) => push(Severity::Warn, "EV7_image_no_history", format!("{rid}: no same-description history for {}", ev.event_id)),
                    _ => {}
                }
            }
        } else {
            push(Severity::Error, "EV0_unknown_source", format!("{rid}: source {source_id} is neither a message nor an image"));
        }
    }
    out
}

pub fn audit_dir(dataset_dir: &Path, evidence_dir: &Path, money_scale: i64) -> Result<Vec<Finding>> {
    let ds = Dataset::load(dataset_dir, &dataset_dir.join("requests.csv"))?;
    let samples = Dataset::load(dataset_dir, &dataset_dir.join("sample_requests.csv"))?;
    let messages = load_messages(dataset_dir)?;
    let mut out = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(evidence_dir)?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(user) = name.strip_suffix(".json") else { continue };
        let json: Value = serde_json::from_str(&std::fs::read_to_string(e.path())?).with_context(|| name.clone())?;
        let set = if samples.requests.iter().any(|r| r.user_id == user) { &samples } else { &ds };
        out.extend(audit_records(set, &messages, user, &json, money_scale));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixtures() -> (Dataset, HashMap<String, Message>) {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../dataset");
        (Dataset::load(&dir, &dir.join("sample_requests.csv")).unwrap(), load_messages(&dir).unwrap())
    }

    #[test]
    fn amounts_any_style() {
        let a = amounts_in("naik menjadi IDR 42750000. EUR 1,422.85 and 1.422,85 or 13 110 000");
        assert!(a.contains(&4_275_000_000) && a.contains(&142_285));
    }

    #[test]
    fn grounded_raise_passes_and_invented_numbers_fail() {
        let (ds, msgs) = fixtures();
        // message_01 (user_02): raise to IDR 42750000 from 2025-08-15.
        let ok = json!([{"record_id": "message_01#0", "fact": {"IncomeAmountChange": {
            "category": "salary", "amount": 42_750_000_i64 * 10_000, "currency": "IDR", "effective": "2025-08-15"}}}]);
        let f = audit_records(&ds, &msgs, "user_02", &ok, 10_000);
        assert!(f.is_empty(), "{f:?}");

        let bad = json!([{"record_id": "message_01#0", "fact": {"IncomeAmountChange": {
            "category": "salary", "amount": 47_750_000_i64 * 10_000, "currency": "IDR", "effective": "2025-09-15"}}}]);
        let codes: Vec<_> = audit_records(&ds, &msgs, "user_02", &bad, 10_000).iter().map(|f| f.code).collect();
        assert!(codes.contains(&"EV2_amount_not_in_text") && codes.contains(&"EV3_date_not_in_text"), "{codes:?}");
    }

    #[test]
    fn event_facts_need_related_event_and_distractors_yield_nothing() {
        let (ds, msgs) = fixtures();
        // message_13 (user_18): own-account note with no related_event_id.
        let rec = json!([{"record_id": "message_13#0", "fact": {"OwnAccountTransfer": {"event_id": "event_1565"}}}]);
        let codes: Vec<_> = audit_records(&ds, &msgs, "user_18", &rec, 10_000).iter().map(|f| f.code).collect();
        assert!(codes.contains(&"EV4_event_fact_ungrounded") && codes.contains(&"EV5_distractor_fact"), "{codes:?}");
    }
}
