# PLAN.md — Buy or Wait? build plan (atrium fleet `buyorwait`)

Shared context for every agent. Read fully before acting. `AGENTS.md` and `problem_statement.md` are the spec; this file is how we build it.

Deadline: **2026-09-13T12:30:00Z** (6:00 PM IST). Target a submittable build by 10:30Z, leaving a 2h buffer.

## 1. Goal

Produce `output.csv` (repo root) with one row per request in `dataset/requests.csv` (request_26–request_275), matching hidden labels. The labels come from a **deterministic simulator**: hand-replaying request_03 with recurring-cadence projection + max-of-last-3 variable spend gave 872,452.6 vs label 873,000. Exact rule recovery is what wins the score. Models are only for evidence the engine cannot read directly.

## 2. System design (the user's design — implement it as written)

### 2.0 Framing

A user asks "can I afford this?" The system must answer exact fields: `amount_safe_to_pay`, `affordability_status`, `recommended_payment_method`, `payment_plan`, `earliest_date_for_full_payment`, plus `spending_changes_needed` and a short `decision_explanation`. Every one of those is **arithmetic, dates, and ranking**. So the core is a **deterministic engine**, and every calculation is deterministic. Language models sit off to the side as interpreters of unstructured input. They never compute, never decide, and never see the request decision.

```
PREPROCESSING (before any request arrives, per user)
  financial_events ──► Ledger reconstruction ──► Recurring-stream detection
  images ──► VLM → typed figure schema ──► deterministic figure selector ──► reconciliation check ──┐
  messages ──► LLM → typed records (0..n per message) ──► validation ──────────────────────────────┤
                                                                                                    ▼
                                                                         amended Ledger (per user)
REQUEST TIME (session bound to one user_id)
  request ──► (text: LLM → 4-field struct | batch: CSV columns) ──► 90-day forecast ──► two hard numbers ──► plan search ──► ranking ──► output row + templated explanation
```

### 2.1 Preprocessing: ledger reconstruction (deterministic)

Every user's financial history is rebuilt into a ledger by applying cash rules by `event_type` and `status`:

| Row kind | Cash treatment |
|---|---|
| `settled` | Counts |
| `pending` debit | **Reserved** (treated as money already gone) |
| `pending` credit (refund, bonus, commission, payout) | **Never counts** until it settles |
| `unrealized` investment valuation (`non_cash`) | **Never counts** |
| `cancelled`, `failed` | **Dropped** |
| `scheduled` | Counts on its settlement date (e.g. next confirmed salary, scheduled bill) |

De-duplication:
- **Linked events count once.** Authorization → later settlement: only the settlement counts. Failed → scheduled retry: only the retry counts. Walk `linked_event_id` chains and keep the terminal cash state.
- **A pending charge flagged as a duplicate of a settled one is ignored.** The flag may come from a message, e.g. a bank note that a matching debit/credit was a transfer between the user's own accounts.

This separation matters because the mix of pending, scheduled, failed, and cancelled statuses is where naive balance math goes wrong.

### 2.2 Preprocessing: recurring-stream detection (deterministic)

Streams are detected from history using three signals together:
1. **Description**: what the bill is ("Landlord standing order", "Payroll credit").
2. **Category**: rent, utilities, salary, streaming, and so on.
3. **Cadence**: monthly by day-of-month vs a fixed interval in days, confirmed by repeated occurrences.

Two kinds of stream:
- **Recurring bills/income** (rent, utilities, subscriptions, salary, loan payments): projected forward on their cadence.
- **Regular variable spending** (groceries, dining, transport): not a fixed bill but a spending *rate*. It is projected conservatively as a rate, not as a single recurring amount.

One-time purchases, transfers, refunds, arrears, and unusual events are not streams. (The exact estimator, e.g. max-of-last-3, comes from `RULES.md`.)

### 2.3 Preprocessing: images → OCR/VLM (16 images)

