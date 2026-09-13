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

Scored against `docs/gold_subset.json` as of `3f1c26a` (message_10 corrected to `salary_first_confirmed` in `3df3082`; image_10/image_11 gold added in `3f1c26a`, **7 labeled + 9 unlabeled images** — an earlier version of this table said "5 labeled" from a stale binary whose header text hadn't picked up the image_10/11 addition; the underlying scoring already used all 7). Selected-figure/reconciliation metrics use **production's own selector code**, `buyorwait::extract::images::{select, reconciles}` (merged from main), not a bake-off reimplementation, rescored via `--rescore-from-cache true` (zero new calls) after extraction's selector fixes (`53a2c1f`, `46d2972`) landed. Nothing here is a pick — the user chooses.

**Production weight note (from the lead):** extraction's deterministic parser now covers 214/215 messages; the LLM is called for exactly 1 message (`msg_86`) in production, while the VLM is called for all 16 images. Weight the VLM table far more heavily than the LLM table when choosing — the VLM choice is the one that matters at scale.

**Caveat (image_07/11/12):** these 3 images had known selector bugs, now fixed; verdicts on them may still shift slightly as extraction's parse-layer work continues (analyst's audit: 36→58→68/100 as further currency/reconciliation tweaks land). Flagged with ⚠ below.

### VLM candidates (image -> typed figure schema)

3 finalists topped up to **N=5** (board decision.bakeoff_runs); `Qwen3.5-397B-A17B` stays at N=3 (screened out, not topped up).

| Model | Provider | Chosen res. (px) | N | Field accuracy vs gold (7 labeled) | Valid-JSON rate | All-field stability | Cross-model agreement (9 unlabeled) | Avg input tok/item | Avg output tok/item | p50 latency (ms) | Est. cost/item | Est. cost/full run (16 images) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Qwen/Qwen3-VL-235B-A22B-Instruct | deepinfra | 1024 | 5 | 74.6% | 100.0% | 100.0% | 100.0% | 936 | 185 | 8215 | $0.00035 | $0.0056 |
| Qwen/Qwen3-VL-30B-A3B-Instruct | deepinfra | 1024 | 5 | 69.8% | 100.0% | 100.0% | 100.0% | 936 | 163 | 6479 | $0.00024 | $0.0038 |
| google/gemma-4-31B-it | deepinfra | 768 | 5 | 84.1% | 100.0% | 100.0% | 100.0% | 557 | 174 | 6567 | $0.00014 | $0.0022 |
| Qwen/Qwen3.5-397B-A17B | deepinfra | 512 | 3 | 0.0% | 0.0% | 100.0% | 92.0% | 432 | 400 | 6460 | $0.00139 | $0.0223 |

