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

**INTERIM — VLM resolution-sweep results (N=3 run in progress, main
stability pass not yet finished).** Posted early per lead request; this
section will be overwritten by the harness with the full table (valid-JSON
rate, stability, cross-model agreement, tokens, latency, cost) as soon as
the main pass completes. Scored against `docs/gold_subset.json` as of
`3f1c26a` (7 labeled + 9 unlabeled images; message_10 corrected in `3df3082`).

Resolution-sweep field accuracy vs the 7 labeled images (1 run per
resolution, `temperature=0`):

| Model | Provider | 512px | 768px | 1024px | 1536px | Notes so far |
|---|---|---|---|---|---|---|
| `Qwen/Qwen3-VL-235B-A22B-Instruct` | deepinfra | 58.7% | 74.6% | 77.8% | 76.2% | Clean run, no failures |
| `Qwen/Qwen3-VL-30B-A3B-Instruct` | deepinfra | 46.8% | 62.7% | 69.8% | 66.7% | Clean run, no failures |
| `google/gemma-4-31B-it` | deepinfra | 69.8% | 85.7% | 81.7% | 79.4% | Best accuracy so far, but frequent transient network errors from this provider during the run (retried and recovered so far; circuit breaker will mark it `unavailable` if it hits 3 consecutive failures) |
| `Qwen/Qwen3.5-397B-A17B` | deepinfra | 0.0% | 0.0% | 0.0% | 0.0% | **0% at every resolution** — producing no field matches at all (likely invalid-JSON or empty output, not just low accuracy; not a network issue). This is itself a real bake-off finding for the "best Qwen3.5/3.6 multimodal" candidate, not noise. Full report will show its valid-JSON rate directly. |

**ml-engineer's read so far (non-binding, main pass still running):**
`google/gemma-4-31B-it` is currently the accuracy leader (peak 85.7% at
768px) but the least reliable network-wise on this provider;
`Qwen/Qwen3-VL-235B-A22B-Instruct` is close behind (77.8% at 1024px) with
zero failures so far — a strong, more stable second read.
`Qwen/Qwen3.5-397B-A17B` looks non-viable pending the valid-JSON number in
the full table.

Full table (valid-JSON, stability at N=3, cross-model agreement, tokens,
latency, cost) below once the main pass finishes.

## Step 4 — Usage report (`code/evaluation/usage_report.md`)

Generated from the final full-dataset cold run's real usage records at ship
time (PLAN.md Phase 3), not from the bake-off.
