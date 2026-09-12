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