**Selected-figure metrics** (lead directive: the one amount the engine's deterministic selector would hand the engine matters more than raw all-field JSON identity):

| Model | Selected-figure accuracy (of 6 checkable labeled images) | Selected-figure stability | Reconciliation pass rate |
|---|---|---|---|
| Qwen/Qwen3-VL-235B-A22B-Instruct | 83.3% (5/6) | 100.0% | 66.7% |
| Qwen/Qwen3-VL-30B-A3B-Instruct | 83.3% (5/6) | 100.0% | 16.7% |
| google/gemma-4-31B-it | 83.3% (5/6) | 100.0% | 66.7% |
| Qwen/Qwen3.5-397B-A17B | 0.0% (0/6) | 100.0% | 0.0% |

All three finalists tie on selected-figure accuracy (83.3%) and stability (100%) at N=5; gemma-4-31B-it and Qwen3-VL-235B-A22B-Instruct both reconcile at 66.7% vs Qwen3-VL-30B-A3B-Instruct's 16.7%. image_05 (`amount_due_after_date`, cutoff-date logic) is null/wrong for every primary candidate — a shared gap, not one model's weakness.

**No ml-engineer recommendation restated here** (a prior weighted auto-recommendation folding in cost/valid-JSON contradicted the lead's plain field-accuracy-then-stability ranking; removed rather than re-litigated — see the user's `[selected]` decision below, which already picked `Qwen3-VL-235B-A22B-Instruct` as primary and `gemma-4-31B-it` as escalation).

Resolution sweep detail (labeled-image field accuracy per candidate max dimension, from the N=3 screen):

- `Qwen/Qwen3-VL-235B-A22B-Instruct`: 512px=59%, 768px=75%, 1024px=79%, 1536px=76%
- `Qwen/Qwen3-VL-30B-A3B-Instruct`: 512px=47%, 768px=63%, 1024px=70%, 1536px=67%
- `google/gemma-4-31B-it`: 512px=82%, 768px=87%, 1024px=82%, 1536px=79%
- `Qwen/Qwen3.5-397B-A17B`: 512px=0%, 768px=0%, 1024px=0%, 1536px=0% (thinking model; see Backup models section — same failure mode as the Kimi-K3 backup candidate)

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

## Backup models (user request: a "close to a guarantee" tiebreaker/fallback)

Separate from the primary bake-off above; catalog lives in `code/config/models.toml`'s `[[fallback.candidates]]` (activation only via `[selected].vlm_fallback`/`.llm_fallback`, never automatic). The user set a 5-criterion gate (board decision.backup_gate), **all required**:

1. Valid-JSON 100% over N=5
2. 0 false-accepts on the audit set (reconciles=true but the selected amount is actually wrong)
3. Selected-figure accuracy ≥ the best primary VLM candidate's (83.3%, see above)
4. N=5 stability on the selected amount
5. ≥2 live providers for the model (so one outage doesn't break it)

### ⚠️ Credit exhaustion mid-run — data below is incomplete

Mid-testing, the shared `HF_TOKEN` **fully depleted its monthly included
credits** (`HTTP 402: "You have depleted your monthly included credits.
Purchase pre-paid credits to continue using Inference Providers."`).
Confirmed **account-wide, not provider-specific**: a direct probe against
`openai/gpt-oss-120b` via a completely different provider (`novita`) hit the
same 402. This blocks **all further live model calls team-wide**, including
the eventual production cold run, until pre-paid credits are added or the
monthly cycle resets. Posted as an urgent blocker (bus topic `blocker`).

Every number below is genuine (real API responses, no fabrication), but
several runs were cut short mid-way by the credit wall. Where that
happened it's called out explicitly, and stability numbers from a
`--rescore-from-cache` pass are marked as **not real N=5 stability** — a
cache-replay of the same cached response 5x is trivially "stable" and
proves nothing about actual run-to-run variance.

**All three runs are stopped** (verified: no `bakeoff.exe` process running).
No retry loop risk: 402 is a plain 4xx, classified `Fatal` not `Retryable`
in `hf.rs` — it was never retried within a call, only the circuit breaker's
"3 consecutive failures → mark unavailable, stop" fired, so no attempts or
rate-limit budget were burned looping. Caches (`store/model_cache`,
`store/gate_cache`, `store/frontier_cache`) are untouched and intact for a
`--rescore-from-cache` resume — nothing needs to be redone from zero.

**Exact progress per run, for a `--rescore-from-cache` resume once credits return:**

| Run | Config | N | Images | Progress when the wall hit |
|---|---|---|---|---|
| Sign-off audit (#215) | Kimi-K3, max_tokens=1500, `store/model_cache` | 5 | 02, 05, 10, 11 | Runs 1–3/5 complete (12/20 calls); run 4 failed on its first call (image_02) |
| Gate (K3+K2.6) | max_tokens=2000, `store/gate_cache` | 5 | all 16 | Kimi-K3: run 1/5 complete + ~4 images into run 2 (~20/80 calls); Kimi-K2.6: ~2 images into run 1 (~2/80 calls) |
| Frontier | Kimi-K3 VLM @400, 3 LLM candidates, `store/frontier_cache` | 3 | all 16 (VLM), 47 msgs (LLM) | Kimi-K3 VLM: **complete**, all 3/3 runs (unaffected — finished before the wall). LLM: DeepSeek-V4-Pro-0813 **complete** 3/3; GLM-5.3 and Kimi-K3-as-LLM hit the wall partway, both circuit-broken |

**Resume order once the user confirms credits (per the lead):** #215 audit
first (2 more runs × 4 images = 8 calls) — it's sign-off-critical — then
the gate run's remainder (Kimi-K3: 3 more runs × 16 = 48 calls;
Kimi-K2.6: needs essentially a full re-run, ~5 × 16 = 80 calls, since it
has no usable data yet).

**Estimated credits still needed** (rough, from the token/cost rates
already observed in this document; actual mix will vary per-image):

| Item | Calls | Est. tokens | Est. cost |
|---|---|---|---|
| Finish #215 audit (Kimi-K3, 2 more runs × 4 images) | 8 | ~22,900 (in 1414 + out 1445 per call, observed rate) | ~$0.20 |
| Finish gate run (Kimi-K3: 3×16; Kimi-K2.6: 5×16) | 128 | ~330,000 (Kimi-K3 at observed 1322in/1258out; Kimi-K2.6 estimated similar profile, unverified — it has zero data so far) | ~$1.10 (K3) + ~$0.55 (K2.6, priced lower at $0.75/$3.50) ≈ **~$1.65** |
| Final cold run — VLM, 16 images × 2 readers/class (`[vlm_routing]` above) | up to 32 | ~600–900in/175–185out per 235B/gemma call (cheap primary/escalation pair); **any image classified `pending_bill_due_date` currently reads vlm_fallback = Kimi-K3 (fails the gate, ~7–8x the token cost of a normal reader) until that's replaced** | ~$0.05–$0.15 if the fallback reader is swapped to something cheap before the final run; meaningfully higher (**+$0.02–0.04/image** on that class) if Kimi-K3 stays as-is |
| Final cold run — LLM, `msg_86` only | 1 | ~75–85in/2–3out (observed primary-LLM rate) | <$0.001 |
| **Total estimate** | ~169 | ~380,000 | **~$2–3**, most of it the two Kimi test completions, not the final run itself |

This assumes the routing table's fallback reader gets swapped to a
gate-passing candidate before the final cold run — if `vlm_fallback`
stays `Kimi-K3` as currently configured, every `pending_bill_due_date`
event pays Kimi-K3's per-token rate instead of a primary-tier rate, which
is exactly the risk the backup gate exists to catch.

### Candidate selection

`moonshotai/Kimi-K3` (multimodal, covers both LLM and VLM fallback roles,
already the routing table's tiebreaker pending this test) plus the
strongest other live multimodal candidate found: `moonshotai/Kimi-K2.6`.
`zai-org/GLM-4.6V` and `baidu/ERNIE-4.5-VL-424B-A47B-Base-PT` were
considered but **disqualified before testing** — both have exactly 1 live
provider on the router, failing gate criterion 5 outright.

### Gate results: VLM role

| Model | Live providers | Criterion 1: valid-JSON@N=5 | Criterion 2: 0 false-accepts | Criterion 3: selected-fig. acc ≥ 83.3% | Criterion 4: real N=5 stability | Criterion 5: ≥2 providers | **Gate verdict** |
|---|---|---|---|---|---|---|---|
| `moonshotai/Kimi-K3` | 5 (deepinfra, together, fireworks-ai, baseten, featherless-ai) | **FAIL** — 25% on the 4-image sign-off audit (max_tokens=1500), 68.8% on the 16-image gate sample (max_tokens=2000); never reached 100% at any tested budget | **FAIL** (moot — never reaches a checkable selected figure reliably enough to even test this) | **FAIL** — 33.3% best sample (2/6, max_tokens=2000), 0.0% on the audit subset (max_tokens=1500) | Not measurable (credit wall hit before a real N=5 completed; only cache-replay data exists) | **PASS** | **FAILS THE GATE** |
| `moonshotai/Kimi-K2.6` | 5 (novita, fireworks-ai, featherless-ai, baseten, deepinfra) | **UNTESTED** — 0 successful calls before the credit wall hit | **UNTESTED** | **UNTESTED** | **UNTESTED** | **PASS** | **BLOCKED, not evaluated** |

**Why Kimi-K3 fails, in the model's own words:** it is a thinking/reasoning
model. At the harness default (`max_tokens_vlm=400`), it burns the entire
budget on hidden `reasoning_content` before ever emitting visible
`content` — confirmed by direct probe (finish_reason `"length"`, `content`
empty, `reasoning_content` populated). At `max_tokens=1500` on `image_01`
specifically, it produces exact, correct JSON (`net_pay: 4365000`, exact
match) — proving it *can* work. But on the harder 4-image sign-off set
(`image_02`, `image_05`, `image_10`, `image_11` — the cases the analyst
flagged, board decision #215) at that same 1500-token budget, only 1 of 4
images produced valid JSON at all; the other 3 were still truncated
mid-reasoning. Output tokens (including reasoning) ran **1,258–1,445
tokens/item** even when it succeeded — 7–8x every primary VLM candidate's
budget, at 6–19x the per-token price. `image_05` (`amount_due_after_date`,
the cutoff-date case the lead/analyst asked about by name) never produced
a usable figure at any tested budget, for either Kimi model or, notably,
**any of the three primary candidates either** — this is a shared gap in
the current selector/prompt for that one case, not Kimi-specific.

**Per the lead's contingency:** since Kimi-K3 (the routing table's current
tiebreaker) fails the gate, `moonshotai/Kimi-K2.6` becomes the candidate —
but it cannot be evaluated at all right now; every one of its calls in the
gate run hit the credit wall before a single response came back. **No VLM
candidate on the HF router currently clears this gate with the available
evidence.** The user has a parked non-HF option (board
decision.claude_backup) if a guarantee-grade fallback is needed sooner
than credits can be restored and Kimi-K2.6 retested.

### Gate results: LLM role (secondary — msg_86 only in production)

| Model | Live providers | Field accuracy (47 labeled) | Valid-JSON@N=3 | Notes |
|---|---|---|---|---|
| `deepseek-ai/DeepSeek-V4-Pro-0813` | 5 (novita, together, fireworks-ai, baseten, deepinfra) | 100.0% | 100.0% | Completed cleanly before the credit wall hit. Fast (p50 1.4s), cheap ($0.0013/full-run at the 13-skeleton estimate). Same DeepSeek lineage as the primary LLM candidate (DeepSeek-V3.2) — known vendor behavior. Strongest LLM backup candidate tested. |
| `zai-org/GLM-5.3` | 5 (novita, together, fireworks-ai, zai-org, baseten) | 100.0%¹ | **0.0%** | ¹ Vacuous: field accuracy defaults to 100% when there is nothing to check (0 valid parses), not a real pass. p50 latency 54.3s/call — reasoning-model-scale latency with no successful output. Effectively failed. |
| `moonshotai/Kimi-K3` | 5 (see above) | UNAVAILABLE | UNAVAILABLE | Hit the credit wall before completing. |

**Reading this table:** `DeepSeek-V4-Pro-0813` is the only LLM backup
candidate with a clean, real pass so far. Not gated as strictly as VLM
(lead: LLM backup is lower-stakes — msg_86 is the only production LLM
call), but the same "no fabricated confidence" rule applies: GLM-5.3's
100% is not real and Kimi-K3-as-LLM is simply untested.

### Recommendation (non-binding — the user decides)

1. **Do not activate `vlm_fallback = Kimi-K3`** in `[selected]` — it fails
   the gate on real evidence, not merely "untested."
2. **Kimi-K2.6 is the next candidate per the lead's contingency**, but
   needs actual testing once credits are restored — right now it has
   zero data, which is not the same as passing.
3. **If a backup is needed before that retest can happen**, the user's
   parked Claude-adapter option (board decision.claude_backup) is the only
   currently-known path that doesn't depend on the same depleted HF
   account.
4. For LLM, `DeepSeek-V4-Pro-0813` is a solid backup candidate on the
   evidence gathered so far, though LLM backup selection is lower-stakes
   given msg_86 is the only production LLM call.

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
