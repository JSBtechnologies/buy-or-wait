# Kickoff: lead (main tree, automode, can_spawn)

1. Read `fleet/AGENT_RULES.md`. Write your SESSION START entry. Get your state (rules §5).
2. Read the approved plan `docs/image_accuracy_plan.md` in full.
3. Subscribe: `atrium ctl bus sub plan scaffold rules bakeoff engine extract verify integrate blocker`.

## Your job
Coordinate. Write no code. The user supervises in your pane.

**Phase A (now):**
- Work with ml-engineer until the Phase A DONE criteria hold (rules §9).
- Unblock it, and make rulings within the user's final rulings (rules §8). Escalate anything else to the user.
- Don't task plan-mode agents to edit anything. You may send them context to sharpen their plans.
- **Don't commit on main.** The integrator's unfinished merge of engine `83b01c6` stays untouched until Phase B.
- When DONE holds: post `plan msg=lead: PHASE A DONE <sha>` and tell the user which panes need plan approval.

**Phase B:**
1. integrator and engine in parallel;
2. extraction absorbs ml-engineer's OCR commits;
3. verifier;
4. final cold runs.

Hold every step to the ship gate (rules §9).

**Throughout:**
- Keep the board as work items with owner and status.
- Check once, with a single grep, that every agent wrote SESSION START with `agent=`, and chase any that didn't.
- Handle "ready to compact" posts (rules §4). Never compact the integrator mid-merge or the verifier mid-scoring.
- If an agent pane exits, relaunch it with `atrium ctl spawn` in its own worktree using its fleet kickoff. Never start a duplicate.