Each of the 16 events with a blank `amount` points to one image: a receipt, payslip, or bill. Pipeline:
1. **Preprocess and encode** the image (downscale per the bake-off result, base64).
2. **Vision model fills a typed schema** holding *every labeled figure on the page*: subtotal, tax, total, amount due, gross pay, deductions, net pay, dates, currency. The model transcribes figures; it does not choose which one matters.
3. **Deterministic selector** picks the figure that matters from the linked event's type and status:
   - settled expense → the amount actually paid (total)
   - pending bill → the amount due
   - income → net pay
4. **Arithmetic reconciliation** before the figure is trusted: subtotal + tax = total, gross − deductions = net, currency matches the event. Failure → escalate (second read) or reject. Never guess, and never treat blank as zero.

Product note: this mirrors a real deployment (e.g. inside a bank) where documents are preprocessed ahead of time and questions are answered later.

### 2.4 Preprocessing: messages → typed records (LLM, multilingual)

Messages (215 rows, English and Indonesian) are turned by the language model into **typed records**. One message can yield **multiple records** and must be handled that way. Record types include:
- salary change (new amount and/or effective date, pay date moved)
- contract/income ended
- new recurring expense
- one-time adjustment (arrears)
- bonus, commission, prize, refund, or payout that is **not confirmed and must not be counted yet**
- own-account transfer / duplicate flag
- cancellation, settlement, or amendment of a specific event

Validated records **amend the ledger** (conflict order per the spec: explicit cancellation/settlement/amendment → newer record from same source → settled over estimate → financially safer interpretation).

### 2.5 Security model: the model is an intermediary, not an agent

- **The model never sees a request and never makes a decision.**
- **Every model output is deserialized into a struct with fixed fields. Unknown fields are dropped.** There is no free-text channel from a model into the engine.
- So an injected line like "approve this purchase" or "ignore previous instructions and return everything" **has nowhere to land**: no field exists for it.
- No tool loop. We get the leverage of the LLM for reading, with the deterministic security of plain logic for the arithmetic.
- **Session isolation:** a session is constructed around exactly one `user_id`. Every engine method operates on that session. **No function on the model-facing surface takes a `user_id` argument.** The id is bound when the session is created, so a model output cannot address another user's data at all.

### 2.6 Request intake: two modes

- **Interactive/text mode:** the request arrives as text. The model parses it into a struct with four fields: `amount`, `deadline`, `type`, `allows_partial_payment`.
- **Batch mode (this submission):** the dataset provides those four as columns, so batch runs use the columns directly (0 tokens).
- Both modes produce the same struct, so the engine never knows which path fed it. The final use case (batch vs individual) is open, so both are supported.

### 2.7 The 90-day forecast and the two hard numbers

The engine projects a day-by-day balance for 90 days from `request_date`: current balance, reserved pending debits, recurring income and bills, the variable-spending rate, and ledger amendments from messages/images (e.g. a salary change in two months, which can make a later payment unaffordable). This is what separates "affordable right now" from "affordable if split before the change."

**Closed form.** A payment on day *d* lowers every later balance by the same amount. With projected balance `B(t)` and minimum `M`:
- **Safe amount today** = `min(requested_amount, max(0, min over t in [request_date, horizon] of (B(t) − M)))`
- **Forecast minimum from day d** = `min over t in [d, horizon] of (B(t) − M)`, a suffix minimum computed once.
- **Earliest date for full payment** = first `d` where the suffix minimum from `d` ≥ `requested_amount`. Empty if none within the horizon.

Any multi-payment plan is checked the same way: subtract each payment's cumulative effect from `B(t)` from its date onward, and require the result to stay ≥ `M` everywhere.

### 2.8 Plan search and ranking

Candidates per request:
1. **Full payment** today, if it is safe.
2. **Each installment option** that survives the user's `payment_methods_user_will_consider` and `max_installment_months`, using the option's exact schedule.
3. **Two-payment partial split**: `amount_safe_to_pay` today, the remainder on `earliest_date_for_full_payment`, only if allowed, accepted, and by the deadline.
4. **Wait** until the earliest safe date (requires the user to accept full payment).
5. Spending-change variants (up to 3 stop/reduce actions on flexible, permitted, non-protected events) when no plan works without them.

