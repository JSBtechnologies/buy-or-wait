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

## Step 4 — Usage report (`code/evaluation/usage_report.md`)

Generated from the final full-dataset cold run's real usage records at ship
time (PLAN.md Phase 3), not from the bake-off.
