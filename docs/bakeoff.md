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

## Step 3 — Bake-off run (`code/src/bin/bakeoff.rs`)

Blocked on `docs/gold_subset.json` and prompts from extraction (bus topic
`extract`). Plan: run every candidate above on the identical fixed subset with
identical prompts, 5 times each at `temperature=0`, sweeping image resolution
(512/768/1024/1536 px max dimension) for the VLM candidates. Report per model:

- valid-JSON rate
- field accuracy vs gold
- agreement on unlabeled items (cross-model)
- stability rate (identical parsed output across the 5 runs) — reproducibility
  is a selection criterion, not just accuracy
- avg input/output tokens per item
- latency
- cost
- smallest image resolution that keeps gold accuracy (per VLM candidate)

Results table to be filled in after the run.

## Step 4 — Usage report (`code/evaluation/usage_report.md`)

Generated from the final full-dataset cold run's real usage records at ship
time (PLAN.md Phase 3), not from the bake-off.