**Evaluation of candidates:** every candidate is run through the forecast. **Unsafe candidates are dropped.** The survivors are **sorted by the ranking key**, and the first one is the recommendation. It works like a priority queue, but a plain sort on a lexicographic key is enough at this size and keeps it simple and deterministic: completes by deadline → no spending changes → lowest total paid → earlier start → fewer payments → lowest `payment_option_id`. Nothing safe survives → `not_recommended`. The explanation is a deterministic template filled from the chosen plan's numbers.

### 2.9 Crate layout — all Rust, one crate at `code/`

```
code/
  Cargo.toml              integrator
  src/main.rs             integrator   CLI: batch run → ../output.csv + evaluation/usage_report.md
  src/lib.rs              integrator   module declarations only
  src/model.rs            integrator   CSV row structs + loaders for every dataset file
  src/hf.rs               ml-engineer  HF Inference Providers client (reqwest, blocking)
  src/bin/bakeoff.rs      ml-engineer  model bake-off harness
  src/extract/            extraction   retrieval index, template induction, typed schemas (§2.3, §2.4, §2.6),
                                       figure selector + reconciliation, validation/grounding
  prompts/                extraction   versioned prompt files (shipped)
  config/models.toml      ml-engineer  chosen models, providers, decoding params, pricing (no secrets)
  src/engine/session.rs   engine       user-bound session (§2.5)
  src/engine/ledger.rs    engine       cash rules, de-dup, amendments (§2.1)
  src/engine/recurrence.rs engine      stream detection (§2.2)
  src/engine/forecast.rs  engine       90-day projection, safe amount, suffix minimum (§2.7)
  src/engine/plans.rs     engine       candidates + ranking (§2.8)
  src/engine/explain.rs   engine       explanation templates
  src/evaluation/         verifier     invariant checks, output-contract validator, sample scorer, independent replay (§2.10)
  src/store/              integrator   processed-data store: cache + persisted preprocessing outputs (§2.11)
  store/                  (generated)  on-disk processed dataset; gitignored, rebuilt by the pipeline
  evaluation/usage_report.md           ml-engineer (numbers from the final run)
  README.md                            integrator
RULES.md                  analyst      reverse-engineered numeric rules (estimators, rounding, cadence thresholds, templates)
docs/bakeoff.md           ml-engineer  bake-off results table
docs/gold_subset.json     extraction   the fixed bake-off subset + expected values
```

- Model calls: `POST https://router.huggingface.co/v1/chat/completions` (OpenAI-compatible). Images inline as `data:image/png;base64,...`. `temperature: 0`, fixed `seed`. Token read from env `HF_TOKEN` only — never logged, printed, or committed.
- Record `usage.prompt_tokens` / `usage.completion_tokens` per call for the usage report.
- Disk cache keyed per §2.11 so reruns are deterministic and free.
- No local inference.
- **Language rule (user decision):** the system is 100% Rust. Any agent may use **Python for throwaway exploration** (checking a hypothesis, profiling data, replaying a sample by hand). Scratch scripts live in `scratch/` in the agent's own tree. That folder is gitignored, never committed, never called by the Rust system, and never shipped in code.zip. If an exploration result matters, it goes into `RULES.md` or a Rust test, not into a Python file.

### 2.10 Invariants and decision facts: nothing unverified leaves the engine

