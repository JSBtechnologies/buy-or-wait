# Plan-mode protocol (analyst, engine, extraction, verifier, integrator)

You start in **plan mode** (read-only) during Phase A.

- Don't edit files, commit, or run mutating commands until the user approves your plan.
- Use read-only state commands and context-mode reads if plan mode allows them. Otherwise read state from files.
- Produce an **executable Phase B plan** for your work: exact files and functions, tests, commands, merge order, and what you need from ml-engineer's OCR branch (`atrium/buyorwait/ml-engineer`).
- Keep refining the plan until the lead posts PHASE A DONE. Don't sit idle.
- Finish with **ExitPlanMode**. The user approves after PHASE A DONE.
- **Logging:** appends are blocked in plan mode. Keep a backlog of your entries (verbatim prompts). Your first action after approval is appending SESSION START plus the backlog.
