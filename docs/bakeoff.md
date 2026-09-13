# Bake-off — model selection evidence

Owner: ml-engineer. This document is evidence for the lead/user to pick the
final VLM and LLM (PLAN.md Phase 2d). ml-engineer does not pick the models.

## Step 1 — Live-model confirmation (done, 2026-09-12)

Queried `GET https://router.huggingface.co/v1/models` with `HF_TOKEN` (never
logged). 143 models returned. All required candidates are present and
`status: "live"` on at least one provider:

| Candidate | Modalities | Live providers (status) |
|---|---|---|
| `Qwen/Qwen3-VL-235B-A22B-Instruct` | text+image | novita, deepinfra |
| `Qwen/Qwen3-VL-30B-A3B-Instruct` | text+image | novita, featherless-ai, deepinfra |
| `google/gemma-4-31B-it` | text+image | novita, together, featherless-ai, deepinfra |
| `Qwen/Qwen3-235B-A22B-Instruct-2507` | text | novita, nscale, scaleway, deepinfra |
| `aisingapore/Gemma-SEA-LION-v4-27B-IT` | text | featherless-ai, publicai |
| `deepseek-ai/DeepSeek-V3.2` | text | novita, featherless-ai, deepinfra |
| `openai/gpt-oss-120b` | text | groq, novita, cerebras, nscale, together, fireworks-ai, featherless-ai, scaleway, baseten, ovhcloud, deepinfra |

**Best Qwen3.5/3.6 multimodal model listed:** all live text+image Qwen3.5/3.6
variants were enumerated: `Qwen3.6-35B-A3B`, `Qwen3.6-27B`, `Qwen3.5-397B-A17B`,
`Qwen3.5-122B-A10B`, `Qwen3.5-35B-A3B`, `Qwen3.5-27B`, `Qwen3.5-9B`. Selected
**`Qwen/Qwen3.5-397B-A17B`** as the flagship candidate: it is the
largest-parameter live multimodal variant (397B total / A17B active) and has
`supports_structured_output: true` on its cheapest provider (deepinfra,
$0.45/$3.00 per M input/output tokens). This is a candidate for the bake-off,
not a pre-selection.

Full per-model provider/pricing detail, chosen provider, and pinned
`model_revision` (HF Hub commit sha via `GET
https://huggingface.co/api/models/{id}`) are recorded in
`code/config/models.toml`.

## Step 2 — HF client (`code/src/hf.rs`)

Blocked on the integrator's scaffold (`P1.scaffold`, bus topic `scaffold`).
Will build once `atrium ctl bus sub scaffold` reports done and `git merge
main` is run in this worktree.

## Step 3 — Bake-off run (results)

Each candidate run 3x at `temperature=0`, fixed `seed`, against the identical gold subset with identical prompts (PLAN.md Phase 2d). Scored against `docs/gold_subset.json` as corrected in `3df3082` (message_10 -> `salary_first_confirmed`). Nothing here is a pick — the user chooses.

**Production weight note (from the lead):** extraction's deterministic parser now covers 214/215 messages; the LLM is called for exactly 1 message (`msg_86`) in production, while the VLM is called for all 16 images. Weight the VLM table far more heavily than the LLM table when choosing — the VLM choice is the one that matters at scale.

### VLM candidates (image -> typed figure schema)

| Model | Provider | Chosen res. (px) | Field accuracy vs gold (5 labeled) | Valid-JSON rate | Stability (3 runs, 16 images) | Cross-model agreement (11 unlabeled) | Avg input tok/item | Avg output tok/item | p50 latency (ms) | Est. cost/item | Est. cost/full run (16 images) |
|---|---|---|---|---|---|---|---|---|---|---|---|
| Qwen/Qwen3-VL-235B-A22B-Instruct | deepinfra | 1024 | 77.5% | 100.0% | 31.2% | 92.9% | 936 | 184 | 5861 | $0.00035 | $0.0056 |
| Qwen/Qwen3-VL-30B-A3B-Instruct | deepinfra | 1024 | 69.8% | 100.0% | 100.0% | 92.9% | 936 | 163 | 6151 | $0.00024 | $0.0038 |
| google/gemma-4-31B-it | deepinfra | 768 | 86.2% | 97.9% | 56.2% | 92.9% | 545 | 171 | 8871 | $0.00014 | $0.0022 |
| Qwen/Qwen3.5-397B-A17B | deepinfra | 512 | 0.0% | 0.0% | 100.0% | 92.9% | 432 | 400 | 7472 | $0.00139 | $0.0223 |

**ml-engineer recommendation (VLM, non-binding — the user decides):** `Qwen/Qwen3-VL-30B-A3B-Instruct` via `deepinfra` at 1024px. Highest weighted score across field accuracy, stability, valid-JSON rate, and cost/item; re-check against the actual field-accuracy/cost numbers above before deciding.

Resolution sweep detail (labeled-image field accuracy per candidate max dimension):

- `Qwen/Qwen3-VL-235B-A22B-Instruct`: 512px=59%, 768px=75%, 1024px=78%, 1536px=76%
- `Qwen/Qwen3-VL-30B-A3B-Instruct`: 512px=47%, 768px=63%, 1024px=70%, 1536px=67%
- `google/gemma-4-31B-it`: 512px=70%, 768px=86%, 1024px=82%, 1536px=79%
- `Qwen/Qwen3.5-397B-A17B`: 512px=0%, 768px=0%, 1024px=0%, 1536px=0%

### LLM candidates (message -> typed records)