**Assert every invariant before a row is written.** A violation is a **hard error**, never a silent clamp or fix-up. Because all arithmetic is deterministic, a violation always means a bug, and we want to see it. Invariants include:
- `0 ≤ amount_safe_to_pay ≤ requested_amount`
- `affordable_now` ⇒ `earliest_date_for_full_payment == request_date` and method `full_payment`
- the recommended plan, replayed against the forecast, keeps the balance ≥ `minimum_balance_to_keep` on every day of the horizon
- `payment_plan` is chronological, and its amounts sum to what the method requires
- `partial_payment` ⇒ exactly 2 payments, the first = `amount_safe_to_pay` on `request_date`, the second on `earliest_date_for_full_payment` ≤ `desired_completion_date`, sum = `requested_amount`, allowed by the request and accepted by the user
- installments ⇒ schedule exactly equals a supplied option that respects preferences and `max_installment_months`
- spending changes ⇒ ≤ 3, only flexible events in permitted non-protected categories, no stop + reduce on the same event, `reduce_to` ≥ `minimum_allowed_amount`
- enum values valid, and the column set and order exact

**Every decision carries its facts.** The engine records a `DecisionFacts` struct for each request: starting balance, reserved pending total, projected **trough balance** and **trough date** (the lowest point of the forecast), headroom above the minimum, safe amount, earliest safe date, which candidates were dropped and why, and the winning ranking key. The explanation is rendered only from these facts. So the explanation can never contradict the numbers: no model computed them.

**Score it.** The evaluation module scores engine output against `sample_requests.csv` field by field, and runs an independent replay of the forecast for every recommended plan, so correctness is measured, not assumed.

### 2.11 Caching and the processed-data store

**Cache every model output**, keyed by:

```
key = sha256( content_hash(input text or image bytes) + model_id + model_revision + prompt_version )
```

- `model_revision` is the pinned HF model commit/revision, so a model update never serves stale results.
- `prompt_version` comes from the versioned prompt file, so a prompt edit invalidates only its own entries.
- Payoff: speed, efficiency, and zero cost for a repeated question or document. Similar (not identical) inputs are handled one level up by template induction (§3 Batching): messages that normalize to the same skeleton share a parse.

**Persist processed data.** Preprocessing outputs are stored, not recomputed per request: per-user reconstructed ledgers, detected streams, extracted image figures, typed message records, plus the model-output cache. Together they form a processed dataset that request time reads from. This is also the path to the product use case: documents are preprocessed ahead of time, and questions are answered later. The store is rebuildable from `dataset/` at any time. The final submission run rebuilds it from empty, so `usage_report.md` reflects real calls.

### 2.12 Three components

The system is split into three components, and the scaffold is built along the same lines:
1. **engine**: ledger, recurrence, forecast, plans, explanation, session (`src/engine/`)
2. **extract**: retrieval, images, messages, request intake, HF client, prompts (`src/extract/`, `src/hf.rs`, `prompts/`)
3. **evaluation**: invariants, contract validation, sample scoring, replay, usage report (`src/evaluation/`, `evaluation/`)

Shared plumbing (`model.rs`, `store/`, `main.rs`) belongs to the integrator.

## 3. The eight judged levers — how we win each one

The challenge explicitly rewards retrieval, multimodal interpretation, financial-state reconstruction, plan generation, deterministic verification, batching, caching, and token efficiency. A plain "LLM agent with some deterministic parts" is the baseline we must beat. Design principle: **the deterministic Rust core decides everything; models only turn unstructured evidence into typed facts, and every model output is checked before use.** Accuracy comes first; efficiency is how we get it for the fewest tokens.

