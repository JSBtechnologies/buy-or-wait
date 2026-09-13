# Buy or Wait? — solution

A deterministic Rust engine decides every request; language models (via the Hugging
Face Inference Providers router) only turn messages and images into typed, grounded
facts before the engine runs. See `../PLAN.md` and `../RULES.md` for the design and the
reverse-engineered numeric rules.

## Prerequisites

- Rust (stable toolchain; developed against 1.96) and Cargo.
- An `HF_TOKEN` environment variable holding a Hugging Face API token, for the model
  bake-off (`bakeoff`) and any future live extraction calls. Never commit this value;
  it is read from the environment only. Not required to build, or to run the batch
  pipeline once the processed-data store is fully populated.
- `OCR_BASE_URL` (required for live OCR ingestion; see "OCR ingestion" below),
  optionally `OCR_MODEL` (default `baidu/Unlimited-OCR`) and `OCR_API_KEY`. These can
  also go in a `.env` file next to this README or at the repo root (`KEY=VALUE` per
  line, `#` comments ok) — loaded automatically, never overriding a variable already
  set in the environment. `.env` is gitignored; never commit it.
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

## Build

```bash
cd code
CARGO_TARGET_DIR=target cargo build --release
```

## Run the batch pipeline

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

## OCR ingestion

Every image in `../dataset/images.csv` is OCR'd at the start of the batch pipeline and
cached at `store/ocr/<image_id>/` (cache-first; `--cold` forces a re-OCR). This replaces
the earlier fixed-key VLM image reads (`fleet/specs/ocr_vllm_pipeline.md`); that path
stays compiled but inactive unless `[selected].vlm_primary` is also set.

Serving is a vLLM OpenAI-compatible endpoint — the POC runs on a RunPod H100:

```bash
docker run --rm --gpus all --network host --ipc host vllm/vllm-openai:unlimited-ocr baidu/Unlimited-OCR \
  --trust-remote-code \
  --logits_processors vllm.model_executor.models.unlimited_ocr:NGramPerReqLogitsProcessor \
  --no-enable-prefix-caching --mm-processor-cache-gb 0
```

Point `OCR_BASE_URL` at it (e.g. `https://<pod>-8000.proxy.runpod.net/v1`). Without it
set, ingestion is skipped (not a hard error) and the batch pipeline still runs on
whatever is already cached.

## Verify

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

## Model bake-off

```bash
CARGO_TARGET_DIR=target cargo run --release --bin bakeoff
```

Candidate models, providers, and decoding parameters live in `config/models.toml`;
results are written to `../docs/bakeoff.md`.

## Layout

```text
code/
  Cargo.toml
  src/
    main.rs         CLI entry point (batch pipeline + verify subcommand)
    model.rs         dataset CSV row structs and loaders
    hf.rs             Hugging Face Inference Providers client
    bin/bakeoff.rs   model bake-off harness
    extract/          retrieval, image/message extraction, request intake, grounding
    engine/           ledger, recurrence, forecast, plan search/ranking, explanations
    evaluation/       invariants, output-contract validator, sample scorer, replay
    store/            processed-data store: model-call cache + persisted preprocessing
  prompts/            versioned prompt files
  config/models.toml  bake-off candidates and decoding/caching config (no secrets)
  evaluation/usage_report.md   token/cost report for the final full-dataset run
  store/              (generated, gitignored) on-disk cache and processed data
```

## Final submission run

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

### The same steps without bash (PowerShell / cmd)

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

## Packaging `code.zip` manually

`final_run.sh`'s last step is just `git archive`, packaging the exact committed `code/`
tree — no manual include/exclude list needed, since anything generated or local-only
(`target/`, `store/`, `scratch/`, `.env`) was never tracked in the first place:

```bash
git archive --format=zip -o dist/code.zip HEAD -- code
```

`evaluation/usage_report.md`, `prompts/`, and `config/models.toml` are included as
required by the submission; no HF token or other secret is ever written to a tracked or
packaged file.
