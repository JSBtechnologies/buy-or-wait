# Spec: Unlimited-OCR (vLLM) ingestion → Rust label mapping → witness gate

Status: user decision 2026-09-13 ~08:55Z. It replaces the Qwen/gemma fixed-key image reads.

## Why
- **Reading is solved, mapping isn't.** The lead's audit showed the HF VLMs read every final amount correctly. All failures came from the model mapping numbers into fixed JSON keys the page doesn't print.
- **Unlimited-OCR test.** Tested locally on all 16 images by the lead (`scratch/ocr_baidu/`), it transcribes each label and value as printed, as text lines or HTML table rows.
- **What that gives:** the right figure on all 16 with this preprocessing: split pages at near-black full-width bars, 2x upscale, per-page gundam, max_tokens 8192.

## Product shape (user)
- Users upload receipts before asking questions. OCR runs at **ingestion** and is **cached**; requests read the cache.
- Serving is a vLLM OpenAI-compatible endpoint, hostable anywhere. The POC runs on a RunPod H100.
- The system stays Rust. No Python ships.

## Serving (vLLM recipe, required pieces)
```
docker run --rm --gpus all --network host --ipc host vllm/vllm-openai:unlimited-ocr baidu/Unlimited-OCR \
  --trust-remote-code \
  --logits_processors vllm.model_executor.models.unlimited_ocr:NGramPerReqLogitsProcessor \
  --no-enable-prefix-caching --mm-processor-cache-gb 0
```
Request (`POST {OCR_BASE_URL}/chat/completions`), one request **per page image**:
```json
{"model": "baidu/Unlimited-OCR",
 "messages": [{"role":"user","content":[
   {"type":"text","text":"<image>document parsing."},
   {"type":"image_url","image_url":{"url":"data:image/png;base64,..."}}]}],
 "max_tokens": 8192, "temperature": 0.0,
 "skip_special_tokens": false,
 "vllm_xargs": {"ngram_size": 35, "window_size": 128}}
```
- **Env:** `OCR_BASE_URL` (e.g. `https://<pod>-8000.proxy.runpod.net/v1`), `OCR_MODEL` (default `baidu/Unlimited-OCR`), optional `OCR_API_KEY`. Never log secrets.

## Work items
### A. ml-engineer (owns extract/images.rs, normalize.rs, witness.rs; new files below)
1. **`code/src/extract/ocr.rs`** (vLLM client + ingestion cache):
   - **Preprocess in Rust** with the `image` crate:
     - split at full-width near-black bands, meaning rows whose 99th-percentile luma is < 25 and runs ≥ 6 px; drop slivers ≤ 50 px
     - resize each page 2x (Lanczos3)
     - encode PNG, base64
   - **Cache:** `store/ocr/<image_id>/page_<n>.md` plus `meta.json` with model, config hash (prompt, dims, scale, max_tokens, xargs), usage tokens and seconds. Reuse the cache unless `--cold` or the config hash changed.
   - **Usage:** record calls, prompt and completion tokens per page for `usage_report.md`.
   - **Runaway guard:**
     - `finish_reason == "length"`, or a long block of prose, flags the page `ocr_truncated`
     - only rows before the runaway are used
     - a truncated page never supplies a final figure unless the figure appears before the runaway point AND has a witness
2. **`code/src/extract/ocr_parse.rs`** (output → rows):
   - unwrap `<|ref|>…<|/ref|>`, drop `<|det|>…<|/det|>` but keep bbox for row grouping, and HTML-unescape
   - **rows** = HTML `<tr>` rows (cells), det blocks grouped into visual rows by y-centre overlap (left→right), plus "label wraps to next line" pairs
   - **multi-value cells:** a label cell holding N labels next to a value cell holding N amounts is zipped in order **only if the counts match** (image_10 summary: 6 labels ↔ 6 values); otherwise it's unmapped
   - **output:** `Vec<LabeledValue { page, label, value_raw, amount: Option<f64>, date: Option<NaiveDate> }>`
3. **`code/src/extract/labels.rs`** (deterministic keyword `Vec`s → `ImageFigures`), roles:
   - grand_total; total ("total", "total amount", "total bill amount", "total amount to be received", "net amount", "total paid", "total amount received")
   - amount_paid ("amount received", "total paid", "total amount received", "amount paid")
   - balance_due ("balance due", "balance"); amount_due ("amount payable", "amount due")
   - subtotal ("sub total", "subtotal", "item bill", "item total", "taxable value")
   - tax lines (CGST/SGST/IGST/UGST/GST/Tax/Taxes/Cess/VAT, **summed**)
   - gross_pay ("total earnings"); deductions ("total deductions"); net_pay ("net pay")
   - previous_balance
   - cutoff: **only** labels `due till|by|before <date>` / `due after <date>` (date parsed with `normalize::parse_date_near`); a bare "Due Date" never counts
   - amount_in_words: a words value, excluding "paid amount in words"
   - **ignored:** "cash paid" / "cash" / "tendered", "change", "payments", "due date"
   - **role conflicts** (different values): prefer the summary section, else None + note. Unknown labels go to notes.
   - amounts through `normalize::parse_amount` with the currency hint (lakh, `$33,50`, `4 543`, `4.543` → thousands)
4. **Gate** in `images.rs`: OCR rows → `labels::figures_from_rows` → the existing `select` + `witness::find_witness` + `final_label_contradicts`.
   - Accept only with a witness. There is one deterministic reader, so two-read agreement no longer applies.
   - `ImageReadProvenance` gets `ocr_notes` (the fail-closed reason codes).
   - Keep the Qwen/gemma path compiled but off by default.
5. **Tests:** dev-only fixtures from the lead's local outputs `scratch/ocr_baidu/pagesgundam/final_r1/*.md`, copied into `#[cfg(test)]` fixtures, no image ids in non-test code.
   - e2e expectations: 02=100000 (balance_due), 05=822.05 (after-cutoff; event settles 2026-02-09), 07=8528 (grand total), 10=79679.26, 11=3650, 12=33.50 USD, 04=None (no final label), 01=4365000 net pay.

### B. integrator (owns main.rs, Cargo.toml, config, README, usage report)
1. `Cargo.toml`: add the `image` crate (png feature) if missing.
2. `main.rs`:
   - an **ingest** step before requests that runs `extract::ocr` over every image in `dataset/images.csv` (cache-first, `--cold` forces re-OCR)
   - then resolve blank amounts from the cache
   - remove the prompt v1 image path from the default flow
3. `code/config/models.toml` `[ocr]` section (model, prompt, max_tokens, scale, xargs). `README`: the vLLM docker command, RunPod note, env vars, cache location.
4. `evaluation/usage_report.md`: OCR calls and tokens from `meta.json`, plus cost as H100 hourly × wall time (state the assumption).
- Wait for ml-engineer's `extract::ocr` public signature on topic `extract` before wiring.

### C. Verification (lead + verifier)
- Against the RunPod endpoint:
  - ingest all 16
  - score with `scratch/ocr_baidu/score.py` pointed at `store/ocr` (the lead runs it)
  - e2e figures as in A5
  - two `--cold` runs byte-identical
  - sample scorer no regression