| Lever | What we build | Owner | Metric we report |
|---|---|---|---|
| Retrieval | Per-request evidence index: messages/images by `request_id`, `related_event_id`, `user_id`, only `sent_at` ≤ request date. Only evidence that can change a forecast fact reaches a model. | extraction | evidence items sent to a model / total items |
| Multimodal interpretation | VLM reads each receipt into strict JSON `{amount, currency, date, doc_type}`. Output is cross-checked: currency matches the event, amount is plausible against that user's history for the same description/category, date is near the event. Escalate to a second read only when a check fails. | extraction + ml-engineer | field accuracy on gold; escalation rate |
| Financial-state reconstruction | Deterministic: lifecycle resolution of linked rows (authorization → settlement, failed → retry, refund), duplicate/own-account transfer removal, status handling, FX by settlement date and direction, recurrence detection, conservative variable-spend estimation. Conflict order from the spec. | engine (rules from analyst) | sample accuracy of amount_safe_to_pay |
| Plan generation | Enumerate every candidate exhaustively (full, partial, each installment option, wait, spending-change sets up to 3), simulate each day by day, apply the spec's ranking. No model involvement. | engine | sample accuracy of method/plan/date/changes |
| Deterministic verification | (a) Output-contract validator. (b) Independent balance replay for every recommended plan. (c) Grounding checks on model facts: every extracted number must literally appear in the source message text or pass the image cross-check, otherwise the fact is rejected. (d) Injection guard: embedded instructions never become facts. | verifier (+ extraction for c/d) | contract violations = 0; rejected-fact count |
| Batching | Template induction: messages are heavily templated (same sentences, different employer/amount/date, EN + ID). Mask numbers, names, and dates to get template skeletons. Parse known skeletons deterministically with zero tokens. Send the model one call per *unseen skeleton* (or per user batch), never one call per message. One call per image. | extraction | model calls / total messages |
| Caching | sha256(model + prompt version + input) disk cache. Stable system-prompt prefix first so provider prefix caching applies. Prompt files are versioned so a prompt change invalidates only its entries. | ml-engineer | cache hit rate on rerun |
| Token efficiency | No explanations from models: `decision_explanation` comes from deterministic templates filled with computed numbers (0 tokens, always consistent). Compact inputs (message text only, no CSV context). Enum-coded JSON outputs. Tight `max_tokens`. Images downscaled/cropped to the minimum resolution that keeps gold accuracy. Request text goes to a model only if the analyst proves it carries information the columns lack. | extraction + ml-engineer | avg input/output tokens per request |

Prompts and model configuration live as files in `code/prompts/` and `code/config/models.toml` (both ship in code.zip, as the submission requires). No prompt strings hardcoded in Rust.

The usage report must describe the **final full-dataset run that produced output.csv**. That run starts from an empty cache so the report reflects real calls. Cache hit rate is reported separately from a second run.

## 4. Ownership (single owner per file — the merge-safety invariant)

Touch only files you own. If you need a change in someone else's file (a new dependency in `Cargo.toml`, a field in `model.rs`, a schema change), post on bus topic `blocker` naming the owner and wait. Do not edit it yourself.

| Agent | Model | Owns | Tree |
|---|---|---|---|
| lead | opus | Task assignment, work order, unblocking, decisions. Writes no code. The user supervises through this pane. | main |
| analyst | opus | `RULES.md` | worktree `analyst` |
| engine | opus | `code/src/engine/` | worktree `engine` |
| extraction | sonnet | `code/src/extract/`, `code/prompts/`, `docs/gold_subset.json` | worktree `extraction` |
| ml-engineer | sonnet | `code/src/hf.rs`, `code/src/bin/bakeoff.rs`, `code/config/models.toml`, `docs/bakeoff.md`, `code/evaluation/usage_report.md` | worktree `ml-engineer` |
| verifier | opus | `code/src/evaluation/` | worktree `verifier` |
| integrator | sonnet | `code/Cargo.toml`, `code/src/main.rs`, `code/src/lib.rs`, `code/src/model.rs`, `code/src/store/`, `code/README.md`, merges, `output.csv`, `code.zip` | main |

## 5. Work order: three phases (the user's plan)

### Phase 1: Scaffold (three components)

Scaffold along the three components of §2.12. Every agent maps to a subsystem, or its task feeds one:

| Component | Agents working on it |
|---|---|
| engine | engine (code), analyst (rules the engine implements) |
| extract | extraction (pipeline), ml-engineer (models, HF client, cache) |
| evaluation | verifier (invariants, scoring, replay), ml-engineer (usage report) |
| shared plumbing | integrator (crate, model.rs, store/, main.rs) |

