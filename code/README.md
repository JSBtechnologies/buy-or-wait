# Buy or Wait? — a verified-or-fail-closed financial decision agent

**Buy or Wait?** answers one question for every purchase or payment request: *should this person pay now, pay in part, use installments, wait, or not go ahead at all?* For each of the 250 requests it:
- **reconstructs** the user's real cash position from their profile, financial events, dated exchange rates, supplied payment options, and untrusted evidence in messages and receipt images;
- **forecasts** the balance day by day against the minimum they want to keep;
- **returns** a contract-exact `output.csv` row: safe amount today, affordability status, payment method and schedule, earliest safe full-payment date, spending changes, and a grounded explanation.

A deterministic Rust engine makes every decision. A self-hosted OCR model (`baidu/Unlimited-OCR` on vLLM) reads receipts once at ingestion. Every figure the engine uses from an image is either proven on the page or rejected, never guessed.

**Final run:**
- 250 decisions, 0 contract violations, 0 invariant violations.
- 0 wrong image figures.
- Cold and warm runs gave byte-identical `output.csv` (sha256 `a9b952bb…`).
- 17 OCR calls, 43,012 tokens (172 per request), ≈ $0.04 total.

## My approach

I treated this as a financial-reconstruction problem where being right and repeatable matters more than clever model use, and I set one rule early: **verified or fail closed**. A wrong number in someone's cash forecast is worse than a missing one, so nothing reaches the ledger unless it's proven, and anything unproven is dropped with a recorded reason. That shaped how the work was split:
- **Models only perceive.** They read documents and nothing else.
- **Rust decides.** Cash treatment, forecasting, plan search and ranking, and the explanation are all deterministic Rust.
- **Rules came from the samples, not guesswork.** I reverse-engineered the decision rules from the 25 solved samples: pending debits reserved, unsettled credits ignored, FX by settlement date, the day-by-day headroom math for the safe amount and the earliest date, and the plan ranking order. I kept requests 19–25 held out as an honest preview of the hidden grading, and accepted a tuning change only if it didn't hurt those held-out counts.
- **Ambiguous documents got explicit human rulings, recorded as rules** rather than left to a model:
  - a lakh-grouped `1,00,000` is 100,000;
  - a telecom bill paid after its cutoff owes the late amount (822.05);
  - a restaurant's Grand Total (8,528) beats its unrounded Total;
  - a cropped delivery screenshot with no total is rejected;
  - "the printed total trumps everything", except that on an outstanding bill the printed amount still owed wins.
- **Verification is part of the product.** A sign-off gate checks the output contract, forecast invariants, false accepts on image figures, hard-coded answers, the usage report, and byte-identical cold runs before anything ships.

My thinking changed most on the images, and that change drove the final architecture.
- **First design:** Hugging Face vision-language models (Qwen3-VL-235B and gemma-4-31B) filled a fixed JSON schema, with two-model agreement and a Claude tiebreak.
- **The problem:** it couldn't meet the zero-wrong-figures bar.
- **What the audit showed:** instead of adding more models, I audited every image against every saved model reply. The models had **read every final amount correctly**. They failed only when *mapping* it: inventing totals, subtotals, balances and due-date cutoffs the page never printed, then doing arithmetic on those invented fields.
- **The reframe:** "the model transcribes, Rust interprets." I considered adding a classification model to label the fields, but rejected it as overkill that would bring back non-determinism, in favour of fixed keyword lists per field plus a witness gate: a printed total, or a line-item sum confirmed by an independent printed figure such as the amount in words.
- **Choosing the reader:** I found **baidu/Unlimited-OCR** (MIT, 3.3B) and tested it on my own RTX 3070 across all 16 images. That surfaced real edge cases:
  - a two-page invoice stitched into one screenshot, fixed by splitting at page bars and upscaling small pages;
  - a table footer that sent the model into made-up text;
  - differences between the transformers and vLLM serving stacks.
