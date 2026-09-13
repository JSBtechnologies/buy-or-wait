# Fleet rules (all agents, buyorwait)

Re-read this file at start and after every compaction. It supersedes PLAN.md §7–§8 where they differ.

Deadline: **2026-09-13T12:30:00Z**. Approved plan: `E:/projects/hackerrank-orchestrate-september26/docs/image_accuracy_plan.md`.

## 1. You are resuming, not starting
The system is ~90–95% built. Never redo your original mission: no re-scaffolding, no re-running old bake-offs, no rewriting existing modules. Get your state first, then continue.

## 2. Logging (mandatory)
- One log: `/e/projects/hackerrank-orchestrate-september26/log.txt`. Never create a log in a worktree.
- Append only with the Bash tool and a single quoted heredoc: `cat >> /e/projects/hackerrank-orchestrate-september26/log.txt <<'EOF' … EOF`. Never use Write or Edit on log.txt. Never rewrite or delete entries.
- Write a SESSION START entry first, then one entry per prompt you respond to (kickoff, pane input, lead instructions). Format: AGENTS.md §5.
- Fields: `tool=Claude Code`, then `agent=<your name>` on the next line, `branch=`, `repo_root=`, `worktree=`, `parent_agent=lead` (lead: `none`). Compute Time Remaining from `date -u`; don't estimate.
- **Plan mode blocks appends.** Keep a backlog and append it right after your plan is approved.
- Never log secrets (`HF_TOKEN` → `[REDACTED]`). Run `tail` afterward to confirm the entry landed.

## 3. Context-mode (mandatory)
- Use `ctx_batch_execute` / `ctx_execute` / `ctx_execute_file` for anything large or processed: cargo build/test, bakeoff and sample reports, git logs/diffs, CSVs, log.txt greps. Print only the answer.
- Use `ctx_search` for follow-ups, and on resume (`sort: "timeline"`).
- Plain Bash only for short output and mutations (git commit/merge, `atrium ctl`, the log append). context-mode doesn't persist files: write with Write/Edit.
- Don't read AGENTS.md (already loaded via CLAUDE.md), problem_statement.md, PLAN.md or RULES.md in full. Pull sections.

## 4. Strategic compaction
Compact only at a boundary: work committed, bus and board updated, nothing in flight. First write `atrium ctl board set state.<name>` (head sha, done, in_progress, next, blockers). If you can't trigger compaction, post "ready to compact at <sha>". After compaction: re-read this file, get your state (§5), then continue.

## 5. Getting your state (one `ctx_batch_execute`)
`atrium ctl board get state.<name>`, `atrium ctl bus feed`, `git log --oneline -8`, `git status --short`. Trust that over memory.

## 6. Ownership and git
- Touch only files you own. Need a change elsewhere? Post on topic `blocker` naming the owner.
- Uncommitted edits to files you don't own: never commit them. `git restore` them (if generated) or `git stash push -m 'stray non-owned edits' -- <files>`, and report.
- Worktree agents: your cwd is your worktree. Don't cd, run no `git worktree` commands, commit on your branch. Only the integrator merges into main.
- Done signal: commit, then `atrium ctl bus pub <topic> msg=<name>: <what> <sha>`, then update `board state.<name>`.
- Bus topics: plan, scaffold, rules, bakeoff, engine, extract, verify, integrate, blocker.

## 7. Language
The system is 100% Rust. Python only for throwaway exploration in `scratch/` (gitignored, never committed, never shipped).

## 8. Final user rulings (do not re-litigate)
- image_02 = **100,000** (Indian grouping `1,00,000`).
- image_05 = **822.05** (outstanding bill settles after the 06-Feb cutoff; fleet gold stands).
- image_07 = **8,528** (Grand Total paid).
- image_11 = **3,650** (the final printed amount is truth; a breakdown that doesn't sum never rejects a final amount).
- image_12 = **USD 33.50**, converted with the exchange_rates.csv row for 2025-10-01.
- image_04 fails closed (cropped, history-only).
- Models: HF-only (Qwen3-VL-235B + gemma-4-31B, extra reads at other resolutions). **No Claude dependency**: the Anthropic org cap blocks it until 2026-10-01.

## 9. Phases
- **Phase A (now): OCR.** Only **lead** and **ml-engineer** execute. ml-engineer temporarily owns `code/src/extract/images.rs`, `extract/normalize.rs`, new `extract/witness.rs`, `prompts/image_transcription.v3.md`, and `bin/bakeoff.rs`.
- **Phase A DONE:** live N=5 persisted-read run shows 0 wrong figures, accepted figures identical in 5/5 runs, 02/05/10/11 accepted 5/5, image_04 fail-closed. Lead posts `plan msg=lead: PHASE A DONE <sha>`.
- **Plan-mode agents** (analyst, engine, extraction, verifier, integrator) write an executable Phase B plan meanwhile. The user approves each one after PHASE A DONE.
- **Phase B:** integrator + engine in parallel → extraction absorbs the OCR commits and takes the files back → verifier → final cold runs and ship.
- **Ship gate:** 0 wrong figures; 02/05/10/11 accepted 5/5; two `--cold` runs give byte-identical output.csv; verifier signoff; sample scorer doesn't regress.