- **integrator (~20 min, blocks code work):** create the crate with every expected dependency (reqwest blocking+json+rustls-tls, serde, serde_json, csv, chrono, base64, sha2, anyhow), `lib.rs` module declarations, empty compiling stubs for every owned path, `model.rs` loaders for all dataset CSVs, and a `--cold` CLI flag stub (Phase 3). `cargo build` green, commit on main, publish `scaffold msg=done`. Every worktree agent then runs `git merge main` before writing code.
- **In parallel (no scaffold needed):** analyst starts reverse-engineering the samples. extraction builds `docs/gold_subset.json`: all 16 images (5 sample-user images with expected values derived from sample outputs/history, 11 unlabeled for cross-model agreement) plus ~30 messages covering every type in English and Indonesian. extraction also drafts schemas and prompts.

### Phase 2: Build the subsystems, in dependency order

**2a. Ledger (preprocessing), engine + analyst.** The ledger is the foundation: it holds every event and records how the data breaks down and what actually happened (cash state, links, duplicates, amendments). It must be a durable, reusable data model, since it is persisted in the store (§2.11) and used again later, not a throwaway intermediate. Gate: sample users' ledgers show correct status handling and no duplicate cash effects.

**2b. Forecast, engine.** Built on the ledger: recurring streams, variable-spend rates, 90-day projection, safe amount, trough, suffix minimum, earliest date (§2.7). Gate: the verifier's independent replay agrees with the engine on the samples.

**2c. Request understanding and retrieval, extraction + engine.** Verify and search what the request is asking: the amount, the deadline ("buy this by that date"), the type, whether partial payment is allowed, and which evidence (messages, images, events) is relevant to it. Batch mode uses the columns. Interactive mode parses text into the same 4-field struct (§2.6). Then plan search and ranking (§2.8) run on top.

**2d. VLM/LLM processing, extraction + ml-engineer.** The image and message pipelines must fit the ledger schema cleanly. Model selection is a bake-off on one fixed subset with identical prompts:
- VLM candidates: Qwen/Qwen3-VL-235B-A22B-Instruct, Qwen/Qwen3-VL-30B-A3B-Instruct, google/gemma-4-31B-it, one Qwen3.5/3.6 multimodal model.
- LLM candidates: Qwen/Qwen3-235B-A22B-Instruct-2507, aisingapore/Gemma-SEA-LION-v4-27B-IT, deepseek-ai/DeepSeek-V3.2, openai/gpt-oss-120b.
- Confirm each model id is live on the router before running. Pin the provider and the model revision.
- **Reproducibility is a selection criterion. One correct read is not enough.** Run each candidate on the subset **5 times** at temperature 0 and report the stability rate (identical parsed output across runs), alongside valid-JSON rate, field accuracy vs gold, cross-model agreement on unlabeled items, tokens per item, latency, and cost.
- Write `docs/bakeoff.md`. **The lead presents results to the user, and the user makes the final VLM and LLM choice.** Changing prompts or schemas after the bake-off is fine, but any change is re-validated the same way.

### Phase 3: Run, validate, report

