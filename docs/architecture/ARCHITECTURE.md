# Buy or Wait? — Architecture

> The full design document with LaTeX/TikZ diagrams is [`architecture.tex`](architecture.tex). Build it with `latexmk -pdf architecture.tex` or `tectonic architecture.tex`. This page is the GitHub-rendered summary, with Mermaid diagrams.

**Revision:** `main@1f381fe`. Verifier sign-off is on `7b68395`.

**Final run:**
- 250 decisions, 0 contract violations, 0 invariant violations.
- 0 wrong image figures.
- Two `--cold` runs gave byte-identical `output.csv` (sha256 `a9b952bb…`).

## 1. Goals

| # | Goal | How it is met |
|---|---|---|
| 1 | Contract-exact `output.csv` | Contract validator plus invariants in `evaluation/` |
| 2 | Verified or fail closed | Witness gate for image figures; literal grounding for message figures; unproven figures are dropped with a reason |
| 3 | Determinism | Fixed-point money (10⁻⁴ units), temperature 0, config-hashed OCR cache; two cold runs byte-identical |
| 4 | Models read, Rust decides | The OCR model only transcribes. Selection, cash rules, plans and explanations are deterministic Rust |
| 5 | Conservative safety | Balance ≥ `minimum_balance_to_keep` after every projected expense and plan payment. Pending debits are reserved; unsettled credits are ignored |
| 6 | Operable and auditable | Terminal CLI, secrets from env, `DecisionFacts` per request, persisted image provenance, usage report |

## 2. System context

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

## 3. Crate structure

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

## 4. Batch pipeline

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

## 5. Image evidence pipeline

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

## 6. Decision engine

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

## 7. Message evidence

- **Selection:** messages are untrusted and retrieved per user up to the request date.
- **Parsing:** a deterministic parser maps known families to typed records. There were 0 LLM calls in the final run.
- **Grounding:** every claimed number must literally appear in the text, and embedded instructions are ignored.
- **Output:** records become engine `Fact`s (amendment, cancellation, settlement, confirmed income, expense amount change). A fact with no matching `Fact` variant is dropped.

## 8. Verification and ship gate

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

## 9. Operations

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

## 10. Known limitations

- **Image 04** (a cropped page) has no printed total. It fails closed by design.
- **vLLM vs. the transformers reference** differ on a few table cells (06 total cell, 14 split total). Both are deterministic, and the witness rules recover the figures. A pointer-only resolver model with Rust re-verification is a possible extension.
- **Handwritten line items** (14) are misread. Only the printed total is used.
- **The POC endpoint** is unauthenticated. Production needs `OCR_API_KEY` and private networking.
- **The labelled sample is small**: 25 requests, 7 held out.