- **Production shape:** I served the model with vLLM on a RunPod H100 to show how it would really run. Users upload receipts, ingestion OCRs and caches them once, and questions are answered from verified, cached evidence.
- **Cleanup:** I removed the old VLM and Anthropic paths entirely so the codebase describes exactly one pipeline.
- **How I ran the work:** as a solo developer, I ran the build as an [atrium](https://github.com/nativelite/atrium) fleet, a multi-agent terminal harness I built. Specialist agents (analyst, engine, extraction, ml-engineer, verifier, integrator) worked in parallel in their own git worktrees, with a coordinating lead, while I watched every pane, made every product ruling, and required a verifier sign-off and byte-identical runs before shipping.

## Setup

**Prerequisites**
- Rust stable toolchain with Cargo (developed against 1.96).
- `dataset/` present as shipped. Run all commands from `code/`, because paths are relative.
- For a `--cold` run, an OCR endpoint: `baidu/Unlimited-OCR` served by vLLM on any NVIDIA GPU. A warm run can reuse the cached OCR in `code/store/ocr/`.
- No `HF_TOKEN` or other API key is needed for the submitted configuration.

**1. Start the OCR server** (the POC used a RunPod H100; any ≥ 8 GB NVIDIA GPU works):
```bash
docker run --rm --gpus all --network host --ipc host \
  vllm/vllm-openai:unlimited-ocr-cu129 baidu/Unlimited-OCR --trust-remote-code \
  --logits_processors vllm.model_executor.models.unlimited_ocr:NGramPerReqLogitsProcessor \
  --no-enable-prefix-caching --mm-processor-cache-gb 0 --host 0.0.0.0 --port 8000
```
- **RunPod:** set container image `vllm/vllm-openai:unlimited-ocr-cu129`, put the arguments above (from `baidu/Unlimited-OCR` onward) in the start command, and expose HTTP port 8000.
- **Check:** `GET <base>/v1/models` should list `baidu/Unlimited-OCR`.

**2. Configure** `code/.env` (gitignored; never commit it):
```bash
OCR_BASE_URL=http://<host>:8000/v1     # e.g. https://<pod>-8000.proxy.runpod.net/v1
# OCR_MODEL=baidu/Unlimited-OCR        # default
# OCR_API_KEY=...                      # only if your endpoint requires one
```

**3. Build, run and verify** (from `code/`):
```bash
CARGO_TARGET_DIR=target cargo build --release
CARGO_TARGET_DIR=target cargo run --release -- --cold     # writes ../output.csv + evaluation/usage_report.md
CARGO_TARGET_DIR=target cargo test                         # unit, scenario and e2e fixture tests
CARGO_TARGET_DIR=target cargo run --release -- verify signoff --output ../output.csv --usage evaluation/usage_report.md
bash final_run.sh                                          # cold + warm determinism check + sign-off + code.zip
```
- **Other options:**
  - `--requests ../dataset/sample_requests.csv --out /tmp/s.csv` scores against the 25 solved samples.
  - `ask` runs an interactive single request.
- **Caution:** `--cold` wipes `code/store/`, and `verify signoff` re-checks the persisted image reads and evidence there. Run sign-off after a full run, against an intact `code/store/`.


## Approach overview (component by component)

1. **Reconstruct cash, then forecast. No model makes the decision.** A deterministic Rust engine:
   - rebuilds each user's ledger from `financial_events.csv`:
     - settled rows are in the balance;
     - pending debits are reserved;
     - pending credits are ignored until they settle;
     - scheduled rows and confirmed salary count on their dates;
     - FX uses the settlement-date rate;
   - detects recurring streams only where history supports them;
   - projects the balance day by day against `minimum_balance_to_keep`.
   - **Safe amount** = the minimum headroom over the horizon.
   - **Earliest full-payment date** = the first day from which the headroom never drops below the request.
2. **Search plans and rank them the way the spec says.**
   - **Candidates:** full, each supplied installment option, a two-payment partial plan, wait, and up to three spending changes on permitted flexible events.
   - **Safety:** each candidate is replayed day by day and dropped with a reason if it breaches the minimum balance, a preference or the deadline.
   - **Ranking:** meets the deadline → no spending changes → lowest cost → earlier start → fewer payments.
   - **Explanations:** templates filled only from the recorded `DecisionFacts`.
3. **Treat messages and images as untrusted evidence.**
   - **Messages:** a deterministic parser handles them. Every number must literally appear in the message, and embedded instructions are ignored. Conflicts resolve as cancellation/settlement/amendment > newer same-source record > settled event > safer interpretation.
   - **Images:**
     - **Reading:** at ingestion, stitched pages are split, small pages are upscaled 2×, and each page is OCR'd **once** by `baidu/Unlimited-OCR`, then cached.
     - **Mapping:** the model only transcribes. Rust maps printed labels to meaning with fixed keyword lists (Grand Total > Total; Balance Due; "Amount due after ⟨date⟩"; subtotals are never totals; change, cash tendered and "Due Date" columns are ignored).
     - **Acceptance:** a figure is accepted only through a **witness gate**: a printed total (or, for an outstanding bill, the printed amount still owed), or a line-item sum confirmed by an independent printed figure such as the amount in words. Anything else **fails closed** and is never guessed.
4. **How we got there.**
   - **The finding:** an audit of the first design, where HF vision-language models filled fixed JSON fields, showed the models *read* every final amount correctly but *mis-mapped* them (invented totals, balances and cutoffs).
   - **The switch:** we moved to "OCR transcribes, Rust interprets", compared OCR modes on all 16 images, and fixed the serving setup to one that gives byte-identical results.
   - **Removal:** the old HF VLM and Anthropic paths were then removed from the codebase.
   - **Rulings:** per-image rulings (e.g. cropped page → fail closed; "total trumps all" except amounts still owed on pending bills) are recorded in `RULES.md`.
5. **Verify before shipping.** `evaluation::signoff` gates every release:
   - contract validation;
   - forecast invariants;
   - a false-accept gate on image facts;
   - a check that shipped rows equal re-derived engine rows;
   - a hard-coded answer scan;
   - usage report sections;
   - byte-identical cold/warm runs.

   Sample requests 01–18 were used for tuning, and 19–25 were held out, reported as pass counts only.
6. **Built with [atrium](https://github.com/nativelite/atrium), my own agentic harness.** atrium is "tmux for coding agents": a multi-agent terminal that hosts real CLI agents (here, Claude Code) in switchable, splittable panes. The whole solution was developed by an atrium **fleet** (`atrium.fleet.json`, kickoffs and rules in [`fleet/`](https://github.com/JSBtechnologies/buy-or-wait/tree/main/fleet)), with me supervising and making every product ruling.

   ```mermaid
   flowchart LR
     H((Me<br/>rulings)) --> L[lead<br/>coordinates, no code]
     L <-->|board + bus| AN[analyst<br/>RULES.md]
     L <--> EN[engine<br/>ledger/forecast/plans]
     L <--> EX[extraction<br/>messages/evidence]
     L <--> ML[ml-engineer<br/>OCR + labels + gate]
     L <--> VE[verifier<br/>tests + sign-off]
     L <--> IN[integrator<br/>merges, runs, zip]
     EN & EX & ML & VE -->|branches| IN --> M[(main)]
   ```

   **Why I used it:**
   - **Parallel specialists.** A 24-hour solo challenge needs parallel specialists without losing control. Each agent owns specific files, works in its own git worktree, commits on its own branch, and only the integrator merges.
   - **Visible work.** Every agent is a pane I can watch, steer or take over, unlike invisible background sub-agents. The status chrome shows who is working, who is waiting on me and who is idle.
   - **Audit trail.** Decisions and evidence travel on a structured control plane instead of chat scroll-back, so the whole process leaves a trail. That includes the per-turn `log.txt`, board entries such as `ruling.*` and `decision.*`, and the bus history.

   **What's cool about it:**
   - **Control plane (`atrium ctl`):** a shared **board** with claims and leases (`board claim`, `state.<agent>` handoffs) and a pub/sub **bus** with topics. Questions can be routed to a specific role (`--decision --to lead`), so design questions reach the coordinator before the human.
   - **Trust policy with ceilings** (`plan < accept < automode < skip`). Planners ran read-only in plan mode while the OCR work executed, and a worker can never raise its own permissions.
   - **Fleet files.** A whole team relaunches from `atrium.fleet.json`, and each role resumes from its board state after context compaction instead of starting over.
   - **Agent-aware panes:** borders and the status bar reflect each agent's real session state.
   - **Zero third-party dependencies,** built on the nativelite stack (pty, rawterm, ansi, vterm, agsess), with Windows ConPTY as the reference platform.

   **In practice on this project:**
   - The fleet reverse-engineered the decision rules and built and tuned the engine.
   - It ran the OCR experiments, and caught and fixed real bugs: contaminated shared build caches, a rent-change rule scaling a one-off balance, and invented cutoffs.
   - It shipped behind a verifier sign-off, with byte-identical cold runs.


## Architecture

The full design document with LaTeX/TikZ diagrams is [`docs/architecture.pdf`](docs/architecture.pdf) (source: `docs/architecture.tex`).

### 1. Goals

| # | Goal | How it is met |
|---|---|---|
| 1 | Contract-exact `output.csv` | Contract validator plus invariants in `evaluation/` |
| 2 | Verified or fail closed | Witness gate for image figures; literal grounding for message figures; unproven figures are dropped with a reason |
| 3 | Determinism | Fixed-point money (10⁻⁴ units), temperature 0, config-hashed OCR cache; two cold runs byte-identical |
| 4 | Models read, Rust decides | The OCR model only transcribes. Selection, cash rules, plans and explanations are deterministic Rust |
| 5 | Conservative safety | Balance ≥ `minimum_balance_to_keep` after every projected expense and plan payment. Pending debits are reserved; unsettled credits are ignored |
| 6 | Operable and auditable | Terminal CLI, secrets from env, `DecisionFacts` per request, persisted image provenance, usage report |

### 2. System context

```mermaid
flowchart LR
  U[User / frontend<br/>uploads receipts, asks] --> I[Ingestion<br/>page split, 2x upscale if small]
  I -->|HTTPS, OpenAI-compatible| V[(vLLM<br/>baidu/Unlimited-OCR<br/>RunPod H100)]
  I --> C[(OCR cache<br/>code/store/ocr)]
  V --> A[Decision agent<br/>Rust crate buyorwait]
  C --> A
  D[(dataset/ CSVs + media)] --> A
  A --> O[output.csv + usage_report.md]
  U -. ask mode .-> A
```

OCR runs **once per page at ingestion** and is cached. Answering a request never calls a model.

### 3. Crate structure

```mermaid
flowchart TB
  main[main.rs<br/>batch + ask CLI] --> model[model<br/>typed CSV loaders]
  main --> store[store<br/>cache + processed]
  main --> extract[extract<br/>evidence & intake]
  main --> engine[engine<br/>pure decision core]
  main --> evaluation[evaluation<br/>verification]
  extract -->|Fact, EvidenceRecord| engine
  evaluation --> engine
  extract -.-> hf[hf<br/>HTTP client + usage types]
```

| Module | Responsibility |
|---|---|
| `engine::money` | Fixed-point money, rounding only at output |
| `engine::rules` | Tunable decision rules and toggles |
| `engine::session` | One user's profile, events and rates |
| `engine::ledger` | Cash rules by status and direction, lifecycle de-dup, FX, evidence facts in conflict order |
| `engine::recurrence` | Recurring streams, only where history supports them |
| `engine::forecast` | Day-by-day balance, safe amount, earliest full-payment date |
| `engine::plans` | Candidates (full, installments, partial, wait, spending changes), safety replay, ranking |
| `engine::facts` / `engine::explain` | `DecisionFacts` plus template explanations (0 tokens) |
| `extract::messages`, `grounding`, `retrieval` | Deterministic message parsing; numbers must literally appear in the text |
| `extract::ocr` | vLLM client, page split, conditional upscale, config-hashed cache, usage |
| `extract::ocr_parse` | OCR HTML tables and `<\|det\|>` blocks into label:value rows (zips multi-value cells) |
| `extract::labels` | Keyword list per role into `ImageFigures` |
| `extract::normalize` | Amount, currency and date traps: lakh grouping, `$33,50`, Rs/Ps split |
| `extract::witness` | Witness identities, amount-in-words parser, contradiction checks |
| `extract::images` | Selector by event type and status, witness gate, `ImageReadProvenance` |
| `store::cache` / `store::processed` | Model cache; persisted ledgers and image reads |
| `evaluation::*` | Contract, invariants, sample scorer, replay mirror, false-accept gate, hardcode scan, sign-off |

### 4. Batch pipeline

```mermaid
flowchart TB
  L[Load dataset] --> S[Open stores<br/>--cold wipes code/store]
  S --> OCR[OCR ingestion, all images<br/>cache-first, live vLLM on miss]
  OCR --> R{{for each request}}
  R --> D1[Session::from_model]
  D1 --> D2[Message evidence<br/>retrieval + deterministic parse + grounding]
  D2 --> D3[Blank-amount image resolution<br/>from OCR cache + witness gate]
  D3 --> D4[Apply Facts to ledger]
  D4 --> D5[Forecast → plans → rank]
  D5 --> D6[DecisionFacts + explanation<br/>persist provenance]
  D6 --> V[Contract + invariants<br/>fallback row on engine error]
  V --> W[Write output.csv + usage_report.md]
```

### 5. Image evidence pipeline

**Why this design:** an audit found the earlier VLM reads (Qwen3-VL-235B, gemma-4-31B; that path has since been removed) transcribed every printed final amount correctly. All failures came from asking the model to fill fixed JSON keys. It invented totals, subtotals, balances and cutoffs. Now the model only transcribes, and Rust maps printed labels to meaning.

```mermaid
flowchart LR
  P[PNG] --> SP[Split at full-width<br/>near-black bands]
  SP --> UP[2x Lanczos if<br/>shorter side < 600px]
  UP --> M[vLLM OCR<br/>'<image>document parsing.']
  M --> C[(store/ocr/&lt;image&gt;/page_n.md<br/>+ meta.json config hash)]
  C --> PR[ocr_parse<br/>label:value rows]
  PR --> LB[labels<br/>keyword roles]
  LB --> SEL[select by<br/>event type/status]
  SEL --> G{witness gate}
  G -->|proven| OK[Fact::EventAmount<br/>witness_accept]
  G -->|not proven| FC[fail_closed + reason]
  FC -. pending/scheduled only .-> RES[Fact::UnverifiedEventAmount<br/>reserve]
```

### Acceptance rules (user rulings)

```mermaid
flowchart TB
  Q1{Pending/scheduled bill with a<br/>printed amount still owed?} -->|yes| A1[Accept amount owed<br/>Balance Due / dated after-cutoff]
  Q1 -->|no| Q2{Printed total field readable?}
  Q2 -->|yes| A2[Accept printed total<br/>Grand Total > Total family — 'total trumps all']
  Q2 -->|no| Q3{Items/charges sum matches an<br/>independent printed witness?}
  Q3 -->|yes| A3[Accept the printed witness figure]
  Q3 -->|no| F[fail_closed — never self-witnessed, never guessed]
```

A breakdown that doesn't sum never rejects a printed final amount, since pages are often cropped. A subtotal is never promoted to a total. Change, cash tendered, prior-balance payments and bare "Due Date" columns are ignored.

### OCR serving

| | |
|---|---|
| Model | `baidu/Unlimited-OCR`: MIT, 3.34B parameters, BF16 |
| Image | `vllm/vllm-openai:unlimited-ocr-cu129` with `--trust-remote-code --logits_processors vllm.model_executor.models.unlimited_ocr:NGramPerReqLogitsProcessor --no-enable-prefix-caching --mm-processor-cache-gb 0` |
| Request | `temperature=0`, `max_tokens=8192`, `skip_special_tokens=false`, `vllm_xargs={ngram_size:35, window_size:128}`, and an explicit User-Agent (the RunPod proxy rejects requests without one) |
| Env | `OCR_BASE_URL`, `OCR_MODEL`, optional `OCR_API_KEY`. `HF_TOKEN` is not needed by default |
| Final cold run | 17 pages, 31,309 in + 11,703 out = 43,012 tokens, 53.6 s, ≈ $0.04 (H100 at $2.69/h); byte-identical over 16/16 images |

### Results

| Image | Event | Status | Figure | Witness / reason | Scope |
|---|---|---|---|---|---|
| 01 | net salary (IDR) | settled | 4,365,000 | gross − deductions | sample |
| 02 | outstanding rent balance | scheduled | 100,000 | total − paid | sample |
| 03 | bulk groceries | settled | 41,272 | repeated final label | sample |
| 04 | delivered grocery order | settled | — | fail_closed: no final label (cropped) | sample |
| 05 | outstanding telecom bill | pending | 822.05 | after-cutoff > witnessed before-cutoff | sample |
| 06 | grocery tax invoice | settled | 1,995 | item sum witnessed by words | eval |
| 07 | restaurant tax invoice | settled | 8,528 | grand total | eval |
| 08 | property maintenance | settled | 15,339 | line-item sum | eval |
| 09 | water bill | settled | 723 | line-item sum | eval |
| 10 | large grocery invoice (2 pages) | pending | 79,679.26 | amount in words | eval |
| 11 | hospital bill | scheduled | 3,650 | repeated final label | eval |
| 12 | taxi fare | settled | 33.50 USD | subtotal + tax | eval |
| 13 | tote bag order | settled | 2,298 | repeated final label | eval |
| 14 | pharmacy (handwritten) | settled | 4,543 | printed final label only | eval |
| 15 | airline ticket | settled | 9,968 | line-item sum | eval |
| 16 | EV charging | settled | 393.22 | amount in words | eval |

### 6. Decision engine

```mermaid
flowchart LR
  S[Session] --> L[Ledger<br/>cash rules, FX, facts]
  E[Facts from messages & images] --> L
  L --> R[Recurrence] --> F[Forecast] --> P[Plans] --> DF[DecisionFacts] --> X[Explain] --> O[Output row]
```

- **Cash rules:**
  - Settled rows are already in the balance.
  - Pending debits are reserved.
  - Pending credits, bonuses, commissions, refunds, lottery proceeds and unrealized gains are never counted.
  - Scheduled rows and confirmed salary count on their settlement dates.
  - Linked lifecycle chains are de-duplicated.
- **FX:** the `exchange_rates.csv` row for the settlement date and stated direction. Full precision is kept, rounding only at output.
- **Conflicts:** an explicit cancellation, settlement or amendment wins first, then newer same-source records, then a settled event, then the safer interpretation.
- **Percentage changes:** "rent +12%" scales only recurring cycle rows, never a one-off scheduled balance.
- **Forecast:** with intraday low ℓₜ, end-of-day bₜ, minimum M and request R:
  - `safe = min(R, max(0, minₜ(ℓₜ − M)))`
  - `E = first d with min(b_d, min_{t>d} ℓₜ) − M ≥ R`
  - Suffix minima make this linear.
- **Rule toggles, chosen by held-out A/B:** fixed bills after same-day salary credit, scheduled replacement by lifecycle or amount, and a mid-step phase for long-interval variable spend.
- **Plans:** full, each supplied installment option, two-payment partial (safe today, remainder on E ≤ deadline), wait, and up to three spending changes on permitted flexible events.
  - Each is replayed day by day and dropped with a reason if it breaches M, breaks a preference or `max_installment_months`, or misses the deadline.
  - Survivors are ranked by: completes by the deadline → no spending changes → lowest cost → earlier start → fewer payments.
- **Explanations:** rendered only from `DecisionFacts`, so they can't contradict the row.

### 7. Message evidence

- **Selection:** messages are untrusted and retrieved per user up to the request date.
- **Parsing:** a deterministic parser maps known families to typed records. There were 0 LLM calls in the final run.
- **Grounding:** every claimed number must literally appear in the text, and embedded instructions are ignored.
- **Output:** records become engine `Fact`s (amendment, cancellation, settlement, confirmed income, expense amount change). A fact with no matching `Fact` variant is dropped.

### 8. Verification and ship gate

```mermaid
flowchart LR
  T[cargo test 208/0] --> G
  C[Contract: 250 rows, 0 violations] --> G
  I[Invariants: min balance, ranges] --> G
  S[Sample scorer: tuning 01–18 / held-out 19–25] --> G
  FA[False-accept gate: 16/16 as ruled] --> G
  H[Hardcode scan clean] --> G
  DT[2 cold runs byte-identical] --> G
  U[Usage report] --> G
  G{{evaluation::signoff}} --> PASS[PASS @ 3c77c81]
```

Held-out sample labels never reach the tuning loop; the scorer reports pass counts only. Each rule change shipped as its own commit, with a check of which output rows moved.

### 9. Operations

```bash
# OCR server (any NVIDIA host; the POC used a RunPod H100)
docker run --rm --gpus all --network host --ipc host \
  vllm/vllm-openai:unlimited-ocr-cu129 baidu/Unlimited-OCR --trust-remote-code \
  --logits_processors vllm.model_executor.models.unlimited_ocr:NGramPerReqLogitsProcessor \
  --no-enable-prefix-caching --mm-processor-cache-gb 0 --host 0.0.0.0 --port 8000

# code/.env (never committed)
OCR_BASE_URL=http://<host>:8000/v1

# from code/
CARGO_TARGET_DIR=target cargo run --release -- --cold      # submission run
CARGO_TARGET_DIR=target cargo test
```

### 10. Known limitations

- **Image 04** (a cropped page) has no printed total. It fails closed by design.
- **vLLM vs. the transformers reference** differ on a few table cells (06 total cell, 14 split total). Both are deterministic, and the witness rules recover the figures. A pointer-only resolver model with Rust re-verification is a possible extension.
- **Handwritten line items** (14) are misread. Only the printed total is used.
- **The POC endpoint** is unauthenticated. Production needs `OCR_API_KEY` and private networking.
- **The labelled sample is small**: 25 requests, 7 held out.

## Operations reference

### Detailed prerequisites

- Rust (stable toolchain; developed against 1.96) and Cargo.
- `OCR_BASE_URL` — **required for a `--cold` run** (forces fresh OCR ingestion; see
  "OCR ingestion" below). A warm run against an already-populated `store/ocr/` cache
  does not need it. Optionally `OCR_MODEL` (default `baidu/Unlimited-OCR`) and
  `OCR_API_KEY`. These can also go in a `.env` file next to this README or at the repo
  root (`KEY=VALUE` per line, `#` comments ok) — loaded automatically, never
  overriding a variable already set in the environment. `.env` is gitignored; never
  commit it.
- An `HF_TOKEN` environment variable holding a Hugging Face API token — **optional**
  for the submitted pipeline (the shipped `config/models.toml` has no `[selected]` LLM
  active, so the final run makes 0 HF calls). Needed only if an LLM is selected in
  `[selected]` for message extraction. Never commit this value; it is read from the
  environment only.
- `../dataset/` present as shipped (this crate reads it with relative paths, so always
  run commands from this `code/` directory).

**Build in your own `target/` directory.** If your shell has `CARGO_TARGET_DIR` set to
a path shared with other checkouts of this crate, override it per command so builds
don't race/corrupt another checkout's cache:

```bash
CARGO_TARGET_DIR=target cargo build
```

(On Windows PowerShell: `$env:CARGO_TARGET_DIR = "target"` for the session, or prefix
each command the same way.)


### Build

```bash
cd code
CARGO_TARGET_DIR=target cargo build --release
```

### Run the batch pipeline

```bash
CARGO_TARGET_DIR=target cargo run --release
```

Defaults: reads `../dataset/requests.csv`, writes `../output.csv`, and writes
`evaluation/usage_report.md`. Flags:

- `--requests FILE` — evaluate a different requests file (e.g.
  `../dataset/sample_requests.csv` for tuning against the solved samples).
- `--out FILE` — write predictions somewhere other than `../output.csv`.
- `--cold` — ignore and wipe the processed-data store (`code/store/`, gitignored)
  before running, so preprocessing and all model calls are redone from scratch. **The
  run that produces the submitted `output.csv` is always a `--cold` run**, so
  `evaluation/usage_report.md` reflects real calls rather than cache hits.

```bash
CARGO_TARGET_DIR=target cargo run --release -- --cold
CARGO_TARGET_DIR=target cargo run --release -- --requests ../dataset/sample_requests.csv --out /tmp/sample_out.csv
```

### OCR ingestion

Every image in `../dataset/images.csv` is OCR'd at the start of the batch pipeline and
cached at `store/ocr/<image_id>/` (cache-first; `--cold` forces a re-OCR). This is the
only image path (`fleet/specs/ocr_vllm_pipeline.md`); the earlier fixed-key VLM image
path and the Anthropic client were removed entirely (board:cleanup.remove_vlm_anthropic).

Serving is a vLLM OpenAI-compatible endpoint — the POC runs on a RunPod H100:

```bash
docker run --rm --gpus all --network host --ipc host vllm/vllm-openai:unlimited-ocr-cu129 baidu/Unlimited-OCR \
  --trust-remote-code \
  --logits_processors vllm.model_executor.models.unlimited_ocr:NGramPerReqLogitsProcessor \
  --no-enable-prefix-caching --mm-processor-cache-gb 0
```

Point `OCR_BASE_URL` at it (e.g. `https://<pod>-8000.proxy.runpod.net/v1`). Without it
set, ingestion is skipped (not a hard error) and the batch pipeline still runs on
whatever is already cached.

### Interactive ask mode (free text)

`ask` turns a free-text question into the same request structure with **Qwen/Qwen3-235B-A22B-Instruct-2507** (via the Hugging Face router; set `HF_TOKEN`), then applies the **same evidence as batch**: this user's messages (deterministic parse + grounding) and this user's receipt figures from the OCR cache (witness gate). It is enabled by `[selected] intake_llm` in `config/models.toml`, which is kept separate from `llm_primary`, so batch message extraction stays model-free and `output.csv` is unchanged.

```bash
CARGO_TARGET_DIR=target cargo run --release -- ask --user user_64 --date 2024-06-04   --text "Can I buy a new sofa for 63,700 rupees? I need it by 16 August 2024 and I can't split the payment."
# ask: image_10 -> event_6033 79679.26 INR (witness_accept)
# amount_safe_to_pay: 0 | not_affordable | not_recommended   (identical to the batch row for request_64)
```
Ad hoc questions have no `request_payment_options.csv` row, so installments are not offered in ask mode. If the amount, deadline or request type can't be grounded in the text, it refuses to answer rather than guessing.

### Verify

The verifier's contract/invariant/scoring checks are reachable as a subcommand of the
same binary:

```bash
CARGO_TARGET_DIR=target cargo run --release -- verify validate --output ../output.csv
CARGO_TARGET_DIR=target cargo run --release -- verify score --output ../output.csv
CARGO_TARGET_DIR=target cargo run --release -- verify selftest
CARGO_TARGET_DIR=target cargo run --release -- verify signoff --output ../output.csv --usage evaluation/usage_report.md
```

Exit code is `0` on pass, `1` on a failing check, `2` on a usage error. `signoff` is the
ship gate: contract, status distribution, injected-text/hardcoded-id scans, secrets, and
the usage report's presence/sections/secrets, all in one pass/fail report.

`verify signoff` is a development verification step, not part of the prediction path. Its
accuracy gate compares model-read image amounts with the analyst's hand-read audit table in
the repository `RULES.md` (not shipped in `code.zip`); a mismatch means model and analyst
disagree and is investigated, never auto-corrected. The batch run never reads the audit
table.

### Layout

```text
code/
  Cargo.toml
  src/
    main.rs         CLI entry point (batch pipeline + verify subcommand)
    model.rs         dataset CSV row structs and loaders
    hf.rs             OpenAI-compatible HTTP client + usage/pricing types
    bin/              coverage, gen_evidence, llm_generalization dev tools
    extract/          retrieval, image/message extraction, request intake, grounding
    engine/           ledger, recurrence, forecast, plan search/ranking, explanations
    evaluation/       invariants, output-contract validator, sample scorer, replay
    store/            processed-data store: model-call cache + persisted preprocessing
  prompts/            versioned prompt files (message extraction, request intake)
  config/models.toml  decoding/caching/OCR config (no secrets)
  docs/               architecture.pdf + architecture.tex (full design document)
  evaluation/usage_report.md   token/cost report for the final full-dataset run
  store/              (generated, gitignored) on-disk cache and processed data
```

### Final submission run

One command runs the whole ship sequence (PLAN.md §5 Phase 3): a cold run producing the
submitted `output.csv` and `evaluation/usage_report.md`, a warm rerun that must reproduce
them byte-for-byte (determinism check) and reports the cache hit rate, `verify signoff`
against the cold run's files, and rebuilding `code.zip` from the exact commit that
produced them:

```bash
cd code
bash final_run.sh
```

It exits non-zero (before touching signoff or the zip) if the warm rerun doesn't
byte-match the cold run's `output.csv`. `dist/` (gitignored, outside git) is where the
zip lands.

#### The same steps without bash (PowerShell / cmd)

`final_run.sh` is a bash script (AGENTS.md: don't assume bash is available). The same
sequence run directly:

PowerShell:

```powershell
cd code
$env:CARGO_TARGET_DIR = "target"
cargo run --release -- --cold
Copy-Item ..\output.csv ..\output.cold.csv
Copy-Item evaluation\usage_report.md evaluation\usage_report.cold.md
cargo run --release
if ((Get-FileHash ..\output.cold.csv).Hash -eq (Get-FileHash ..\output.csv).Hash) {
    "byte-identical: PASS"
} else {
    Write-Error "byte-identical: FAIL (cold and warm runs produced different output.csv)"
}
Move-Item ..\output.cold.csv ..\output.csv -Force
Move-Item evaluation\usage_report.cold.md evaluation\usage_report.md -Force
cargo run --release -- verify signoff --output ..\output.csv --usage evaluation\usage_report.md
New-Item -ItemType Directory -Force ..\dist | Out-Null
git -C .. archive --format=zip -o dist/code.zip HEAD -- code
```

cmd.exe:

```bat
cd code
set CARGO_TARGET_DIR=target
cargo run --release -- --cold
copy /Y ..\output.csv ..\output.cold.csv
copy /Y evaluation\usage_report.md evaluation\usage_report.cold.md
cargo run --release
fc /B ..\output.cold.csv ..\output.csv >nul && echo byte-identical: PASS || echo byte-identical: FAIL
move /Y ..\output.cold.csv ..\output.csv
move /Y evaluation\usage_report.cold.md evaluation\usage_report.md
cargo run --release -- verify signoff --output ..\output.csv --usage evaluation\usage_report.md
mkdir ..\dist 2>nul
git -C .. archive --format=zip -o dist/code.zip HEAD -- code
```

### Packaging `code.zip` manually

`final_run.sh`'s last step is just `git archive`, packaging the exact committed `code/`
tree — no manual include/exclude list needed, since anything generated or local-only
(`target/`, `store/`, `scratch/`, `.env`) was never tracked in the first place:

```bash
git archive --format=zip -o dist/code.zip HEAD -- code
```

`evaluation/usage_report.md`, `prompts/`, and `config/models.toml` are included as
required by the submission; no HF token or other secret is ever written to a tracked or
packaged file.