- **Full run** of the complete pipeline over all 250 requests.
- **Validate that everything makes sense:** invariants (§2.10) pass, and verifier scores request_01–18 (tuning) and request_19–25 (held out, scored only by the verifier). Mismatches go to engine/analyst/extraction and loop until the score plateaus.
- **Cold mode:** `--cold` ignores every cache and stored artifact and recomputes from `dataset/`. The **final run that produces output.csv is a cold run**. A warm rerun must produce a byte-identical `output.csv` (determinism check) and reports the cache hit rate.
- **The usage report is part of the output:** `evaluation/usage_report.md` with providers, models, calls, input/output tokens, total and **per-request tokens and cost** (each user has exactly one request, so preprocessing calls are attributed to that user's request), and per-model plus overall totals.
- **Ship (integrator):** merge, README with build/run commands, `code.zip`. verifier signs off `output.csv` against the contract.

### Known risks to watch (from the user's review of the data)

| Risk | What to do | Owner |
|---|---|---|
| **Data states** (pending, scheduled, failed, cancelled, unrealized) | Apply the §2.1 cash rules exactly. Test each status on sample users. | engine, analyst |
| **Duplicates** | No double counting: linked lifecycles, authorization→settlement, failed→retry, own-account transfers, pending copies of settled rows. The verifier checks for duplicate cash effects. | engine, verifier |
| **Rounding** | **Accuracy first:** recover the exact rounding rule from the samples (e.g. 873,000 / 28,820 / 8,401,800 vs unrounded values like 87,170.56). Keep rounding in one function so it is easy to change. | analyst, engine |
| **Cold mode** | Must run with no cache at all and produce output identical to a warm run. | integrator, ml-engineer |
| **user_03 salary oddity** | event_253 "August 2019 net salary" has a blank amount (image_01). The same month has event_211 "Promotion arrears payment", and message_02 says the payslip shows regular pay and a one-time adjustment separately. The payslip likely mixes regular net pay with a one-off. The figure selector must separate the recurring salary from one-time amounts so the salary stream is not inflated. Check the other payslip images for the same pattern. | extraction, analyst |
| **Currency conversion** | Use `exchange_rates.csv` by settlement date and stated direction. Handle inverse pairs and cross rates only as RULES.md specifies. Test one sample per currency. | engine, analyst |

**Live exchange rates: not in the submission path.** The spec fixes dated rates ("no live banking, market-data, or exchange-rate calls"), and the hidden labels were computed from `exchange_rates.csv`, so live rates would make results wrong and non-deterministic. For the product story, rates sit behind a `RateProvider` trait, and the dataset implementation is the only one used here. A live provider is a post-submission extension.

## 6. Bus protocol

Topics (strict): `plan`, `scaffold`, `rules`, `bakeoff`, `engine`, `extract`, `verify`, `integrate`, `blocker`.
- Done signal: `atrium ctl bus pub <topic> msg=<agent>: <what is done, commit sha>`.
- Blockers: `atrium ctl bus pub blocker msg=<agent>: <need> (owner: <agent>)`. The lead triages every blocker.
- Commit on your own branch before announcing done. Only the integrator merges into main.

## 7. Logging — mandatory for every agent (AGENTS.md §2, §5)

There is exactly ONE log: **`E:\projects\hackerrank-orchestrate-september26\log.txt`** (Git Bash path: `/e/projects/hackerrank-orchestrate-september26/log.txt`). Worktree agents: your worktree has its own copy of AGENTS.md, but do NOT create a log beside it — always use the absolute path above.

When to append:
1. A `SESSION START` entry (AGENTS.md §5.1) as your first action.
2. A per-turn entry (§5.2) after every prompt you respond to: your kickoff, anything typed into your pane, and each instruction the lead sends you. Bus polling alone does not need an entry.

How to append (prevents clobbering the shared file):
- Use the **Bash tool** with a single quoted-heredoc append: `cat >> /e/projects/hackerrank-orchestrate-september26/log.txt <<'EOF' ... EOF`. Get the timestamp first with `date -u +%Y-%m-%dT%H:%M:%SZ` and paste it in.
- **Never** use the Write or Edit tool on log.txt (Write overwrites everyone's history). Never rewrite, reorder, or delete entries.
- One entry per append, UTF-8, `\n` line endings, under ~4 KB. Reference large content by path.
- Afterward run `tail -n 25` on the log and confirm your entry landed intact.

Required field values:
- `tool=Claude Code`
- `branch=` output of `git branch --show-current` (e.g. `atrium/buyorwait/engine`); main-tree agents use `main`
- `worktree=` your worktree's absolute path, or `main`
- `parent_agent=lead` for every agent except lead; lead uses `parent_agent=none`
- Add a line `agent=<your agent name>` directly under `tool=` so entries from different panes can be told apart

Never log secrets: the HF token, API keys, cookies. Write `[REDACTED]`.
