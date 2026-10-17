# Final live validation: summary

The run of [`CHECKLIST.md`](CHECKLIST.md) on 2026-10-17: the fork built from the `code-review`
branch, driven through the isolated live harness (`script/live-harness/zh`), one agent per session.
GitHub writes went only to the private sandbox `drew-council/code-review-sandbox`. The raw results
(each session's `RESULTS.md`, screenshots, saved shell output) stayed in the orchestrator's state
directory and aren't part of the repository. What the findings led to is in
[DESIGN.md §0](../../DESIGN.md#0-status--how-to-resume): the paragraphs **Fix 1**, **Fix 2** and
**Fix 3**, and **Known issues / notes for the next slices** for what was recorded instead of fixed.

## Sessions

| Session | Scope | Result | Then |
|---|---|---|---|
| V1 | Local targets, picker, line cursor, vim navigation | 28/28 PASS | One minor finding (`e` on a removed row of a unified snapshot) fixed in fix 1 |
| V2 | Panel, reviewed marks, filters, mark persistence | 16/17 PASS | V2.08 (filter counts) and V2.15+ (focus lost after closing a split clone) fixed in fix 1 |
| V3 | Comments, export, outdated comments | 20/24 PASS | V3.15, V3.17, V3.02 fixed in fix 1; V3.11 (Tab in insert mode inserts the editor's soft tab) accepted as designed |
| V4 | Pull requests: picker, checkout, description, threads, comment gate | 20 PASS + 1 N/A | N/A: `gh`'s keyring token isn't reachable from the isolated harness (clean fallback); observations polished in fix 2 |
| V5 | Submit flows (sandbox writes) | 14/14 PASS | Observations polished in fix 2 |
| V6 | CLI, `--wait`, `zed://review` links | 18/18 PASS | Observations polished in fix 2 |
| V7 | Robustness: restarts, two windows, split toggles, narrow window, large repository | 8/8 PASS + 3 exploratory FAILs | X1 (focus after a split change), X2 (`ctrl-w h` after a split rebuild), X3 (old-side card after a pure deletion) fixed in fix 1 |
| V8 | Non-vim mode | 7/7 PASS | Palette ranking trap recorded as a known issue |

## Regression sweeps (after fix 1 and fix 2)

| Sweep | Scope | Result | Then |
|---|---|---|---|
| R1 | V1, V2, V3, V7, V8 condensed, plus every earlier FAIL | 74/74 PASS | O1 (panel header with a second repository) and O5 (`g g r` at the top) fixed in fix 3; O2, O3, O4, O6 recorded as known issues (upstream or by design) |
| R2 | V4, V5, V6 condensed (pull requests, submit, CLI) | 48/50 PASS | N1 (a deleted file's stale buffer, upstream-inherited) recorded as a known issue; the one KNOWN row (panel selection after `]t` from the left side) and O2 (posted summary below the capped description) fixed in fix 3; O1 (typing after a refused `c`) recorded as a known issue |

Fix 3 was checked live too (its four fixes, plus a short smoke test of the uncommitted, branch and
pull request reviews); see DESIGN.md §0 **Fix 3**.

Sandbox pull requests after the run: A #1 (read-only fixture), B #14 (bot, clean), C #15 (own,
clean), D #4 (API probe residue).