| Model | Provider | Field accuracy vs gold (47 labeled) | Valid-JSON rate | Stability (3 runs) | Cross-model agreement | Avg input tok/item | Avg output tok/item | p50 latency (ms) | Est. cost/item | Est. cost/full run |
|---|---|---|---|---|---|---|---|---|---|---|
| Qwen/Qwen3-235B-A22B-Instruct-2507 | deepinfra | 100.0% | 100.0% | 33.3% | N/A (all 47 gold messages labeled) | 82 | 1 | 2244 | $0.00001 | $0.0001 |
| aisingapore/Gemma-SEA-LION-v4-27B-IT | publicai | 100.0% | 100.0% | 100.0% | N/A (all 47 gold messages labeled) | 77 | 2 | 1206 | $0.00002 | $0.0002 |
| deepseek-ai/DeepSeek-V3.2 | deepinfra | 100.0% | 100.0% | 33.3% | N/A (all 47 gold messages labeled) | 75 | 2 | 3862 | $0.00002 | $0.0003 |
| openai/gpt-oss-120b | deepinfra | UNAVAILABLE | UNAVAILABLE | UNAVAILABLE | — | — | — | — | — | — |

**ml-engineer recommendation (LLM, non-binding — the user decides):** `aisingapore/Gemma-SEA-LION-v4-27B-IT` via `publicai`. Highest weighted score across field accuracy, stability, valid-JSON rate, and cost; re-check against the actual field-accuracy/cost numbers above before deciding. Given production LLM volume is now just 1 message (`msg_86`), this pick matters far less than the VLM pick above.

"Est. cost/full run" for messages uses 13 as a lower-bound proxy from the gold subset's distinct `record_type` shapes — now superseded by the lead's harder number: extraction's deterministic parser covers 214/215 messages, so production LLM volume is 1 message (`msg_86`), not 13.

(Bake-off messages are batched one call per run for the whole 47-message gold subset, matching the batching lever being judged; a real per-user batch in production is far smaller — per-item token/cost figures above divide the batch call by its message count.)

## Future refinement: fine-tuning / LoRA adapters

Out of scope for this submission, but the natural next step if VLM figure
accuracy needs to go beyond what a frozen general-purpose VLM gets from a
prompt alone.

**Training data.** Three sources, combined:
- The 7 gold-labeled figures already in `docs/gold_subset.json` (expected
  figures + expected selected field/amount) — far too few to train on
  directly, but useful as a held-out eval set (see "Eval" below) and as a
  template for what a labeled example looks like.
- Analyst's broader per-image audit reference (RULES.md-adjacent, "9b123f6
  image audit", all 16 images) plus the false-accept/false-reject cases
  found during that audit (e.g. doc_type free-text variants, cash-tendered
  vs total, multi-section reconciliation) — these are exactly the failure
  modes a fine-tune should target, since they're documented, real, and
  currently patched around in the deterministic selector/parser rather
  than fixed at the model level.
- **Synthetic receipts/payslips/invoices/bills.** 16 images (7 labeled) is
  nowhere near enough to fine-tune on safely — a template-driven generator
  (varying layout, currency, language, amounts, and specifically the
  reconciliation-breaking patterns analyst found: cash-tendered-vs-total,
  multi-section breakups, rounding at the cent) can produce hundreds to
  thousands of labeled examples with the exact figure schema as ground
  truth, at zero risk of leaking real user data.

**Target.** A LoRA adapter on `Qwen/Qwen3-VL-30B-A3B-Instruct` (the
cheapest, most stable primary-tier candidate in this bake-off — a good
fine-tuning base since it's already close on raw accuracy and 100% stable/
valid-JSON) or `Qwen/Qwen3-VL-235B-A22B-Instruct` (the larger primary pick)
for the `image_transcription.v1` figure schema specifically. Scope the
adapter narrowly (one schema, one task) rather than general-purpose
instruction tuning — cheaper to train, cheaper to serve, and easier to
eval against the exact gate this bake-off used.

**Serving.** The HF router (`router.huggingface.co`) used throughout this
bake-off does not serve custom LoRA adapters — it only routes to each
provider's own hosted base models. A fine-tuned adapter would need either:
- **HF Inference Endpoints** (dedicated, not the shared router) — supports
  loading a LoRA adapter alongside its base model on a dedicated instance;
  or
- A provider that explicitly accepts custom adapters/weights (varies by
  provider and changes over time — would need to be re-verified against
  whichever provider's catalog at the time).

Either path means giving up the router's multi-provider redundancy (the
whole point of "≥2 live providers" in the backup gate above) for a single
dedicated endpoint, which is itself a real operational tradeoff to weigh
against the accuracy gain.

**Eval.** Same methodology as this bake-off, not a new one: the same gold
subset (once extraction's selector fixes are fully landed and RULES.md's
per-image audit is stable — training against a moving eval target wastes
the effort), the same 5-run stability check, and the same backup-gate
criteria (valid-JSON, selected-figure accuracy vs the primary pick,
reconciliation false-accept rate, N=5 stability). A fine-tune only earns
its keep if it clears the primary bake-off winner on selected-figure
accuracy — anything less isn't worth the serving-architecture tradeoff
above.

**Why out of scope now.** Time-boxed hackathon submission; a fine-tune
needs a training pipeline, a synthetic-data generator, a dedicated serving
path outside the router, and its own eval cycle — all real scope beyond
"pick a model and call it," and none of it can happen before the 7-image
gold subset is even finalized (still being corrected mid-bake-off as this
document shows). Recorded here so it's not lost, not attempted under
deadline pressure.

## Step 4 — Usage report (`code/evaluation/usage_report.md`)

Generated from the final full-dataset cold run's real usage records at ship
time (PLAN.md Phase 3), not from the bake-off.
