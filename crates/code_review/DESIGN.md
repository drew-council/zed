# `code_review` — design & agent handoff

A tuicr-style code review mode built into Drew's fork of Zed (`drew-council/zed`, checked out at
`~/personal/zed`). This document is the single source of truth for resuming the work: it records the
agreed design (from a design-grilling session with the user on 2026-10-16), the research behind it
(with file:line pointers), the implementation plan, and the build/dev setup.

> **Line numbers** in this file refer to Zed commit `2d4c4831943153f63ad0aeddf49c8f5a3d008e75`
> (main, 2026-10-16, version 1.26.0-dev) — the fork's `main` at the time of writing. Re-verify
> after rebasing. The `code-review` branch is based on `v1.25.0-pre`, 43 upstream commits behind
> that commit, so expect small drift there too. tuicr references are to `~/personal/tuicr` branch `bizmythy-tweaks` @ `5ea82dc`
> (upstream v0.27.0 + the user's fork commits).

---

## 0. Status & how to resume

**Status: implementation complete; final live validation passed (see
`crates/code_review/research/final-validation/`).** Milestones 1–9 and polish A/B are done, and so
is the final validation: eight sessions over its checklist (`research/final-validation/
CHECKLIST.md`), fix 1 (slices A and B of `plans/FIX-final-validation.md`: every FAIL), fix 2 (slice
C plus the orchestrator's additions: the validators' cheap observations), two regression sweeps of
the fixed build (R1, local reviews: 74/74; R2, pull requests and the CLI: 48/50, one
upstream-inherited FAIL and one known issue) and fix 3 (the sweeps' last cheap findings, see "Fix
1", "Fix 2" and "Fix 3"; per-session counts in `research/final-validation/SUMMARY.md`). What is
left of the observations is under "Known issues". After the user's own testing: opening a review
shows the review panel (see "UX: opening a review shows the review panel"), and the UX batch
(`plans/UX-1.md`, the user's answers in its §7) is complete: slice A, the review's overview
scrolling with the diff as one document (see "UX-1 slice A"), slice B, comments with the mouse: a
GitHub-style "+" in the gutter, the comment editor's footer, file header buttons (see "UX-1 slice
B"), and slice C, the review toolbar in the pane, the review's right-click menu, hover controls in
the panel and line numbers that select rows (see "UX-1 slice C"), with a live pass over the whole
batch. Then the Review Pull Request picker, a GitHub search of the repository's pull requests
prefilled from the first `code_review` settings, reachable from the panel's "+" (see "Review Pull
Request picker"; since merged into the Start Review picker's Pull Requests section). Then GitHub
stacks: stacked pull requests are one row per stack in the Start Review picker, which opens the
stack's first pull request to review, and a pull request review in a stack shows the stack and
steps up and down it (see "Stacked pull requests"). Then the user's bug findings on movement: the
review tab dropped its own line cursor and keys for **Zed's own vim, unchanged**, on the read-only
diff (see "Upstream vim in the review tab"), and the gutter's "+" moved between the line numbers
and the text, off the excerpts' expand buttons (see "Gutter buttons don't overlap"). Then issue
references (`#123`, `owner/repo#123`, `GH-123`) in GitHub's text link to GitHub (see "GitHub
references link"), and long lines wrap in the diff by default, on both sides (see "Long lines
wrap"). Then the Start Review picker loads its pull requests about 3× faster and takes most of the
window (see "Faster, bigger Start Review picker"). Then `ctrl-u`/`ctrl-d` work over the diff's blocks
(see "`ctrl-u`/`ctrl-d` over blocks"), and cursor movement was measured against Zed's own editors
(see "Cursor movement speed"), and stepping through a stack shows the next pull request's tab at
once (see "Stepping shows at once"), and the Start Review picker reopens with its last pull requests
while it refreshes them (see "Instant reopen"). Then the overview stopped multiplying over folded
files (see "The overview is one block"), and wrapping follows the language settings again, so
`ctrl-u`/`ctrl-d` move whole lines as in other buffers (see "Wrapping follows the settings"). Then the review stopped redrawing its own UI on every keypress
(see "Less work per keypress"), and file headers read as paths (see "Headers read as paths").
Bugs round 2 (the user's second list; item 1 came first, the rest were fixed on another machine from
the same base and merged in on top of all of the above) is done too: comments stay in the diff's
hunks, with ranges across hunks and from removed lines to added ones as GitHub takes them (see "Bugs
2, item 1" and its follow-up), the overview collapses and expands at once in split view (item 2) and
with `; t`, a squash merge's commit tab leads with its pull request number (item 3, "Titles" under
"Implementation decisions (milestone 3)") while the overview and the panel show the whole title,
wrapped list items in commit messages stay in their list (item 4), drafts are left out of the pull
request search (item 5), comments are written in the buffer font as Markdown (item 6, "Editors"
under "Milestone 5, part 1") and the panel's tree looks like the project panel's (item 7).

**History: milestones 1–7 done: M4 (panel, reviewed marks, filters), a hardening pass over the M1–M4
code reviews (see "Hardening" below), M5 (comments: part 1 "comments core", part 2 "old side,
persistence, panel list" with the M5 part 1 review fixes), M6 (export, see "Milestone 6") and M7
(GitHub): slice (a), the GitHub client (see "Milestone 7, slice (a)"), is done, followed by a second
hardening pass over the M5 part 2 + M6 review (see "Hardening 2"), then slice (b), pull request
reviews from the picker with checkout in place or in a new worktree (see "Milestone 7, slice (b)"),
and slice (c), the pull request's description and its review threads read-only in the diff, with
GitHub's comment gate (see "Milestone 7, slice (c)"), then a third hardening pass over the M7 (a) +
(b) review: token scoping, safe checkouts, Restricted Mode (see "Hardening 3"), then slice (d),
submitting reviews, with the M7 (c) review's fixes (see "Milestone 7, slice (d)"). M7 is complete,
and so is M8, the CLI: `zed [-n|-e|-a] [--wait] --review TARGET [REPO_DIR]` and `zed://review` links
(see "Milestone 8"). M9 (fork ops) is done too: the rebase skill, a `releaseChannel` knob in
`nix/build.nix` and the install guide; applying the install to `~/nixconf` awaits the user (see
"Milestone 9"). Polish A fixed the M7d (submit) and M8 (CLI) review findings (see "Polish A").
Polish B reverted core patch #1, added the opt-in core patch #3 (no cursor highlight on spacers
below an empty row), made Outdated clear when a comment's lines get their text back (stash/pop),
and fixed three small known issues (see "Polish B").** The
long-lived fork branch `code-review`
exists locally (not pushed): upstream tag `v1.25.0-pre` + the fork commits. The fork's `main` is
untouched and equals upstream `main`. Review tabs exist for uncommitted changes, the branch vs a base
(changeable in place), and one commit or a contiguous range (live when it ends at HEAD, a read-only
historic snapshot otherwise), and a GitHub pull request (checked out in place, Zed's way, or in a new
`pr-N` worktree), opened from the `code_review::StartReview` picker or the direct actions
`ReviewUncommitted` / `ReviewBranch { base }` / `ReviewCommits { commits }` / `OpenPullRequest {
reference }`. Each tab has
Zed's own vim cursor and motions, file navigation, an overview at the top that scrolls with the diff
(branch base, or the commit messages; UX-1 slice A),
per-file and per-hunk reviewed marks persisted in Zed's DB (reviewed files fold to their header,
reviewed hunks are dimmed, marks stop counting while the diff content differs), and per-tab filters (include /
exclude regexes, hide reviewed, generated/vendored files from `.gitattributes` hidden by default). The
review panel shows the active review's file tree with progress, filters, path search and a context
menu, and below the tree the review's comments. Comments on lines and ranges of either side (`c`;
old lines on the left side of a split diff or the removed rows of a unified one), files (`C`) and the
whole review (`;c`) open vim editors in insert mode, render as markdown, have types, and are edited,
deleted and navigated with `i`/`Enter`, `dd` and `]t`/`[t`; they are stored in Zed's DB per
review, follow the code while the review is open and are flagged outdated when their lines changed
while it was closed or were deleted (current again once their text is back, e.g. after a stash
pop). `; y` copies the review as Markdown in tuicr's format, `; Y` the comment at the cursor, Copy
Lines (menu, palette) the selected lines. A GitHub client resolves a checkout's GitHub repository (a
fork's parent), lists and reads its pull requests, threads, reviews and commentable lines; the
picker lists pull requests; a pull request review shows the description and review summaries in its
top block, the existing review threads read-only below their lines (resolved, outdated and bot
threads hidden by default, with toggles; GitHub's line numbers are translated through the
checkout's uncommitted and unsaved changes), and only takes comments on lines inside GitHub's diff
hunks, with the pull request's head checked out. `; s` submits a pull request review (Comment /
Approve / Request Changes, from a modal that shows what is sent and what blocks it) or finishes a
local one (its Markdown to the clipboard, or to a `--wait` waiter). `zed --review TARGET [REPO_DIR]`
opens any of these reviews from a terminal in the window that has the repository open (`--wait`:
the posted review's URL or the finished review's Markdown on stdout, exit 0; closed or Zed quit,
exit 2; errors, exit 1), and `zed://review?repo=PATH&target=TARGET` links open them too (asking
before a pull request's checkout).

What exists:

| Path | What |
|---|---|
| branch `code-review` | `v1.25.0-pre` + fork commits; all feature work goes here |
| `crates/code_review/DESIGN.md` | This document |
| `crates/code_review/src/code_review.rs` | `init` (registers actions per workspace), `CodeReviewSettings` (the `code_review` settings), all `code_review::` actions (incl. `ReviewBranch`/`ReviewCommits` with fields), keymap loading helpers, test helpers (`init_test`; `test_support`: `init_vim_test`, the review fixture, editor helpers, fake-repo commit/branch helpers like `record_commit`) |
| `crates/code_review/src/target.rs` | `ReviewTarget::{Uncommitted, Branch { base: BranchBase }, Commits(CommitRange), PullRequest(PullRequestTarget)}`, `BranchBase` (the only way to build a branch base: syntax check), `is_same_review`, `review_key`, empty-state label, `parse_commit_spec` (`rev` / `A..B`), `PullRequestReference` + `parse_pull_request_reference`, `validate_branch_name` (forge branch names); M8: `TargetSpec` + `parse_target_spec` (the CLI's TARGET grammar) |
| `crates/code_review/src/commits.rs` | `CommitDiffList` (a commit range's files: live over project buffers, or historic snapshot buffers), `GitBlob`/`build_buffer` copied from `CommitView`, git helpers over `Repository::send_job` (`first_parent_chain`, `resolve_commit_spec`, `check_branch_base`, `resolve_commits`) |
| `crates/code_review/src/diff.rs` | `ReviewDiff`: copy of `git_ui`'s `DiffMultibuffer`, driven by a `DiffSource` (`DiffBufferList` for uncommitted/branch, `CommitDiffList` for commits); `ReviewAddon` (header statuses against the review base + the reviewed checkbox: every header control goes here); loading/error state; `set_branch_base`; file order, `path_at_cursor`, `move_to_path` (polish B: `file_start_autoscroll`), single-file mode; per-file `FileDigest`s, the filter-hidden set (`set_hidden`), `ReviewDiffEvent` |
| `crates/code_review/src/session.rs` | `ReviewKey`, `ReviewSession` (reviewed files/hunks and the `CommentStore` of one review, shared by its tabs through the `ReviewSessions` global; marks and comments loaded from/saved to the DB, positions flushed on close/quit, old reviews pruned; M7d: `mark_submitted`, `ReviewOutcome` + `wait_for_outcome`/`has_outcome_waiters`/`resolve_outcome` for `--wait`; polish A: `shown_by` + `show`/`hide` (waiters follow the tabs showing the review), submitted comments immutable), FNV-1a `file_digest` (content hash + hunk keys), `reconcile` |
| `crates/code_review/src/comments.rs` | Comment model: `CommentId`, `Side`, `CommentType` (cycle, labels, colors, legend definitions), `CommentTarget` (`Review`/`File`/`Lines`), `CommentState`, `Comment` (`location()`, JSON for the DB), `CommentStore`, `CommentEvent`; unit tests |
| `crates/code_review/src/comments/blocks.rs` | `CommentBlocks` (one per review tab): a `CommentView` per comment, the comment and thread blocks on both sides of the diff (`ItemKey::{Remote(RemoteKey), Comment(CommentId)}`, `set_remote_items`/`RemoteItem`, `reconcile`, `placement`, `line_row_anchor`, `old_row_in_unified_diff`, `block_below_row`, re-hosting when the left side is recreated), the tab's own anchors (`TabAnchors`/`LineAnchor`, parked per session on a key switch; polish B: each holds its buffer, and remembers where deleted lines were, `restored_lines`) with the outdated check (both ways since polish B), deleted-line detection and live line numbers (`update_anchors`), comment editors (incl. the `vim::VimSave` capture) (`begin_edit`, `leave`, focus return + guard), the comment keys (`c`/`C`/`;c`/`i`/`dd`/`]t`; UX-1 B: `add_line_comment_on` a given side and rows, `add_file_comment_for`, `set_type`), comment at cursor, `Y`/visual `y` text; comment GPUI tests (UX-1 B: the mouse ones too) |
| `crates/code_review/src/comments/blocks/gutter.rs` | UX-1 B: the gutter's "+" (`install` sets the editor's gutter row renderer on each side; visibility, the row's target and the gate's refusal, the click and the drag from it, `select_display_rows`); UX-1 C: the area over each row's line number (`render_row`, `line_number_column`, `select_from_line_number`): clicks on line numbers select rows instead of opening the file |
| `crates/code_review/src/comments/view.rs` | `CommentView`: the comment card (type and OUTDATED/SUBMITTED badges, location, edit/delete buttons, markdown or the editor; UX-1 B: the editor's footer with the type menu, Cancel, Save, Delete) and its `ReviewComment` key context with the leave/keep actions |
| `crates/code_review/src/comments/remote.rs` | M7c: `RemoteThreadView` (a pull request's review thread, or a file's outdated/moved threads, read-only: badges, location, comments with author/time/markdown, "…N more on GitHub", Open on GitHub), `RemoteThreadContent`, `thread_location`; M7d: `open_web_link` (only http(s) opens), `HoveredLink` + `untrusted_markdown` (links show their destination while hovered); fix 2: `github_markdown` (GitHub's `<details>`/`<summary>` tags dropped); `link_references` (GitHub's issue references as links, through `github_markdown`, which takes the pull request's repository) |
| `crates/code_review/src/submit.rs` | M7d: `comment_body`/`review_body` (`[TYPE] ` prefixes), `needs_body` (LV §5.1), `plan_review` → `PlannedReview` (the payload mapping, counts, outdated and refused drafts), `SubmitReviewModal` (events, blockers, warnings, body prompt; polish A: re-plans on every change, starts `PullRequestState::submit`/`check_uncertain_submit` and shows their reply, can't be dismissed while its submit runs), `finish_local_review`; payload/flow/modal tests |
| `crates/code_review/src/export.rs` | M6: `review_markdown` (tuicr's Markdown export), `ExportPreamble`; golden tests |
| `crates/code_review/src/forge.rs` | M7: forge-neutral pull request types (`ForgeRepo`/`ForgeRepoRef`, `RepositoryContext`, `PullRequestSummary`/`Details`, `HeadStatus`, `RemoteThread` + `ThreadAnchor`, `RemoteReviewSummary`, `ChangedFile` + `Hunk`, `CommentableLines` with `allows`/`allows_range`/`current_path`; M7d: `ReviewEvent`, `ReviewSubmission`, `ReviewLineComment`/`ReviewFileComment`, `SubmittedReview`; stacks: `PullRequestStack` (`review_start`, `neighbor`), `StackDirection`), the `Forge` trait (with `submit_review`; stacks: `pull_request_stacks`, `pull_request_stack`), `resolve_forge`, `error_message` / `github_error` / `show_error_toast` |
| `crates/code_review/src/github.rs` | M7: `GitHubError` + hints, endpoints, host rules (`GitHubHost::{Cloud, Configured, Unverified}`, `GitHubHosts`: user-configured providers, registry hosts, SSH aliases via `ssh -G`), remote parsing (`remotes`: the checkout's GitHub remotes, preferred first), token lookup (gh's rules) and cache per (host, work dir), `GitHubClient` (GraphQL/REST over `HttpClient`, paging), response parsing (threads, summaries, files/patch hunks, stacks), `GitHubForge`, the forge cache (keyed by every remote URL); `test_support` (fake GitHub `HttpClient`, fixtures, `FakeLookups`) and tests, an ignored live test |
| `crates/code_review/src/cli.rs` | M8: `is_review_url`, `ReviewLink` (`zed://review` parsing), `CliReviewRequest` + `handle_cli_review` (the CLI's request: the ping marker, open, `--wait`, the quit hook, one `Exit` per request through `CliReply`), `open_review_link` (a link Zed got without the CLI), `open_review` (a `TargetSpec` in a repository: checks, then `deploy_in_repo` or `open_pull_request`), `confirm_link` and the large-review prompt (links only), `repository_root`, `find_review_workspace`; CLI GPUI tests with a fake response sink (`tests::Cli`, also used by the pull request link test) |
| `crates/cli/src/review.rs` | M8 (fork file in the upstream `cli` crate): `--review` → the `zed://review` link + the repository's canonical path, its usage errors, `requested()` (exit 1 for clap's usage errors), `check_exit` (polish A: a Zed that answered without the ping marker doesn't know `--review`) |
| `crates/code_review/src/opening.rs` | `OpeningReview`: the tab a stack's pull request takes while `open_in_stack` fetches and checks it out — its number, title, author, state and branches from the stack's summary at once, then "Checking out …" or the error |
| `crates/code_review/src/pull_request.rs` | M7b: `PullRequestState` (forge + details, one per review key through the `PullRequests` weak global), `open_pull_request` / `OpenPullRequestOptions` (the entry point the CLI calls; one at a time per repository, `OpeningPullRequest`; M8: `ReviewCancelled`, `repository_at` crate-visible), the in-place flow, `plan_worktree_fetch`, `trusts_head_repository` and the new-worktree flow (own `git worktree add --detach`, `trust_worktree`, `repository_at`). M7c: threads, summaries, `CommentableLines`, `ThreadFilters`, `load`/`reload` (+ forge re-resolution after auth errors, from any open checkout: `add_checkout`, `resolve_forge`, `use_forge`), `PullRequestEvent`, `place_thread`/`ThreadPlacement`, `check_comment` (the per-tab gate)/`check_comment_target`; M7d: `LocalChanges` (per tab: each file's diff against HEAD, `head_to_working`/`working_to_head` → `LineMapping`; polish A: `read_head_texts`); polish A: `submit`/`SubmitRequest`/`SubmitTarget`/`SubmitReply`, `uncertain_submit`/`check_uncertain_submit`, `retry_with_new_token`; stacks: `PullRequestState::stack`/`stack_position` (`StackPosition`), `open_stack_neighbor`, `open_in_stack` (in place, the new tab replaces the old); PR review GPUI tests and their `Fixture` |
| `crates/code_review/src/checkout.rs` | M7b: `CheckoutStep`, `LocalState` (`read_local_state`/`local_state`: HEAD, base present, local branches + upstreams, read from git), the pure `plan_checkout`, `head_remote`, `RestrictedMode` + `ensure_trusted`, `run_checkout` (Zed's fetch and branch switch, plus our own git jobs: `fast_forward` with the branch check, `ensure_fast_forward`, detach; `run_git` with Zed's safety flags and askpass), the askpass delegate; planner and real-git tests |
| `crates/code_review/src/github/queries.graphql` | M7: every GraphQL operation in one document (sent whole with `operationName`) |
| `crates/code_review/src/github/stacks.graphql` | Stacks: the stacked pull request operations, sent after `queries.graphql` (`STACK_QUERIES`) so that a GitHub without stacks fails only them |
| `crates/code_review/test_data/github/*.json` | M7: trimmed live responses from the sandbox repo (PR A #1 threads/summaries/details/head/files, PR D #4 files, the open list, the draft #17 found by `is:draft`, the `drew-council/zed` fork context, a NOT_FOUND) |
| `crates/code_review/src/persistence.rs` | `CodeReviewDb` DB domain (migration 0: `code_review_sessions`, `code_review_reviewed_files`, `code_review_reviewed_hunks`; migration 1: `code_review_comments`), queued writes, `prune`, `sync` barrier |
| `crates/code_review/src/gitattributes.rs` | `.gitattributes` parser/loader for `linguist-generated`/`gitlab-generated`/`linguist-vendored` (tuicr's rules, via `ignore`'s gitignore matcher) |
| `crates/code_review/src/reviewed.rs` | Header controls (UX-1 B: "Comment" button, checkbox labelled "Reviewed"), `ReviewHunkRenderer` (hunk control "Mark Reviewed"/"✓ Reviewed"), dimmed reviewed-hunk painting; reviewed-mark GPUI tests |
| `crates/code_review/src/toolbar.rs` | UX-1 C: `ReviewToolbar` (`ToolbarItemView` added to every pane by `register`, from `init`, after Zed's own items): counts (`summary_text`), single-file info, stack position with previous/next pull request in the stack, previous/next file/hunk/comment, Review Comment, single-file toggle, Copy as Markdown, Submit/Finish Review; toolbar GPUI tests |
| `crates/code_review/src/context_menu.rs` | UX-1 C: the review's right-click menu in both sides of the diff (`install` → `Editor::set_custom_context_menu`; entries filled right after the click: `menu_items` → `MenuItem`/`MenuEntry`, disabled entries with their reason); menu GPUI tests |
| `crates/code_review/src/top_block.rs` | `ReviewTopBlock`: the review's overview, a block above the diff's first file that scrolls with it (UX-1 slice A; `block_properties`, `is_empty`) — branch base line with a Change Base button, the commit messages (collapsible), or the pull request's block (header, its stack (clickable), warnings, description and review summaries, thread toggles, load errors; collapsible, expanded by default); then the review-level comments; commit tab titles |
| `crates/code_review/src/picker.rs` | `StartReviewModal` / `StartReviewDelegate`: Local section (uncommitted, branch vs base, recent commits with range multi-select), Stacks section (one row per stack the search found pull requests of: `ListedStack`, `group_search_results`, `render_stack_row`), Pull Requests section (the GitHub search of `code_review.pull_request_search`, edited in place with `EditPullRequestSearch`, approved-by-you hidden, open by reference, "Review in Worktree"); `CommitSelection`; the `OpenPullRequest` action; `render_pull_request_row`, `pull_request_state`, `stack_member_label`. (The separate `pull_request_picker.rs` of the Review Pull Request picker was merged into this picker.) |
| `crates/code_review/src/view.rs` | `ReviewView` workspace item (key context `CodeReview`), reuse-or-create `deploy`/`deploy_in_repo`, titles, Change Base (`open_base_picker`/`select_base`/`change_base`), handlers for the review keys and the direct actions, `FocusDiff`; its session, filters (`ReviewFilters`), attributes, fold transitions, `r`/`R`/`]c`/`[c`, the panel's and toolbar's read API (`panel_files`, `summary`, `single_file_position`, `draft_comment_count`, `shown_thread_count`), `submit_review`/`copy_review_as_markdown`; GPUI tests |
| `crates/code_review/src/panel.rs` | `ReviewPanel` (left dock): follows the active review tab; file tree (compacted directories, status letters, ✓, icons, indent guides), header with progress/status line/menu (and the stack position with previous/next in a stacked pull request review), filter and search prompts, context menu (pull request entries in PR reviews), empty states, the comment and thread list; workspace-level filter/copy actions (`CopyPullRequestUrl` too); UX-1 C: hover controls (a file row's reviewed checkbox, a comment row's edit/delete); GPUI tests |
| `assets/keymaps/code_review.json`, `code_review_vim.json` | Built-in keymaps (non-vim: `tab` → `picker::MultiSelectNext` and `ctrl-r` → `TogglePullRequestFilter` in the Start Review picker, the review panel's keys, `ctrl-enter`/`ctrl-s` in comment editors; the review tab's keys are vim-only, `] s`/`[ s` for the stack) |
| `crates/code_review/research/benchmark-results.txt` | Measured build times (see §8) |
| `crates/code_review/research/github-api-live-verification.md` | M7's live GitHub API probes against the sandbox repository (payload shapes, line/side rules, error texts, pending reviews, the sandbox's PRs and its reset recipe), the source of §4's and `submit.rs`'s rules |
| `crates/code_review/research/final-validation/` | The final live validation: `CHECKLIST.md` (sessions V1–V8 as run, with their setup) and `SUMMARY.md` (results per session, the fixes they led to, the sweeps R1/R2) |
| `crates/code_review/research/cargo-timing-*.html` | `cargo --timings` reports for the clean build and the one-week bump |
| `script/live-harness/README.md` | `zh`: headless real-GUI Zed harness (Xvfb, input, screenshots) for agent verification; costs CPU, use only when a slice asks. M8: `zh cli [-C DIR] ARGS` runs the fork's CLI against the isolated instance only |
| `.claude/skills/rebase-code-review/SKILL.md` | M9: the fork rebase procedure (§2.1) as a Claude Code project skill: exact old tag, backup, per-commit keep/adapt/drop, the conflict table for every upstream file the fork touches, `range-diff`, capped build/tests and a Nix evaluation, DESIGN refresh, push only after the user confirms |
| `crates/code_review/INSTALL.md` | M9: installing the fork as the user's only Zed from `~/nixconf` (flake input, overlay with `releaseChannel = "stable"`, removing the `zed` alias), first launch, updates, Cachix, rollback. Not applied |
| `target/` (gitignored) | **Warm build cache** (see §8.3): `release-fast` for `-p zed` plus `debug` test artifacts, 43 GB |
| `.rules` ("Drew's Zed fork" section) | Pointer to this file for agent sessions (`CLAUDE.md`/`AGENTS.md` symlink to it) |
| `README.md` first two lines | `> [!IMPORTANT]` banner required by the `.rules` HARD RULE; never remove it (the user does) |

Upstream glue so far (all append-style, §3.10): root `Cargo.toml` (member + workspace dep),
`crates/zed/Cargo.toml` (dep), `crates/zed/src/main.rs` (`code_review::init` after `git_ui::init`),
`crates/zed/src/zed.rs` (`ReviewPanel::load` in `initialize_panels`; `code_review::key_bindings`
after the base keymap and `code_review::vim_key_bindings` right after `vim.json` in
`load_default_keymap`; `"code_review"` in `test_action_namespaces`); M8: `crates/zed/src/zed/
open_listener.rs` (+27: `OpenRequestKind::Review`, the link in `parse`, the CLI request routed to
`code_review::cli::handle_cli_review`), `crates/zed/src/main.rs` (+11: the `Review` arm of
`handle_open_request`), `crates/cli/src/main.rs` (+58/−4 after polish A: `--review` and its
conflicts, help, `try_parse`, the link and path, the ping marker, "Zed exited before the review
finished") and the new `crates/cli/src/review.rs`; M9:
`nix/build.nix` (`releaseChannel`, §3.10). Core patches #2 and #3 are
applied (`crates/editor/src/scroll.rs`, `scroll/autoscroll.rs`, `editor.rs`; §3.10); #1 was reverted
in polish B; UX-1 slice A added #5–#7 (`display_map/block_map.rs`, `split_editor_view.rs`,
`element.rs`, `element/mouse.rs`, `editor.rs` and the new `gutter_row.rs`; §3.10). M3 and M4 added
no upstream glue (M4: new deps `db`, `file_icons`, `fs`, `ignore`, `regex`, `smallvec`, all already in
`zed`'s graph; dev-deps `db` and `buffer_diff` with `test-support`).

**Implementation decisions (milestone 1)** — implementation details, not user questions:

- Keymaps are `include_str!`-embedded by `code_review` (`key_bindings` / `vim_key_bindings`) instead
  of read through `settings::SettingsAssets`: rust-embed (release builds) doesn't notice new files,
  and each edit of an embedded keymap would rebuild `settings` and everything downstream. Current
  bindings (vim only): `; h` in the review editor's normal mode and `; l` in the panel →
  `code_review::ToggleFocus` (§2.5 `;h`/`;l`, approximated by toggle-focus until the panel exists).
- `ReviewDiff` drops from `DiffMultibuffer`: conflict UI, git-panel coupling, upstream's
  `diff-review` button/comment count, `GitPanelSettings` sorting, folding of deleted/untracked files
  (folding is reserved for reviewed marks, §3.7), and `move_to_path`/`pending_scroll` (copy them
  back from `git_ui/src/diff_multibuffer.rs` when the panel needs scroll-to-file).
- Hunk renderer `HiddenUnstagedDiffHunkRenderer`: no stage/restore controls, staged and unstaged
  hunks drawn alike. Read-only = `MultiBuffer::new(Capability::ReadOnly)` + `rhs.set_read_only(true)`
  (the split LHS is read-only by construction). Note `set_diff_hunk_renderer(None)` collapses hunks.
- File order: Zed's git-panel tree order (directories before files at each level; `PathKey` sort
  prefix 0 + NUL-prefixed directory components). tuicr lists a directory's files before its
  subdirectories — revisit with the panel (M4) if the user prefers that.
- The repository is fixed when a review opens (the active repository); `deploy` reuses an open tab
  with the same target and repository. No repository → auto-hiding toast, nothing opens.
- `ReviewView`: `Item::capability` = `ReadOnly` (tab tooltip "Read-Only Tab"); `act_as_type` → self,
  `Editor` (RHS), `SplittableEditor`, `ReviewDiff`; `as_searchable` → the `SplittableEditor`;
  `can_split`; `is_dirty`/`can_save` false; not a `SerializableItem` yet, so review tabs don't come
  back after a restart (needs persisted targets).
- `ReviewPanel`: left dock only (`position_is_valid`), since no setting persists a move;
  `activation_priority` 4 (must be unique per dock: debug builds panic on duplicates, `dock.rs:786`);
  icon `ListTodo`, always shown; `persistent_name` "Review Panel", `panel_key` "ReviewPanel".

**Implementation decisions (milestone 2)** — the M2 plan's defaults unless noted:

- **No column pinning** (deviates from §3.4 and the plan): a cursor pinned to column 0 breaks `n`,
  because `Editor::match_index_for_direction` (`editor/src/items.rs:1969`) picks the first match
  starting *after* the cursor, so it re-finds the same row's match forever. The caret is hidden and
  every column motion is a no-op, so the column vim keeps is invisible (search, `G`, `ctrl-d` can leave
  it non-zero; `e` then opens the file at that column).
- Painting: `show_local_selections = false` on both sides (re-asserted on every refresh, since a rename
  turns it back on) and `highlight_rows` with three marker types (context/added/deleted rows): color =
  editor background, then the row's diff background, then the theme's `players().local().selection`, so
  green/red stay visible under the cursor (row highlights replace diff backgrounds). One marker set
  serves cursor and visual rows. Mirroring maps the focused side's display rows to the real rows of the
  other side (split display rows are aligned); a row facing a spacer gets no mirror.
- `LineCursor` observes the `SplittableEditor` to configure each new LHS (hidden selections,
  `set_forbid_horizontal_autoscroll`) and to follow focus between the sides.
- `h`/`l` (and `left`/`right`): 4 columns on the focused side (the x offset is shared). Reads are clamped
  to the widest line, so `l` at the right edge never accumulates. `h` at x = 0 dispatches `FocusPanel`
  in normal mode only (in visual mode it only scrolls). In split view, `h` at the left edge of either
  side focuses the panel (orchestrator decision; `ctrl-w h/l` switches sides).
- **Soft wrap off by default** in the review editor (tuicr's `wrap = false`): without an override, the
  first file's language settings decide for the whole multibuffer (Plain Text wraps), so `h`/`l` would
  depend on which file comes first. `editor::ToggleSoftWrap` is captured on the `CodeReview` div and
  switches both sides between no wrap and editor-width wrap (Zed's own toggle falls back to the
  language settings first, which may change nothing). *Superseded*: the review wraps by default
  (see "Long lines wrap").
- Keys (`code_review_vim.json`): `v`, `V`, `ctrl-v`, `ctrl-q` start line-wise visual. Null in normal +
  visual: `w W b B e E ge gE f F t T ; , ^ $ | home end g0 g^ g$ g<home> g<end> gM space backspace`,
  `( ) % * # g* g#`. Null in normal: `i a I A o O s S c C r R gi insert`; in visual: `i a c s S r R I A gI gA`.
  Left as vim's: row motions, `0` (binding it would break counts like `10j`), `y`, `d` and the editing
  keys (blocked by read-only, though `dd`/`x` may move the cursor), `]c`/`[c`, `K`, `gd`. `; e`/`; l` also
  work in the panel.
- File navigation: `]f`/`[f`, `}`/`{` stop at the first/last file; `]u`/`[u` wrap around (skipping the
  current file) — tuicr's behaviour, confirmed by the orchestrator. `is_file_reviewed` is a stub (false)
  until M4. Landing: the first row of the file's first excerpt (`location_for_path`), scrolled with
  `Autoscroll::focused()`; on the LHS through its own multibuffer (same `PathKey`s). Order: sorted
  `PathKey`s of files with hunks (`ReviewDiff::file_order`), including files hidden by single-file mode.
- Counts: not supported by `code_review` actions (orchestrator: no vim patch). Each handler dispatches
  `vim::ClearOperators` (built by name) so a typed count doesn't multiply the next motion.
- Single-file mode (`; f`): excerpt visibility in `ReviewDiff` (`single_file`, `sync_excerpts` honours
  `is_visible`) — not folding (reserved for reviewed marks), not a second multibuffer. It shows the new
  file before hiding the others (an empty multibuffer moves focus to the placeholder), leaves the mode
  when the shown file loses its changes, and keeps files that change meanwhile hidden. No indicator yet.
- `e` = `Editor::open_excerpts` on the focused side (the LHS is translated by `SplittableEditor`).
- Panel: subscribes to workspace events — adopts the active `ReviewView` on `ActiveItemChanged`, falls
  back to another open review (or "No review open") on `ItemRemoved`, and keeps the last active review
  while another kind of tab is active. `FocusDiff` = active review tab, else the panel's review, else any
  open review. `TogglePanel` closes the panel only when it is the left dock's visible panel and returns
  focus to the diff if it was in the panel. M1's `; h`/`; l` → `ToggleFocus` bindings became
  `FocusPanel`/`FocusDiff`; `ToggleFocus` stays the panel's toggle action.
- Tests: `vim` is a dev-dependency (the keymap only loads when its actions are registered);
  `test_support::init_vim_test` binds `default-linux.json`, `vim.json` and ours in `zed`'s order.

**Implementation decisions (milestone 3)** — the M3 plan's defaults unless noted. No upstream file
changed (only `Cargo.lock`); `code_review` now depends on `git_ui` (branch picker), `picker`,
`fuzzy`, `markdown`, `menu`, `time`, `time_format`, `serde`, `schemars`, `log`.

- **Branch**: the base is a ref *name* (`Repository::default_branch(true)`, e.g. `origin/main`), passed
  to `DiffBase::Merge`, so the review follows fetches; title "‹branch› vs ‹base›" ("HEAD" when
  detached, updated on `RepositoryEvent::HeadChanged`). `ReviewBranch { base: Some(..) }` checks the
  base before opening (`check_branch_base`, see Hardening), so a typo is an error toast instead of an
  empty review. No default branch → the branch picker opens. Until the first merge-base tree diff has
  started the review shows the spinner, not the empty label (`ReviewDiff::awaiting_tree_diff`).
- **Change Base** works in place (Zed's Branch Diff precedent) through Zed's own branch picker
  (`git_ui::branch_picker::select_modal`); if another tab already reviews the branch against the
  chosen base, that tab is activated instead. Deviation: `ChangeBase` and `ToggleHeader` are
  view-level actions on the `CodeReview` div (in the palette only while a review is focused;
  `ChangeBase` only in branch reviews) rather than workspace actions that propagate; the top block's
  base button calls the view directly.
- **Commits**: git's `A..B` semantics (A excluded) in `ReviewCommits { commits }` and
  `parse_commit_spec`; a single commit is reviewed against its first parent (merges too, like
  CommitView's `--first-parent`); every revision (full shas too) resolves through `rev^{commit}`.
  Root commits diff against git's empty tree. **Live vs snapshot is decided once**, when the review
  opens (range head == HEAD): a live review keeps showing working-tree buffers even if HEAD moves
  later, and its split clones share its `CommitDiffList` (same mode, one load). Live reviews list only
  the range's files (`diff-tree base head`) over project buffers (`GitStore::open_diff_since`), so
  uncommitted edits to those files show, files changed only in the worktree don't.
- **Snapshots**: `Capability::ReadOnly` buffers whose file is a `GitBlob` (`DiskState::Historic`, not
  local → no language server), syntax highlighted. Deviation: both sides load through
  `git cat-file --batch` (`load_revisions` with `head:path` / `base:path` specs), one job per 32 files
  (other git work runs in between; closing the tab stops the rest), instead of a `load_blob_content`
  call per old blob. Binary files (a NUL byte in the first 8000, git's rule; other non-UTF-8 text is
  decoded lossily) show a "(binary file not shown)" line as an addition or deletion so they stay
  listed; entries git lists without a blob on one side (submodules) are skipped with a warning. A
  `GitBlob`'s path is worktree-relative (the project may be a subdirectory of the repo). All snapshot
  buffers are built before the first excerpt appears. `e` on
  a snapshot row opens the working-tree file at the same row (clamped), like CommitView's "open file
  at HEAD". The checkout, index and HEAD are never touched.
- **Start Review picker**: Local section = "Uncommitted changes", "Branch vs ‹base›" (secondary
  confirm or no default branch → base picker; a failed default-branch lookup shows its error under the
  row), then the first-parent chain from HEAD (50 commits).
  Range selection follows tuicr (`CommitSelection`: outside extends and fills gaps, ends shrink,
  middle cuts back to the newer commits), so non-contiguous selections are impossible; only commit
  rows get checkboxes. Multi-select via `tab` (bound in `StartReview > Picker > Editor`),
  `ctrl-shift-s`, ctrl-click; a selection wins over the highlighted row on Enter. Deviation: a query
  filters fuzzily but keeps history order (no score sort). `ReviewCommits` without `commits` opens the
  picker with the newest commit selected. No default key for `StartReview` (palette).
- **Top block**: a fixed header above the editor (CommitView precedent; an editor block can't sit
  above the first file header and is lost when the split LHS is recreated). Branch: one line
  "Changes on ‹branch› since its merge base with [‹base› ▾]". Commits: collapsed by default (summary =
  the title with the whole subject, truncated to the width on one line, plus "Read-only snapshot"),
  expanded = up to 100 commits newest first with sha, subject (wrapped), author, date and the
  markdown body (16 lines, then it scrolls; "…and more commits" past the limit). Commit messages
  can't be commented on. `ToggleHeader` is `; t` in the review tab's vim normal mode (user decision,
  bugs round 2; the disclosures' tooltips show it) and in the palette.
- **Titles**: "Review: Uncommitted changes", "Review: ‹branch› vs ‹base›", "Review: ‹sha7› ‹subject (20
  chars)›" ("Review: #‹N› ‹subject (20 chars)›" for a squash merge's "subject (#N)"; the tooltip keeps
  the sha and the whole subject), "Review: ‹n› commits ‹oldest7›–‹newest7›" ("‹n›+ commits up to
  ‹newest7›" when truncated; before the commits load, the head's short sha). Snapshot tabs add
  "Read-only snapshot (no language server)" to the tooltip. The panel shows the same title and
  follows changes. Only the tab cuts the subject at 20 characters (and a pull request's title at
  64): the overview's header and the panel's header show the whole title and truncate it to their
  width (user decision, bugs round 2).
- **Reuse**: same target (`ReviewTarget::is_same_review`) and same repository → activate the open tab;
  anything else opens another tab. Persistence keys for M5 (§2.6): branch = repo + current branch name
  + base; commits = repo + `base..head`.
- File header statuses come from `ReviewAddon::override_status_for_buffer_id` (status against the merge
  base / range base, like `BranchDiffAddon`/`CommitDiffAddon`). M4's header controls must go into the
  same addon (only the first addon renders them).
- Everything git-side runs on the repository's job queue with the Local backend (`send_job`); remote
  repositories get "Code review only supports local repositories".
- Tests: `askpass` (fake historic commits through `Repository::commit`) and `remote_connection` with
  `test-support` (feature unification: `project`'s test support enables `remote`'s extra variant,
  which `remote_connection`, pulled in by `git_ui`, only matches with its own) are dev-dependencies.
  The fake `diff_tree` ignores refs and lists files by comparing merge-base blobs with HEAD's
  contents, so tests can't check which base/head reach git — the live check covers that.

**Implementation decisions (milestone 4)** — the M4 plan's D1–D11 unless noted:

- **Persistence** (D1): own domain `CodeReviewDb` (`db::static_connection!(CodeReviewDb, [])`, no
  `WorkspaceDb` dependency: state is keyed by review, not workspace). Schema in §3.8. Every query goes
  through `ThreadSafeConnection::write`'s queue at call time (its future borrows the connection, so
  results come back through a oneshot), so toggles land in order and loads see earlier writes. M5
  part 2 appended migration 1 (`code_review_comments`, §3.8) and prunes reviews unopened for 90 days;
  migrations are append-only (migration 0 has shipped). Tests set `db::AppDatabase::test_new()` in
  `init_test`.
- **`ReviewKey`** (D4, `ReviewTarget::review_key(&RepositorySnapshot)`): `uncommitted:{work_dir}`,
  `branch:{base}:{branch}:{common_dir}` (`branch:{base}:HEAD:{work_dir}` when detached: any number of
  worktrees can be detached; earlier detached keys are orphaned), `commits:{base}..{head}:{common_dir}`
  (empty base for a root commit); M7 adds `pr:{host}/{owner}/{name}#{n}`. Change Base and a branch
  switch change a branch review's key: the tab switches to that key's session (marks stay with their
  key; switching back restores them).
- **Sessions** (D5): `ReviewSession::for_key` shares one entity per key through the `ReviewSessions`
  global (weak entries), so split tabs share marks; filters, the hidden set and single-file mode are per
  tab and not persisted. Marks can't change until the stored ones have loaded; a failed load keeps
  them unchangeable and shows a toast, as does a failed write (`ReviewSessionEvent::Failed`).
- **Hashes/invalidation** (D3, made non-destructive in the hardening pass): FNV-1a 64; hunk key
  `hunk-content-v1:{hash}` (span-qualified `hunk-span-v1:…:{old_start}:{old_count}:{new_start}:{new_count}`
  when a file repeats a hunk); file hash over the hunk hashes. Digests are taken in `sync_excerpts` only
  while the buffer has no unsaved edits, and survive refreshes (a re-registered buffer keeps `shown`,
  `has_hunks` and its digest). **Rows are deleted only when the user unmarks.** A file counts as
  reviewed while the hash it was marked with equals the last digest the session observed
  (`observe_digest`; none observed yet → trusted), a hunk while its key is in the file's digest. So a
  change that is undone, a `git stash`/`pop`, or a branch switch and back (marks of the other key are
  never touched) brings the marks back. Digests of files without hunks aren't observed: a file whose
  changes vanish keeps its state. Marking needs the file's digest; without one (unsaved edits since
  the review opened) a toast says to save it. Stale hunk keys accumulate within a review (whole reviews
  are forgotten after 90 days unopened, M5 part 2). Legacy
  rows without a hash still adopt the next digest.
- **Folding** (D7): fold when a file becomes reviewed or its excerpts reappear; unfold when unreviewed
  or invalidated; single-file mode shows its file unfolded and leaving re-folds. A session load or
  switch syncs every shown file (also unfolding unreviewed ones). Folding the RHS folds the LHS.
- **`r`** (D6): marks and moves to the next file (no wrap; the last file keeps the cursor on its folded
  header), or unmarks and lands on the file's first row; with hide-reviewed on, the file disappears and
  the cursor moves to the next shown file, wrapping. Panel `r` and the header checkbox toggle without
  moving. **`R`** toggles the hunk at the cursor (both views; split RHS rows of a pure deletion can't,
  use the LHS); a context row or a folded file shows a toast.
- **Dimmed hunks** (D2): row highlights (`include_gutter: false`) in faint versions of the diff colors
  (diff background at 40% opacity over the editor background), painted per row kind so deleted/added
  stay distinguishable; the gutter bars keep the kind. Repainted on mark/digest/excerpt/fold changes
  and LHS re-creation, always followed by `LineCursor::refresh` (newest highlight wins). The hunk
  control (`ReviewHunkRenderer`, replaces `HiddenUnstagedDiffHunkRenderer`) reads "Mark Reviewed" or
  "✓ Reviewed" with the `R` binding. It covers the end of long rows at the cursor, like Zed's own
  hunk controls in the same place: kept as is (fix 2, orchestrator decision).
- **`]c`/`[c`** (D8) → `code_review::NextHunk`/`PreviousHunk`: Zed's wrap-around, centered `GoToHunk`
  minus hunks of folded buffers (checked on the focused side); `[c` inside a hunk goes to its start.
- **Folded headers vs vim**: a cursor on a folded header resolves (`newest()`, "wrapping blocks") to
  the whole folded file, so vim entered Visual whenever the diff regained focus there (`r` then did
  nothing). `ReviewView` dispatches `vim::SwitchToNormalMode` on `EditorEvent::Focused` when the
  cursor is a single anchor but resolves non-empty. `has_visual_selection` compares anchors (+ line
  mode). Vim's status still shows "N lines, M characters" there (cosmetic).
- **Filters** (D11): include then exclude, case-insensitive regexes over the repo path; prompts
  prefilled, empty clears, invalid keeps the prompt with the error. Hidden files lose their excerpts
  (`ReviewDiff::set_hidden`; computed from the source's file list before buffers load, so filtered files
  don't flash) and leave the panel, `]f`, `]u` and `]c`. "Clear Filters" clears include, exclude and
  hide-reviewed (not the generated/vendored defaults). Diff placeholder when everything is hidden: "All
  files reviewed" / "All N changed files are hidden" + a hint.
- **`.gitattributes`** (D9): tuicr's rules; read through the project `Fs` on every file-list refresh.
  Deviation: always from the working tree (also for historic snapshots). Generated/vendored files may
  show until the attributes load (milliseconds). Toggles `ToggleShowGenerated`/`ToggleShowVendored`.
- **Panel** (D10): tree order = review order (directories first, single-child chains compacted);
  directories default open, collapse state per tab; status letters `U`/`A`/`D`/`M` (deleted struck
  through); ✓ column; "(generated)"/"(vendored)" suffix when shown. Header: title, "reviewed/total"
  (files passing patterns and attribute defaults, ignoring hide-reviewed), a status line ("include … ·
  N filtered out · N generated hidden · N vendored hidden · N reviewed hidden · single file") and a
  ⋯ menu. Keys: `j k h l Enter Space r i e I E / n N gg G` (+ arrows, Escape = focus the diff); `j`/`k`
  only move the selection except in single-file mode; `l` on a file shows it and keeps focus, Enter
  focuses the diff; a click selects and shows; the selection follows the diff cursor while the panel
  isn't focused (a file in a collapsed directory selects the directory). Path search: case-insensitive
  substring, next match after the selection, expands parents, `n`/`N` wrap. `space` is also bound in
  the vim keymap: vim's `space f`/`space w …` (context `!Editor && !Terminal`, loaded later) would keep
  it pending. Filter/search/toggle/copy actions are workspace actions (palette works anywhere; they
  act on the active review tab, else the panel's).
- **Context menu** (right-click a row or the panel, or ⋯): Mark as (Not) Reviewed, Open File, Copy
  Relative Path (file rows), Hide Reviewed / Show Generated / Show Vendored toggles, Include/Exclude
  Filter…, Search Paths…, Clear Filters, Copy Branch Name (`CopyBranchName`; disabled/toast when HEAD
  is detached). M7 adds "Copy Pull Request URL" next to it.
- **Single-file indicator**: a bar under the top block ("Single file · i of N  path   Show All Files
  ;f") plus "single file" in the panel's status line. (UX-1 slice C moved it into the review
  toolbar.)
- Tests: `test_support::path_at_cursor` returns repo paths; `init_vim_test` also binds
  `code_review.json`; `open_tree_fixture` (nested dirs, `.gitattributes`, two-hunk file). 46 tests,
  stable under `ITERATIONS=10`.
- **M4 live check** (harness, scratch repo with nested dirs, `.gitattributes`, multi-hunk files, a
  feature branch with commits): panel tree/letters/progress/status, `r` fold+advance, `R` dimming in
  split and unified, `]c`/`[c`/`]u` skipping folded files, panel keys, include/exclude/invalid regex,
  ⋯ menu toggles, Copy Branch Name / Copy Relative Path (clipboard), single-file bar, split toggles
  keeping folds and dimming, marks restored after a restart (uncommitted, branch, commit snapshot),
  a reviewed file's mark clearing after a shell edit while an unchanged hunk keeps its mark. Found and
  fixed there: the folded-header Visual-mode bug above.

**Hardening (M1–M4 review findings, `reviews/M1-M3.md` and `reviews/M4.md` in the orchestrator's
state dir):**

- **Read-only guard**: the editor registers `git::ToggleStaged`, `git::StageAndNext`,
  `git::UnstageAndNext` (staging first saves the dirty buffers under the cursor) and
  `editor::ReloadFile` (drops the unsaved edits of every buffer in the review) without checking
  `read_only`; they are captured on the `CodeReview` div (`block_write_action`, which also clears
  vim's pending operator), covering both split sides, keymaps (`d u`, `d shift-o`, `d shift-u`,
  `alt-y`…), the palette and `:e`/`:e!`. Audit of the rest: restore/apply hunk (`git::Restore`,
  `RestoreFile`, `RestoreAndNext`, `Apply*DiffHunks`), format, organize imports, rename, code actions,
  completions and inlay-hint edits are only registered on editable editors; plain edits are refused by
  the read-only multibuffer; `ToggleReadOnlyFile` only acts on singleton buffers; `workspace::Save`
  isn't registered for a read-only item and `SaveAs` does nothing (`can_save_as` false); workspace-level
  git actions (stage all, stash…) are repo-wide and stay allowed. Breakpoints/bookmarks aren't file
  writes and stay allowed.
- **Branch bases** (one choke point): `ReviewTarget::Branch` holds a `BranchBase`, built only by
  `BranchBase::new` (non-empty, no leading `-`, no whitespace/control characters), so nothing git
  could parse as an option reaches `git diff --merge-base` (no `--end-of-options` there: it would need
  an unapproved core patch). `check_branch_base` resolves `base^{commit}` (unknown, ambiguous and
  non-commit bases are errors) and runs `diff-tree --merge-base` against HEAD ("no merge base" → "‹base›
  and HEAD have no common history", "multiple merge bases" likewise). It runs before a typed base opens
  (toast) and in every branch review (`ReviewDiff::check_base`: on open, Change Base and
  `HeadChanged`): a failure replaces the empty label (spinner until the first result), and a tree diff
  that failed although the check passed says "Couldn't compare HEAD with ‹base› (see the log)".
- **Commit ranges**: revisions resolve in one `cat-file --batch-check` (`rev` and `rev^{commit}`, so
  errors say unknown / ambiguous / not a commit). **`A..B` needs A on B's first-parent history**
  (walked with the commit reader, up to 10,000 commits) and A ≠ B, else an error toast. Decision
  (orchestrator: reject or merge-base): reject, because the diff from a diverged A would also revert
  A's own changes (that is `git diff A..B`, which tuicr shows), and title, top block and key all
  assume a run of consecutive commits; a merge-base base needs a `merge_base` backend call that only
  exists inside `diff_tree` (core patch).
- **Errors in the UI**: failed per-file loads (one toast per refresh), `e` from a snapshot and the
  panel's Open File (`detach_and_notify_err`), the picker's default-branch lookup, mark load/save
  failures. A built-in keymap with a bad binding keeps the others and logs the error
  (`test_a_bad_binding_keeps_the_others`; `test_keymaps_load_without_errors` fails on any).
- **Panel**: selection keys reveal the file in single-file mode (the window is passed through instead
  of a nested window update that failed silently).
- **Live harness**: the short link is `$XDG_RUNTIME_DIR/zh` (else `/tmp/zh-$UID`); a stale link is
  only followed when owned by the user, and the other harness's PID is parsed, not sourced. The
  harness mirrors the user's `autosave` (500 ms), so unsaved-edit checks there need `"autosave":
  "off"` in `$(zh path config)/settings.json`.
- **Live check** (scratch repo + an orphan branch): in an uncommitted review `d u`, `d shift-o`,
  `d shift-u`, `alt-y`, `ctrl-alt-y`, `:e!` and `:e` left `git status`, the index and the files
  untouched, and an unsaved edit in another tab survived `:e!`; "Branch vs" with the orphan branch as
  base shows "lonely and HEAD have no common history", Change Base back to `main` recovers.
- Not done: the per-file linear scans in `ReviewDiff` (`entry`, `repo_path_for`) (M4 L4; refreshes no
  longer emit per-file events for unchanged files, which was the quadratic part), `.gitattributes`
  macros / global attributes file (M4 L5), pruning the panel's `collapsed` set (M4 L6), and a sweep of
  doc comments that restate their code (M1–M3 L11).

**Milestone 5, part 1 (comments core)** — `plans/M5-comments.md` §2–§5 unless noted:

- **Store**: comments belong to the review's `ReviewSession` (`CommentStore` + `ReviewSessionEvent::Comment`),
  so split tabs share them and a key switch (Change Base, branch switch) shows that key's comments,
  like marks (switching back restores them; open editors save into the session they belong to first).
  Model (`comments.rs`): `CommentId` (per-review sequence), `Side::{Old, New}`, `CommentType`
  (`None` = untyped), `CommentTarget::{Review, File { path }, Lines { path, side, lines }}` (1-based,
  numbers when made), `CommentState::{Draft, Outdated, Submitted { review_id }}` (the last two are
  unused yet), `body` (trimmed), `line_text` (the lines' text when made, for part 2's outdated check),
  anchors (runtime buffer anchors: `anchor_after` the first line's start, `anchor_before` the last
  line's end, so text typed before the first line or after the last stays outside; per tab since
  "Hardening 2", no longer on the comment). Stored in the DB since part 2.
- **Hosting** (`CommentBlocks`, one per tab; `CommentView` per comment): right side only, one
  `BlockStyle::Sticky` block per comment (not moved by `h`/`l`), priority = id, opaque card and
  wrapper (covers the empty-row bleed). File comments sit `Above` the file's first row (below its
  header). A line comment's block is `Below` its last row, except on an unchanged row with a next row in
  the same excerpt, where it is `Above` that row: upstream `balancing_block` mirrors a `Below` block on
  an unchanged row as a spacer *above* the facing row, which misaligned every row after it in split
  view. Blocks are reconciled on `BufferRangesUpdated`/`BuffersRemoved`/`BufferFoldToggled` and comment
  events: comments in files hidden by single-file mode or filters, in folded (reviewed) files, or on
  lines outside every excerpt have no block. The right side survives split toggles, so open editors do.
- **`c`** (normal: the line; visual: the line-wise range, block below its last line): on the right
  side only new-file rows count (a unified selection over removed and added rows comments on the added
  ones); removed rows alone or the left side are `Side::Old` → toast "Comments on the old side of the
  diff aren't supported yet" (part 2). On a folded file header `c` adds a file comment; `c`/`C` unfold
  the file first. The selection collapses first (leaves visual mode).
- **Editors**: vim auto-height editors (`set_use_modal_editing(true)` inside `cx.new`) created with
  their text (undo can't empty them), editor-width wrap, placeholder "Leave a comment…". They enter
  insert mode on their first `Focused` event (the vim action needs the painted node), at the end for a
  new comment and at the start for `i`/`Enter` (tuicr's `i`). Written like code (bugs 2 item 6): the
  buffer font and size (`comments::view::comment_editor_style`, rendered through `EditorElement`, as
  auto-height editors default to the UI font), a Markdown buffer with the `LanguageRegistry` so fenced
  code blocks get their language's highlighting through the grammar's injections (the language loads
  in the background; the editor works before it does). Saved comments still render as markdown in
  the UI font (`hover_markdown_style`, as the top block).
- **Leave/keep** (on the card's `ReviewComment` context): `SaveComment` (`ctrl-enter`, `ctrl-s` in both
  keymaps; vim insert mode's `ctrl-s` is signature help otherwise), `editor::Cancel` (vim normal-mode
  `Esc`, which the child passes on), `workspace::Save`, `workspace::CloseActiveItem` (`:q`, `:wq`, `:x`,
  `ZZ`, `ctrl-w q`, the user's `ctrl-q`: keep; `save_intent: Skip` = `:q!`/`ZQ`: discard) and
  `DiscardCommentChanges` (palette). **`:q` keeps the text** (like `Esc`, not vim's E37). `:w` is vim's
  private `VimSave` → `save_active_item`: `ReviewView::capability()` is `ReadWrite` and `can_save` true
  while any comment editor is open, and `save()` leaves the focused one; `is_dirty` stays false, so
  autosave and close prompts never touch comments (live: `:q!` discards with the mirrored 500 ms
  autosave). While editing, the tab tooltip loses "Read-Only Tab" and the tab menu offers a no-op
  "Make Tab Read-Only". **A new comment left empty, or discarded, is deleted**; an existing one left
  empty keeps its text (`dd` deletes). Closing the tab saves open editors into the session.
- **Focus return**: to the diff editor the comment was opened from (else the focused side), with the
  selection collapsed to the cursor row (vim normal mode); a one-shot `on_focus` guard re-focuses it
  after the editor's own `handle_focus` re-focused a stale `last_focused_descendant` (palette leaves).
- **Types**: `tab`/`shift-tab` in the editor's vim normal mode (insert-mode `tab` is text: Zed's
  indent, soft-tab spaces per the user's editor settings rather than a tab character, as Zed does;
  V3.11, accepted by the orchestrator after the final validation), untyped →
  Issue → Suggestion → Note → Praise → untyped; changed at once (`:q!` keeps the type). Badge + left
  accent: Issue error, Suggestion info, Note warning, Praise success colors.
- **Comment at the cursor** (`i`, `Enter`, `dd`; its card gets the focused border): the one `]t`/`[t`
  moved to while it is on the cursor row, else the first line comment whose last line is the cursor
  row, else a file comment on the file's first row. Left-side cursor rows map to the facing right row.
  No comment: `i` does nothing, `Enter` moves down, `dd` shows a toast.
- **`dd`** is bound as the second `d` (`vim_operator == d`): a `d d` binding keeps every `d` pending and
  replays it without vim's operator context, which broke the blocked `d u`/`d o` staging keys. Asks
  "Delete this comment?" (first line as detail).
- **`]t`/`[t`, `m`/`M`** (`m`/`M` give up vim's marks and `WindowMiddle` in the review tab, per §2.5):
  line and file comments in diff order (file, file comments first, then last line, then id; since
  fix 1 old lines facing the same new line go by their old line before the id), wrapping;
  review-level comments, filtered files and lines outside every excerpt are skipped; single-file mode
  switches files; folded files unfold (fix 1: their line comments and threads count too). From a
  plain row, a comment below that row is the next one.
- **Review-level comments** (`;c`): a "Review comments" section in `ReviewTopBlock` after the branch /
  commit part (scrolls past 16 lines). Every non-editing card has edit/delete buttons (the only
  keyboard-free way to reach review-level comments until the panel list).
- **`ToggleSoftWrap`**: the `CodeReview` capture only acts while a diff editor itself is focused; in a
  comment editor Zed's own toggle runs (it drops the override first, so it may look unchanged).
- Tests: 13 new (74 total, stable under `ITERATIONS=10`); `command_palette` dev-dependency for `:`
  keys in tests.
- **Live check** (harness, mirrored config with autosave): split and unified, uncommitted, branch and
  commit-snapshot reviews: every leave key, `:q!` with autosave, type cycling, markdown, `dd` confirm /
  cancel, `]t`/`[t`/`m`/`M` (wrap, single-file switch), `C` (also on a folded header), `;c`, edit/delete
  buttons, typing review keys into a comment, split toggles while editing, single-file/folding hiding
  blocks. Found and fixed there: split rows misaligned below comments on unchanged rows, a stale `]t`
  selection targeting a comment in another file, file comments scrolled under the sticky header.

**Milestone 5, part 2 (old side, persistence, panel list) and the M5 part 1 review fixes** —
`plans/M5-comments.md` §7 unless noted:

- **Old side**: `c` on removed rows of a unified diff, and on any row of the left side of a split diff
  (context rows too: left = old numbering; M7 maps them at submit), makes `Side::Old` comments
  anchored in the file diff's base-text buffer (`BufferDiff::base_text_buffer`, which the left
  multibuffer excerpts; a new base is applied to it as edits, so anchors follow). A unified selection
  over removed and added rows still comments on the added ones (since bugs 2's follow-up: a range
  from the old line to the new one, see §0). Card labels "Old line N" / "Old lines
  a–b" (a single new line has none).
- **Hosting**: `CommentBlocks::hosted` records each block's side (`Host::{Rhs, Lhs}`) and editor.
  Old-side comments sit on the left side when split; unified, below their removed row (the
  multibuffer anchor of the hunk start in the file's buffer `.with_diff_base_anchor(..)`, as the
  multibuffer anchors removed rows) or below the unchanged row they face. File and new-side comments
  stay on the right. An observer of the `SplittableEditor` reconciles when the left editor is recreated
  or dropped (`split()`, unsplit, `minimum_split_diff_width`); the dropped side's blocks go with it, the
  `CommentView`s and open editors stay, and an open editor keeps the focus (live: insert mode survived
  three toggles and a window shrink/grow). Leaving a comment whose opening side is gone focuses the side
  showing it, on its row.
- **Cursor and navigation**: "comment at the cursor" compares the cursor's row on each side (the other
  side's row facing it); comments on the focused side win. `]t` orders by the new file's row (old rows at
  the new row they face, `base_text_point_to_buffer_point`), old before new at the same place; a
  left-side row facing a spacer positions by its own base row.
- **Positions**: on every reconcile and buffer edit, line comments' `lines` and `line_text` follow the
  tab's anchors (in memory; an outdated comment keeps the text it was about), and so does the card's
  label. A range whose start passed its end (typing on an empty commented line) collapses onto its end,
  where the block is; one whose lines were deleted doesn't (see "Hardening 2"). Saved with the
  comment's next change, when its tabs close (`on_release`) and on quit (`on_app_quit`, awaiting a DB
  barrier; live: an editor open at quit came back).
- **Persistence**: migration 1 `code_review_comments(review_key FK → sessions ON DELETE/UPDATE CASCADE,
  comment_id, data TEXT, PRIMARY KEY(review_key, comment_id)) STRICT`; `data` is JSON `{target: {kind:
  review|file|lines, path, side, start, end}, comment_type, body, line_text, state: draft|outdated|
  {submitted: {review_id}}}` with serde defaults, so fields can be added without a migration. Loaded with
  the marks (`open_review`); nothing can be added until then (toast). A comment is stored once it has
  text: a new one starts empty, and a type set before that stays in memory until the text comes. Ids
  continue after the largest stored id, also of rows that can't be read (a newer build wrote them):
  those are skipped (logged) and never overwritten. A deleted id may be reused after a restart (rows
  are per id).
- **Outdated**: a stored line comment's lines are compared with its file once per run, by the first tab
  that has the file loaded (its buffer; old side: the diff base, which must exist): the rows of `lines` must
  exist and their text equal `line_text`, else the comment becomes `Outdated` (stored; until its
  lines have that text again, see "Polish B", or `dd`) on the clamped rows: an OUTDATED badge with
  a tooltip, "outdated" in the panel, ` (outdated)` in the export, still navigable. Other tabs
  anchor it on its lines without comparing. A comment whose lines
  were deleted while the review is open, or whose buffer was replaced (the file left the review and
  came back, a snapshot or diff base rebuilt), is anchored on its line numbers again and compared the
  same way. Anchors are never resolved against another buffer. Comments on files not in the review stay
  unanchored (panel and export only).
- **Pruning**: once per run, after the first review has loaded (so never that one), reviews not opened
  for **90 days** are deleted with their marks and comments (`ON DELETE CASCADE`). Every write and every
  closed tab or quit (`flush`) counts as opening the review (`ensure_session` upserts
  `last_opened_at`). Hunk keys inside a live review aren't pruned (that would need a per-key timestamp).
- **Panel list**: below the tree in the same `uniform_list`: a "Comments (N)" heading (skipped by the
  selection), then per comment the type, `path:line` (`~` = old side) or "Review", the body's first line
  and the outdated/submitted state. Enter/click: a line or file comment lands like `]t` (activating the
  tab and focusing the diff; toast when its lines aren't in the diff), a review comment opens for
  editing. Order: review comments, then files in review order, file comments first, then by last line.
  Comments without text (being written) aren't listed.
- **M5 part 1 review fixes** (`reviews/M5a.md`): H1 a review-key switch (Change Base, checkout, detached
  HEAD) saves open editors and positions into the old session, and the tab keeps its previous sessions
  (`ReviewView::previous_sessions`), so switching back finds them as they were (and the DB has them after
  a restart); M1 above; M2 `dd`'s prompt deletes from the session it was asked in; L2 an edited comment
  whose block goes away (fold, filter, single-file switch, reverted hunk) is left keeping its text; L3
  another tab doesn't show a comment until it has text (`CommentBlocks::is_listed`), and reconciles on
  every comment change; L4 `:bd`/`:bd!` (`CloseItemInAllPanes`) leave like `:q`/`:q!`; L6, L7 above;
  nits: "No comments in the diff", `i` on an open editor updates where focus returns, one snapshot per
  side per reconcile, `c` on the left side's folded header comments on the file.
- **Enter** in a comment editor (insert mode, and without vim) types a new line (`editor::Newline` in
  `ReviewComment > Editor`, `code_review.json`): auto-height editors don't bind it.
- Clippy (`./script/clippy -p code_review`) is clean; it flagged two older spots (`gitattributes.rs`
  `filter_map` → `map`, `line_cursor.rs` `last` → `next_back`), fixed.
- Tests: 15 new (89 total).
- **Live check** (harness; scratch repo with removed, changed and added lines and a deleted file;
  screenshots in the orchestrator's `validation/M5b/`): old-side comments in split and unified, three
  split toggles and a window shrink/grow with an editor open in insert mode (text and focus kept), a
  restart without `--fresh` (all comments and types back), a quit with an open editor (back), a
  commented line edited from the shell (OUTDATED badge, panel, export marker), panel jumps (file, old
  line, review comment), `y` byte-identical with the expected text, `Y`, visual `y` on both sides.

**Milestone 6 (export)**:

- `export.rs` `review_markdown(&ExportPreamble, &[Comment]) -> Option<String>` (`None` without
  comments) reproduces tuicr's `generate_markdown` with its default `[export]` settings **minus the
  session header** (`## Session: <slug>` serves tuicr's `review add --session` protocol, dropped in §2.9):
  intro line, banner, "Comment types: …" (the used types in the order Issue, Suggestion, Note, Praise,
  defined as "problems to fix", "improvements", "observations", "positive feedback"; omitted when nothing
  is typed), the heading `## Local tuicr Comments` (**kept literally**: agents prompted with tuicr's
  format look for it), each followed by a blank line, then ``N. **[TYPE]** `location` - first line`` with
  continuation lines indented by `len("N.") + 1`, `\r` trimmed, every line `\n`-terminated. Locations
  `path:L`, `path:S-E`, `path:~L`, `path:~S-~E` (old side), `path`, and `Review Comment (scope: …)`.
  Order as tuicr's: review comments, files by path string, file comments, then line comments by last
  line (sides interleaved), creation order. Submitted (M7) and empty comments are left out; outdated ones
  get ` (outdated)` after the location (where tuicr writes ` (commit sha)`). Comment editors open in the
  exporting tab count with their current text (as leaving them would save it); other tabs' don't.
- Banners/scopes: uncommitted "Reviewing staged + unstaged changes" / "staged + unstaged changes"
  (tuicr's staged + unstaged source); commits "Reviewing commit: abc1234" or "Reviewing commits: a, b"
  (oldest first, from the top block's commits; "Reviewing commits: base..head" while they load or past
  100) / "selected commit range"; branch, which tuicr doesn't have: "Reviewing branch ‹branch› against
  ‹base› (its commits since the merge base + staged + unstaged changes)" / "branch ‹branch› vs ‹base›".
- Keys: `y` = `CopyReviewAsMarkdown` (a workspace action: the active review, else the panel's, so the
  palette works anywhere), toast "Review copied to clipboard" / "No comments to copy". `Y` =
  `CopyCommentAtCursor` (the raw body). Visual `y`/`Y` = `CopySelectedLines`: the lines of the side `c`
  would comment on (left side = old; a unified selection over removed and added rows = the new and
  unchanged rows), raw and joined by `\n` without a trailing newline (decision, as tuicr's visual copy),
  leaving visual mode; toast "Copied N lines". They replace vim's yank in the review tab (§2.5 table).
- `ReviewView::review_markdown(&self, cx) -> Option<String>` (pub) is the text M8's `--wait` finish
  prints.

**Milestone 7, slice (a) (GitHub client)** — `plans/M7-github-v2.md` §2 with the live verification
(`plans/M7-live-verification.md`, "LV") unless noted. No upstream file changed (new deps
`http_client`; dev-deps `http_client/test-support`, `git_hosting_providers`):

- **`forge.rs`/`github.rs`** as in the plan; `Forge` methods return `BoxFuture<'static,
  anyhow::Result<_>>` whose errors are a `GitHubError` (`forge::error_message` finds it in the chain
  and returns its hint, else `{error:#}`; `Display` of `GitHubError` is the hint, so
  `detach_and_notify_err` shows it too). `forge::show_error_toast(workspace, &error, cx)` is the toast
  helper (SSO errors get an "Authorize" button). No `submit_review` yet (slice d).
- **Resolution** (`resolve_forge(repo, reference, project, cx)`): GitHub remotes from
  `Repository::remote_urls()`, preferred: the branch's upstream remote, `origin`, `upstream`, then by
  name. Which remotes are GitHub's, and which hosts get a token: see "Hardening 3" (H1/L5; the
  original rule, any host of a registry provider named "GitHub"/"GitHub Self-Hosted", sent tokens to
  look-alike hosts). `RepositoryContext` query → `canonical` = `parent` when `isFork` (**user
  decision: a fork's checkout lists the upstream's PRs**). A `reference` that names a remote's
  repository (case-insensitive) uses that repository as is (no parent); otherwise it must equal the
  canonical repository, else `GitHubError::OtherRepository` ("Open this pull request from a checkout of
  o/r."). No GitHub remote → `GitHubError::Unsupported`. `canonical_https_url` is GitHub's `url`
  (no `.git`), `canonical_ssh_url` its `sshUrl`.
- **Caches**: forges per (work dir, every remote's URL, host rules, branch remote, reference) in the
  `GitHubForges` global; tokens per (host, work dir) in `GitHubLookups` (`Shared` tasks, so parallel
  callers share one lookup; "Hardening 3" L4/L9). A failed lookup or resolution is retried on the
  next call. A 401 marks the token revoked (`GitHubToken` clones share the flag): the next
  `resolve_forge` resolves the forge and looks the token up again. **Holders of an `Arc<dyn Forge>` (slice b/c's `PullRequestState`) must call
  `resolve_forge` again after a `GitHubError::Auth`** to pick up a new token.
- **Token** (never logged; `GitHubToken`'s `Debug` redacts it; requests are logged at debug level
  with method, URL, operation and status only): gh's rules since "Hardening 3" (H1). `gh auth token
  --hostname <host>` runs with the project's environment minus the token variables (`kill_on_drop`,
  10 s executor timer; stdout trimmed). `GitHubError::Auth { host, failure: Rejected(origin) |
  NoToken | NoTokenNoCli }`: the last one (gh missing) says "or install the GitHub CLI"; a rejected
  token names its source. Tests install fakes (`github::set_fake_token_source` for gh logins,
  `set_fake_lookups` for environment variables and SSH aliases too), so they never read the real
  environment or run `gh`/`ssh`.
- **Transport**: endpoints `api.github.com` / `api.{H}` (`*.ghe.com`) / `{H}/api/v3` +
  `{H}/api/graphql` (GHES; `H` keeps an HTTPS remote's port); headers `Authorization: Bearer`, `Accept: application/vnd.github+json`,
  `X-GitHub-Api-Version: 2022-11-28` (also on GraphQL, accepted live), `Content-Type` on bodies, 30 s
  timeout; GraphQL body `{query: <whole document>, operationName, variables}`. GraphQL connections
  are paged at `hasNextPage` (cap 20 pages); `pulls/N/files?per_page=100&page=k` until a short page
  (cap 30 pages → `truncated`).
- **Queries** (C12, C14): `OpenPullRequests` = `repository.pullRequests(states: OPEN, first: 50,
  UPDATED_AT DESC)`; review-requested = `search` "repo:o/r is:pr is:open review-requested:@me
  sort:updated-desc" (`{}` and null nodes skipped); `PullRequestDetails` and `PullRequestHead` select
  the viewer's pending review through an aliased `pendingReviews: reviews(states: [PENDING], first:
  1)` (filtered on `viewerDidAuthor`); thread comments select `state` and `commit { oid }`. Commit ids
  from GitHub must be full 40/64-hex ids (they become git arguments in slice b).
- **Thread parsing** (C6/C7/C12, pure `parse_thread`): FILE first; `line == null` → `Outdated
  { original_line, original_start }`; single-line iff `startDiffSide == null`; a range starts at
  `startLine` (RIGHT) or `originalStartLine` (LEFT); `(startSide, start) == (side, line)` → single
  line. `anchor_commit` = the first comment's `commit.oid` (placement compares it with the local HEAD in
  slice c). Threads whose first comment is PENDING are dropped, and so are pending replies
  (`total_comments` excludes them). `Actor.is_bot` = `__typename == "Bot"` or a `[bot]` suffix; a
  deleted author is "ghost". `RemoteThread::is_by_bot` (its first comment's author) is what the bot
  toggle hides (**user decision: bot threads hidden by default, like bot summaries**; the filtering
  itself is slice c). Summaries drop empty/whitespace bodies and PENDING reviews.
- **Commentable lines** (LV §5.2): GitHub's `patch` hunks (`Hunk { old, new, rows }`; `rows` are the
  patch body's runs of context/deleted/added rows). `allows(path, side, line)` = inside a hunk's
  range on that side (context rows on both); no `patch` → no lines (file comments only).
  `allows_range` = both ends allowed and the start not after the end **in the patch's row order**
  (a hunk with several changes interleaves deletions and additions, so "deletions first" isn't
  enough); ranges may span hunks (LV L8), new comments too. Paths: a renamed file is
  found by its old path too; `current_path` gives the new path for submit. `contains_file` is true
  for files of the PR, and for any file when the list is `truncated` (they might be in the PR;
  lines stay refused).
- **Errors** (`GitHubError::hint`, LV §5.6 wording verbatim): 401 → Auth (token revoked); 403/429
  with `x-ratelimit-remaining: 0`, `retry-after` or a "rate limit" message → `RateLimited { reset }`
  ("Try again in N minutes"); 403 with `X-GitHub-SSO` → `Sso { url }`; other 403 → `Forbidden`; 404
  and GraphQL `NOT_FOUND` (HTTP 200 + `errors`) → `NotFound { what: "o/r#N" }`; GraphQL
  `RATE_LIMITED` → `RateLimited`; GraphQL `FORBIDDEN` (+ `X-GitHub-SSO`) → `Forbidden`/`Sso`; other
  GraphQL errors → `GraphQl`; 422 → `Validation { messages, raw_message }` (string or object
  `errors`, `message` fallback, empty strings kept); anything else (5xx, network, unparsable
  responses) → `Transport`. Deviation: the commitOID hint has no sha and the pending-review hint no
  PR link yet; slice (d) knows both and should add them when it maps its errors.
- **Live check (a)** (read-only): every query of `queries.graphql` run with `gh api graphql --input`
  against the sandbox (no `errors`; PR A threads match LV §3 exactly; `drew-council/zed` → `isFork`,
  parent `zed-industries/zed`); the fixtures in `test_data/github/` are those responses (REST files
  trimmed to `filename`, `previous_filename`, `status`, `patch`). Then the ignored test
  `github::tests::live_github_sandbox` ran the real code path (`resolve_forge` → every `Forge`
  method, PR #999 → NotFound) against the sandbox, once with `GITHUB_TOKEN=$(gh auth token)` and once
  without it (the `gh auth token` path): both passed. Its HTTP client is a test shim that runs `curl
  --config -` (the token goes through stdin, not argv) to avoid a `reqwest_client` dev-dependency.
  Run it by hand with `cargo test -p code_review live_github -- --ignored`.
- Tests: 21 new (110 total + 1 ignored live test). Clippy clean.

**Hardening 2 (M5 part 2 + M6 review findings, `reviews/M5b.md` in the orchestrator's state dir):**

- **H1, two projects reviewing one key** (a repository's root and a subfolder in two windows, `zed -n`,
  worktrees reviewing one commit range): anchors are per tab (`CommentBlocks::anchors: TabAnchors`, a
  `LineAnchor` per line comment: the range in this tab's buffer of the side plus the lines and text this
  tab last saw), no longer on the shared `Comment`. A tab reports only its own moves
  (`ReviewSession::set_comment_positions`, one `CommentEvent::Moved(ids)` per batch, which other tabs
  only use to relabel their cards: no reconcile), so tabs whose buffers differ never undo each other.
  The outdated check runs once per run (`ReviewSession::unchecked`, `needs_check`, `check_comment`, which
  only emits when the state changes). Key switches park the tab's anchors per session
  (`parked_anchors`) and restore them on the way back. Last writer wins for the stored `lines` when
  windows disagree (unsaved edits in one): each window places blocks by its own anchors, and the card
  labels and the panel show the stored numbers.
- **M2, deleted lines**: a `LineAnchor` whose range is empty or reversed while the text it last saw
  wasn't empty lost its lines (deleted; or deleted and put back before the tab looked, like a stash
  and a pop: the ends come back the wrong way around). It is anchored on its line numbers again:
  same text there → kept (a multi-line comment stays multi-line), else `Outdated` on those lines,
  immediately (it doesn't move onto the next line). So `cc`/`S` on a commented line, or deleting its
  whole text, also outdates it; a comment on an empty line can't tell and still moves with the next
  line. A `git stash` while the file stays in the review outdates its comments (since polish B the
  pop makes them current again).
- **M1**: `CommentStore::restore` takes the largest stored id, including unreadable rows. **L1/L2**:
  above ("Pruning"). **L3**: a panel selection whose row went away moves to the nearest selectable
  row (the earlier of two), never the "Comments (N)" heading. **L4**: one `Moved` event per update and
  one reconcile per buffer edit (was one event and reconcile per moved comment); loading emits only for
  newly outdated comments.
- **Nits**: `can_resolve` before resolving base-text anchors (`old_row_in_unified_diff`,
  `comment_order`); the export uses open editors' text (above); `Y` on a comment still being written
  (no text) says "No comment at the cursor"; summarizing doc comments removed.
- **Vim writes in comment editors**: `vim::VimSave` is private and `no_register`, so its type id comes
  from an instance: `command_palette_hooks::GlobalCommandPaletteInterceptor::intercept("w")` (vim's
  interceptor, ready immediately for `w`; cached in a global). Each comment editor registers a
  capture-phase listener for it (`Editor::register_action_renderer` + `Window::on_action`) that stops
  propagation and leaves the comment keeping its text. So every `:w` form (`:w`, `:w!`, `:'<,'>w`,
  `:%w file`, `:w file`) leaves the comment; `:%w file` no longer reaches vim's
  `editor.project().unwrap()` (`vim/src/command.rs:424`, a panic in comment editors, which have no
  project). No vim patch. New dependency `command_palette_hooks` (already in `zed`'s graph).
- Tests: 3 new (113 total + 1 ignored): two projects (root + subfolder, two windows) on one fake repo
  (must terminate, no outdated flag, no position fight, both reopen), deleted and restored lines,
  unreadable stored comments; extended: forgotten reviews (the first review opened is kept, a written
  review is kept), panel selection, export with an open editor, `Y` on an empty comment, `:w!`,
  `:'<,'>w`, `:%w x` among the leave keys. Clippy clean.
- **Live check** (harness, scratch repo; screenshots in the orchestrator's `validation/hardening2/`):
  lines 5–6 commented, then deleted from the shell → OUTDATED badge, still "Lines 5–6"; `:%w
  /tmp/hardening2/x` in a comment editor → no crash, comment saved and left; a second window (new
  window + Zed's path prompt on the repository's `src/` subfolder) opens the same uncommitted review:
  both show each other's comments, a shell edit moves them alike, an unsaved edit in one window moves
  them only there, Zed stays idle (no ping-pong).

**Milestone 7, slice (b) (pull request reviews, picker, checkout)** — `plans/M7-github-v2.md` §3
unless noted. No upstream file changed (new deps `askpass` (was a dev-dependency), `git_ui_core`,
`zed_actions`; dev-dependency `tempfile`):

- **Target** (`target.rs`): `ReviewTarget::PullRequest(PullRequestTarget { repo, number, base })`;
  `repo` is the forge's canonical repository, `base` = `BranchBase::new(base_oid)` (GitHub's full oid
  through the hardening choke point). `is_same_review` compares repo (case-insensitive) + number, so
  a tab is reused when the base moved; reopening moves that tab to the new base
  (`ReviewView::update_pull_request_base`). Key `pr:{host}/{owner}/{name}#{n}` (lowercased),
  independent of the checkout, so a `pr-N` worktree's tab shares the session (marks, comments) with
  the main checkout's. Diff = `DiffBase::Merge { base_ref: base oid }` (merge-base(base, HEAD) →
  working tree); `ReviewDiff` checks the base like a branch review's (spinner, "Unknown revision …").
  Empty label "No changes in this pull request". `HeadChanged` re-checks the base and retitles.
- **References** (`parse_pull_request_reference`): `123`, `#123`, `pr:123`, `owner/repo#123`,
  `http(s)://host/owner/repo/pull/123[/…]` (query/fragment ignored, `.git` stripped); digits only
  (`u64::from_str` would take `+5`), > 0, no overflow; owner/name `[A-Za-z0-9._-]`, not `.`/`..`.
- **`validate_branch_name`** (every `head_ref`/`base_ref` before a refspec or `change_branch`): no
  leading `-`, whitespace or control characters (`is_plain_revision`), and git's check-ref-format
  rules (`~^:?*[\`, `..`, `//`, `@{`, `@`, leading/trailing `/`, trailing `.`, components starting
  with `.` or ending in `.lock`). Unicode, `+`, `@` inside are allowed, as git allows them.
- **Checkout plan** (`checkout::plan_checkout`, pure; amended by "Hardening 3": the plan reads a
  `LocalState` from git, reuses a same-named branch only when it tracks the PR's branch, fast-forwards
  a named branch, fetches a missing base): HEAD == PR head → nothing (no fetch). A
  remote pointing at the head repository (`head_remote`, C15) → `FetchRemote{R}` (Zed's fetch) →
  `ChangeBranch{"R/head_ref"}` (Zed's `change_branch`: a local tracking branch, checked out) → plus
  `FetchRefspec{canonical, ["refs/heads/{base}"]}` when R isn't the canonical repository (the user's
  fork: its remote doesn't have the base commit) → `FastForward`. Otherwise `refs/pull/N/head` from
  the remote pointing at the canonical repository, else its URL (SSH when origin is SSH):
  `FetchRefspec{src, ["refs/pull/N/head:refs/heads/pr/N", "refs/heads/{base}"]}` → `ChangeBranch{pr/N}`
  → `FastForward`; already on `pr/N` (git refuses to fetch into the checked-out branch):
  `FetchRefspec{src, ["refs/pull/N/head", "refs/heads/{base}"]}` → `FastForward`. Non-forced, as
  `gh`. Decisions beyond the plan: `head_remote` is skipped for a pull request that isn't open (its
  branch is often deleted; GitHub keeps `refs/pull`) and for a fork's pull request whose head branch
  is named like its base (a fork's `main` into `main`: switching to `origin/main` would make the
  local `main` track the fork). **`FastForward` always targets the head oid** (not `R/head_ref` or
  `FETCH_HEAD`: with two refspecs `FETCH_HEAD` holds both heads and `merge` would try an octopus), so
  HEAD ends at exactly the head the review was loaded with; it is skipped when HEAD already is it
  (a branch ahead of the PR stays where it is: "Already up to date").
- **Executor** (`run_checkout`): Zed's `Repository::fetch` / `change_branch` for those steps; the
  others through our runner `run_git` on the repository's job queue (status bar "git fetch" / "git
  merge --ff-only"): `git -c core.fsmonitor=false --no-pager`, and for an untrusted repository the
  same `-c` overrides as `GitBinary::build_command` (hooks, ssh command, credential helper, ext
  protocol off), the repository's environment, `GIT_TERMINAL_PROMPT=0`, Zed's askpass
  (`AskPassSession`, `GIT_ASKPASS`/`SSH_ASKPASS`/`SSH_ASKPASS_REQUIRE=force`, unless the environment
  sets `GIT_ASKPASS`; the delegate is a copy of the worktree service's private one with
  `AskPassModal`), `--end-of-options` before the source/refs. Hints (since "Hardening 3"): a rejected
  non-fast-forward fetch into `pr/N`, a diverged branch and a refused fast-forward → "‹branch› has
  diverged from the pull request: … Rename the branch (`git branch -m …`), then open the pull request
  again." **Dirty
  tree**: nothing stashes; git carries changes over or refuses, the stderr shows in the error toast
  (Zed's `change_branch` creates the tracking branch before its checkout fails, as in its branch
  picker).
- **Open flow** (`pull_request::open_pull_request(workspace, repo, reference, options)`):
  `resolve_forge(reference.repo)` → `pull_request(n)` → `PullRequestTarget::new` → remotes, branch,
  HEAD → plan → (`options.confirm_checkout` and a non-empty plan: a "Check out pull request #N?"
  prompt, for M8's links) → `run_checkout` → `PullRequestState::set` → `deploy_in_repo`. The
  checkout only moves after the user picked the pull request (picker / action). Errors:
  `forge::show_error_toast` (GitHub hint or the git error chain); a success dismisses an earlier
  error toast (`forge::dismiss_error_toast`).
- **Review in Worktree** (`options.in_new_worktree`, picker `ctrl-enter`; reworked in "Hardening 3":
  our own `git worktree add --detach <head oid>` for the PR's repository only, trust only for the
  repository's own code, an existing `pr-N` moved only after asking): worktree name `pr-N`; an
  existing linked worktree at Zed's path for that name is opened with
  `MultiWorkspace::find_or_create_local_workspace` and the in-place flow runs there. Otherwise
  `plan_worktree`: a head remote → `NewWorktreeBranchTarget::RemoteBranch{R, head_ref}` (Zed's
  service fetches R; plus the base fetch for a fork); else **`refs/pull/N/head` + base fetched into
  `FETCH_HEAD` only, then `ExistingBranch{name: head oid}`** (deviation: no `pr/N` branch for
  worktrees — `git worktree add --detach` takes any revision, and a branch can't be fetched into
  while another worktree has it checked out). `git_ui_core::worktree_service::create_worktree_workspace`
  (opens in the background, detached at the head), then `MultiWorkspace::activate`, then the
  in-place flow in the new workspace's repository (plan empty) opens the tab there. The original
  checkout is untouched. Zed's service shows its own error toast; ours is skipped for those
  (`ShownByWorktreeService`). Note: with the multi-workspace sidebar disabled (AI disabled, as in the
  harness) `activate` replaces the window's workspace; with it enabled both stay in the window and
  the title bar's worktree picker switches between them (verified live).
- **Picker** (`picker.rs`): Pull Requests section after Local, loaded when the picker opens
  (`resolve_forge(None)` → `list_pull_requests`); header "Pull Requests · ‹canonical›" or "Review
  Requests · ‹canonical›"; rows `#N` + title (fuzzy positions) + Draft chip + author + updated time,
  second line `head → base` (+ " · fork" for cross-repository; text, not a chip, so rows keep one
  height). Status rows "Loading pull requests…", the error hint (`forge::error_message`), "No open
  pull requests" / "No pull requests request your review"; no GitHub remote
  (`GitHubError::Unsupported`) → no section; after an error the forge is dropped, so the next load
  resolves it (and the token) again. Candidate ids from `1 << 20` keep the sections apart. A query
  that parses as a reference selects its listed pull request (a URL doesn't fuzzy-match its row, so
  that row is added) or adds "Open pull request ‹ref›" first in the section and selects it. Footer:
  "Review Requests" / "Open Pull Requests" (`code_review::TogglePullRequestFilter`, `ctrl-r` in
  `StartReview > Picker > Editor`, which beats `projects::OpenRecent`), and on a pull request row
  "Review in Worktree" (`ctrl-enter`) and "Review". `StartReviewFocus::PullRequests` selects the first
  pull request once loaded. Placeholder "Review changes, the branch, commits, or a pull request
  (#123, URL)…".
- **`code_review::OpenPullRequest { reference: Option<String> }`** (deviation from the plan's `number:
  Option<u64>`, as the slice brief asked): `None` opens the picker at the pull requests; a reference
  that doesn't parse is a toast; otherwise the in-place flow.
- **Tab and top block**: tab text "PR #N: ‹title›" without the "Review:" prefix (it already says
  what the tab is), at most 64 characters; tooltip "Review: PR #N: ‹title› (o/r)"; panel title the
  same. Top block (placeholder until slice (c)): "o/r#N ‹title› ‹author› wants to merge ‹head› into
  ‹base›", plus a warning "Checked out ‹local7›, pull request head ‹pr7›" when the repository's
  HEAD (`head_commit` of its snapshot) isn't the pull request's head (another branch checked out,
  or the pull request moved on; the diff then shows what HEAD has since the base). Export preamble
  (M6): "Reviewing pull request o/r#N: title", "URL: …", "Head: sha7"; scope "pull request o/r#N".
- Tests: 7 new (120 total + 1 ignored): reference parsing, branch-name validation, `plan_checkout`
  (already at head, origin canonical, fork + upstream, own fork PR, third-party fork via remote /
  SSH URL / HTTPS URL, on `pr/N`, closed PR, fork `main`→`main`, deleted head repo, hostile names),
  `plan_worktree`, real git in temp dirs (a bare remote with `refs/pull/7/head` → fetch into `pr/7`;
  a new push → FETCH_HEAD + fast-forward; a force-push → rejected with the hint, `pr/7` unchanged; a
  diverged branch → refused with the hint), GPUI picker with the fake GitHub (rows and header, toggle
  and back, `#1`/`o/r#77`/URL selection, opening PR #1 at its head → `PullRequest` target, key, title,
  reuse via `OpenPullRequest{"1"}`, `OpenPullRequest{None}` → first PR selected, bad reference,
  reopening a rebased PR moves the tab's base; a 401 → the auth hint, recovered by toggling). The
  fake repository's fetch is `unimplemented!()`, so GPUI tests only open pull requests whose plan is
  empty. Clippy clean.
- **Live check (b)** (harness, `validation/M7b/`; HTTPS clone of the sandbox with the env-token
  credential helper, `ZH_EXTRA_ENV="GITHUB_TOKEN=$(gh auth token) …"`; nothing posted; leak grep
  clean): the picker lists #4, #1, #3, #2 (not the closed #5; bot PRs by github-actions), `ctrl-r` →
  "No pull requests request your review" and back; `1` + Enter on a clone with only `origin/main` →
  fetch, `feature/template-filters` tracking origin at `3f22f1b`, tab "PR #1: …", panel docs/guide.md,
  src/util/strings.rs, src/engine.rs, src/filters.rs (A), src/format.rs (D) + src/render.rs (A) (no
  rename detection, as every review), src/legacy.rs (D), src/lib.rs; gen/schema.rs and vendor/lib.js
  hidden; `branch -f feature/template-filters 5c5cdf4` + reopening by URL → fast-forwarded to
  `3f22f1b`, same tab; PR #2 with a conflicting uncommitted edit → git's "Your local changes … would be
  overwritten" in a toast, HEAD and the edit unchanged, no stash; after reverting, PR #2 in place →
  `fix/truncate-ellipsis` at `632ee13` with GitHub's two files; the PR #1 tab then warns "Checked out
  632ee13, pull request head 3f22f1b"; "Review in Worktree" on PR #1 → `pr-1` worktree detached at
  `3f22f1b` opened as a second workspace with the review, the main checkout still on
  `fix/truncate-ellipsis`, repeating it reuses the worktree. Fork listing: a probe repo whose origin
  is `drew-council/zed` lists "Pull Requests · zed-industries/zed" (not confirmed). Fork pull
  requests (`refs/pull`, read-only): a clone of the public `octocat/Hello-World` → #11572 in place →
  `pr/11572` at its head with GitHub's diff; `pr/11572` reset back one commit (warning shown) and
  reopened → FETCH_HEAD + fast-forward; #11569 "Review in Worktree" → a detached `pr-11569` worktree
  at its head, no branch created. Found and fixed: the tab title cut at 48 characters, the fork chip
  making rows uneven, a stale error toast after a successful retry, "entity released" when the
  worktree's workspace replaced the source one.

**Milestone 7, slice (c) (description block, read-only threads, comment gate)** —
`plans/M7-github-v2.md` §4 with LV §5.5 unless noted. No upstream file changed, no new dependency:

- **Loading** (`PullRequestState`, one per review key, shared by the pull request's tabs, also a
  `pr-N` worktree's): the open flow's `set` stores forge + fresh details and loads threads, summaries
  and GitHub's files (`CommentableLines`) as concurrent background requests; `reload` (palette
  `ReloadPullRequest`, the top block's Reload button, every tab's `HeadChanged`; a new load cancels
  the running one) loads the details too. A part that fails keeps what was loaded before, and its
  hint (deduplicated) shows in red in the top block; the review itself never fails.
  `PullRequestEvent::{Loaded, FiltersChanged}`. **After a `GitHubError::Auth`** the forge is resolved
  again (`resolve_forge` with the canonical repository as reference, which looks the token up again)
  and the load retried once at once; if that fails too, the next load resolves first. Decision: a
  reload doesn't move the tab's base (`details.base_oid` may differ from the target's base until the
  pull request is reopened, whose checkout fetches the base: a reload has no fetch, so the new base
  might not exist locally).
- **Top block** (expanded by default for pull requests, `ToggleHeader`/disclosure collapses it):
  header = disclosure, title, `#N`, Draft/Closed/Merged badge, "‹author› wants to merge ‹head› into
  ‹base›", review decision badge, Open on GitHub; warnings "Checked out ‹local7›, pull request head
  ‹pr7›" and "You have a pending review on GitHub" (C5). Expanded: the URL (a button), then **one
  scroll area of 8 lines** (deviation from the plan's 16: the diff kept too little room in a
  1000-pixel window; 12 until fix 2, where it still took ~40% of such a window) with the markdown description ("No description provided." when empty) and the
  review summaries ("Reviews": author · state · relative time, markdown; bots hidden, "N bot reviews
  hidden"), then "Show: ☐ Resolved threads (n) ☐ Outdated threads (n) ☐ Bots (n)", "Loading from
  GitHub…", Reload, and the load errors. Then M5's review comments, as before.
- **Filters** (`ThreadFilters`, on `PullRequestState`: all tabs of the pull request, not persisted;
  all off by default): resolved, outdated, and bots (**bot threads and bot review summaries behind
  one toggle**, user decision). Workspace actions `ToggleResolvedThreads`, `ToggleOutdatedThreads`,
  `ToggleBotReviews` (the current review, like the file filters; a non-PR review shows "Open a pull
  request review first"), the top block's checkboxes, and toggles in the panel's menu.
- **Placement** (pure `place_thread`, per tab with the tab's repository HEAD): hidden by the filters →
  nothing; `FILE` → at the file (above its first row, like a file comment); `Outdated` (null line),
  or a line thread whose first comment's commit isn't the checked-out HEAD (C7: another branch
  checked out, the pull request moved on, or a comment made on an older commit) → the file's
  **"Outdated / moved threads (N)" block** at the file (one block per file, after the file's other
  threads), each thread labelled "Originally line 223 at 5c5cdf4" / "Line 134 at abc1234"; an unknown
  HEAD or commit counts as the same. Line threads: `Line{side, n}` → lines `n..=n` on that side
  (LEFT = `Side::Old`, the base's numbering, i.e. the diff base buffer); a same-side range →
  `start..=line` ("Lines 20–25"); a LEFT→RIGHT range sits below its end row ("Old line 191 – line
  195"). **Renamed files**: the local diff has no rename detection (old path deleted, new path
  added), so LEFT threads go under the old path and RIGHT/file threads under the new one, whichever
  path the thread has (`CommentableLines::file` finds both).
- **Hosting** (`CommentBlocks`, per tab): `hosted` is keyed by `ItemKey::{Remote(RemoteKey::{Thread(id),
  Moved(path)}), Comment(id)}` and blocks carry an `AnyView`; `set_remote_items` replaces the tab's
  thread items (views kept for unchanged threads). **Thread anchors are per tab**
  (`RemoteBlock.anchor`, never on `PullRequestState`): made from the line numbers the first time
  `update_anchors` sees the file's buffer of that side, they follow edits like comments' and are made
  again when the buffer is replaced; lines past the end of the local file aren't shown. Placement,
  re-hosting on split toggles, folds, filters and single-file mode are the comments' code
  (`placement` takes a `CommentTarget` now). Threads come before comments at the same place (block
  priority: the item order; comments `1 << 20` + id).
- **Thread card** (`RemoteThreadView`, no editing actions): GitHub icon, RESOLVED/OUTDATED badges,
  location (nothing for a single new line), Open on GitHub (`comments[0].url`), then per comment
  author (BOT badge) · relative time and the markdown body (GitHub `suggestion` blocks render as code
  blocks), replies separated by a rule, "…N more comments on GitHub" past the 100 fetched; the focused
  border when it is the item at the cursor.
- **Keys**: `]t`/`[t`/`m`/`M` include threads (and moved blocks), in diff order with comments (a
  thread before a comment at the same place); the panel lists them. `i`/`Enter`/`dd` act on the
  review's own comment at the cursor row (threads never count, also after `]t` landed on one): `i` on
  a thread-only row does nothing, `Enter` moves down, `dd` says "Threads on GitHub can't be changed
  in the review". `Y` copies a thread's first comment.
- **Panel**: one row per thread (a moved block's threads each get a row landing on the block): GitHub
  icon, `path:line` (`~` old side; the path for file threads and moved ones), "‹author›: ‹first
  line›", "resolved"/"outdated" (moved counts as outdated), tooltip "… (on GitHub)". The location
  label keeps up to 60% of the row (comments' rows too: it was squeezed to "s…" by the first line).
  Menu (pull request reviews): Show Resolved Threads / Show Outdated Threads / Show Bot Reviews,
  Reload Pull Request, Open Pull Request in Browser, and Copy Pull Request URL after Copy Branch Name.
- **Copy actions**: `CopyPullRequestUrl` (workspace action, palette + menu). `CopyBranchName` copies a
  pull request's head branch in a pull request review (works detached, e.g. in a `pr-N` worktree).
- **Gate** (made per tab in slice (d), see there): `CommentBlocks::gate` (`CommentGate =
  Arc<dyn Fn(&CommentTarget, &App) -> Result<(), SharedString>>`), set by a pull request tab (holding
  weak handles of the `PullRequestState`, its repository and its `LocalChanges`), checked in
  `CommentBlocks::add_comment` (the only place comments are made); a refusal is an auto-hiding toast. `check_comment_target`: lines → `allows_range` with New=RIGHT,
  Old=LEFT ("GitHub only accepts comments within its diff hunks"; a file without GitHub diff → "GitHub
  shows no diff for this file, so it only takes a file comment"; a file not listed → "This file isn't
  part of the pull request on GitHub", or when the list is truncated "GitHub didn't list this file's
  diff (the pull request has too many files), so it only takes a file comment"); files →
  `contains_file`; review comments always. Before GitHub's files load: "Loading the pull request's
  diff from GitHub…"; after they failed: "The pull request's diff couldn't be loaded from GitHub:
  reload the pull request to comment on its files and lines". Rows the local diff shows but GitHub
  doesn't (expanded context) are refused, as GitHub would. (As built in slice (c) the check compared
  the working tree's numbers with GitHub's whatever was checked out; slice (d) fixed that: line
  comments need the pull request's head checked out, and new lines are mapped through the
  checkout's changes first.)
- **Export**: decision: `y` still exports only the review's own comments (drafts), never GitHub's
  threads.
- **Line cursor fix** (`line_cursor.rs`): focusing a side of a split diff directly (as `]t` to a
  comment or thread on the other side does) only reaches the split editor during the window's focus
  phase, when `notify` doesn't reach observers (`App::notify` → `invalidate_view` returns early while
  drawing), so the highlight stayed on the other side's row. `LineCursor` now also syncs on the sides'
  `EditorEvent::Focused`. This affected old-side comments since M5 part 2 too.
- Tests: 7 new (127 total + 1 ignored): `place_thread` (every PR A thread, filters, moved, unknown
  HEAD, renamed paths both sides, LEFT→RIGHT range, LEFT context), `thread_location`, the gate over PR A
  and PR D files; GPUI with a fake GitHub and files shaped like PR A (`pull_request::tests::review`):
  description/summaries/bot summary hidden, inline rows of each kind in unified view, every toggle,
  `]t`/`[t` order and wrap, read-only `i`/`dd`/`Y`, panel rows, copy actions, reload; split view (the
  LEFT thread on the left side, four split toggles, `]t` onto the left side with the line cursor);
  401 on threads → forge resolved again + one retry, then recovery on reload; HEAD moved → line
  threads into moved blocks and a reload; the gate (R100 refused with a toast, R59 and old L193
  allowed, file and review comments, `i` on a row with a thread and a comment, export without
  threads); the line cursor's direct-focus case. New GPUI tests stable under `ITERATIONS=5`. Clippy
  clean.
- **Live check (c)** (harness, PR A #1 read-only, HTTPS clone + `ZH_EXTRA_ENV` token; screenshots in
  the orchestrator's `validation/M7c/`; nothing posted; leak grep clean twice): description with
  headings, list, code block and blockquote, the "Initial pass…" summary and "1 bot review hidden";
  split: file thread on docs/guide.md, engine.rs:134 with its reply, the LEFT thread "Old line 193" on
  the left side; unified: the LEFT thread below the removed row, engine.rs:134; palette toggles →
  strings.rs:48 RESOLVED, engine.rs "Outdated / moved threads (1)" with "Originally line 223 at
  5c5cdf4", the bot summary and the filters.rs:38 bot thread (BOT badge, suggestion block), "Lines
  20–25"; panel: 4 rows by default, 7 with every toggle, the menu's thread toggles (Show Bot Reviews off
  → 6), clicking a row lands on its thread; `c` on engine.rs line 100 (context expanded up to it) →
  "GitHub only accepts comments within its diff hunks"; `c` on line 59 (between local hunks, context
  expanded) → editor (then `:q!`); `CopyPullRequestUrl` → `zh clip` = the PR URL, Copy Branch Name →
  `feature/template-filters`. Found and fixed there: the line cursor staying on the right side after
  `]t` to the left side (above), panel locations squeezed to "s…", the description area taking too
  much height (16 → 12 lines). Not verified live: the auth-error re-resolution, the pending-review
  warning and Closed/Merged badges (no such state on PR A), a HEAD switch moving threads (GPUI tests
  cover them).

**Hardening 3 (M7 slices (a) + (b) review findings, `reviews/M7ab.md` in the orchestrator's state
dir)** — orchestrator decisions for H1, M1–M4 and the lows; no upstream file changed (new dependency
`git_hosting_providers`, was a dev-dependency; `zed_actions` dropped):

- **H1, tokens only for GitHub's hosts** (gh's rules): `GitHubHost::Cloud` (github.com, `*.ghe.com`;
  `ssh.github.com` is github.com) gets `GH_TOKEN`, then `GITHUB_TOKEN` (Zed's environment, then the
  project's directory environment), then `gh auth token --hostname`. `Configured` (a `github` provider
  in the **user's** `git_hosting_providers` settings; a project's `.zed/settings.json` never counts)
  gets `GH_ENTERPRISE_TOKEN`, `GITHUB_ENTERPRISE_TOKEN`, then gh. `Unverified` (only Zed's hosting
  provider registry calls it GitHub's: `origin`'s host containing "github", or a project setting)
  counts only when `gh` has a login for it, and then gets only that login's token. `gh` runs without
  the four token variables, so it prints its own login's token; that makes "known to gh" a local check
  (no network, unlike `gh auth status`). An unverified host without a login → `GitHubError::
  HostNotConfigured` ("GitHub isn't configured for H. If it is a GitHub Enterprise Server, add it to
  `git_hosting_providers` in your settings (provider "github") or run `gh auth login --hostname
  H`."): the picker shows it in the Pull Requests section and nothing is sent anywhere. Picker status
  rows are truncated with the whole text in a tooltip (the picker's list measures rows unwrapped).
- **L5**: an HTTPS remote's port is part of the API host (`ghe.corp:8443`; PR URLs keep it too); an
  SSH remote's port isn't. SSH remotes on hosts that aren't GitHub's by name are resolved with `ssh -G
  <alias>` (its `hostname`; plain host names only; cached per session), so `github-work` or
  `github.com-personal` aliases are github.com. A configured provider's `host:port` serves SSH remotes
  on its host.
- **L4**: tokens are cached per (host, work dir), so a project's direnv token never serves another
  project. **L9**: the forge cache key has every remote's URL and the host rules. **N1**: saturating
  rate-limit arithmetic. **N2**: the SSO "Authorize" button only opens an `https` URL on the host's own
  web server.
- **M1**: `FastForward { branch, to }` checks `git symbolic-ref -q HEAD` == `refs/heads/<branch>` in
  the same git job as `merge --ff-only`, else "Didn't fast-forward B to the pull request's head: X is
  checked out now (HEAD is detached now). Open the pull request again." The plan's branch, HEAD and
  branches come from git (`read_local_state`: `revparse_batch` + `branches()` on the job queue), not
  the snapshot. `open_pull_request` refuses a second open in a repository while one runs ("Another pull
  request is being opened in this repository. Try again once it is open.").
- **M2**: a local branch named like the head branch is reused only when its upstream is
  `refs/remotes/<head remote>/<head_ref>` (`ChangeBranch{head_ref}`: Zed checks the local branch out,
  no upstream change); any other same-named branch → `refs/pull/N/head` → `pr/N`.
- **L3**: an existing `pr/N` that isn't checked out is fetched into `FETCH_HEAD`, checked by
  `EnsureFastForward` (`merge-base --is-ancestor` both ways) before switching, then fast-forwarded: one
  ahead of the head (own commits on top) stays as it is, like the checked-out case; a diverged one stops
  with "pr/N has diverged from the pull request: … Rename the branch (`git branch -m pr/N pr/N-old`),
  then open the pull request again." (never "delete"). A reused tracking branch gets the same check.
- **L1**: HEAD already at the PR head but the base commit missing → fetch `refs/heads/<base>` from the
  canonical source. **L2**: `validate_branch_name` rejects `HEAD` and pseudo-refs (all capitals and
  `_`, ending in `HEAD`: `FETCH_HEAD`, `ORIG_HEAD`, …).
- **M3**: no git runs to check out a pull request in a repository Zed doesn't trust
  (`Repository::is_trusted`, Restricted Mode): in place when the plan isn't empty, in a worktree when
  one is made or moved. `checkout::RestrictedMode` → a toast "This project is in Restricted Mode, so
  Zed doesn't run git to check out pull requests in it: … Trust the project, then open the pull request
  again." with a "Trust Project…" button (`workspace::ToggleWorktreeSecurity`). Listing and reading
  pull requests stays allowed. A linked worktree shares its source checkout's git configuration, so an
  existing `pr-N` worktree is moved (with its own `-c` restrictions if untrusted) once the source is
  trusted.
- **M4 + L6/L7/L8, Review in Worktree**: no longer through `create_worktree_workspace` (it made a
  worktree for every repository of the project and always propagated trust). It fetches
  `refs/pull/N/head` + the base into `FETCH_HEAD` in the source repository, runs `git worktree add
  --detach <loaded head oid>` (`Repository::create_worktree`) for the PR's repository only, opens the
  path with `MultiWorkspace::find_or_create_local_workspace` and takes the new workspace's repository
  at that path. The worktree is trusted (`TrustedWorktrees::trust`, then the security modal refreshes)
  only when the head repository is the base repository or the viewer's own fork; any other fork's
  worktree gets Zed's standard Restricted Mode prompt. An existing `pr-N` worktree that isn't at the
  head is moved (`git checkout --detach`) only after a prompt ("Move the worktree pr-N to pull request
  #N? <path> has B checked out / is at X. … branches stay as they are."). Lost compared to Zed's
  service: the source workspace's dock layout and open files aren't carried over, and there is no
  "creating worktree" indicator.
- **In-place checkout of a stranger's fork** (M4, kept like `gh pr checkout`; documented risk): the
  checkout keeps the project's trust, so the fork's `.zed/settings.json` (a language server binary,
  say), its tasks and the build scripts and proc macros language servers run all apply. "Review in
  Worktree" is the way to read such code. The M8 confirmation says so for fork PRs and names the branch
  the plan checks out (N4).
- **N3**: a same-side range thread whose start GitHub's diff doesn't have before its end shows on its
  last line. **N5**: the toast helper logs "pull request: …". **N6**: no change needed
  (`active_repository` already toasts "Code review needs a git repository"). **N7**: not changed: our
  runner uses `git` from the repository environment's `PATH`; Zed's `GitBinary` path isn't reachable
  through `GitRepository` (would need a core patch).
- Tests: 9 new (136 total + 1 ignored): host rules (configured with ports, SSH aliases,
  `ssh.github.com`, look-alikes), `is_host_name`, token precedence per host kind and the rejected/
  not-configured hints, tokens per host and project, **no request at all to `github.attacker.dev`**
  (registry auto-registration, GitHub and enterprise tokens in the environment; then configured in
  user settings → only the enterprise token goes there), a host known to gh, forge cache vs `git
  remote set-url`, the picker's hint for a look-alike host (zero requests, also for a typed PR),
  SSO URL checks, saturating headers, `plan_checkout` over `LocalState` (base missing, tracking/own
  same-named branch, existing/checked-out `pr/N`, `HEAD` names), real git: the fast-forward refusing
  another checked-out branch or a detached HEAD, `ensure_fast_forward` (behind/ahead/diverged, rename
  hint), a same-named branch tracking another remote left alone (`RealGitRepository` +
  `local_state` + Zed's `change_branch`) and a tracking one reused, one open per repository, Restricted
  Mode refusing in place and in a worktree (the fake fetch would panic). Clippy clean.
- **Live check** (harness, `validation/hardening3/`; sandbox reads only, nothing posted; leak grep
  clean): the sandbox picker lists #4/#1/#3/#2 with `GITHUB_TOKEN`; PR #2 with the user's own
  `fix/truncate-ellipsis` (no upstream, at the base) → `pr/2` at `632ee13`, the user's branch untouched;
  PR #1 with `feature/template-filters` tracking origin one commit behind → reused and fast-forwarded to
  `3f22f1b`; "Review in Worktree" #2 → `worktrees/sandbox/pr-2` detached at `632ee13`; that worktree
  moved to the base in a terminal → reopening asks "Move the worktree pr-2 …" and moves it back. A
  scratch repository with `origin` `https://github.attacker.dev/x/y` (fake tokens in the environment):
  the Pull Requests section shows the not-configured hint, the log has the local `gh auth token` check
  and no request. Trust prompts on (`trust_all_worktrees: false`): staying in Restricted Mode, the
  picker still lists, opening #2 → the Restricted Mode toast; "Trust Project…" → Zed's modal → trusted
  → #2 checks out; "Review in Worktree" #1 → the `pr-1` worktree is trusted (no Restricted Mode); in a
  clone of the public `octocat/Hello-World`, "Review in Worktree" on the fork PR #11572 → the
  `pr-11572` worktree opens in Restricted Mode with Zed's prompt. Not verified live: the fast-forward
  branch race, a GHES host, SSH aliases (unit tests cover them).

**Milestone 7, slice (d) (submit) and the M7 (c) review fixes** — `plans/M7-github-v2.md` §5 with
LV §5 unless noted; `reviews/M7c.md` in the orchestrator's state dir. No upstream file changed, no
new dependency (keymaps: `; s` in `code_review_vim.json`, the modal's arrows in `code_review.json`):

- **`code_review::SubmitReview`** (`; s` in the review tab's vim normal mode, user decision; palette;
  acts on the current review like the copy actions). In a pull request review it leaves the tab's
  open comment editors (keeping their text) and opens `SubmitReviewModal`; in a local review it
  **finishes** the review (M8 plan D3, adopted now): the `y` Markdown goes to whoever waits for the
  review's outcome (`--wait`, toast "Review sent to the terminal"), else to the clipboard ("Review
  finished and copied as Markdown" / "Review finished (no comments)"); drafts stay drafts.
- **Modal**: Comment / Approve / Request Changes (buttons; `tab`/`shift-tab`/`left`/`right` cycle,
  skipping disabled ones; Enter confirms, Escape cancels). On the viewer's own pull request
  (`viewerDidAuthor`) Approve and Request Changes are disabled with GitHub's wording as tooltips
  (O1/O2) and a note under the buttons. Counts ("N line comments (M ranges) · N file comments · N
  review comments"), the body's markdown preview. **Blockers** (Submit disabled): the viewer's
  pending review on GitHub (from the `PullRequestHead` re-query when the modal opens; with the pull
  request's link), line comments while the tab's HEAD isn't the pull request's head ("Check out the
  pull request's head X to submit line comments…"), drafts GitHub wouldn't take, listed with why
  (outside its hunks, lines changed locally, file not in the pull request, diff not loaded).
  **Warnings**: the head moved since loading (C13: the comments go to the loaded head, which GitHub
  accepts), outdated drafts (left out, listed, "delete them (dd) or write them again"), a closed or
  merged pull request, a base that moved on GitHub while old-side comments are sent (L4, below), a
  failed re-query. A review that needs a body (LV §5.1: no line/file comments, not Approve,
  whitespace = empty) turns Submit into **"Add Review Comment"**: the modal closes and `;c`'s editor
  opens in the tab. Submitting shows "Submitting…"; a refusal stays in the modal (red) with GitHub's
  hint; Busy while the head re-query runs.
- **Payload** (`plan_review`, pure): drafts with text in creation order; `Outdated` and `Submitted`
  ones are left out. New-side lines → RIGHT, numbered in HEAD through the tab's `LocalChanges`
  (below); old-side lines → **LEFT with the base's numbers, ranges LEFT/LEFT** — also for left-side
  context rows (deviation from the plan's "old context single → RIGHT": LEFT on context rows is
  accepted (L6), and the pre-check confirmed LEFT context→deleted 189–192 and deleted→context
  191–196 on PR D (pending, deleted)). `start_line`/`start_side` only for a real range, sides always
  sent. Paths through `current_path` (a renamed file's new path). Bodies `[TYPE] ` prefixed; the
  review body = the review comments joined by a blank line (tuicr's `build_review_body`).
  `commit_id` = the pull request's head as loaded (= the checkout's HEAD whenever line comments are
  sent). Every line comment is checked again with `allows_range` (N5).
- **GitHub** (`GitHubClient::submit_review`): without file comments one `POST …/pulls/N/reviews`
  `{commit_id, event, body, comments}` (atomic). With file comments the pending flow (C4): `POST`
  without event and body → `AddFileThread` (new mutation in `queries.graphql`, whose whole document
  was validated live) per file, `thread: null` = `GitHubError::FileCommentRefused` → `POST
  …/reviews/{id}/events {event, body}`; any failure after the pending review deletes it (a failed
  delete is logged; the first error is returned; polish A: failures that don't say what GitHub did
  are looked up, and a pending review that can't be deleted is named). 422s of a review become `GitHubError::
  ReviewRefused`, whose hints add the pull request's URL (pending review) and the short commit
  (`commitOID`). A 401 resolves the forge again (`PullRequestState::resolve_forge` from any open
  checkout, `use_forge`) and retries once (polish A: from the submitting tab's checkout first; the
  submit runs on `PullRequestState`, not in the modal).
- **Success**: `ReviewSession::mark_submitted(ids, review id)` (stored), `PullRequestState::reload`,
  `ReviewOutcome::Submitted { url }` for waiters, a toast "Review submitted: ‹url›" with **Copy URL**
  (not auto-hiding). Submitted comments leave the diff, `]t` and the top block
  (`CommentBlocks::is_shown`; their threads come back with the reload); the panel still lists them,
  "submitted", and a click says the thread is on GitHub.
- **Outcome hooks for M8** (`session.rs`): `ReviewOutcome::{Submitted { url }, Finished { markdown
  }}` (pub), `wait_for_outcome() -> oneshot::Receiver` (canceled = the session was released = every
  tab closed: M8's `Closed`; polish A: once no tab shows the session), `has_outcome_waiters`,
  `resolve_outcome`. A session re-key (Change Base, branch switch) doesn't move waiters yet: M8
  must.
- **M7c M1, threads on locally changed files** (the review's "best" fix): `LocalChanges` (one per pull
  request tab) holds each review file's diff against HEAD (`Project::open_uncommitted_diff`, which
  follows uncommitted and unsaved edits). `remote_items` maps a RIGHT thread's lines HEAD → working
  tree; a thread on lines changed locally goes to the file's "Outdated / moved threads" block,
  labelled "Line 26 at 632ee13 · changed locally"; a thread whose file isn't compared yet waits (not
  shown for that moment). Recomputed on every comparison change (anchors are made again when the
  mapped lines change). LEFT threads count in the base, which local edits don't touch.
- **M2, per-tab gate**: the gate lives on `CommentBlocks` (not the shared session):
  `PullRequestState::check_comment(target, local_head, local_changes)` refuses line comments unless
  the tab's HEAD is the pull request's head ("Check out the pull request's head 3f22f1b to comment on
  its lines: GitHub numbers them in that commit") and new lines changed locally ("These lines have
  local changes, which the pull request doesn't have"); new lines are mapped to HEAD's numbers before
  `allows_range`. File and review comments stay allowed.
- **L1**: markdown from GitHub and commit messages (`untrusted_markdown`) opens only http(s) links
  (others: a toast "Not opening ‹url›…", logged) and shows the hovered link's destination in the
  view's bottom-left corner (`LinkPreview`, per markdown text so one text's mouse-out doesn't clear
  another's); "Open on GitHub" buttons, the PR URL button and `OpenPullRequestInBrowser` go through
  the same check and show their URL as a tooltip. Own comments' markdown is unchanged.
- **L2**: `]t`/`[t` order items on one row like their blocks: threads by GitHub's order, then
  comments by id. **L3**: `PullRequestState` keeps every checkout that reviews it (each tab
  registers), and resolves the forge from the latest one still open. **L6**: `originalCommit` is
  fetched; outdated threads say "Originally line N at ‹original commit›". **L7**: patch row counters
  and hunk row indices saturate. **N2**: the failed-diff message says "files and lines". **N3**: a
  pull request tab reloads on `HeadChanged` only when the HEAD commit changed. **N6**: done.
- **Not fixed (documented)**: **L4** a reload doesn't move the tab's base; when GitHub's base moved,
  old-side numbers may differ (a submit warns when old-side comments are sent). **L5** a lone `\r`
  in a file shifts Zed's rows against git's lines (Zed counts it as a line break). **N1** moved
  threads count as "outdated" in the panel. **N4** deleting a thread's lines collapses its anchor.
- Also: the crate is formatted with rustfmt (earlier slices weren't); clippy clean.
- Tests: 16 new (152 total + 1 ignored): `plan_review` mapping (RIGHT through a local mapping, ranges,
  LEFT and LEFT/LEFT, renamed path, file comment, refused/outdated/submitted, not at head, no diff),
  body and `needs_body` matrix, the golden one-shot request, the pending flow request by request,
  cleanup after a null thread / a failed submit / a failed delete, refused-review hints (URL, sha),
  hostile patches, web links, hovered links; GPUI: submit with line + file + review comments
  (pending flow, payload, Submitted state, hidden inline, reload, outcome), blockers and warnings
  (pending review, moved head, own vs others' pull request, Add Review Comment), line comments
  without the head, a refused review after an auth retry, local finish (clipboard vs waiter),
  `mark_submitted` persistence, threads/comments through local changes, `]t` on one line, the forge
  resolved after the opening checkout closed, `originalCommit`. New GPUI tests stable under
  `ITERATIONS=5`.
- **Live check (d)** (harness, `validation/M7d/`; `ZH_EXTRA_ENV` token; sandbox only, never PR A;
  leak grep clean twice):

  | Check | Result |
  |---|---|
  | PR C #2 (own): R26 (Issue), R65–68 (Suggestion), old L24 (Note), old L23–25 (left side), notes.md R6, file comment notes.md, review comment (Praise) → Comment | PASS: one COMMENTED review, threads RIGHT 26, RIGHT 65–68, LEFT 24, LEFT/LEFT 23–25, RIGHT 6, FILE notes.md, `[TYPE]` prefixes, body "[PRAISE] …"; drafts replaced by the reloaded threads; toast with the URL |
  | PR C: Approve / Request Changes | PASS: disabled, `right`/`tab` stay on Comment |
  | PR B #3 (bot): Approve, no comments | PASS: APPROVED, empty body |
  | PR B: Request Changes, no comments | PASS: "Add Review Comment" → `;c` editor → CHANGES_REQUESTED with "[ISSUE] …" |
  | PR B: pending review made with `gh api` | PASS: blocker with the PR link, Enter does nothing; deleted with `reset.sh clear-pending` |
  | PR B: head pushed to aa155ba (contents API) | PASS: warning "moved on to aa155ba…"; review posted on 073e29a (accepted), then "Checked out 073e29a, pull request head aa155ba" |
  | Local review `; s` | PASS: clipboard = the `y` Markdown, toast |
  | M1: PR C reopened with 2 lines inserted above and line 26 edited locally | PASS: range thread below working row 70, line 26's thread "changed locally" in the moved block, `c` on the edited line refused |
  | L1: summary links | PASS: web and `file://` destinations shown on hover (not clicked, harness rule); Open on GitHub tooltip shows the URL |

  Afterwards: `reset.sh new-pr-c` (PR C is now **#6**), `new-bot-pr` (PR B is now **#7**, branch
  `bot/harden-settings-1792229777`), #2 and #3 closed, no pending review anywhere; the harness clone
  is back on `feature/template-filters`. Not verified live: the auth retry, refused-review hints, a
  failed pending flow (fake HTTP tests cover them).

**Milestone 8 (CLI)** — `plans/M8-cli-v2.md` (D1–D9) unless noted. Upstream glue in `cli`
(`main.rs` + the new `review.rs`), `zed/src/zed/open_listener.rs` and `zed/src/main.rs` (§3.10); new
dependency `cli` (lib only, already in `zed`'s graph):

- **Command line**: `zed [-n|-e|-a] [--wait] --review TARGET [REPO_DIR]` (`--review` before the
  path: clap passes everything after the first path through as paths, so a later `--review` is "--review
  must come before the repository path"; more than one path, a missing or non-directory path are
  errors too; `--diff` conflicts). The CLI canonicalizes the directory (default: the current one) and
  sends `urls = [zed://review?repo=<path>&target=<target>]` (every byte but `A-Za-z0-9-._~/`
  percent-encoded) **plus `paths = [<path>]`**: Zed trusts a request only when its paths are exactly
  the link's repository (D1: xdg-open runs the CLI with the link alone, a browser can't add a path).
  With `--review`, clap's usage errors exit 1 instead of 2 (`try_parse` + `review::requested()`),
  and a Zed that goes away without an `Exit` is "Zed exited before the review finished" (exit 1, so
  a crash never looks like a submit). `zed --help` documents the targets, `--wait`'s exit codes and
  links; `before_help` has the `zed --review pr:123` example.
- **TARGET** (`parse_target_spec`, checked before anything opens): `uncommitted`, `branch` (the
  default branch; none → "No default branch: use branch:BASE"), `branch:BASE` (checked like a typed
  base: `BranchBase::new` + `check_branch_base`), `commit:REV` (one commit only), `commits:A..B`
  (a range only), `pr:N`, `#N`, `OWNER/REPO#N`, PR URLs; a bare number is refused with "did you mean
  pr:123? (A commit is commit:123.)"; anything else lists the forms. Trimmed, at most 1024 bytes.
- **Zed side** (`code_review::cli`, D2: GPUI-testable in the crate instead of `zed/review_cli.rs`):
  `handle_cli_connection` hands a request whose only URL is a review link to `handle_cli_review`
  (with the CLI's open behaviour as `OpenOptions` and its environment); `OpenRequest::parse` maps a
  review link to `OpenRequestKind::Review`, which `handle_open_request` passes to `open_review_link`
  (a link Zed got some other way: untrusted, errors as a toast). The repository is the deepest
  directory at or above the link's path with a `.git` file, or a `.git` directory **with a HEAD**
  (deviation found live: `/tmp/.git` is an empty directory on this machine, and `--review
  uncommitted /tmp` opened all of `/tmp`; git needs the HEAD too).
- **Window reuse** (D5): unless `-n`/`-r` (`WorkspaceMatching::None`), the active window first,
  then the others, each window's displayed workspace first: a local project with a visible worktree
  at or above the repository's root and a repository whose work directory is the root (a window on a
  subfolder doesn't count: the review couldn't open the other files). The window is activated and
  that workspace shown. Otherwise `workspace::open_paths(root, open options)` (Zed's `zed DIR`
  behaviour: a new window, or the existing window's sidebar when the multi-workspace sidebar is on;
  no first-run "open behaviour" prompt) and `repository_at`.
- **Opening** (`open_review`): `deploy_in_repo` (an open tab of the same review is activated) after
  the target's checks, or `open_pull_request` with `confirm_checkout` for an untrusted link (D6: M7b's
  "Check out pull request #N?" prompt, only when the checkout would move; nothing is ever posted or
  finished by a link). Cancel is `pull_request::ReviewCancelled` ("Review cancelled", exit 1, no
  toast). Without `--wait` the CLI exits 0 once the tab is open (a pull request: after its checkout).
- **Errors** (D8): stderr gets `forge::error_message` (the GitHub hint, or the whole chain), exit 1;
  an untrusted request's errors also show as a toast (in the workspace it opened in, else the active
  window's), since nobody may see the terminal.
- **`--wait`** (D4, D7): the handler keeps only a `ReviewSession::wait_for_outcome()` receiver (a
  handle on the tab or session would keep the session alive) and pings the CLI every second.
  `Submitted { url }` → the URL on stdout, exit 0; `Finished { markdown }` (`; s` in a local review,
  M7d) → the Markdown without its last newline (the CLI's `println!` adds it back: byte-identical to
  `y`), exit 0, nothing without comments; the receiver canceled (no tab shows the review any more:
  every tab of it closed, split clones and other windows included; polish A: also when a tab that
  switched away still holds the session to switch back) → "Review closed without submitting",
  exit 2; Zed quitting → an `on_app_quit` hook answers "Zed quit before the review was submitted",
  exit 2 (tasks don't run again during shutdown); a failed ping (the CLI was killed) ends the wait, so
  a later `; s` copies to the clipboard. `CliReply` sends exactly one `Exit`. **A tab whose review key
  changes (Change Base, branch switch) moves the waiters to its new session**
  (`ReviewView::update_session`), so a base change doesn't end the wait (polish A: only when no other
  tab, a split clone, still shows the old session).
- **Harness**: `zh cli [-C DIR] ARGS` (the CLI with its default open behaviour, against the isolated
  instance; README row). `zh open` passes `--existing`.
- Tests: 14 new (166 total + 1 ignored): `parse_target_spec`, `ReviewLink::parse` (`+`/`%2B`,
  missing/relative repo, unknown keys), `repository_root` (nested, `.git` file, empty `.git`),
  `find_review_workspace` (subfolder window, several windows, nested repository), and with a fake
  response sink: wait until closed (pings, exit 2), finish (stdout, clipboard untouched, no waiter →
  clipboard, no comments), submitted URL, no window (a new one on the root of `src/`), `-n`, errors
  (trusted: stderr only; untrusted: a toast), split clone + two waiters (finish → both; closing the
  last tab → both 2), a lost CLI and quitting, the waiter following Change Base, and the pull request
  link prompt (Cancel → exit 1, no toast, HEAD unchanged; at the head → no prompt, both trusted and
  not). The `cli` crate's `review.rs` tests (URL encoding, usage errors) were run with a standalone
  `rustc --test` against the workspace's `anyhow`/`tempfile` rlibs, not `cargo test -p cli` (a cold
  `-p` set). New GPUI tests stable under `ITERATIONS=5`; clippy (`-p code_review`) clean. A test
  closing a tab must redraw the window (`window.refresh()`) before the session is released: the last
  painted frame's listeners hold the tab's top block and comment blocks until the next draw (the app
  draws after a tab closes anyway; live `:q` exits 2 at once).
- **Live check** (harness, `validation/M8/`; local fixture of FINAL §1.3, the sandbox clone with
  `ZH_EXTRA_ENV` token; nothing posted; leak grep clean; `reset.sh status` unchanged):

  | Check | Result |
  |---|---|
  | `zh cli --help` / `-h` | PASS: the TARGET table, "--review must come before PATH", exit codes 0/2/1, links; example `zed --review pr:123` |
  | `uncommitted`, `branch`, `branch:main`, `commit:HEAD~1`, `commits:HEAD~2..HEAD` | PASS: exit 0 each, matching tabs in the one window; again → the tab is activated |
  | `commit:nope`, `bogus`, `123`, `branch:lonely`, `o/r#0`, `commit:a..b`, no value, after the path, two paths, missing dir, `--diff` | PASS: exit 1 with the messages above |
  | `--review uncommitted /tmp` | FAIL → fixed: opened `/tmp` (empty `/tmp/.git`); now "/tmp isn't in a git repository", exit 1 |
  | `REPO_DIR` = `$W/src`; `repo2` twice; `-n`; `-C $W/src --review branch` | PASS: same window; repo2 in a new window, then reused; `-n` a second window; the branch review in the active window with the repository |
  | `--wait` + `:q` | PASS: no exit for 5 s, then exit 2 "Review closed without submitting" |
  | `--wait`, `c` "cli comment", `; s` | PASS: exit 0, toast "Review sent to the terminal", clipboard still SENTINEL, stdout byte-identical to `y` |
  | `--wait commit:HEAD~1`, `; s` (no comments) | PASS: exit 0, empty stdout |
  | `--wait branch`, Change Base → `main`, `; s` | PASS: still waiting after the switch ("vs main"), exit 0 after `; s` |
  | `--wait`, CLI killed (SIGINT), then `; s` | PASS: Zed alive, no toast; clipboard = the Markdown, "Review finished and copied as Markdown" |
  | `--wait` + "zed: quit" / `kill -9` of Zed | PASS: exit 2 "Zed quit before the review was submitted" / exit 1 "Zed exited before the review finished" |
  | links via the CLI: `branch:origin/main` (+ an unknown key), `repo two` (space), `relative/path` | PASS: exit 0, branch tab; repo two opens; exit 1 + toast |
  | a link plus another URL (Zed's generic URL path → `open_review_link`) | PASS: `commit:HEAD` opens; `bogus` → toast |
  | sandbox: link `pr:1` with `main` checked out | PASS: "Check out pull request #1?" prompt before anything moved; Cancel → exit 1 "Review cancelled", no toast, HEAD `main` |
  | `--wait --review pr:6` | PASS: no prompt, `fix/truncate-ellipsis` at `632ee13`, tab "PR #6: …"; `:q` → exit 2; nothing submitted |
  | `drew-council/code-review-sandbox#1`, the PR URL, a link while at the head | PASS: `feature/template-filters` at `3f22f1b`, one tab reused, no prompt, exit 0 |

  Not verified live: two waiters with a split clone (V6.11; GPUI test), `--wait` + a submitted pull
  request review (V6.18, left to the final validation: posting), Wayland window activation, a
  symlinked repository path (the CLI canonicalizes; a lookup miss would open a second workspace).

**Milestone 9 (fork ops)** — the user was away, so everything is prepared in the repo and nothing
touches `~/nixconf` (decisions by the agent, can be vetoed):

- **Rebase skill** at `.claude/skills/rebase-code-review/SKILL.md` (Claude Code's project skill
  path; a fork-only directory, so it can't conflict and isn't in the Nix source). Its footprint
  check excludes `crates/code_review`, `assets/keymaps/code_review*.json`, `script/live-harness`
  and `.claude`; at M9 that leaves 14 files: §3.10's list plus `.rules`, `README.md` and
  `Cargo.lock`. It builds with `-p zed -p cli` and checks the Nix package by evaluation only.
- **`nix/build.nix` knob** (§3.10): upstream's flake always wrote `nightly` into
  `RELEASE_CHANNEL` and installed `dev.zed.Zed-Nightly.desktop`, so a flake build never was the
  stable app §2.1 assumes. The new `releaseChannel ? "nightly"` argument keeps upstream's output
  by default; `.override { releaseChannel = "stable"; }` gives `RELEASE_CHANNEL` `stable`, version
  `1.25.0+<sha7>`, the stable icons and `dev.zed.Zed.desktop` ("Zed"). `preview` works too; on
  Darwin anything but nightly fails an assert (its install phase hard-codes the nightly bundle).
  This replaces the draft's `overrideAttrs` that patched `preBuild`/`postInstall` from the user's
  config.
- **Verified by evaluation only** (no Nix build; `allow-import-from-derivation false`): with
  `commitSha = null`, `packages.x86_64-linux.default` and its deps derivation have the same `.drv`
  paths before and after the change (`…hxg15nqi…-zed-editor-1.25.0-nightly.drv`,
  `…f90wncdj…-zed-editor-deps-1.25.0-nightly.drv`), i.e. upstream behaviour is byte-identical; the
  stable override evaluates to `zed-editor-1.25.0` with `echo stable > crates/zed/RELEASE_CHANNEL`,
  `app-icon.png`/`app-icon@2x.png`, `APP_NAME="Zed"` and `dev.zed.Zed.desktop`; `preview` →
  `zed-editor-1.25.0-preview`; Darwin + stable → assert; an unknown channel → "attribute missing".
  INSTALL.md's overlay snippet, evaluated against this checkout, stops with "code-review is on a
  preview tag" (as intended until the stable rebase) and evaluates to `zed-editor-1.25.0`,
  `mainProgram = "zed"` with the channel forced to stable.
- **Install guide** `crates/code_review/INSTALL.md` (from the orchestrator's draft): `zed-fork`
  flake input without `follows`, an inline `zed-editor` overlay entry in `overlays.nix` (Linux
  only, asserting the fork's `RELEASE_CHANNEL` is `stable`), removing the `zed` alias in
  `modules/packages/graphical.nix` (lines 14–24 and 91; the fork ships `bin/zed`);
  `home/xdg-mime.nix`, `vars.defaults.editor`, the `zed` shell alias and lazygit keep working.
  **Not applied: waits for the user** (and for the push and the stable rebase).

**Polish A (the M7d and M8 review findings, `reviews/M7d.md` and `reviews/M8.md` in the
orchestrator's state dir)** — orchestrator decisions; upstream glue only in `cli` (`main.rs`: the ping
marker check and two more `--review` conflicts), no new dependency:

- **M7d H1 + L1, the submit belongs to the pull request**: `PullRequestState::submit(SubmitRequest)`
  owns the task (holding the pull request and the session strongly until it ends) and a `submitting`
  flag shared by all tabs and windows of the pull request; it marks the drafts submitted, tells the
  `--wait` waiters, reloads and shows the toast itself, and answers the modal through a oneshot
  (`SubmitReply`). The modal only starts it and shows the reply: while its submit runs,
  `on_before_dismiss` returns `DismissDecision::Pending`, so Escape, Cancel (also disabled), a click
  outside and another modal (the palette, say) leave it open with "Submitting…". Another window's
  modal is Busy ("…being submitted from another tab…") and a second submit is refused. The modal
  plans again on every change of the drafts, the pull request (loads, submits elsewhere), HEAD and
  the local comparisons (not while its own submit runs), and is Busy until the drafts loaded (N6).
- **M7d M1, failures that don't say what GitHub did** (a timeout, a dropped connection, a 5xx, an
  unreadable 2xx: `GitHubError::Transport`; 4xx are definitive): a one-request review is looked up
  (`GET …/reviews`, paged: the viewer's, the same commit, the event's state, the same body,
  submitted at most 5 minutes before the attempt started by this machine's clock, and the same
  comment bodies through `GET …/reviews/{id}/comments`); found = success. Pending flow: an
  unreadable 2xx to `/events` = submitted; otherwise the cleanup `DELETE` decides: 422 "Can not
  delete a non-pending pull request review" = submitted (success, with the pending review's id and
  URL), 2xx/404 = nothing left; a failed delete asks `GET …/reviews/{id}` (PENDING = left,
  submitted = success). An unreadable 2xx to the pending `POST`: the viewer's pending review (their
  only one) is found through `PullRequestHead` and deleted. An ambiguous pending `POST`: a pending
  review found afterwards is named, not deleted (it might be the user's own from GitHub's page).
  When GitHub can't be asked: `GitHubError::ReviewUncertain { attempt }`; the pull request keeps
  the attempt (`uncertain_submit`, while it is open, not persisted), every modal warns "The last
  submit may have posted the review…" and Enter becomes **Check GitHub**
  (`Forge::find_submitted_review`; found = success for the attempt's drafts; none = "GitHub has no
  review from the last attempt, so nothing was posted: submit again."; an attempt's pending review
  still pending is deleted).
- **M7d L2/L3/L4**: a pending review that couldn't be deleted is `GitHubError::PendingReviewLeft {
  review_id }`, whose hint says so and links the pull request. After a 401 (or a `PendingReviewLeft`
  caused by one) the forge is resolved again, from the submitting tab's checkout first
  (`resolve_forge_from`; tokens are per project), a pending review left behind is deleted with the
  new token (`Forge::discard_pending_review`), then the review is tried once more.
- **M7d L5**: `LocalChanges::read_head_texts(repository, head, paths)` reads the pull request head's
  text of the files with new-side drafts from git when the modal plans at the head;
  `working_to_head(.., head, ..)` is `Unknown` while a file's comparison base doesn't hash the same
  (CR and a BOM ignored). A modal opened right after a checkout shows "Comparing the files with
  HEAD…" or the "still being compared" refusal, and plans again once Zed reloads the base (it
  emits `LocalChangesEvent`). The comment gate uses the same check once texts were read. **L6**: a
  file whose comparison failed is `LineMapping::Failed` for comments ("couldn't be compared with
  HEAD"); threads keep the identity mapping.
- **M7d L7/L8**: `set_comment_body`/`set_comment_type`/`delete_comment` ignore submitted comments,
  and another tab's open editor of a comment that becomes submitted closes without saving.
  `SubmitReview` (`; s`, palette) acts only on the active item when it is a review tab (decision:
  also for pull request reviews), else the toast "Switch to the review tab to submit its review".
- **M7d nits**: N1 links open the parsed URL; N2 the closed/merged warning uses the head check's
  state; N3 a pending review without `databaseId` still blocks (`PendingReview::database_id:
  Option`); N4 checked live (below); N5 the `Forge::submit_review` doc says what failures leave;
  N7 the refused-link toast cuts the URL at 80 characters. **Not done**: L9 (a draft made in
  another checkout of the pull request is numbered by the submitting tab; storing HEAD numbers
  needs a comment model change), N8.
- **M8 M1/M2, waiters follow the tabs showing a review**: `ReviewSession::shown_by` counts the tabs
  showing the session (`show` in `ReviewView::new` and on a key switch, `hide` when the tab is
  released and on a key switch); sessions a tab keeps to switch back don't count. At 0, the waiters
  move to the session the last tab switched to, or are dropped (Closed, exit 2). So Change Base in
  one split clone leaves the waiters with the other tab, and another tab's `previous_sessions` no
  longer keeps `--wait` hanging. Caveat: closing every tab while a submit runs ends the wait as
  closed although the submit completes.
- **M8 L1**: `handle_cli_review` sends `CliResponse::Ping` first; the CLI (`review::check_exit`)
  turns an `Exit { 0 }` without a ping before it into exit 1 "This Zed doesn't support --review:
  restart Zed after updating it" (a Zed without `--review` answers the link with Exit 0 at once).
  No protocol change.
- **M8 L2**: a link (a request without the CLI's path) asks before opening a repository that isn't
  open in any window ("Open ‹root› for a review?", in the active window, else in the repository's
  new window before the review opens), and before a branch or commits review with more than 2000
  changed files (`commits::changed_file_count`, the committed tree diff). Cancel = exit 1 "Review
  cancelled", no toast. Link error toasts are cut at 300 characters (I4).
- **M8 L3–L7**: a `Stdout` send that fails (the CLI died after its last ping) puts the Markdown on
  the clipboard with the toast "The terminal went away: review finished and copied as Markdown";
  options after the path → "Options must come before the repository path (got --wait)"; a blank
  TARGET → "--review needs a TARGET: …" (no toast: a command line's own errors are decided from its
  paths, before the link parses); `--foreground` and `--dev-container` conflict with `--review`
  (clap, exit 1); `zh cli -C DIR` dies when DIR isn't a directory. I2: canonicalize errors other
  than "not found" show the OS error. I3: the quit hook is registered as soon as the request comes.
- Tests: 14 new (180 total + 1 ignored, stable under `ITERATIONS=5`): GitHub (one-request lookups
  after a 5xx/timeout/unreadable answer, pending-flow outcomes incl. the non-pending 422 and a 401,
  the unreadable pending `POST`, later checks), submit GPUI (dismissal refused while submitting,
  another window's modal Busy and a second submit refused; uncertain → Check GitHub → nothing →
  resubmit → found without a second post; submitted comments immutable across a split clone; a 401
  after the pending review with the fresh-token cleanup; Submit needs the review tab), the HEAD-text
  check, waiter counts with a split clone + Change Base + close, CLI (the ping first and no toast
  for a command line's error, a lost CLI → clipboard, link prompts for an unopened repository and
  for more than 2000 files, a link without any window). `cargo test -p cli`: 8 passed (incl.
  `check_exit` and the new usage errors). Clippy (`-p code_review`) clean.
- **Live check** (harness, `validation/polishA/`; leak grep clean; one review posted, PR C reset
  afterwards): PR C #6 with a file comment and a review comment (the pending flow without line
  comments, N4), `Return Escape`: the modal stays with "Submitting…" and a disabled Cancel; one
  COMMENTED review with the body and one FILE thread, threads reloaded, the toast; `zh cli --wait
  --review pr:6` printed the review's URL, exit 0 (V6.18). Then `reset.sh new-pr-c`: PR C is
  **#8**. Local fixture: `--wait --review branch:main`, split, Change Base → `other` in the clone,
  `; s` in the original → the Markdown on stdout, exit 0 (before: the clipboard); `--wait` again,
  `:q` in the original → exit 2 although the clone holds the session; the usage errors; an
  untrusted link to a repository not open → the prompt, Cancel → exit 1 "Review cancelled", no
  window. Not live: the old-Zed marker (no older Zed to pair with), the 2000-file prompt, ambiguous
  failures (fake HTTP tests).

**Polish B (core patches, sticky Outdated, small fixes)** — orchestrator slice; upstream glue only
in `editor` (core patch #3; #1 reverted) and a clippy fix in `cli`'s `review.rs` tests:

- **Core patches** (one `editor` commit): #1 (`set_number_deleted_lines` → `pub`) is reverted,
  nothing outside `editor` calls it (`git diff v1.25.0-pre -- crates/editor/src/config.rs` is
  empty). #3, opt-in (§3.10): `RowHighlightOptions::include_end_row` (default `false`, so every
  existing caller is unchanged) keeps the row a highlight ends in when the range ends at column 0.
  The line cursor sets it and ends its ranges at the end of the last row's text, so the cursor on an
  empty row directly above spacer rows (a blank line before a pure addition, a file's last empty
  line before an added file) no longer paints the spacers, focused or mirrored; a line-wise
  selection still covers the spacers it spans. Reviewed-hunk dimming is unchanged.
- **Outdated is no longer sticky** (the stash/pop backlog item): `ReviewSession::check_comment(id,
  false)` turns `Outdated` back into `Draft` (stored; `Submitted` is never touched). A comment is
  current again when (a) the once-per-run check finds its text at its stored lines (the file
  changed back while the review was closed, e.g. a stash popped then); (b) a tab anchors it on its
  line numbers (its lines were deleted, its buffer replaced, or the tab anchors it for the first
  time) and finds its text there; (c) the text of a tab's anchored lines changes *to* the comment's
  text (an edit putting it back); (d) the tab that outdated it because its lines were deleted kept
  their collapsed anchor (`LineAnchor::deleted_from`, `restored_lines`), and the comment's text is
  back at that place (an undo, or a pop re-inserting the lines right before the place its anchor
  on the line numbers had moved to). Only the exact text at that exact place counts: the same text
  elsewhere, or other text there, keeps it outdated. Existing tabs only react to changes in their
  own buffers, so a window whose buffer never lost the lines doesn't clear what another window's
  unsaved deletion outdated, and clearing never makes another tab re-check (no ping-pong; the
  two-window test covers it).
- **Buffers of commented files stay open** (found live): in an uncommitted review a stash empties
  the review and the files' buffers were released, so after the pop the comments came back on new
  buffers with only their line numbers from the stashed text, and a comment below lines the pop put
  back above it was anchored on the wrong line and outdated. Each `LineAnchor` now holds its buffer
  (`_buffer`), so `open_buffer` hands the review the same buffer when the file comes back, and the
  anchors have followed its edits. Costs one open buffer per commented file per tab.
- **Small known issues fixed**: the hovered link's destination (pull request description, review
  summaries, commit messages) hangs below the top block (a `deferred` overlay over the top of the
  diff) instead of covering its bottom row (thread toggles, Reload, review comments); thread cards
  keep it in their bottom-left corner. `]f`/`[f`/`]u`/`[u` and the panel's `l`/Enter put the file's
  header at the top of the view (`diff::file_start_autoscroll`) when the file's first row still fits
  below it with the scroll margin (else the old focused scroll), so file comments, file threads and
  the "Outdated / moved threads" block aren't under the sticky header (the end of the review still
  clamps the scroll). Filter regex errors are one line ("Invalid regex: unclosed group") instead of
  the regex crate's caret diagram.
- Tests: 4 new (184 total + 1 ignored; the new and changed ones stable under `ITERATIONS=5`): the
  cursor on an empty row above spacers (both sides, mirrored, a line-wise selection), outdated
  comments coming back with their text (a pop at the same place, the text elsewhere, other text
  there, an undo, a replaced line put back), a stash and pop in an uncommitted review (it passes
  without the buffer hold too: something else keeps the buffer alive in tests; the live check
  covers that), `]f` putting the header at the top with a file comment in view; extended: the
  session's stored Outdated → Draft,
  reopening after the file changed back and an edit putting a line back, two windows (one window's
  deletion outdates, the other doesn't clear it, putting the line back clears it in both, no
  ping-pong), the regex error text. `cargo test -p editor highlight`: 30 passed (incl.
  `test_highlighted_display_rows_in_range` and the bookmark row-highlight tests).
  `cargo test -p zed test_action_namespaces`: passed (cold test build of `zed`: 3m18s, 878 crates,
  about 30 GB of `target/`). `cargo test -p cli`: 8 passed. Clippy (`-p code_review -p cli -p
  editor`) clean after fixing two `cloned_ref_to_slice_refs` in `cli/src/review.rs`'s tests;
  `cargo shear` clean.
- **Live check** (harness, `validation/polishB/`, fixture `mkfixture.sh`: a feature branch with a
  committed change, uncommitted added/modified lines, a blank line before a pure addition; split
  view): the cursor on the blank row above the spacers highlights only that row, on the right side
  (mirrored on the left) and with the left side focused; uncommitted review with comments on an
  added line and a modified line: `git stash` → the added line's comment OUTDATED, `git stash pop` →
  both current on their lines (before the buffer fix the modified line's comment came back OUTDATED
  on the wrong line); branch review (the file stays in it): stash → OUTDATED, pop → current on
  "added line A"; `]f` to a file with a file comment → its header at the top, the comment in view;
  exclude prompt `(` → "Invalid regex: unclosed group"; a commit review's message link hovered →
  the destination below the top block, over the file header. No errors in the log beyond the known
  noise.

**Fix 1 (the final validation's FAILs: `plans/FIX-final-validation.md` slices A and B)** —
`validation/FINAL/FAILS.md` in the orchestrator's state dir. No upstream edits:

- **Focus lost after a split clone closes, or after the focused left side goes away** (V2.15+,
  V7.X1). `ReviewDiff`'s container (`track_focus`, the empty state's focus) could end up focused
  while the diff wasn't empty, outside every editor: no vim keys, no `editor:` palette actions, a
  frozen or missing mode indicator. (a) A review that gets the focus while it loads (opened from the
  palette, the panel, `; l`) leaves its pane's `last_focus_handle_by_item` on the container, and
  focusing that pane again (closing a split clone, `force_remove_pane`) restores it. (b) Unsplitting
  while the left side is focused (`ToggleSplitDiff`, a pane getting too narrow) drops the focused
  editor, and GPUI's focus-lost fallback focuses the nearest focusable ancestor, the container,
  without a redraw. The container now forwards the focus to the editor when it is focused with a
  non-empty diff (`on_focus`, route a) and when the focus-lost fallback lands on it (`on_focus_lost`,
  registered after the workspace's, plus a next-frame notify so vim's indicator updates, route b).
  The comment editors' focus guard holds its handle weakly. The fold the V2 validator correlated
  with the bug is incidental: the regression test fails the same with and without it; what matters
  is opening the review from outside its pane (the validator's passing controls had re-recorded the
  editor through palette/panel round trips). Upstream `git_ui`'s `DiffMultibuffer` has the same
  latent gap.
- **`ctrl-w h` after a split toggle or a resize jumped to the end of the review** (V7.X2). Each split
  creates a new left side whose cursor upstream only mirrors from the right side when the right
  cursor moves. `LineCursor` now puts a new left side's cursor on the row facing the right cursor
  (`selections.change_with`, so no selection effects and nothing mirrors back).
- **`]t`/`[t`/`m`/`M` skipped the line comments and threads of reviewed (folded) files** (V3.15):
  `comment_order` used the placement check, which leaves folded files out. It now only needs the
  row in an excerpt (`line_row_in_excerpts`); landing unfolds the file as before.
- **Unified placement of an old-side comment on the unchanged row right after a pure deletion**
  (V7.X3): the row's anchor was left-biased and resolved to the removed rows at the same place, so
  the card sat under the red row. It is right-biased now, and `]t` orders old lines facing the same
  new line by their old line.
- **Filter counts** (V2.08): generated/vendored files hidden by `.gitattributes` count as such
  before the include/exclude patterns ("include src · 3 filtered out · 1 generated hidden · 1
  vendored hidden"), and "All N changed files are hidden" counts the files the attribute filters
  don't hide (all of them when that is none).
- **Line breaks** (V3.02): comment cards, review threads, the pull request's description and review
  summaries, and (agent decision, same text going to GitHub) the submit modal's review body render
  a line break as a line break, as GitHub does (`comments::comment_markdown_style`,
  `soft_break_as_hard_break`). Commit messages keep CommonMark.
- **Panel comment rows** (V3.17/V4.O5): the location takes its full width (up to the row's) and the
  first line gets the rest, instead of the location being capped at 60% of a row sized to its
  content.
- **`e` on a removed row of a unified snapshot** (V1.F1) opens the working-tree file at the removed
  line (the path now comes from the row's file, `path_at_cursor`; the row stays the old one, as on
  the left side).
- Tests: 7 new (191 total + 1 ignored; the new and extended ones stable under `ITERATIONS=5`), each
  red before its fix: closing a split clone opened from the panel (with and without a fold), the
  focused left side dropped by a resize and by `ToggleSplitDiff`, an old-side comment after a pure
  deletion (placement and `]t` order), multi-line card height, the panel location width
  (`debug_bounds`), `e` on a removed row of a snapshot. Extended: `test_split_mirrors_cursor_row`
  (`ctrl-w h` after toggles and a resize), `test_comment_navigation` (`[t`/`M`/`]t`/`m` into a
  folded file), `test_filters_and_search` (counts, the all-hidden label). Clippy (`-p code_review`)
  clean.
- **Live check** (harness, `validation/FINAL/fix1/`, the V7 fixture in `fix1/fx`): V2.15+ ×4 (`:q`
  and `ctrl-w q` after `r` folded the original's file from the clone, without a fold, and right
  after a click), V7.X1 (palette toggle and narrowing after the narrow/type/widen/save sequence),
  V7.X2 (toggle ×2 and narrow/widen, then `ctrl-w h`), V3.15 (`]t`, `[t`, `M`, `m` into folded
  lib.rs), V7.X3, V2.08, V3.02, V3.17, V1.F1: all PASS; closing two split clones one by one still
  keeps the focus. The relative line numbers after closing a clone (V2.O6) looked right in these
  runs. No new log errors.

**Fix 2 (the final validation's observations: `plans/FIX-final-validation.md` slice C and the
orchestrator's additions)** — upstream edits: one appended help paragraph in `crates/cli/src/main.rs`:

- **File starts in view** (V4.O1, V3.O5, V5.O5). A file's comments and threads (and its "Outdated /
  moved threads" block) sit between its header and its first row, under the sticky header when the
  view's top is between them. Now the header goes to the top (`diff::file_start_autoscroll` /
  `file_header_row`, when the header, the blocks and the row fit with the scroll margin) when:
  landing on a file item (`]t`/`[t`/`m`/`M`, the panel's Enter/click: `land_on`); entering
  single-file mode on the file's start (`; f`); a reload brings or changes a file's threads above
  the cursor on its first row (`keep_file_start_in_view`); and any scroll that follows the cursor
  (`ScrollPositionChanged { autoscroll: true }`: vim's `gg`, a diff reload after HEAD moves) leaves
  the cursor on a file's first row with file blocks above it (`show_file_start_at_cursor`; it only
  asks when the header isn't at the top already, so it settles). The root of the first-file case
  (a PR opening on `docs/guide.md`'s file thread, V5's post-submit reload on `notes.md`): scrolled
  to the very top, Zed's `set_scroll_position` anchors the view on the first file's first row (the
  header is a block; `clip_point` has no earlier row), so blocks inserted above that row, and their
  measured heights (inserted as 3 rows, measured in prepaint), push the header and themselves above
  the top. A view at y = 0 is anchored on `Anchor::Min` instead (`keep_scroll_at_start`, on every
  scroll event and around block insertion), which always resolves to the top.
- **Visual `y`** (V3.O2) leaves the cursor on the selection's first row, as vim's yank does.
- **Panel**: `/` starts with the last search selected (typing replaces it; the filter prompts still
  start at the end of the pattern); a search nothing matches keeps the prompt open with "No file path
  contains “…”", and `n`/`N` toast when nothing matches any more (V2.O2/V8.O4). The status line
  wraps instead of being cut (V1.O3/V2.O1). `; h` in the panel is `FocusPanel` (a no-op there)
  instead of `;` + `h` collapsing the directory (V1.O6).
- **GitHub text**: `comments::github_markdown` drops `<details …>`/`</details>`/`</summary>` tags and
  makes `<summary>X</summary>` a bold line, for lines that start with one of them, outside fenced
  code (description, review summaries, thread comments; HTML stays unparsed: it could load remote
  images) (V4.O2). The bot badge of review summaries is "BOT" like threads' (V4.O3). Hints and
  toasts quote commands with "…" instead of Markdown backticks (V4.O4). The description and
  summaries scroll after 8 lines (decision: was 12, ~40% of a 1000-pixel window; collapsing long
  bodies by default was the alternative). *Superseded by UX-1 slice A: nothing scrolls on its
  own, the overview scrolls with the diff.*
- **Picker**: the pull request a query names (`1`, `#1`, a URL) comes first among the pull
  requests, above rows that only fuzzy-match (V4.O6).
- **Export**: a commit range of more than 10 commits says "Reviewing N commits: base..head" instead
  of listing every commit (V7.O3; tuicr's list up to 10).
- **Several repositories** (V6.O3, decision): when the project has more than one repository, review
  tabs and the panel header start with the repository's name ("zed · Review: Uncommitted changes",
  "zed · PR #12: …") and the tab tooltip adds its path; one repository: unchanged.
- **Submit modal**: it opens on the event (Comment / Approve / Request Changes) it was last set to
  for the pull request (`PullRequestState::review_event`, shared by its tabs and windows) until a
  review is posted, so "Add Review Comment" doesn't lose Request Changes (V5.O1). The counts line
  adds "N blocked" (line comments waiting for the head + refused drafts) and "N outdated" instead of
  "No comments" (V5.O2). After GitHub refuses a review, the modal asks GitHub about the pull request
  again: a pending review found then replaces the refusal's message with the same blocker as when
  the modal opens, and Submit stays disabled; coming back to the window (activation) asks again
  while a pending review or a failed check blocks (V5.O3). Other refusals keep GitHub's message and
  Submit.
- **Panel after a submit** (V5.O4): once threads are loaded after the submit
  (`PullRequestState::threads_include_submitted`), submitted drafts aren't listed (their threads
  are; the review comment is a review summary in the top block). Before that reload, and when it
  fails, they are, "submitted".
- **`--wait`** (V6.O1): `; s` in a local review with no comments says "Review finished (no
  comments)" also when a waiter gets it (nothing is printed, exit 0). V6.O8: `--help` of `-w,
  --wait` adds "With --review, waits until the review is submitted, finished or closed (see
  --review)." (appended paragraph in `cli/src/main.rs`).
- Tests: 7 new (198 total + 1 ignored): the `<details>` sanitizer, the commits banner, the counts
  line, a refusal for a pending review (blocker, disabled Submit, re-check on activation), a reload
  keeping a file's header at the top, the top of the review staying the top when the first file's
  threads load, repository names with two repositories. Extended: `]t`/`; f`/`gg` header-at-top
  (`test_file_navigation_shows_the_file_header`), visual `y`'s cursor, panel search and `; h`
  (`test_filters_and_search`), the picker's order, the event memory and the panel after a submit.
  The scroll tests were checked red without their fixes. `cargo test -p cli`: 8 passed. Clippy
  (`-p code_review`) clean.
- **Live check** (harness, `validation/FINAL/fix2/`; local fixture `mkfixture.sh`, run in
  `/tmp/fix2/ws`, since `~/.local` and `/tmp` hold stray empty `.git` directories Zed takes for
  parent repositories; sandbox clone for GitHub), all PASS: `; f` and visual `y`; panel `; h`,
  search prefill/replace/no match, wrapped status line; "Reviewing 12 commits: d288685..a496a19";
  `--wait` + `; s` without comments (toast, exit 0, nothing printed); two repositories' tab titles,
  panel header and tooltip; `--help`; PR A #1: `1` lists #1 first, opening on `docs/guide.md`'s
  file thread with the header at the top (before the `Anchor::Min` fix it opened 2 rows down, the
  thread under the sticky header), `gg` from the end, HEAD moved to `5c5cdf4` and back, a panel
  click on a moved thread, the bot summary's Notes section without HTML and its BOT badge, thread
  bodies, "1 blocked" with HEAD off the pull request's head; PR B #10: Request Changes → Add Review
  Comment → `; s` opens on Request Changes (closed without sending); PR C #11: one COMMENTED review
  (body, a line and a file comment, checked with `gh api`), then the panel lists the 2 threads only
  and `notes.md`'s file thread shows under its header. Not live: `n`/`N`'s toast, the re-check on
  window activation (no window manager in the harness; GPUI test). Sandbox writes: that review; PR
  C reset to **#12**. Token leak grep clean.

**Fix 3 (the regression sweeps' last cheap findings: R1 O1/O5, R2 KNOWN/O2)** — no upstream edits:

- **Panel selection on the left side** (R2, V2.O5/V4.O7): landing on an item of the other side of a
  split diff (`]t` from engine.rs "Old line 193" on the left side to filters.rs 20–25 on PR A)
  moves that side's cursor and focuses it before `SplittableEditor` switches its last focused side
  (on the next frame), so `ReviewDiff` didn't count either selection change as the diff's and the
  panel stayed on the previous file. A selection change counts while either side has the focus; the
  mirrored cursor of the side left behind changes last and names the new file.
- **Panel header with a second repository** (R1 O1): review tabs subscribe to the `GitStore`'s
  `RepositoryAdded`/`RepositoryRemoved` and re-title themselves (`TitleChanged` + notify), which the
  panel observes, so its header names the repository at once.
- **`r` at the top** (R1 O5): `r` moves to the next file with its header at the top and folds the
  file; from the very top that scrolled the folded header out of view. A view at y = 0 stays at the
  top when the cursor's new row fits with the scroll margin (`comments::stay_at_start`).
- **The review just posted** (R2 O2): `PullRequestState` remembers the posted review's URL; once its
  summary is loaded each tab's top block scrolls its 8-line description block to it (once; tabs
  opened afterwards start at the description). *Removed in UX-1 slice A (Q4): the description
  doesn't scroll on its own any more, and the summary is in the document.* Decision: chosen over listing the newest summaries
  first, which would differ from GitHub's order.
- Tests: 3 new (201 total + 1 ignored), each red without its fix: `]t` across sides with the panel
  (both directions), a repository added after a review opened, `g g r` at the top; extended:
  `test_submit_a_pull_request_review` (the description block scrolls to the posted summary). Stable
  under `ITERATIONS=5`. Clippy (`-p code_review`) clean.
- **Live check** (harness, `validation/FINAL/fix3/`): all four PASS: PR A `]t` from Old line 193
  to filters.rs 20–25 (panel on filters.rs) and a local left-side → right-side → left-side round
  trip; adding `repo2` renames the panel header with the tab; `g g r` keeps main.go's folded header
  and strings.rs in view (`r` further down unchanged); one Comment review on PR C #13 shows its
  summary under "Reviews" without scrolling (gh: one COMMENTED review), then `new-pr-c` → **#15**.
  Smoke (uncommitted, branch, PR A: `j`/`k`/`]f`/`]c`/`]t`, a comment on each side, `y`): no
  regressions; logs only the usual noise. Token leak grep clean.

**UX: opening a review shows the review panel** (the user's report after testing the fork: "when
opening review, the left panel doesn't default to opening the 'review' panel") — no upstream edits:

- `ReviewView::deploy_in_repo`, the funnel of every entry point (the Start Review picker,
  `ReviewUncommitted`/`ReviewBranch`/`ReviewCommits`/`OpenPullRequest`, the branch picker of a new
  branch review, pull requests in place or in a new worktree, `zed --review`, `zed://review` links),
  calls `Workspace::open_panel::<ReviewPanel>`: the left dock opens if it was closed and switches to
  the review panel if another panel (project, git…) was showing. The focus stays where the item's
  activation puts it, in the diff (`;h` still focuses the panel). It also does this when the review
  was already open (opened again from an entry point).
- Decisions: switching between open review tabs, `;l`, refreshes and reloads, split clones and
  Change Base (in place, or activating the tab that already reviews that base: a tab switch) leave
  the dock as it is, so a dock the user closed stays closed. Review tabs aren't restored on startup
  (not serializable), and the dock's own restored state applies as before. `open_panel` rather than
  `reveal_panel`, which would unzoom a zoomed center pane. A review deployed before the workspace's
  panels load wouldn't show the panel; no entry point gets there (the CLI and new-worktree flows
  wait for the repository scan, the panel loads on `initialize_panels`' first poll).
- Tests: 1 new (202 total + 1 ignored), red without the fix: with another left-dock panel active
  and the dock closed, opening a review shows the review panel with the diff focused (`j` moves);
  the same when the other panel is showing; after `; e` closes the dock, switching tabs and a
  repository change leave it closed; opening an open review shows the panel again.
- **Live check** (harness, `validation/ux-panel/`): PASS. Start Review → Uncommitted with the
  project panel showing, and → Branch vs main with the dock closed: the review panel shows, `j`
  moves in the diff; after `; e` the dock stays closed through `ctrl-pgup`/`ctrl-pgdn` tab switches
  and an external edit's refresh; `zh cli --review uncommitted` (already open) with the git panel
  showing and `zh cli --review commit:HEAD` with the dock closed show the panel, `j` moves. Logs:
  only the usual noise.

**UX-1 slice A (the review's overview scrolls with the diff)** — the user's report: "when i scroll,
the PR description stays pinned to the top. that is wrong. it should be part of the scrollable
body, as it is in tuicr. PR description is SEPARATELY SCROLLABLE which is wrong, should be part of
the single scroll for the entire body". `plans/UX-1.md` part A with the user's §7 answers (Q1 `gg`
tuicr-like, Q2 full width in split view, Q3 PR expanded / commits collapsed, never inner-scrolled,
Q4 no automatic scroll after posting). Upstream glue: one `editor` commit with core patches #5–#7
(§3.10), all opt-in; #6 is for slice B and unused so far.

- **One document**: `ReviewTopBlock` (PR header, warnings, description, review summaries, thread
  toggles, load errors; the commit messages; the branch's base line; then the review-level comments
  with their vim editors) is a custom block at `Above(Anchor::Min)` in the right side's editor
  (`ReviewTopBlock::block_properties`, hosted by `ReviewView::sync_top_block_host`, which tells
  `CommentBlocks::set_overview_block`). Core patch #5 sorts it above the first file's header (and
  a folded first file) on both sides. It exists while the overview has something to show
  (`is_empty`: not in an uncommitted review without review-level comments, whose diff starts with
  its first file). Every inner scroll is gone: the description's 8-line cap (fix 2), the commit
  messages' and review comments' 16-line caps, and fix 3's scroll to a review just posted (with
  `PullRequestState::posted_review_url`; Q4, the toast keeps the URL).
- **Split view, full width** (Q2): core patch #7, `Editor::set_full_width_block`, on the right
  side's editor: the split view's header overlay (`SplitBufferHeadersElement`, which already paints
  file headers across both sides) draws and measures the block at the whole width, over the left
  side's spacer for it; the right side skips it. It stays rendered (below the view) while it holds
  the focus, as the editor does with focused blocks, so a review comment being written keeps the
  focus when scrolled away. The block lives on the right side, which survives split toggles, so it
  and its comment editors survive them (and a pane too narrow for two sides) with their text and
  focus. Unified, the editor draws it as any `Sticky` block (the editor's whole width).
- **Decisions** (implementation): the block holds the overview weakly (`AnyWeakView`): the overview
  holds the diff, whose editor holds the block, and a strong handle leaked the tab's entities on
  close (caught by `test_comments_come_back_after_reopening`: open comment text wasn't saved). While
  the diff has no rows (loading, nothing to review, every file hidden) `ReviewDiff` draws a
  placeholder instead of its editor: `ReviewView` then draws the overview above it in one scroll
  container (`review-overview-without-diff`); the block stays hosted meanwhile. A collapsed
  overview takes two rows (blocks are whole rows). Its height is measured on render, so it grows as
  a review comment is typed.
- **Scrolling**: opening a review shows the overview at the top (y = 0, `keep_scroll_at_start`
  keeps it anchored there). `show_file_start` never moves a view at the very top any more (the top
  shows the overview, or the first file's header already: nothing is under a sticky header), so
  threads loading on open don't scroll the description away. `g g` (Q1, tuicr): the first file's
  header at the top, the overview just above it (`show_file_start_at_cursor` treats the overview
  like a file block of the first file); `ctrl-u`/`ctrl-y`/the wheel/the scrollbar reach it. `; c`
  and editing a review comment (its edit button, the panel's row) scroll the overview into view
  (`CommentBlocks::reveal_overview`: all of it when it fits, else its bottom, where the review
  comments are, at the bottom of the view; once more after the next frame, when a new editor's
  height is known; pending until the block exists when `; c` creates it). After leaving a review
  comment the view stays on the overview; the diff's cursor stays where it was, and the next motion
  scrolls back to it. Sticky file headers: none while the view's top is in the overview.
- **Collapsing** (Q3): the existing disclosure / `ToggleHeader` (`; t` since bugs round 2); pull
  requests start expanded, commit messages collapsed; collapsed keeps the header line and the
  warnings.
- Tests: 4 new `code_review` tests (206 total + 1 ignored; stable under `ITERATIONS=5`): the PR
  overview as the first block above the first header, the 21-line description taller than 8 lines,
  scrolled away with the first header sticky, `G g g` → header at the top, collapse by a click and
  expand, a tall overview's bottom shown for `; c`; split view: the left side's spacer of the same
  height first, headers facing, drawn across both sides (red without patch #7), a review comment
  keeping its text and focus through unsplit/split and a pane too narrow and wide again; an
  uncommitted review: no overview, `; c` from the end brings it scrolled into view, an empty one
  takes it away, editing it from the panel's path scrolls it into view; a branch review: `g g r`
  keeps the overview above the folded first file, every file hidden draws it above the
  placeholder. Changed: `test_submit_a_pull_request_review` (the posted summary is in the overview
  instead of the description block scrolling to it). Core: `test_block_above_multibuffer_start`
  (block_map: order, rows, sticky header, folded first buffer, resize, removal),
  `test_block_above_start_leads_both_sides` and `test_full_width_block_in_split_view` (split: the
  left spacer first, after a resplit too; drawn once at full width, measured there, kept while
  focused out of view, unsplit as usual), `test_gutter_row_renderer_hover_and_drag` (patch #6).
  `cargo test -p editor -- block_map split:: gutter display_map element::`: 176 passed. Clippy
  (`-p editor -p code_review`) clean.
- **Live check** (harness, `validation/ux-a/`; sandbox PR A #1, read-only, token via
  `ZH_EXTRA_ENV`, leak grep clean; local fixture `validation/FINAL/fix2/mkfixture.sh` in
  `/tmp/uxa/ws/fx` plus a `feature` branch 6 commits ahead of `main`), all PASS: PR #1 opens split
  with the description at the top across both sides, no inner scrollbar; the wheel (over either
  half) and `ctrl-d`/`ctrl-u` scroll it away with the diff and back, guide.md's header sticky once
  it's gone; the disclosure click collapses it to its header line and expands it; `G g g` →
  guide.md's header at the top; `; c` from the end → the overview in view, insert mode, typed,
  then unsplit and split again (palette) while typing, text and focus kept, saved; the Bots toggle
  shows the bot summary in the same scroll; a local uncommitted review starts with its first header
  (no blank row), `; c` + split toggles while typing as above, every file excluded → the review
  comment above "All 4 changed files are hidden", cleared → back in the diff; a branch review's
  base line with its Change Base button (opens Zed's branch picker), `g g r` folds the first file
  under it; a 12-commit review (`zh cli --review commits:HEAD~12..HEAD`) collapsed, expanded by a
  click, the whole list in the document's scroll. Not live: a pane too narrow for two sides (the
  harness window has the screen's size; GPUI test), a hovered link's destination (none found in
  PR A's description and the visible summaries). Logs: only the usual noise.
- **Notes for slice B** (core patch #6, `crates/editor/src/gutter_row.rs`): set the renderer on the
  right side once and on each new left side (`Editor::set_gutter_row_renderer(Some(Rc::new(
  |row: &GutterRow, window, cx| -> Option<AnyElement>)), cx)`; `Rc`, not `Arc`: it never leaves the
  foreground). It's called in prepaint, outside any editor update (it may read the editor), for
  every visible row with a buffer row (deleted rows and the left side included, soft-wrap
  continuations and block rows not), with `GutterRow { row, info: &RowInfo, hovered, drag }`; the
  element is laid out like breakpoints (`Gutter::prepaint_button`: x = git strip + 2 px, centered
  in the row) and painted with the gutter indicators, under blocks. `hovered` is the row under the
  pointer in the gutter or the text (none over blocks or spacers; the editor notifies when it
  changes; `hovered_gutter_row()` reads it). A drag: the element's left mouse down calls
  `editor.start_gutter_row_drag(row, Box::new(|rows: RangeInclusive<DisplayRow>, window, cx| …),
  cx)` and must `window.prevent_default()` (ButtonLike does; then the gutter's own line selection
  doesn't start); while the button is held every `GutterRow.drag` is the rows from `row` to the
  nearest row under the pointer (the owner paints the range, e.g. with the line cursor's
  highlight), and the release calls the callback (deferred, so it may update the editor) with the
  rows, a plain click included (`row..=row`). Display rows are the side's own; map them through
  `RowInfo`/the side's snapshot.

**UX-1 slice B (comments with the mouse)** — the user's report: "not really any buttons for certain
parts to have proper mouse interactions, no way to leave comment with mouse for example — take
inspiration from github review UI etc. to make this happen properly". `plans/UX-1.md` part B with
the user's §7 answers (Q5 breakpoint/bookmark hover off, Q6 "+" on the hovered row, the cursor row
and the selection's end, Q7 "Cancel"/"Save", Q9 "Reviewed", Q11 drag from the "+": yes). No
upstream edits: core patch #6 from slice A (§3.10) is now used; the keymaps' order changed only.

- **The "+"** (`comments/blocks/gutter.rs`): `gutter::install` sets the editor's gutter row renderer
  on the right side once and on every new left side (`CommentBlocks`' left-side observer). A solid
  accent "+" (GitHub's) shows (since "Gutter buttons don't overlap": between the line numbers and
  the text; it was over the start of the line-number column) on the row under the pointer (gutter or
  text), and on the cursor's row and the selection's last row of the side that has the cursor (the
  split editor's focused side). It's for rows with a buffer row only: no blocks, spacers or folded
  headers. Its target is the row's line: the old line on the left side and on removed rows of a
  unified diff (`RowInfo`'s buffer and row), so the comment gate can say per row whether the review
  takes it.
- **Click / drag**: pressing the "+" focuses its side, moves the line cursor to the row (unless the
  row is in the side's selection of several rows) and starts `start_gutter_row_drag`. While the
  button is held, the renderer sees the drag's rows and (deferred to after the frame) sets the
  side's selection to them as whole rows, so the line cursor paints the range on both sides and
  vim shows VISUAL. The release comments on the rows (`add_line_comment_on(editor, Some(rows))`,
  the same rules as `c` on a selection: a range over removed and added rows takes the added ones
  (since bugs 2's follow-up: spans both sides, see §0), removed rows alone are old lines, across files the last file's rows) and leaves the cursor on
  the last row; a plain click on a "+" inside a selection comments on the whole selection. The
  side is passed explicitly: a side focused directly is the split editor's focused one only on the
  next frame. Shift-click on a "+" is left to the gutter (extends the selection); otherwise the
  mouse down prevents the default and stops propagating, because the excerpts' expand buttons sat
  under the "+" on the first and last rows of excerpts (found by a test: the click expanded the
  excerpt and the editor never saw the mouse up, so the drag never ended; the "+" has been off them
  since "Gutter buttons don't overlap").
- **Ranges the native way**: shift-click on a row's text extends the selection; a drag in the
  gutter (not on the digits: a click on a line number of a multibuffer opens the file there,
  upstream) selects whole lines; either way the "+" at the selection's end (or on a hovered row
  inside it, tooltip "Comment on the Selected Lines") comments on all of it.
- **Gate** (pull request reviews): a refused row gets a muted, disabled "+" whose tooltip is the
  gate's reason (e.g. "GitHub only accepts comments within its diff hunks"); its click does
  nothing (no toast, no line selection). Enabled "+": "Add a Comment `c`", "Drag to comment on
  several lines".
- **Breakpoints/bookmarks**: `set_show_breakpoints(false)`/`set_show_bookmarks(false)` in
  `LineCursor`'s `configure`, which runs for the right side and every new left side.
- **Comment editor footer** (`comments/view.rs`, below the editor while editing): a type menu
  (`PopoverMenu` + `ContextMenu`, built when opened: No Type / Issue / Suggestion / Note / Praise,
  `CommentBlocks::set_type`; the menu gives the focus back to the editor, which stays in its vim
  mode), Delete (saved comments only: the same confirm as `dd`), Cancel (`Leave::Discard`: a new
  comment goes away, an edit gets its old text back), Save (`Leave::Keep`) with its key. Decision:
  the keymaps list `ctrl-s` before `ctrl-enter` for `SaveComment` (the last binding is the one a
  key hint shows), so Save shows `ctrl-enter`. The card's pencil/trash stay, always visible with
  tooltips.
- **File headers** (`reviewed.rs`, the only header addon): a "Comment" button (chat icon, tooltip
  "Comment on File `shift-c`") makes a file comment on that header's file wherever the cursor is
  (`add_file_comment_for`; a folded reviewed file unfolds for it), and the checkbox is labelled
  "Reviewed" (the label toggles it too). Split and unified headers are the same function.
- **Clicking a row's text** still moves the line cursor there (no caret), verified by a test and
  live.
- Tests: 8 new (214 + 1 ignored; the new ones stable under `ITERATIONS=5`): click "+" on a hovered
  row → comment on that line, insert mode, cursor there; on a removed row (unified) → the old line;
  a text click moves the line cursor; drag "+" over 3 rows → live painted rows, a range comment,
  cursor on the last row; an upward drag over a removed row → the added lines; shift-click range
  and the "+" at its end / on a hovered row inside it → the whole range (on the excerpt's last row:
  the excerpt doesn't expand); a "+" outside a selection → its own row; split view: the left
  side's "+" (hover only while the right side has the cursor) → old line, block on the left, then
  the right side's; no breakpoint/bookmark hover button (red without the fix); the footer: Save,
  Cancel on a new comment and on an edit, the type menu (`MENU_ITEM-…`), Delete with the prompt
  (Cancel, then Delete), Save's key is `ctrl-enter`; the header's "Comment" in unified and split
  view, also on a folded header; PR review: a disabled "+" on a refused row with the gate's reason,
  no comment and no toast on click, enabled on GitHub's lines (new and removed). Clippy
  (`-p code_review`) clean.
- **Live check** (harness, `validation/ux-b/`; local fixture `validation/FINAL/fix2/mkfixture.sh`
  in `/tmp/uxb/ws/fx`; sandbox PR A #1 read-only, token via `ZH_EXTRA_ENV`, leak grep clean), all
  PASS: split view: "+" on the cursor row and on a hovered row, its tooltip; click → comment editor
  in insert mode with the footer; the type menu → Suggestion, typing continues; Save; left side:
  hovered removed row's "+", a drag over 2 rows paints both sides live → "Old lines 2–3"; Cancel on
  it removes it; the card's pencil (tooltip "Edit Comment") → footer with Delete; an edit +
  Cancel restores the text; Delete → the confirm → deleted; header "Comment" (tooltip "Comment on
  File shift-c") → file comment; "Reviewed" label → marked and folded; a gutter drag (beside the
  digits) → VISUAL 3 lines, the "+" at its end → "Lines 1–3"; a right-side drag over 3 rows →
  "Lines 2–4"; unified: removed row's "+" → "Old line 2", a drag over a removed row → "Lines 1–3";
  no breakpoint dot on gutter hover; shift-click range, "+" on a hovered row inside it ("Comment
  on the Selected Lines") → "Lines 1–3". PR #1: solid "+" on GitHub's rows, a muted "+" on rows of
  an expanded excerpt outside GitHub's hunks with the tooltip "GitHub only accepts comments within
  its diff hunks", its click does nothing. Logs: only the usual noise.
- **Notes for slice C**: the "+" sits where Zed draws excerpt expand buttons on excerpt edge rows
  (it wins the click there; fixed in "Gutter buttons don't overlap"); a review context menu (Q10)
  can reuse `add_line_comment_on` / `add_file_comment_for` / `set_type`; the toolbar's "Add Review
  Comment" is `AddReviewComment`.

**UX-1 slice C (pane toolbar, review context menu, panel mouse controls, line numbers)** —
`plans/UX-1.md` part B (toolbar, panel) with the user's §7 answers (Q8: a pane toolbar row, the
single-file bar moved into it; Q10: a review context menu replacing Zed's editor menu in review
editors). No upstream edits.

- **Toolbar** (`toolbar.rs`): a `ToolbarItemView` added to every pane by `toolbar::register` (from
  `init`'s `observe_new::<Workspace>`, for the panes there and on `PaneAdded`, deferred so that it
  comes after Zed's own items): `zed.rs` doesn't change. It shows while the pane's active item acts
  as a `ReviewView` (split clones have their own), after the search bar's collapse and
  split/unified buttons: the split/unified toggle the plan lists is that shared
  `DiffStyleControls`, so the review toolbar doesn't repeat it. Left: "x/y reviewed · N comments"
  (drafts with text, not submitted) "· N threads" (pull request reviews: the threads the tab shows)
  and, in single-file mode, "single file i of N: path" (the single-file bar is gone); cut short in
  narrow panes. Right: File/Hunk/Comment ↑↓, "Review Comment", the single-file toggle, Copy Review
  as Markdown, then "Submit Review" (pull requests) or "Finish Review" (local). Tooltips show the
  (vim) keys. Navigation and Review Comment focus the review and dispatch the action (its
  `CodeReview` div handles it); Submit and Copy call `view::submit_review` /
  `copy_review_as_markdown` on the toolbar's own review (the workspace actions act on the active
  item, which needn't be this pane's). It observes the review, which marks, comments, threads and
  single-file mode notify.
- Decision (found live): first at `PrimaryRight` like `ProjectDiffToolbar`, but that side of the
  row never shrinks, so a split clone's narrow pane lost the search bar's buttons and the end of
  "Finish Review". At `PrimaryLeft`, the counts take the remaining width (truncated) and the buttons
  stay whole at the right end.
- **Context menu** (`context_menu.rs`): `Editor::set_custom_context_menu` on the right side and on
  every new left side (`ReviewView`'s left-side observer). Like Zed's menu, a right click outside
  the selection first moves the cursor to the clicked row. The callback runs inside the clicked
  editor's update, so the entries are added by a deferred fill right after (before the menu draws),
  worked out for that side explicitly (`CommentBlocks::comment_at_cursor_of`/`item_at_cursor_of`,
  `line_comment_target`, `refusal`): the split editor takes a side as focused only on the next
  frame. Entries: Add Comment / Comment on Selected Lines and Comment on File (the gate), Edit /
  Delete / Copy Comment on a comment (Edit and Delete disabled on GitHub threads and submitted
  comments; Copy also takes a thread's first comment), Mark Hunk / Mark File as (Not) Reviewed (no
  hunk at the row: disabled), Copy Line / Copy Selected Lines, Copy Review as Markdown, in pull
  request reviews Copy Permalink to Line (right side) and Copy Pull Request URL, then Open File at
  Line, Go to Definition and Find All References (disabled on the old side and in snapshots).
  Disabled entries say why on hover (`documentation_aside`); keys come from the vim keymap.
  Entries acting on a comment, file or hunk call the crate with what the click resolved, the others
  dispatch from the clicked editor. Mark File from the menu toggles like the header checkbox (the
  cursor doesn't move on to the next file, unlike `r`).
- **Panel**: file rows swap their ✓ for a checkbox on hover (`end_slot_on_hover`); its mouse down
  prevents the panel's focus transfer and stops propagation, its click stops the row's click, so it
  marks without selecting the row or moving the focus. The review's own comment rows (not threads,
  not submitted) get pencil/trash buttons on hover: edit (`CommentBlocks::land_and_edit`: the diff
  moves to the comment, unfolding its file, then its editor opens; a review comment opens in the
  overview) and delete (`dd`'s confirm).
- **Line numbers** (`gutter.rs`): a multibuffer opens the file at a clicked line number
  (`mouse_left_down`, no opt-out; `set_delegate_open_excerpts` would only cover the right side: the
  split editor opens the left side's requests itself). The gutter row renderer (core patch #6) now
  draws, on every row with a buffer row, an area over the line-number column (the "+" inside it when
  shown) whose mouse down takes the clicks whose x is on the line numbers (`line_number_column`:
  the editor's last bounds, `EditorSnapshot::gutter_dimensions` and `max_line_number_width`, all
  public): it prevents the default (the editor's handler then returns), moves the line cursor to
  the row and starts a gutter row drag that selects rows as it goes and doesn't comment; shift-click
  extends the selection. Clicks elsewhere in the gutter (beside the digits, the fold area) keep
  Zed's behaviour; blocks and sticky headers draw over the area. It was a fixed 6 rem wide, past the
  gutter into the text, where it let every click through; since "Gutter buttons don't overlap" it
  ends with the gutter (`GutterRow::right_padding`).
- Tests: 8 new (222 + 1 ignored): toolbar: counts follow `r` and comments, a split clone's toolbar
  acts on the clone, hidden for a file editor; each button against its key (`]f [f ]c [c ]t [t`,
  pressed while the panel has the focus), the single-file toggle and its info, Copy, Review
  Comment, Finish → the `--wait` outcome; a pull request review's thread count, Submit opens the
  modal. Menu: entries and disabled reasons on a hunk, a context row, the left side, a comment, a
  reviewed hunk and file; a right click → Add Comment on the clicked row, Copy Comment, Mark File as
  Reviewed, Comment on Selected Lines inside a visual selection; pull request: the gate's reason, a
  thread's disabled Edit/Delete and its Copy, Copy Permalink/Pull Request URL. Panel: the hover
  checkbox marks without moving the selection or the focus, edit and delete buttons. Line numbers
  (red without the fix): a click on either side moves the cursor and opens nothing, a drag selects
  (VISUAL), shift-click extends. Clippy (`-p code_review`) clean.
- **Live check** (harness, `validation/ux-c/`: the whole UX-1 batch; local fixture
  `validation/FINAL/fix2/mkfixture.sh` in `/tmp/uxc/ws/fx`; sandbox PR A #1 read-only and PR C #15,
  token via `ZH_EXTRA_ENV`, leak grep clean), all PASS: a review opens with the panel; the toolbar's
  counts update live (comments, marks, threads, a posted review → "0 comments · 1 thread"); its
  File/Hunk/Comment buttons, the single-file toggle ("4 of 4 src/lib.rs" shown), Copy as Markdown
  (clipboard), Review Comment (full-width editor in the overview), Finish Review (clipboard,
  toast), `; s`; a split clone's narrow panes with both toolbars whole (after the fix above); the
  menu on a context row (hunk entry disabled, its reason on hover), Add Comment → editor → the
  footer's Save, Copy Comment, Edit Comment, Mark Hunk and Mark File as Reviewed, the left side's
  menu (Go to Definition/Find All References disabled); the panel's hover checkbox (marked,
  selection kept), a comment's edit (unfolds the reviewed file, insert mode) and the footer's
  Cancel, delete with the confirm; line numbers: a click (no tab opens), a drag (VISUAL 4 lines), a
  shift-click (7 lines) and the "+" at its end → "Lines 3–9", a left-side click; a "+" drag →
  "Lines 28–30", Cancel; header Comment and Reviewed; keys `r j k [f ]c ]t c y ; s`. PR #1: the
  overview scrolls away with the diff (no pinning, no inner scroll, full width), toolbar "0/8
  reviewed · 0 comments · 4 threads", the menu's Copy Pull Request URL (clipboard) and Copy
  Permalink entry, Add Comment disabled with GitHub's reason on an expanded row, a thread's
  Edit/Delete disabled and its Copy Comment. PR C #15: a comment from the menu, the toolbar's Submit
  Review → the modal → Comment → posted (`gh api`: one COMMENTED review with the line comment on
  `src/util/strings.rs:26`, RIGHT), the draft then shows as its thread; a draft typed Issue with the
  footer's type menu, deleted from the panel. PR C reset afterwards: #15 closed, **#16** opened.
  Logs: only the usual noise.

**Review Pull Request picker (the user's request after UX-1)** — "some kind of top-level UI for
opening a new review", listing pull requests the way `~/nixconf/home/scripts/src/reviewprs.nu` does
(a GitHub search per readability team, without the user's own, drafts or dependabot, minus the ones
the user approved), single selection, the filter a user setting that can be edited while the UI is
open. Upstream edits: the settings block only (§3.10).

- **Settings** (the first `code_review` block, `CodeReviewSettings` in `code_review.rs`):
  `pull_request_search` (default `"is:open review-requested:@me"`) and
  `hide_approved_pull_requests` (default `true`).
- **Picker** (`pull_request_picker.rs`, action `code_review::SearchPullRequests`): the query box is
  the GitHub search, prefilled from the setting; each edit searches again after 300 ms. Old results
  stay until new ones arrive (Enter opens what is shown); the footer says "Searching…", how many
  approved pull requests are hidden, or the repository. A query that parses as a pull request
  reference (`#123`, `owner/repo#123`, URL) shows "Open pull request …" and doesn't search. Enter
  opens in place, the secondary confirm in a new worktree (the Start Review picker's flows).
- **Search** (`GitHubClient::search_pull_requests`, `Forge::search_pull_requests`): `repo:{canonical}
  is:pr ({search})` plus `sort:updated-desc` unless the search has a `sort:` term, sent as
  `SearchPullRequests` with `type: ISSUE_ADVANCED`: verified live (2026-10-24) that only advanced
  search understands `OR`/parentheses (`ISSUE` returns 0 for `(a OR b)`), so one setting covers the
  user's three teams; the parentheses keep a top-level `OR` inside the repository. A malformed
  search returns 0 results, not an error. The old review-requested list still uses `ISSUE`. Not
  checked: GitHub Enterprise Server versions without `ISSUE_ADVANCED`.
- **Approved by you**: the summary fragment fetches `latestOpinionatedReviews(first: 100)` and
  `PullRequestSummary::viewer_review` is the viewer's (`RepositoryContext::viewer`) Approved /
  Changes requested. Not `viewerLatestReview`: a comment after an approval would replace it. Both
  pickers' rows show it as a chip (`picker::render_pull_request_row`, now shared).
- **Entry points**: the review panel's header has a "+" (New Review: Start Review…, Review Pull
  Request…), and its empty state has Review Pull Request… and Start Review… under Review Uncommitted
  Changes. No default key binding. `OpenPullRequest` without a reference still opens the Start Review
  picker at its pull requests.
- The user's setting for `sheerhealth/sheer` (one string, global, not per repository):
  `is:open -author:@me draft:false -author:app/dependabot (team-review-requested:SheerHealth/go-readability
  OR team-review-requested:SheerHealth/api-readability OR team-review-requested:SheerHealth/build-readability)`
  (17 results live, against the script's three searches of 12, 11 and 1).
- Tests: 2 new (224 + 1 ignored): `github::test_search_pull_requests` (the query built around the
  search, `sort:` kept, an empty search, `viewer_review` from another user's and the viewer's
  reviews) and `pull_request_picker::test_review_pull_request_picker` (prefilled from the setting,
  approved hidden with the footer count, debounced re-search keeping the old rows, a status row
  doesn't confirm, `#1` opens without searching). Clippy (`-p code_review`) clean.
- Live check (harness, sandbox clone, `GH_TOKEN` via `ZH_EXTRA_ENV`, setting `"is:open"`): the
  palette action opens the picker prefilled, PR #4 shows its "Changes requested" chip, typing `
  (author:drew-council OR label:nonexistent)` narrows to #16 and #1, the panel's empty-state button
  and the header's "+" menu open it; confirming starts the usual checkout (the harness's git fetch
  asked for credentials and was cancelled).

**Bugs 2, item 1 (comments stay in the diff's hunks)** — every line or range comment, from any
path, must have both ends in the diff's hunks, the start not after the end (`forge::check_lines`,
checked by `CommentBlocks::check`; the "+" on a selection checks the whole selection).
**Decisions**: what GitHub's API takes is allowed (the user's rule), so a range may span hunks (LV
L8), in local reviews too; local reviews use the hunks `git diff` would show (3 context lines,
`Hunk::with_context`), like tuicr, so expanded context rows are refused there. Refusals are toasts
or disabled entries with the reason.

**Bugs 2, item 1 follow-up (ranges from one side to the other)** — GitHub takes a range from
either side to either side, in diff order (LV MX), so a unified selection from removed rows to
added ones, or back, is such a range: `CommentTarget::Lines.start_side` (stored in the comment's
JSON only when it differs, so no migration), posted as `start_side`/`side`, shown below its last
row (that row's side when split), labelled "Old line 6 – line 8" / `path:~6-8`. An unchanged end
takes the other end's side (new when both are unchanged), so a selection from unchanged rows to
removed ones is a range of old lines (it used to keep only the unchanged rows). Such a comment is
anchored at both ends (`TabAnchors::starts`), its text being its first and last lines'; threads
with mixed sides are placed the same way (a renamed file's still at their last line). Visual `y`
over both sides still copies the new side's rows.

**Bugs 2, item 2 (collapsing the overview in split view)** — the split view measured the
full-width overview after both sides were prepainted, and a notification during a draw schedules no
frame, so the rows only moved at the next input event (an empty band after a collapse, the overview
over the diff after an expand; also for typing in a review comment, the Bots toggle). Core patch #7
now lays the block out in `SplitBufferHeadersElement::request_layout`, ahead of both sides, when it's
in view (at the last frame's width), and otherwise asks for another frame when its height changes.

**Bugs 2, item 4 (wrapped list items in commit messages)** — hard-wrapping a list item whose first
word is too long (a URL) leaves its marker alone on a line, which CommonMark (and GitHub) read as an
empty item plus a paragraph outside the list. **Decision**: commit message bodies get such a
marker's next line back (`top_block::join_bare_list_markers`, driven by pulldown-cmark's events, so
quotes, code, headings and real items are untouched); pull request descriptions are left as GitHub
renders them.

**Bugs 2, item 5 (drafts left out of the pull requests)** — the query built around
`code_review.pull_request_search` (`github::pull_request_search_query`) adds `draft:false` unless the
search names drafts (`is:draft`, `draft:…`, negated too), as it adds `is:open` unless it names a
state, so every search leaves drafts out and searches that already say `draft:false` (the user's
own) are unchanged. A stack is still listed when one of its other pull requests matches, and shows
its drafts. **Decision**: no setting; drafts show when the search names them (`ctrl-r`:
`is:draft`, or `(is:draft OR draft:false)` for all). The sandbox's empty list with the default
search is intended: nothing there requests the viewer's review (`review-requested:@me`).

**Bugs 2, item 7 (the panel's tree looks like the project panel's)** — **Decision**: the review
panel's tree (and its comment rows) reads the `project_panel` settings (`indent_size`,
`entry_spacing`, `file_icons`, `folder_indicator`, `bold_folder_labels`, `indent_guides.show`; new
dependency on `project_panel` for `ProjectPanelSettings`) and the project panel's sizes (default
label and icon size, `h_6` names, its indent guide offsets); a long name is now cut off instead of
pushing the status letter out of the panel.

**Stacked pull requests (the user's request after the picker)** — "the code review panel … has no
notion of what PRs are together in a GitHub stack … display in a separate section all PRs that
are associated with the stack, where you can select a single entry to review that GitHub stack …
step through with … next and previous"; individual pull requests outside stacks stay reviewable;
drafts, merged, closed and open parts of a stack are shown and handled; a stack is listed when any
of its pull requests matches the search, and opening it skips ahead to the first one open for
review. No upstream edits.

- **GitHub's API** (verified live 2026-10-26 on `sheerhealth/sheer`): GraphQL `PullRequest.stack`
  (`PullRequestStack`: `number`, `baseRefName`, `size`, `entries { position pullRequest }`,
  positions 1-based from the bottom; a `Node`, so `nodes(ids:)` takes its id) and
  `PullRequest.stackEntry`; REST `repos/{owner}/{repo}/stacks[?pull_request=N]` (gh-stack 0.2.1
  uses it). Merged and closed pull requests stay in their stack; after the bottom merges, the next
  one is retargeted to the base. A stack's number shares the issue/pull request numbering.
- **Queries** (`github/stacks.graphql`, sent as `queries.graphql` + it, `STACK_QUERIES`): GitHub
  validates every operation of a document (checked live: an unknown field in an unused operation
  fails the request), so stack fields in `queries.graphql` would break every request on a GitHub
  without stacks (older GHES). The summary fragment gained only `id`. The picker sends
  `PullRequestStackIds` (the found pull requests' stacks by node id) then `PullRequestStacks` (each
  distinct stack, `entries(first: 50)` with the full summary fragment: 10 stacks with up to 22
  entries cost nothing notable live); a review sends `PullRequestStackOf`. Stack failures are logged
  only: the picker lists everything alone, the review has no stack.
- **Picker**: `group_search_results`: approved-by-you hidden first (counted as before); a pull
  request in a stack goes to its stack's row (listed once, in the order of its first pull request
  found), the others to Pull Requests. Sections: Local, **Stacks**, Pull Requests, Recent Commits.
  A stack row: the review start's number, title, author and time; below, the stack gh-stack's way
  from its base (`main ← #1 ← #2`), each number colored by state (Open green, Draft muted, Merged
  accent, Closed red and struck through), the start bold and underlined; chips "N of M", the
  start's state, review decision and the viewer's verdict. Its fuzzy candidate has every member's
  number and title. Enter reviews the start in place, the secondary confirm in a worktree; opening
  the picker at its pull requests selects the first stack. A `#N` of a stacked pull request gets
  the "Open pull request #N" row (the user named that one).
- **Review start** (`PullRequestStack::review_start`): the lowest pull request ready for review
  (open, not a draft) the viewer hasn't approved; else the lowest ready for review; else the lowest
  open (a draft); else the bottom.
- **In a review** (`PullRequestState::stack`, loaded with threads/summaries/files on every load):
  the top block's header is followed by "Stack #N · 2 of 9: main ← #1 ← #2 …" (always shown, also
  collapsed; each number a button with the title and state in its tooltip; a click reviews it,
  merged and closed ones too); the toolbar has "Stack 2/9" with ← / → (disabled with "none open"
  at the ends); the panel header the same line, its menu Previous/Next Pull Request in Stack.
  `code_review::NextPullRequestInStack` (up, away from the base) / `PreviousPullRequestInStack`
  (down), `] s` / `[ s` in vim mode. **Stepping skips merged and closed pull requests** (drafts
  are stepped onto). Without a neighbor, or before the stack loaded, a toast says why.
- **Stepping replaces the tab** (`open_in_stack`, agent's choice): the neighbor is opened in place
  in the review's repository (`open_pull_request` with the canonical repository as the
  reference), and once its tab is open the previous tab closes (`SaveIntent::Skip`; its drafts
  stay in its session), so a stack is reviewed in one tab. A failed open (checkout refused) keeps
  the old tab and shows the error toast.
- Tests: 5 new (232 + 1 ignored): `forge::test_stack_review_start_and_neighbors`,
  `github::test_pull_request_stacks` (the two-step lookup, ordering by position, no request without
  ids, `QUERIES` free of stack fields), `picker::test_group_search_results`,
  `picker::test_stacks_section` (sections, start, fuzzy match on another member, Enter opens the
  start), `pull_request::test_stack_navigation` (position, top block/toolbar/panel drawn, toast at
  the bottom, → skips a closed one and replaces the tab, `[ s`, the toolbar button). Not done: a
  live run in the app.

**Upstream vim in the review tab (the user's bug findings, items 3 and 5)** — "general lag when
scrolling around with j or k … you should remove any customization and mostly stick to the normal
editor controls (keeping things read-only) instead of customizing to have this cross-bar
highlight"; "ctrl+u seems to break if scrolling up to the top of the diff … you should use the
upstream CORRECT implementation or no implementation at all"; then "set the buffer to read-only and
otherwise, just use upstream vim binding stuff … i want the full vim motions so i can go over a
symbol and do `gd`". Split view stays (upstream's works with a real cursor).

- **Removed**: `line_cursor.rs` (hidden caret, the full-width row highlight repainted on both sides
  on every cursor move, `h`/`l` horizontal scroll and `h` at the left edge focusing the panel),
  `code_review::ScrollLeft`/`ScrollRight`, the keymap block that nulled vim's character and word
  motions and made `v`/`ctrl-v` line-wise, the review's soft-wrap override (`SoftWrap::None`, for
  `h`/`l`), and `show_file_start_at_cursor`: on every scroll that followed the cursor it scrolled
  the cursor's file header back to the top when the cursor was on a file's first row (tuicr's
  `gg`), a second scroll fighting vim's. Core patches #2 and #3 (only the line cursor used them)
  are reverted: the editor's scroll files and `RowHighlightOptions` are upstream's again.
- **Keys given back to vim** (they shadowed motions or operators): `e` (Open File at Cursor → Zed's
  own `g space`, which the review captures for snapshots to open the working tree's file), `{`/`}`
  (files: `[f`/`]f` stay), `m`/`M` (comments: `[t`/`]t` stay), `y`/`Y` (copy review / comment →
  `; y` / `; Y`), visual `y` (Copy Lines: menu and palette), visual `i`/`a` (text objects).
- **Kept**: insert/change/replace keys as review actions or no-ops (read-only diff), the review's
  `]`/`[`/`;` sequences, `]c`/`[c` as the review's hunk navigation (Zed's `GoToHunk` doesn't skip
  folded files and gets stuck on a reviewed file's header), `face_cursor` for a new left side,
  `keep_scroll_at_start` (block heights at the very top; not a cursor follow).
- **`ctrl-u` loop**: not reproduced in GPUI tests, neither before nor after (the old code passes the
  new test too), so the cause isn't pinned down. All the review's own cursor and scroll code is gone; what
  scrolls is upstream's. **Wrong: it was still broken**, upstream's own — see "`ctrl-u`/`ctrl-d`
  over blocks".
- Tests: `line_cursor.rs`'s tests removed with it; tests on `e`, `{`/`}`, `m`/`M`, `y`/`Y`, visual
  `y`, the cursor highlight and `gg`'s header-at-top updated or dropped; new
  `pull_request::test_half_page_scrolls_reach_both_ends` (`ctrl-u`/`ctrl-d` from end to end, view
  and cursor moving one way, overview expanded and collapsed; `j` moves one row) and `yiw` through
  vim in `test_copy_review_comment_and_lines`. 225 pass, 1 ignored. Not done: a live run.

**Gutter buttons don't overlap (the user's bug findings, item 5)** — ""expand excerpt" and +
buttons overlap, should not happen. both are frequently used."

- **Cause**: upstream draws an excerpt's expand button (`layout_expand_toggles`: `ExpandUp` on an
  excerpt's first row, `ExpandDown` on its last, `ExpandVertical` on a one-row excerpt; both sides
  of a split diff, the left side's delegated) in the gutter's left padding, from the git strip on,
  as wide as that padding (an em more when the row's line number is shorter than the widest).
  `Gutter::prepaint_button`, which core patch #6 uses for the review's row elements, draws at the
  git strip + 2 px, so the "+" (15.5 px square in the tests) sat on the expand arrow of an edge row
  whenever it showed there: on the hovered row, the cursor row and the selection's end. Hovering an
  expand button always hovers its row, so the "+" covered it exactly when the user went for it, and
  the "+"'s mouse down (`stop_propagation`) took the click: with the mouse, an excerpt couldn't be
  expanded.
- **Decision**: the "+" moved to the gutter's right padding, between the line numbers and the text,
  on every row: GitHub's own place for it (at the code's edge), the same place on every row, and
  nothing else draws there in a multibuffer (fold toggles are for singletons only; the review's
  sides have 3 ch of it). The expand buttons keep upstream's place and size. Core patch #6 got
  `GutterRow::right_padding` (where that padding is, from the element's origin), which the review's
  row element (`render_row`) uses: it now reaches to the gutter's end (was a fixed 6 rem) and
  centers the "+" in the padding. The line-number area and its clicks (UX-1 slice C) are unchanged.
  Alternatives: no "+" on edge rows (upstream's own rule for its breakpoint hover button and run
  indicators, `Gutter::layout_item`; but the user comments on those rows with the mouse too); the
  "+" shifted right only on edge rows (a target that moves between rows; and a short line number's
  expand button reaches an em into the digits' column); the "+" on top with the expand button
  reachable around it (too small to hit). Not covered: with line numbers turned off, a multibuffer's
  right padding is empty, and the "+" straddles the gutter's end.
- Tests: `test_gutter_button_beside_expand_buttons` (unified: the cursor's "+" on an excerpt's first
  row and the hovered "+" on its last row don't intersect their `ICON-ExpandUp` / `ICON-ExpandDown`
  and sit between the line numbers and the text; with the "+" showing, a click on each expand button
  expands the excerpt and opens no comment; the "+" of an excerpt's last row comments on it and
  doesn't expand it) and `test_gutter_buttons_beside_expand_buttons_in_split_view` (the right side's
  cursor "+" and the left side's hovered one on a first row, the left side's cursor "+" and the
  right side's hovered one on a last row; the left side's expand buttons are found at the right
  side's place in its own gutter, and clicking them expands both sides); both red before the fix,
  stable under `ITERATIONS=5`. `test_no_breakpoint_button_in_the_gutter` now finds the line number
  to hover with `line_number_column` (it went from the "+"'s old place). Editor:
  `test_gutter_row_renderer_hover_and_drag` checks `right_padding` against the gutter's dimensions.
  227 pass, 1 ignored; editor `gutter` tests 9 pass; clippy (`-p code_review -p editor`) clean. Not
  done: a live run.

**GitHub references link (the user's bug findings, item 4)** — "issue links not rendered, stay as
plain numbers. need to have a link so i can view them." (and "at some point, should ideally render
into dropdown so i can view inline": not built, see "Known issues").

- **Where**: all GitHub-authored markdown the review renders: a pull request's description and
  review summaries (top block) and every comment of its review threads (`RemoteThreadView`), all
  through `github_markdown`, which now takes the pull request's repository
  (`forge().context().canonical`; threads get it on `RemoteThreadContent::repo`). Not linked: the
  review's own comments and the submit modal's body preview (the user's text; GitHub links it once
  posted), and commit messages (only commit reviews show them, which have no pull request, so no
  repository to resolve `#123` against).
- **How** (`link_references` in `comments/remote.rs`, no upstream edit: the markdown crate has no
  hook for custom autolinks): the source is rewritten before it is parsed, `#123` →
  `[#123](https://HOST/OWNER/REPO/issues/123)` (an issue URL reaches a pull request too: GitHub
  redirects; HOST is the pull request's, so Enterprise works). It walks pulldown-cmark's events with
  the markdown crate's own `PARSE_OPTIONS`, so code spans, code blocks, links, images, autolinks,
  HTML and metadata blocks are skipped exactly as the renderer sees them; adjacent text events are
  joined into runs, and text that isn't verbatim source (escapes like `\#1`, entities, smart
  quotes) is left alone. Bare URLs already link (the markdown crate runs linkify over text), so
  linkify's URLs in a run are skipped too (`…/page#1`). Clicks go through `untrusted_markdown` →
  `open_web_link` (only http(s) opens), and hovering shows the destination, as for other links.
- **Rules** (GitHub's autolinks): `#N`, `owner/repo#N` (owner `[A-Za-z0-9-]`, not starting with
  `-`; repo `[A-Za-z0-9._-]`) and `GH-N` (upper case only); N doesn't start with 0 and fits a u64.
  A reference starts a line or follows whitespace, `(`, `[`, `{`, `"` or `'`, possibly through
  emphasis delimiters (`**#1**`, `_#1_`, `~~#1~~`), so not `abc#1`, `a_#1`, `x-#1` or `a/b/c#1`; and
  no letter or digit follows it (again through delimiters: `#1abc`, `#1_x` don't link).
- **Decisions**: @mentions and commit SHAs aren't linked: GitHub links them only when the user or
  commit exists, which isn't known offline, and look-alikes are common (`@Override`, `@here`, hex
  words like `deadbeef`). Issue references have the same limit (GitHub only links existing issues,
  so `step #1` links here and not on GitHub), but they are what was asked for. Alternatives: linking
  inside the markdown crate's parser (upstream edit, rebuilds `markdown`'s dependents); a regex over
  the raw source (would link inside code spans and links). New dependencies: `pulldown-cmark` and
  `linkify` (workspace dependencies the markdown crate already uses).
- Tests: `test_github_markdown_links_references` (line starts, lists, quotes, headings, tables,
  brackets, quotes and emphasis around references, cross-repo with `.`/`-`/`_`, `GH-`, an
  Enterprise host, a URL and a reference in one text, a `<summary>`'s references; untouched: inside
  words and paths, `-o/r#1`, trailing letters, URLs with fragments and autolinks, `#0`/`#01`/`#`/
  overflow, code spans, fenced and indented code, inline and reference links, images, escapes,
  entities, inline and block HTML); `pull_request::tests::review::test_references_link_to_github`
  (PR A with `GH-7`/`other/repo#8`/`` `#9` `` in the description, `#3` in a summary and `#5` in a
  thread: each source links to the right URL, and a click on the description's first link opens
  `…/drew-council/code-review-sandbox/issues/7`; red without the linking). 229 pass, 1 ignored;
  clippy clean. Not done: a live run.

**Long lines wrap (the user's bug findings, item 1)** — "markdown is rendered with line wrapping
for me, but diff shown doesn't wrap lines (generally i'd guess). leads to cases where the entire
line is off-screen and i can't actually see anything (no horizontal scroll either...)".

- **Cause**: the review forced `SoftWrap::None` (tuicr's `wrap = false`, for the line cursor's
  `h`/`l`); "Upstream vim in the review tab" removed that override, which left wrapping to the
  language settings, and Zed's default (`"soft_wrap": "none"`) doesn't wrap either. "No horizontal
  scroll" was the line cursor's doing too: core patch #2 forbade horizontal autoscroll on both
  sides and the caret was hidden, so no cursor motion scrolled sideways, only the review's own
  `h`/`l` (4 columns at a time). Both went with the line cursor. Nothing the review adds blocks
  horizontal scrolling now: its blocks use `block_mouse_except_scroll`, the full-width overview
  (core patch #7) adds no hitbox, the review sets no scrollbar settings, and `keep_scroll_at_start`
  and the overview's scrolls keep the x offset.
- **Decision** (the orchestrator's, for the user): the diff wraps at the editor's width by default
  on both sides, whatever the language settings say (the first file's would decide for the whole
  multibuffer): `ReviewDiff::new` sets the right side's `set_soft_wrap_mode(EditorWidth)`, and
  upstream's `SplittableEditor::split` copies the right side's override to every new left side
  (also the automatic ones when the pane gets wide enough). `ToggleSoftWrap` (the review's capture,
  unchanged) turns it off and on for both sides; a left side created while it's off doesn't wrap
  either. Comment editors wrap on their own, as before.
- **Upstream behaviour, not patched**: with wrapping off, a horizontal wheel/trackpad delta, the
  horizontal scrollbar (`scrollbar.axes.horizontal`; seen live in the final validation, V1.O5) and
  cursor motions (`$`, `l`, `w`, …) scroll sideways. In
  split view the sides share the x offset, but each side's view stops at its own widest line, so a
  side with only short lines stays at x = 0 and a wheel over it doesn't scroll (its maximum is 0);
  `0` after `$` leaves the gutter's margin (half a column) scrolled. With wrapping on, the split's
  block map balances wrapped rows too (spacers from the other side's wrap snapshot), so a line
  wrapped into more rows on one side faces a spacer and the rows after it stay side by side. Line
  numbers and the gutter's "+" show on a wrapped line's first row only (continuation rows have no
  buffer row).
- Tests (`view::tests::soft_wrap`, with `"soft_wrap": "none"` set explicitly):
  `test_long_lines_wrap_on_both_sides` (split: both sides wrap a 300-column line; the toggle turns
  both off; a left side recreated by toggling the split follows (off, then on after the toggle);
  too narrow for two sides and wide again, the new left side wraps),
  `test_wrapped_lines_stay_aligned_in_split_view` (a 900-column new line against a 300-column old
  one: the wrapped lines start on the same display row, the new one takes more rows, the next
  file's facing rows and the last display rows match), and
  `test_long_lines_scroll_horizontally_without_wrapping` (unified) / `…_in_split_view` (wrapping
  off: a trackpad scroll over the long line moves the view, on both sides together and from
  either side; scrolled back, `$` scrolls to the line's end on both sides and `0` back).
  `test_typing_in_a_comment_isnt_taken_by_review_keys` now expects the diff to wrap at first. All
  four new tests are red without the fix. 233 pass, 1 ignored; clippy clean. Not done: a live run.

**Faster, bigger Start Review picker (the user's request after the bug findings)** — "loading pull
requests takes a VERY long time. is there not a better way?"; "the view for the pulls selection is
kinda small on screen … i care about width AND height being FAR too small".

- **Cause** (measured live 2026-10-26 on `sheerhealth/sheer`, `review-requested:@me`, 34 hits, 8
  stacks with 63 entries): `reviewDecision`. GitHub works it out per pull request from the base
  branch's protection rules: the search took ~3 s with it and ~0.75 s without; `PullRequestStacks`
  ~4–6 s with it and ~0.65 s without (no other field of the summary costs anything notable:
  `latestOpinionatedReviews(first: 100)`, author, refs, `updatedAt` together ~0.5 s). The three
  requests run one after the other, so the picker showed nothing for ~9–10 s.
- **Fix**: `reviewDecision` left `PullRequestSummary` (searches and stacks come without it,
  `review_decision: None`); a new `PullRequestReviewDecisions(ids)` (`nodes(ids:)`, in
  `queries.graphql`: every GitHub has it) behind `Forge::review_decisions` fetches it for every
  pull request listed (search hits and stack members) once the rows show, and
  `SearchResults::set_review_decisions` fills the chips in. A failure is logged only (the rows
  stay, without decision chips). A new search drops the pending request with the load task.
  Measured live with `gh`: rows after ~2.7 s (search 1.2, stack ids 0.6, stacks 1.0), decision
  chips ~5 s later. The review's own `PullRequestDetails` still asks for `reviewDecision` (one pull
  request).
- **Size**: the modal's wrapper had a fixed `w(rems(34.))`, which also kept Zed's picker resizing
  from widening it. It's gone; the picker opens at 70% of the window's width (at least Zed's 34
  rem) and up to 80% of its height (a plain picker shrinks to fit fewer rows), and its edges drag
  as any Zed picker's, the size remembered per picker by Zed (`picker::persistence`). Not a
  setting: Zed has no general picker size setting (only the file finder's `modal_max_width`).
  A plain picker's opening width is also its minimum.
- Tests: `github::test_review_decisions` (parsing, empty ids skipped, no request without ids; the
  summary fragment has no `reviewDecision`), `picker::test_stacks_section` extended (the
  decisions reach the rows and stack members; the request order), `picker::
  test_picker_takes_most_of_the_window`; `github::test_pull_request_summaries` now expects no
  decisions from a search. 235 pass, 1 ignored. Not done: a live run.
- Possible next step (not done): ask for the search hits' decisions while the stacks load (chips
  ~3 s sooner). Keeping the last results per search: done, see "Instant reopen".

**`ctrl-u`/`ctrl-d` over blocks (the user's report after the picker)** — "ctrl+u is STILL BROKEN! …
it still has cases where it starts to just switch between two adjacent lines or move DOWN LINE BY
LINE for NO REASON! it should keep going up until it reaches the top of the diff!" The earlier
"Upstream vim in the review tab" left it unreproduced and claimed nothing about the cause; it was
upstream's, in Zed's editor and vim, and shows wherever tall blocks are (the review is full of
them: the overview, file headers, comments and threads).

- **Reproduced** with a probe over the PR A fixture plus two 8-line comments, 4 window sizes, every
  5th row as the start, `ctrl-u` and `ctrl-d` with a frame drawn after each key: the first `ctrl-u`
  from the diff's first row moved the cursor 3 rows *down*; after the first fix, `ctrl-d` left the
  cursor behind (stuck on a row while the view scrolled on) in 33 of the runs.
- **Cause 1** (`ctrl-u` down): Zed's scroll position is an anchor (a buffer row) plus an offset; a view
  whose top is inside the blocks above a row keeps that row's anchor with a negative offset.
  `scroll_top_display_point` returned the anchor's row and dropped the offset, so vim saw the top
  as the row below the blocks, unchanged by the scroll (`old_top == top`), and its scroll margin
  (`top + 3`) pulled the cursor down. Every user of it took it for the first visible row (vim's
  `ctrl-u/d/f/b`, `H`/`M`/`L`, the editor's own scroll-and-clamp, `newest_selection_on_screen`).
  Fix: the real top row, `floor(scroll_position.y)`.
- **Cause 2** (`ctrl-d` left the cursor behind): vim moves the cursor by display rows; a target inside
  a block was clipped with `Bias::Left`, to the row above the block, which after a scroll down is
  above the view: off screen, so the next `ctrl-d` didn't move it at all
  (`newest_selection_on_screen`). Fix: leave a block the way the view scrolled, the other way only
  when that stays on screen and still goes with the scroll.
- Both in core patch #8 (§3.10); 237 pass, 1 ignored. Tests: `pull_request::test_half_page_scrolls_go_one_way` and
  `…_in_split_view` (comment blocks, 800×400 and 1600×1000, every 9th row as the start, `ctrl-u` and
  `ctrl-d` until they stop: the view and the cursor move one way only and the cursor ends on the
  first or last row); both fail without the fix (`ctrl-u` from row 0: row 0 → row 3). vim's 577
  tests and editor's scroll/split tests pass. Not done: a live run.

**Cursor movement speed (the user's report after the picker)** — "navigating around with vim
bindings is STILL EXTREMELY LAGGY! … it should run at the same speed as the normal zed editor
navigation"; "it will just skip several lines to try and catch up, when normal editor actually does
proper key-repeat handling". "Upstream vim in the review tab" removed the line cursor but measured
nothing; this does.

- **Harness** (a timing test, removed after use): the PR A fixture with `src/engine.rs` grown to
  20,269 lines and a hunk every 40 lines, 1600×1000, the cursor in that file, `j`/`k` each followed
  by a drawn frame; against Zed's own diff view (a bare `SplittableEditor` over the review's
  multibuffer, wrapping like it) and a plain editor on the file. A `refresh()`-ing draw invalidates
  GPUI's view caches and overstates everything; the numbers are without it.
- **Release-fast** (per key + frame): review 1.8 ms (small) / 3.2 ms (20k lines); Zed's diff view
  1.2 / 2.5 ms; plain editor 1.4 / 1.6 ms. All far under a frame (8 ms at 120 Hz).
- **Debug** (the user ran `target/debug/zed`): review ~43 ms, Zed's diff view ~33 ms, plain editor
  ~23 ms. macOS key repeat comes every ~30 ms or faster, so a debug build's diff views (Zed's own
  too) fall behind and skip lines; a plain editor just keeps up. Use a `release-fast` build to judge
  speed.
- **The review's own share** over Zed's diff view (debug profiles, `sample`): sticky file header
  controls ~1.1 ms, hunk controls (`hunk_key_at`) ~1.1 ms, the gutter "+" layer ~0.8 ms, the
  toolbar ~0.2 ms; the rest spread thin. Fixed here: the "+" took a multibuffer snapshot for every
  visible row of every frame (now a cached newest selection per side, cleared on buffer events);
  the comment gate hashed a file's whole base text on every frame (now cached per base text
  version, `LocalChanges::base_text_hashes`); the comment at the cursor took a snapshot per hosted
  block on every cursor move (now one per side). Zed's diff view itself costs ~10 ms (debug) over a
  plain editor (the hunk gutter's `diff_hunks_in_range`, anchors over a long file): upstream's.
- Not done: a live check by the user in a release-fast build.

**Stepping shows at once (the user's report after the picker)** — "there is also a delay when
pressing \"] s\". there should be no delay, UI updates immediately and shows loading state if things
are loading"; "the loading state should show immediately, and the elements should load
independently and show when ready … decouple properly and keep things simple".

- **Cause**: `open_in_stack` showed nothing until `open_pull_request` had fetched the details and
  run the checkout (the network and git), then swapped the tabs.
- **Now**: `] s`, `[ s`, the toolbar's arrows and the top block's stack all go through
  `open_in_stack`, which first adds an `OpeningReview` tab after the review and focuses it: the
  stack's summary already has the number, title, author, state, draft and branches, so it shows
  them at once with "Checking out <head>…". `open_pull_request` is unchanged; the tab only reflects
  its outcome: opened, the new review replaces both tabs (the review's and the opening one); failed,
  the opening tab shows the error (and the usual toast) while the old review stays open behind it,
  so closing it goes back. The new review's parts already load independently once it exists (the
  diff, the threads, summaries, files and stack each as they arrive). The review tab itself can't
  appear earlier: its diff needs the checkout and the details' base commit.
- Tests: `test_stack_navigation` extended: the opening tab is the active item right after the
  action, before any request is answered, with its status drawn; #4's details failing shows the
  error, keeps #1's review, and closing the tab goes back to it; on success one tab is left. 237
  pass, 1 ignored. Not done: a live run.

**Instant reopen (the user's request after "Faster, bigger Start Review picker")** — "the
\"instant re-open\" sounds like a good idea, as long as you indicate refreshing status somehow".

- `SearchResultsCache` (a global): the last `SearchResults` and hidden-approved count of each search,
  by repository entity, search text and whether approved pull requests are hidden; written when a
  search lands and again when its review decisions do. `load_pull_requests` (not while typing a
  search, which keeps the shown results already) starts from the cached results when there are
  some, else "Loading pull requests…"; the picker refreshes its rows right after opening so they
  show in the first frame.
- While a search runs over shown results (`searching` with `PullRequestList::Loaded`), the first
  pull request header (Stacks, else Pull Requests) has a "Refreshing" `LoadingLabel` at its end
  (`start-review-refreshing`). The fresh results replace the cached ones; a failure replaces them
  with the error, as before.
- Kept for Zed's session only (memory), not across restarts.
- Test: `picker::test_reopening_shows_the_last_results_while_refreshing` (the reopened picker shows
  the stack and the pull request before GitHub answers, with "Refreshing"; the second search
  lands, the indicator goes). 238 pass, 1 ignored. Not done: a live run.

**The overview is one block (the user's report)** — a screenshot of a pull request review with the
overview (header, stack, description, toggles) repeated eight times down the view, nothing else
in sight; "6/7 reviewed".

- **Cause** (Zed's block map with core patch #5): reviewed files fold to a `FoldedBuffer` block,
  which replaces the file's rows. An edit landing within it (any re-sync of those rows) is
  extended to the start of the replaced rows, but only after the transforms ahead of the edit's
  original start were preserved, the overview among them (it takes no input rows and sits right
  above the first file); the extended edit then rebuilt the overview as well: one more copy per
  such edit. A folded first file is the common case after `r`. Copies are blocks the editor lays
  out every frame, which also made everything slower.
- **Fix**: the extension happens before anything is preserved (§3.10, core patch #5).
- Tests: `block_map::test_block_above_multibuffer_start` (an edit within a folded first buffer:
  one start block; it failed with two); `pull_request::test_overview_stays_one_block` (unified and
  split: resizes, `G`/`gg`/`ctrl-d`/`ctrl-u`, reload, wrap and split toggles, reviewing the first
  files; split failed at once with a folded first file). The editor's block map tests (20, with
  the randomized ones) pass.

**Wrapping follows the settings (the user's report)** — "CONTROL AND U ARE STILL … BROKEN!!!! now
it moves by ONE CHARACTER instead of one line! … IT WORKS PERFECTLY IN ANY OTHER ZED BUFFER!"

- **Cause**: "Long lines wrap" forced `SoftWrap::EditorWidth` on the review. Vim's half-page
  scrolls move the cursor by display rows, so with wrapping they land along a long line (from
  the uncommitted fixture's last row: row 20, column 166 of the 300-column line), and over long
  lines the cursor creeps along lines instead of moving by lines. Any other buffer of the user's
  doesn't wrap (Zed's default), so vim moves whole lines there.
- **Fix**: the override is gone; wrapping follows the language settings like Zed's own diff
  views (they set nothing), so the review behaves like the user's other buffers, wrapped or not.
  Long lines out of view scroll sideways (trackpad, scrollbar, `$`, `l`; covered since "Long
  lines wrap"). The review's `ToggleSoftWrap` capture stays: it switches both sides together and
  only while the diff has the focus (Zed's split toggle would also take a comment editor's).
- Tests: `test_half_page_scrolls_move_whole_lines` (the uncommitted fixture at 800×400 with its
  300-column line: `ctrl-u` and `ctrl-d` from the end to either end keep column 0 and reach the
  first and last rows; it failed with the override); `test_wrapping_follows_the_settings` (no
  wrapping by default, `ToggleSoftWrap` on both sides, new left sides follow) and
  `test_wrapping_settings_wrap_the_diff` replace `test_long_lines_wrap_on_both_sides`; the
  half-page tests also run over reviewed (folded) first files and fail a move along a line.

**Less work per keypress (the user's report)** — "navigating around with vim bindings is STILL
EXTREMELY LAGGY … it should run at the same speed as the normal zed editor navigation"; "you need to
find ALL of them introduced on this branch by your changes and fix".

- **Measured** with a timing harness (removed after use; median of 200 `j`/`k` presses with a frame
  each, debug build, the PR A fixture): before, the review ~34 ms per key against ~26 ms for Zed's
  own diff view over the same multibuffer (a bare `SplittableEditor`) and ~19 ms for a plain
  editor; after, ~30 ms, and ~27 ms with every review addition switched off: what's left is spread
  over the remaining additions (~1 ms each group).
- **Toolbar** (~3.5 ms): the pane redraws its toolbar on every cursor move (breadcrumbs), and the
  review's toolbar is a row of buttons. `ReviewToolbarItem` now hosts `ReviewToolbar` as a cached
  view (`AnyView::cached`, `w_full().h_8()`), redrawn only when the review notifies it.
- **Gutter "+"** : the renderer returned an element for every visible row, only to take clicks on
  line numbers. Those now come through `Editor::set_line_number_click_handler` (§3.10, core patch
  #6), and the renderer draws an element only for a row with a "+".
- **Lookups by file** (every frame, for every visible file header, the hunk controls and the
  comment gate): `repo_path_for`, `repo_path_for_buffer` and `entry` scanned every file; the diff
  keeps `repo_paths_by_key` / `repo_paths_by_buffer` (updated as files register, rebuilt after a
  refresh drops some).
- **Hunk controls**: "Mark Reviewed" looked its key binding up on every frame (`KeyBinding`); the
  binding is in its tooltip now, looked up on hover.
- Also checked, unchanged: one `SelectionsChanged` per key (no feedback loop with the panel, whose
  cursor following costs ~2 ms only when its file changes), and an idle review notifies none of
  its views (`test_idle_review_stays_idle`: a realistic overview with a long description and a
  stack of seven, reviewed files folded at the start, unified and split at five widths). The idle
  redraws seen in the user's profile came with the multiplying overview ("The overview is one
  block").

**Headers read as paths (the user's report)** — "you display the filename BEFORE the path, which
is super confusing. should do the other way around."

- Zed's multibuffer file header draws the file name, then its directory dimmed after it. The
  review turns on core patch #9 for both sides (`configure_side`, also on every new left side):
  `src/util/` then `strings.rs`. The name stays the button that opens the file, colored by
  status; Zed's other multibuffers are unchanged. The panel is a tree and comment locations are
  `path:line` already.
- Test: `test_file_headers_read_as_paths` (both sides, and a left side recreated by toggling the
  split).

**Known issues / notes for the next slices:**

- **Observations recorded, not fixed** (final validation; `plans/FIX-final-validation.md` group
  (b)): a 13 s bare spinner while a 307-file snapshot loads (V7.O2; needs `CommitDiffList` progress
  in the empty state); palette actions vanish while a comment editor is focused but scrolled out of
  view (V7.O1); sticky headers are narrower than the editor (V7.O5); the cursor row can sit under
  the sticky header after a split toggle (V7.O6); the status bar `line:col` goes stale after an
  external edit (V3.O4); the horizontal scrollbar thumb stays visible (V1.O5); `app_will_quit`
  timeouts on quit, also without review tabs (V3.O1/V7.O4); non-vim character selections are
  invisible (V8.O3); a visual `y` across a file boundary copies the last file's rows (V3.O3); `[t`
  visits the file comment first
  (V3.O7); Review in Worktree replaces the workspace (V4.O8); two `zed://review` links in one
  request: only the last opens (V6.O2, upstream `OpenRequest` keeps one kind); `R` on a folded
  header gives the generic toast (V2.O3); a thread posted on a previous head shows as moved until
  GitHub re-anchors it (V5.O6). Stale relative line numbers after closing a clone (V2.O6) weren't
  seen again.
- **Issue references, inline preview** (the user's bug findings, item 4, "at some point, should
  ideally render into dropdown so i can view inline"): references link to GitHub in the browser
  (see "GitHub references link"); showing the issue or pull request (title, state, body) in a
  dropdown inside the review needs a GitHub query per reference (`repository.issueOrPullRequest`)
  and a popover anchored on the markdown link, which the markdown crate doesn't offer (only
  `on_url_click`/`on_url_hover`). Also not linked yet: @mentions, commit SHAs, references in the
  review's own comments and in commit messages.
- **Non-vim users and the palette** (V8.O1, by design): the review tab's keys are vim-only (no
  default non-vim bindings, which would clash with the base keymaps), so non-vim users reach the
  review actions through the command palette, and its recency ranking is a trap there: once
  "code_review: add review comment" has been used, typing "add comment" ranks it above "code_review:
  add comment" and Return adds a review comment instead of a line comment. Read the highlighted row,
  or bind the actions in the user keymap. Since UX-1, the toolbar, the gutter's "+", the right-click
  menu and the panel's hover controls cover the main review actions with the mouse.
- **Deleted file, stale buffer** (R2 N1, upstream-inherited): a file deleted on disk while a review
  holds its buffer (a checkout that deletes it, `rm`) keeps showing the buffer's old text as its new
  side ("+1 −0" instead of its rows removed) until every tab holding the buffer closes; Zed's own
  `git: diff` does the same. GitHub's comment gate still refuses lines GitHub's diff doesn't have,
  so no wrong comment is posted. Possible fix here: treat a buffer whose file is deleted as empty
  for the new side.
- **Refused `c`, then typing** (R2 O1, vim semantics): when `c` is refused by a toast (for example
  "Check out the pull request's head … to comment on its lines"), the text typed next runs as
  normal-mode commands (`e` opens the file, `s` substitutes; autosave then writes the edit). Inherent
  to modal editing; a blocking hint or swallowing keys until Escape would be safer.
- **Line numbers** (fixed in UX-1 slice C): a click on a line number selects the row instead of
  opening the file (see "UX-1 slice C"); `e`, a header's Open File and the menu's Open File at Line
  open it. Seen in slice C, cosmetic: the panel's hover checkbox keeps the tooltip of the state it
  was hovered in until hovered again; the search bar's "Collapse All Files" button in the same
  toolbar row folds every file without reviewing it (like a header click, R1 O6).
- **Seen in the R1 sweep, upstream or by design**: `ctrl-u` back to the very top leaves the cursor
  three rows into the first file (vim's scroll clamps the cursor to the scroll margin below the
  header block; R1 O2); V3.O7 above reads like a dead key right after `r` (`r r M`: the first `M`
  lands on the file comment at the cursor; R1 O3); vim `/pattern` with the cursor below the last
  match picks the last match instead of wrapping, as in a plain Zed editor (R1 O4); clicking a file
  header outside its chevron or checkbox folds the file without reviewing it, Zed's header click
  (R1 O6).

- **Notes for the final validation (V6)**:
  - Build with `-p zed -p cli` before `zh start` (the harness finds `target/release-fast/cli` at
    start). Run the CLI only through `zh cli` (default open behaviour, `-C DIR` for the cwd) or `zh
    open` (adds `--existing`); never the fork's `cli` directly. Close review tabs with `: q Return`.
  - `pkill -f 'release-fast/cli.*--wait'` also matches the shell running it (its command line has
    the pattern): anchor it, e.g. `pkill -INT -f '^/home/drew/personal/zed/target/release-fast/cli '`.
  - Change Base's picker preselects `origin/main` when typing `main`: press Down for the local `main`.
  - `--review` lands in the *active* window among those with the repository open (GPUI's active
    window, which `-n` doesn't always change without a window manager).
  - Sandbox (after UX-1 slice C): PR A #1 (read-only), PR C **#16** (own, clean; #15 closed by
    UX-1 slice C, #13 by fix 3), PR B **#14** (bot, clean), PR D #4 (API probes).
  - `zh keys`/`click` always refocus the window `zh` started with: when a CLI request opens a second
    window, the keys still go to the first one (it typed a draft into the PR tab once). Close the
    first window (`ctrl+shift+w`) or drive the second with `zh xdo`.
- CLI nits: a link error's toast is logged as "pull request: …" (`forge::show_error_toast`'s
  prefix). (`--foreground --review` and a new CLI with an older Zed: fixed in polish A.)
- **Comment editors, known gaps**: every `:w` form leaves the comment keeping its text (see "Hardening
  2"); none writes a file. `:qa`, `:qa!`, `:wqa`, `:xa` close the review tab(s); open comment text is
  saved even by `:qa!`. Two tabs editing one saved comment: last writer wins. Two windows whose buffers
  differ (unsaved edits) disagree on a comment's line numbers: each places it by its own buffer, the
  stored numbers are the last mover's.
- Split view: a block below a *changed* row is still mirrored below the start of its hunk, so rows inside
  a multi-row hunk don't face each other until the hunk ends; a block below the last unchanged row of an
  excerpt is mirrored above the facing row (no next row to anchor to); a comment on the unchanged row
  right above a pure deletion is drawn after the deletion's spacer on the right side, so it reads as
  belonging to the next row (V8.O2: `block_below_row` picks `Above(next row)`, which on the right side
  comes after the spacer). All are upstream `balancing_block` behaviour (`editor/src/display_map/
  block_map.rs`); the fix needs core (candidate core patch #4: mirror `Below` as `Below` for
  unchanged rows, which would retire all three).
- Harness: `zh type` turns newlines into a key that isn't Enter for Zed; use `zh keys Return`.
- Upstream quirks seen live: vim relative line numbers apply to the focused side only and count across
  file sections; on the LHS the current line shows 0 instead of its number; after toggling soft wrap in
  split view the LHS is mis-laid-out until the next frame.
- M4 cosmetics: the line cursor's colors blend the full diff color, also on dimmed rows; the file row
  tooltip can overlap the panel's context menu; `e` on a folded header opens the file at its last
  row. Header controls go only through `ReviewAddon` in `diff.rs`.
- Multi-cursor commands (`g l`, `g a`) only show the newest selection.
- `cargo test -p zed test_action_namespaces` passes (polish B; the rest of `zed`'s tests not run).
  `cargo test -p cli`: 8 passed (fix 2). `cargo test -p code_review`: 229 tests + 1 ignored live
  GitHub test (after GitHub references link; 227 after the gutter buttons; 232 after stacked pull requests; 224 after the Review Pull Request picker; 222 after UX-1 slice C; 214 after UX-1 slice B; 206 after UX-1 slice A; 202 after the review panel fix; 201 after fix 3; 198 after fix 2; 191 after fix 1; 184 after polish B; 180 after polish
  A; 166 after M8; 152 after M7 slice d, 136
  after hardening 3, 127 after M7 slice c, 120 after slice b, 113 after hardening 2, 110 after
  M7 slice a, 89 after M6, stable under
  `ITERATIONS=5`; the hardening-2 comment tests under `ITERATIONS=10`).
- M3 was verified live (harness): picker rows, every target, Change Base (button, palette, reuse),
  snapshot read-only + checkout untouched, live range following edits, top block, titles, reuse,
  `]f`/`]c`/line cursor/`e` in branch, live and snapshot reviews.
- M3 notes: huge snapshot ranges load every blob (in 32-file jobs) before the first excerpt.
  Hover/go-to-definition in snapshots couldn't be checked live (the harness disables language
  servers); snapshot buffers aren't local, so no server attaches by construction. `DiffBufferList`
  keeps its last tree diff when a reload fails, so a base deleted (or HEAD switched to an unrelated
  branch) after a review opened shows the stale file list; the error label only replaces an empty
  list (the base is re-checked on `HeadChanged`).
- Pull request tabs: `ReviewView::title` is "PR #N: ‹title›" (the plan's `#{n} {title}` with a prefix,
  as the slice (b) brief asked). A pull request tab follows HEAD: with another branch checked out it
  shows that branch's changes since the base, with the warning, and its line threads move to the
  moved blocks (C7). The palette lists the pull request actions in every review (they toast "Open a
  pull request review first" elsewhere). Thread line numbers count in HEAD's files and are mapped
  through the checkout's uncommitted and unsaved changes (slice (d)).
- Upstream, outside this crate (seen in hardening 3): Zed's GitHub hosting provider fetches blame
  avatars from `https://api.{host}/repos/…/commits/…` with `GITHUB_TOKEN` for every GitHub provider,
  including the "GitHub Self-Hosted" one it registers for any `origin` whose host contains "github"
  (`git_hosting_providers/src/providers/github.rs`, `fetch_github_commit_author`): the leak H1 fixed in
  `code_review` exists upstream for blame. Not patched (core).
- Upstream behaviours seen in slice (b): Zed's `change_branch` splits `remote/branch` at the first
  `/` (remote names with `/` fail); a clone whose fetch refspec lacks the PR branch (single-branch)
  fails with "Branch 'origin/…' not found"; `create_worktree_workspace` creates a worktree for every
  repository of the project. Not verified live: the askpass modal (the harness clone uses a token
  credential helper), the `confirm_checkout` prompt (M8 calls it), SSH remotes.
- DESIGN's file:line pointers checked on `v1.25.0-pre` for this slice (`diff_buffer_list.rs`,
  `diff_multibuffer.rs`, `split.rs:1192`, `git_store.rs`, `zed.rs`, `main.rs`) match within a line or
  two. `git_ui` on the tag also has `staged_diff.rs` — a read-only `DiffMultibuffer` item, the
  closest template to `ReviewView`.

**Decisions** (made by the user, 2026-10-16; don't ask again):

- Branch: **`code-review`**, one long-lived branch rebased onto each upstream stable tag (§2.1).
- Base: **`v1.25.0-pre`** (`aa9c16803d`) for now, because it matches the warm `target/` cache. Rebase
  onto the first 1.25 stable tag once it ships; until then `crates/zed/RELEASE_CHANNEL` is `preview`.
  That tag may well be `v1.25.N` rather than `v1.25.0` (1.23 first shipped stable as `v1.23.1`,
  1.24 as `v1.24.2`). Always name the exact old tag: `git rebase --onto <new> v1.25.0-pre
  code-review`; a merge-base is wrong because each minor's release branch gets cherry-picks and the
  next minor's tags don't descend from it (`.claude/skills/rebase-code-review`).
- Agent build cap: **`-j8` under `nice -n 10`** for every cargo invocation. Never run cargo without
  these limits. This applies to agent sessions only; the Nix package build stays uncapped (§2.1).
- Core patch #2 (horizontal-autoscroll opt-out in `editor`) accepted; batch core edits so `editor`
  rebuilds once.
- Polish B (orchestrator): core patch #3 (the empty-row highlight) only as an opt-in that leaves
  every existing caller unchanged; core patch #1 reverted (no caller).
- Fix 2 (orchestrator): the "Mark Reviewed" hunk control stays where Zed's hunk controls go, over
  the end of long rows; the pull request's description and summaries scroll after 8 lines
  (superseded by UX-1: the user wants one scroll for the whole document); with
  several repositories, review tabs and the panel header name the repository; the submit modal
  remembers its event per pull request until a review is posted.
- M2 (orchestrator): `]f`/`[f` stop at the ends and `]u`/`[u` wrap, as in tuicr; `h` at the left edge of
  the right side in split view focuses the panel; no counted `3}`-style jumps (no vim patch).
- M7 (user): a fork's checkout lists the upstream parent's pull requests; bot threads hide behind the
  bot-summaries toggle; live GitHub writes only in `drew-council/code-review-sandbox`; submit is `; s`
  in the review tab (plus the palette).
- M7 slice (d) (agent, from M8's plan D3, can be vetoed): `; s` in a local review finishes it, its
  Markdown to the clipboard or to a `--wait` waiter.
- M8 (agent, from the plan): only the CLI's own request (the link plus the repository's path) checks
  a pull request out without asking; `--review` usage errors exit 1; `--review` lands in the active
  window among those with the repository; a repository needs a `.git` file or a `.git` directory with
  a HEAD.

**Before writing code, the next agent should:**

1. Read this whole file, then skim the tuicr UX it ports (§9) — tuicr is a *UX reference only*.
2. Work on `code-review`; never commit fork work to `main`; don't push unless asked.
3. Follow the milestones in §6. Milestone 1 is a walking skeleton; keep each milestone shippable.
4. Use the dev loop below.

**Dev loop** (verified 2026-10-16 on `code-review`, Ryzen 7 7800X3D, `-j8`, `nice -n 10`):

- **Devshell:** wrap every command in `direnv exec /home/drew/personal/zed …`. It loads
  nix-direnv's cached profile (instant; no Nix evaluation unless `flake.nix`/`flake.lock`/`.envrc`
  change) and works whether or not the calling shell already has direnv's env. `cargo` there is the
  devshell wrapper (`script/cargo`, adds `--timings`), which is fine. **Don't** use
  `nix develop -c …`: the devshell includes a crane build of `docs_preprocessor` whose source is all
  of `crates/` (`nix/modules/devshells.nix`, `nix/build.nix` `mkIncludeFilter`), so after any edit
  Nix would rebuild it, uncapped (and all of Zed's deps if a `Cargo.toml`/`Cargo.lock` changed).
  The same happens once via direnv when a rebase changes `flake.lock`.
- **Build & run** (the warm-cache profile; since M8 always `-p zed -p cli`, never `-p cli` alone,
  so `target/release-fast/cli` matches `zed`: the first build added only the CLI's own crates, 19 s;
  an edit in `code_review` then takes ~30 s):

  ```sh
  direnv exec /home/drew/personal/zed env CARGO_PROFILE_RELEASE_FAST_DEBUG=limited \
    nice -n 10 cargo build -j8 --profile release-fast -p zed -p cli
  ./target/release-fast/zed   # agents: only through script/live-harness/zh
  ```

  **After the first stable rebase**, dev builds are stable-channel: run directly,
  `target/release-fast/zed` and `cli` open the user's real database
  (`~/.local/share/zed/db/0-stable`) and talk to the user's running Zed (nixpkgs' or the installed
  fork, `INSTALL.md`) through `zed-stable.sock`. Always run them through `script/live-harness/zh`
  (isolated HOME) or with `--user-data-dir <tmpdir>` (the socket lives in the data dir too); never
  directly. (On `v1.25.0-pre` they are preview-channel: their own `0-preview` DB and
  `zed-preview.sock`, but still the user's display and settings.)

- **Type-check a crate:** run the same `-p zed` build. Every crate we touch is in `zed`'s graph,
  errors show before the link step, and only the `-p zed` selection hits the warm cache. **Don't**
  `cargo build/check --profile release-fast -p <crate>`: there is no workspace-hack crate, so a
  different `-p` selection resolves different features and recompiles third-party crates (`-p
  go_to_line` started recompiling `proc-macro2`, `syn`, `serde_json`, … and was stopped after 181
  crates). `cargo check` never reuses `build` artifacts, so it is cold for the whole graph (not
  measured).
- **Tests:** default `test` profile, like upstream CI (`cargo nextest run`). `--profile release-fast`
  buys nothing: test-support dev-dependency features change the hashes of gpui/editor/project…, so
  most of the closure rebuilds anyway, at opt-level 3.

  ```sh
  direnv exec /home/drew/personal/zed nice -n 10 cargo test -j8 -p <crate>
  # nextest (uses .config/nextest.toml timeouts); its -j is test threads, the build cap is --build-jobs:
  direnv exec /home/drew/personal/zed nice -n 10 cargo nextest run --build-jobs 8 -p <crate>
  ```

- **Clippy:** `direnv exec /home/drew/personal/zed nice -n 10 ./script/clippy -p code_review -j8`
  (runs `--release --all-targets --all-features`, a separate check-mode build that is cold the first
  time; not run yet).

| Step | Time |
|---|---|
| First `-p zed` build on the branch (189 crates: workspace crates + 6 third-party in the webrtc/livekit chain) | 6m05s |
| No-op `-p zed` build | 2s |
| Leaf crate edit (`journal` + `go_to_line` touched) → `-p zed` | 22s |
| First `cargo test -p go_to_line` (cold `test` profile, 688 crates, +11 GB) → 9 GPUI tests pass in 0.7s | 2m27s |
| Re-run after touching the crate (`cargo test` / `cargo nextest run`) | 4s / 3s |
| Then `cargo test -p git_ui --no-run` (131 crates recompiled, 53 third-party, because features differ per `-p`; +8 GB) | 1m22s |

Cosmetic quirk: the cached devshell was evaluated on `main`, so it exports
`ZED_COMMIT_SHA=2d4c483…` and builds report main's SHA (`crates/zed/build.rs` prefers the env var
over `git rev-parse HEAD`).

**Working agreements with the user** (learned the hard way — follow these):

- Only ask the user about **user-facing** behaviour. Decide implementation details yourself and
  record them (e.g. how review state is keyed was "not a user-facing detail… should not have been
  asked").
- tuicr is a UX reference; do **not** port its architecture or expect its code to survive. Build
  Zed-idiomatically (GPUI entities, Zed's `HttpClient`, `GitStore`, workspace items/panels).
- No bespoke external protocols (no `tuicr review`-style CLI/JSON protocol). Agent integration will
  come later "the way Zed typically does it".
- Defer to how Zed already does things (e.g. branch checkout) when in doubt.
- Reuse existing Zed code (especially vim) rather than hand-rolling emulation; the user explicitly
  does not trust hand-rolled vim behaviour.
- Fork footprint: keep changes to existing upstream files tiny and append-only; put everything in
  new crates/files. Edits to core crates are expensive both to rebase and to rebuild (§8.1).

---

## 1. Context

- **tuicr** (`~/personal/tuicr`, the user's fork of agavra/tuicr) is a ratatui TUI the user uses
  heavily for code review: a continuous multi-file diff, line-oriented vim navigation, per-file /
  per-hunk "reviewed" marks, line/range/file/review-level comments with types, GitHub PR review
  submission, and markdown export for coding agents. Its weakness: no LSP, no real filesystem/editor
  context, partial diffs from the GitHub API.
- **Goal:** the same review flow inside Zed, with full LSP and a real local checkout, blending with
  Zed's vim mode.
- **Why a fork, not an extension:** Zed's extension API (WIT `crates/extension_api/wit/since_v0.8.0/`)
  has no UI, no commands/actions, no keybindings, no buffer access, no decorations. Zed staff
  (RFC discussion #53403, 2026-05-21) called UI extensions "not something we're likely to get to in
  the near future"; only an experimental spike exists (`zed-industries/embedded_gpui`). A no-fork
  design (review language server + MCP + tasks) was evaluated and rated "functional but bolted on"
  (no inline comment boxes, no reviewed toggle/file tree, comment authoring via separate files).
- **Upstream landscape (avoid collisions):**
  - Upstream has a hidden, feature-flagged in-editor comment prototype (`diff-review` flag,
    `crates/feature_flags/src/flags.rs:30-40`; UI in `crates/editor/src/git.rs:557-790, 1021-1508`;
    storage in `pub(super)` `Editor` fields; `editor::SendReviewToAgent` has no handler). **We do
    not build on it and keep the flag off.** Expect upstream churn in `editor/src/git.rs`.
  - Zed's own review strategy is Delta (zed.dev/blog/delta-public-beta, 2026-09-16), so native
    GitHub PR review in Zed is unlikely soon (discussion #34759, 607 votes, unscheduled). Open
    community PRs: #62419 (review → agent), #61311 (inline PR comments).
  - Prior art outside Zed: `ellie/lgtm` (GPUI PR reviewer), `thinkter/zmux` (embeds Zed's editor+vim
    crates out of tree).

---

## 2. Agreed design

### 2.1 Delivery

- A feature of the fork: new crate **`crates/code_review`**, actions in the **`code_review::`**
  namespace (clear of upstream's `git::ReviewDiff` / `agent::ReviewBranchDiff`). No "guicr" branding.
- Fork follows **upstream stable tags** (`vX.Y.Z`, weekly), rebasing one long-lived branch onto each
  new tag. Write a rebase skill modelled on tuicr's `.pi/skills/rebase-bizmythy-tweaks/SKILL.md`
  (backup branch, review each commit, drop obsolete ones, `range-diff`, run checks). Done (M9):
  `.claude/skills/rebase-code-review/SKILL.md`.
- **Install:** the fork's own Nix flake (Zed's standard crane build, `packages.default`) replaces
  nixpkgs' `zed-editor` as the user's only Zed. Use the normal Nix build settings — no `-j` caps.
  Built from a stable tag, `crates/zed/RELEASE_CHANNEL` is `stable`, so it shares the user's
  existing settings/keymap/DB and the CLI single-instance socket works (dev-channel builds do not
  start the socket listener — `crates/zed/src/main.rs:360-364`). Upstream's `nix/build.nix`
  overwrites `RELEASE_CHANNEL` with `nightly`, so the install uses the fork's
  `.override { releaseChannel = "stable"; }` (M9, §3.10, `crates/code_review/INSTALL.md`).
- Settings: no `code_review` settings block until the first setting needs to persist; then add it
  (requires editing `crates/settings_content`, accepted). Added with the Review Pull Request picker
  (`code_review.pull_request_search`, `code_review.hide_approved_pull_requests`).

### 2.2 Review targets

| Target | Diff | Buffers |
|---|---|---|
| **GitHub PR** | merge-base(base, head) → head | real working-tree files (PR checked out in place) |
| **Branch vs base** | merge-base(base, HEAD) → working tree (includes uncommitted, like Zed's Branch Diff). Base = default branch, changeable via branch picker | real files |
| **Uncommitted** | HEAD → working tree (staged + unstaged together) | real files |
| **Commits** | one commit or a contiguous range | real files if the range ends at HEAD; otherwise read-only historical snapshots (syntax highlighting, **no LSP**), like Zed's CommitView. Never move the user's checkout. |

- **PR checkout** (opening a PR checks it out in place, Zed's way):
  - same-repo PR → local branch tracking `origin/<headRef>` + `git checkout` (Zed's
    `change_branch`, `crates/git/src/repository.rs:2370-2422`);
  - fork PR → fetch `refs/pull/N/head` into local `pr/<N>`, then checkout;
  - dirty tree → exactly like Zed's branch picker: git carries changes over or refuses, error shown,
    **no auto-stash**;
  - secondary action **"Review in new worktree"** (as built: our own detached `git worktree add`
    for the PR's repository, opened as another workspace in the window; trusted only for the
    repository's own code or the viewer's fork, see §0 "Hardening 3");
  - nothing is checked out in a repository Zed doesn't trust (Restricted Mode); an in-place checkout of
    a stranger's fork runs its project settings, tasks and build scripts with the project's trust,
    like `gh pr checkout` (documented risk; the worktree is the safe way to read it).
- **GitHub access:** in-process via Zed's `HttpClient` (GraphQL for reads, REST for review
  creation). Token (as built, gh's rules): `GH_TOKEN`/`GITHUB_TOKEN` (Zed's or the project's
  environment) only for github.com and `*.ghe.com`; `GH_ENTERPRISE_TOKEN`/`GITHUB_ENTERPRISE_TOKEN`
  for an Enterprise Server the user configured in `git_hosting_providers`; else `gh auth token
  --hostname` (the user's `gh` is logged in with `repo` scope). A host that merely looks like GitHub's
  gets nothing unless `gh` has a login for it. Signing into Zed with GitHub does **not** give a usable token (the OAuth flow
  returns a zed.dev token; `crates/client/src/client.rs:1430-1551`). API host from the git remote.
  Wrap behind a small provider trait for future forges. git-only VCS (jj later maybe).

### 2.3 Entry points

- **Start Review picker** (`code_review::StartReview`): one modal with a **Local** section
  ("Uncommitted changes", "Branch vs ‹base›", recent commits with multi-select for ranges) and a
  **Pull Requests** section (open PRs; toggle to review-requested; open by number/URL).
- Direct actions per kind (for keymaps/CLI): review uncommitted / review branch / review commits /
  open pull request.
- **CLI:** `zed [-n|-e|-a] [--wait] --review <TARGET> [REPO_DIR]`
  - `TARGET`: `pr:123` · `owner/repo#123` · PR URL · `branch` · `branch:<base>` · `uncommitted` ·
    `commit:<sha>` · `commits:A..B`. `REPO_DIR` defaults to cwd. Must be clear and documented
    (`zed --help`).
  - Reuses the window that already has the repo open; `--wait` blocks until submitted/closed,
    prints the posted review URL; exit 0 = submitted, 2 = closed without submitting, 1 = error.
  - Also `zed://review?repo=<abs>&target=<t>` links (xdg-open, clickable).

### 2.4 Layout

- **Left dock: Review panel** — review title/status; file tree (directories, status letters,
  reviewed ✓); include/exclude regex filters; path search; **generated/vendored files hidden** via
  `.gitattributes` (`linguist-generated`, `gitlab-generated`, `linguist-vendored`) — hidden in both
  panel and diff, with a toggle; hide-reviewed toggle; **comment list** below the tree when comments
  exist; copy actions (PR URL, branch) in a context menu / palette.
- **Center: Review tab** (a new kind of item) — **read-only** diff over real buffers (LSP hover,
  go-to-definition, references work), split or unified per the user's `diff_view_style`. At the top,
  scrolling with the diff as one document (UX-1; full width in split view), a collapsible block with
  the **PR description** rendered with Zed's markdown (or the commit
  message(s) for commit reviews — not commentable), followed by the **review-level comments**
  section. Reviewed files fold to their header. **Single-file focus mode** toggle (continuous view is
  default). Multiple review tabs may be open; the panel follows the active one.
- **Pane toolbar** (UX-1): the review's counts, previous/next file/hunk/comment, Review Comment,
  single-file toggle, Copy as Markdown and Submit Review / Finish Review, after Zed's own diff
  buttons (collapse, split/unified). A right click in the diff opens the review's own menu instead
  of Zed's editor menu.

### 2.5 Movement & keys (review tab)

**Zed's own vim, unchanged, on a read-only diff** (the user, after using it: the tuicr-style line
cursor and its custom movement lagged and broke `ctrl-u`; "just use upstream vim binding stuff …
I want the full vim motions so I can go over a symbol and do `gd`"). This replaced the original
line cursor (no caret, a full-width row highlight, `h`/`l` scrolling; see "Upstream vim in the
review tab").

- The caret, selections, visual modes (`v`, `V`, `ctrl-v`), every motion, text object, count,
  search, `gd`/`gr`/`K`, yank (`y`, `yiw`, …), `h`/`l`, `{`/`}`, marks, `ctrl-d/u/f/b` and the
  scroll keys are vim's. Wrapping follows the language settings, as in Zed's own diff views
  (Zed's default doesn't wrap; long lines then scroll sideways: trackpad, scrollbar, cursor
  motions); `ToggleSoftWrap` turns it on and off for both sides together, and a new left side
  follows the right side (see "Wrapping follows the settings", which replaced "Long lines
  wrap").
- The review binds only keys that would edit the read-only diff, and two-key sequences on vim
  prefixes: insert/change/replace keys become review actions or no-ops, `]`/`[` and `;` lead
  review keys. `;` alone still repeats `f`/`t` after the keymap's pending-key timeout.

| Keys | Action |
|---|---|
| `]f` / `[f` | next / previous file |
| `]c` / `[c` | next / previous hunk (vim's keys; the review's action skips folded, reviewed files, where Zed's would get stuck on the header) |
| `]t` / `[t` | next / previous comment thread |
| `]u` / `[u` | next / previous unreviewed file |
| `]s` / `[s` | next / previous pull request in the stack |
| `c` (normal / visual) | comment on line / range |
| `C` | file comment |
| `;c` | review-level comment |
| `i` / `Enter` | edit the comment at the cursor line (else `Enter` is vim's) |
| `dd` | delete comment at cursor (with confirmation) |
| `r` | toggle file reviewed and advance to next file |
| `R` | toggle hunk reviewed |
| `g space` | open file at cursor line in a normal editor (Zed's key; a snapshot's file from the working tree) |
| `;y` / `;Y` | copy review as markdown / copy comment at cursor (Copy Lines is in the menu and palette) |
| `;f` / `;e` / `;h` / `;l` / `;s` | focus mode / toggle panel / focus panel / focus diff / submit |
| `;t` | collapse / expand the review's overview (`ToggleHeader`) |
| `a`, `o`, `s`, … | no-ops (would otherwise enter a useless insert mode on a read-only buffer) |

- **Old-side (deleted-line) comments:** unified view → `c` on deleted rows (they are real rows);
  split view → `ctrl-w h` to the left side, then `c`.
- **Panel keys:** `j`/`k`/`h`/`l`/`Enter`, `Space` toggles a directory, `i`/`e` include/exclude
  filters, `I`/`E` clear, `/` path search, `r` toggles reviewed.
- Everything is also in the command palette (`code_review: …`).

### 2.6 Comments

- Inline boxes under the last line of their range. Drafts are vim-enabled auto-height editors that
  open **in insert mode**; non-editing comments render as markdown.
- Leave & keep text: `ctrl-enter`, `ctrl-s`, `:w`, `:wq`, `:x`, `Esc` in normal mode. `:q!` discards
  unsaved edits. `dd` (from the review tab) deletes a comment. (No tuicr-style double-Esc cancel.)
- **Types:** untyped default; Issue / Suggestion / Note / Praise; `Tab` in the editor's **normal**
  mode cycles (insert-mode `Tab` is a tab); colored badge; `[TYPE] ` prefix on GitHub submission.
  Custom type sets later (settings).
- **Mouse** (UX-1, GitHub-like): a "+" in the gutter (between the line numbers and the text, clear
  of the excerpts' expand buttons) of the hovered row, the cursor row and the selection's end (both
  sides, removed rows too) comments on the row or the selection it's in; pressing it and dragging
  over rows comments on them; disabled with the gate's reason where a pull request doesn't take
  comments. The editor's footer has a type menu, Cancel, Save and (saved comments) Delete; file
  headers have "Comment" and the "Reviewed" checkbox. Line numbers select rows (click, drag,
  shift-click) instead of opening the file. The right-click menu comments on the row or selection,
  or on the file, and edits, deletes or copies the comment there; the panel's comment rows have
  edit/delete buttons on hover.
- **Review-level comments** (`;c`) live in the top section and become the **GitHub review body** on
  submit (tuicr: `src/forge/submit.rs:568` `build_review_body` joins them with `[TYPE]` prefixes).
- **Persistence:** drafts + reviewed marks saved locally in **Zed's DB**, keyed by review target
  (PR: repo+number; branch: repo+branch+base; uncommitted: repo+worktree; commits: repo+range) — not
  by head SHA, so new pushes don't start a new review. (M3: "branch" is the checked-out branch's name
  and "base" the base ref name as in `ReviewTarget::Branch`; "range" is `CommitRange`'s `base..head`
  shas. Change Base and branch switches change a branch review's key.) Reviewed marks clear when that file's/hunk's
  diff content changes (tuicr: FNV-1a over hunk lines, `src/model/review.rs:164-189`; as built, a
  mark stops counting while the content differs and counts again if it changes back, see §0). Drafts follow
  code via Zed anchors while running; after reload a draft whose line text no longer matches is
  flagged **outdated** (not dropped).

### 2.7 PR specifics

- Existing review threads render inline, **read-only**; resolved and outdated hidden by default
  (toggle). **Bot review summaries hidden by default** (tuicr `050f0f8`: `is_bot` via
  `__typename == "Bot"` or `[bot]` login, `src/forge/github/review_summaries.rs:138`), and **bot
  threads too, behind the same toggle** (user decision during M7).
- A fork's checkout (e.g. `drew-council/zed`) lists the **upstream parent's** pull requests (user
  decision during M7); `owner/repo#N` or a URL still reaches the fork's own.
- **Submit modal:** Comment / Approve / Request changes; counts; body preview (from review-level
  comments); stale-head warning (head moved since load); when GitHub requires a body (Request changes,
  and Comment, without line or file comments — LV §5.1) and none exists, prompt to add a review-level
  comment. On success: drafts become `submitted` (locked), threads refetched. (As built: §0 slice
  (d).)
- Comments can only be created on rows GitHub accepts, so tuicr's unplaceable-comment resolver is
  unnecessary. (Verify which context lines GitHub accepts — §7.)

### 2.8 Export

- `code_review::CopyReviewAsMarkdown` (`y`): tuicr's numbered `[TYPE] path:line` format
  (`~/personal/tuicr/src/output/markdown.rs`, AGENTS.md "Review Format"). Works for local and PR
  reviews. Hard requirement for v1; a stopgap until Zed-native agent integration.

### 2.9 Out of scope for v1 (ordered)

1. Reply to / resolve threads (first after v1).
2. GitHub suggested-change blocks (ASAP after v1).
3. Then, unordered: PR commit-subset selection + "since my last review" scoping; GitHub "Viewed"
   sync; showing review comments/diff in ordinary editors outside the review tab; Zed-native agent
   integration (replaces markdown copy); `code_review` settings block; staged-only / unstaged-only
   views; AI review guide + file reordering (tuicr `5ea82dc` — explicitly excluded for now); jj;
   other forges; SSH/remote projects (v1 is local-only).

Explicitly dropped: tuicr `:stage`, `.tuicrignore`, tuicr `:summary` view (the panel's comment list
covers it), tuicr's `tuicr review` CLI protocol.

---

## 3. Architecture

### 3.1 Crate layout (proposed; as of M8 `session.rs`, `persistence.rs`, `gitattributes.rs`, `reviewed.rs`, `comments.rs` + `comments/{blocks,remote,view}.rs`, `export.rs`, `forge.rs`, `github.rs` + `github/queries.graphql`, `pull_request.rs`, `checkout.rs`, `submit.rs`, `cli.rs` exist too)

```
crates/code_review/
  Cargo.toml            # deps: gpui, ui, editor, multi_buffer, buffer_diff, project, git, workspace,
                        #       language, markdown, picker, db, http_client, fs, util, settings, …
  LICENSE-GPL -> ../../LICENSE-GPL   # required by script/check-licenses
  src/code_review.rs    # init(cx): actions, observe_new::<Workspace>, item/panel registration
  src/target.rs         # ReviewTarget (Pr/Branch/Uncommitted/Commits), parsing (CLI + picker)
  src/session.rs        # ReviewKey, ReviewSession (reviewed marks, shared per key), digests (M4)
  src/reviewed.rs       # header checkbox, hunk control, dimmed reviewed hunks (M4)
  src/diff.rs           # building the multibuffer: DiffSource (DiffBufferList / CommitDiffList)
  src/commits.rs        # commit ranges: live (open_diff_since) or historic snapshot buffers; git helpers
  src/view.rs           # ReviewView: workspace Item wrapping a SplittableEditor (+ top block)
  src/top_block.rs      # fixed header above the diff: branch base / commit messages (M5/M7 extend it)
  src/comments/         # model, blocks, CommentEditor (vim child editor), focus/keys
  src/panel.rs          # ReviewPanel (dock): file tree, filters, comment list
  src/picker.rs         # StartReview picker (Local section; PR section in M7)
  src/forge.rs          # forge-neutral PR types, Forge trait, resolve_forge, error helpers (M7a)
  src/github.rs         # GitHub impl (GraphQL/REST via HttpClient), auth, parsing (M7a)
  src/github/queries.graphql  # every GraphQL operation (M7a)
  src/pull_request.rs   # PullRequestState, open_pull_request (in place / new worktree) (M7b)
  src/checkout.rs       # PR checkout plan + executor (change_branch / refs/pull fetch) (M7b)
  src/submit.rs         # submit modal + payload mapping
  src/export.rs         # markdown export
  src/persistence.rs    # Zed DB domain (sqlez) for drafts & reviewed marks
  src/gitattributes.rs  # linguist-generated/vendored detection
  src/cli.rs            # zed://review links and `zed --review` requests (open, --wait); called from crates/zed glue
```

### 3.2 Showing the diff

- Reuse BranchDiff's machinery. `DiffBufferList` (`crates/project/src/git_store/diff_buffer_list.rs`
  — `DiffBase` :27, `new` :61, `set_diff_base` :137, `load_buffers` :294; `DiffBase::Merge{base_ref}`
  passes `base_ref` verbatim to `git diff --merge-base`, so a SHA works) opens **real project
  buffers** and picks diffs; `git_ui/src/diff_multibuffer.rs` (`register_buffer` :437-577,
  `pub(crate)`) builds excerpts via `SplittableEditor::update_excerpts_for_path`
  (`crates/editor/src/split.rs:1192`). `DiffMultibuffer` is private → **copy ~600 lines into the
  crate** (preferred: lets us add `DiffTreeType::Since{base, head}` for commit ranges ending at HEAD)
  or make it `pub` (3-line git_ui patch).
- Live diffs against any commit: `GitStore::open_diff_since(oid, buffer, repo)`
  (`crates/project/src/git_store.rs:1664`) — public, cached, recalculated on edits.
- File lists: `Repository::diff_tree(DiffTreeType::{MergeBase{base,head}, MergeBaseWithWorktree{base},
  Since{base,head}})` (`git_store.rs:9716`; impl `crates/git/src/repository.rs:1931`).
- Any other git command: `Repository::send_job` (`git_store.rs:6917`) gives the local
  `GitRepository` backend (e.g. `git merge-base`, `git fetch origin pull/N/head:pr/N`,
  `load_revisions(["B:path"])`). Validate refs (no leading `-`; use `--end-of-options`).
- Read-only: build over `MultiBuffer::new(Capability::ReadOnly)` (as `git_ui/src/commit_view.rs:306`
  and the split LHS do). Project buffers stay writable; vim motions/visual/yank unaffected.
  Go-to-definition from a multibuffer opens a normal editable editor (`editor/src/navigation.rs:1904-1975`).
- Commit ranges not ending at HEAD: copy CommitView's `GitBlob` + `build_buffer`
  (`git_ui/src/commit_view.rs:1070-1147`, `pub(crate)`); `BufferDiff::new` + `set_base_text`
  (`crates/buffer_diff/src/buffer_diff.rs:1618, 2234`). No LSP (LspStore only registers local files,
  `project/src/lsp_store.rs:3172-3177`).
- Hunk nav (`editor::GoToHunk`), context expansion (`editor::ExpandExcerpts*`, default `shift-enter`),
  split/unified (`editor::ToggleSplitDiff`) come for free.
- Row classification (added/deleted/context + old/new line numbers): `RowInfo{buffer_id, buffer_row,
  diff_status}` from `MultiBufferSnapshot::row_infos` (`multi_buffer.rs:4232`, struct :846-853);
  context rows' old line via `BufferDiffSnapshot::buffer_point_to_base_text_point`
  (`buffer_diff.rs:811-823`). Unified deleted rows are real rows (`DiffTransform::DeletedHunk`,
  `multi_buffer.rs:715-727`).
- ~~**Core patch #1:** old-side line numbers on unified deleted rows need `set_number_deleted_lines`
  made `pub` (`crates/editor/src/config.rs:373`).~~ Never called by `code_review` (only the split
  editor sets it, for its own left side: `split.rs:735`); reverted in polish B.

### 3.3 Split view specifics

- LHS is a separate full-mode `Editor` over a read-only base-text multibuffer (`split.rs:719-751`);
  accessors `lhs_editor()` :562, `focused_editor()` :605, `is_split()` :581. Vim works there.
- Keyboard side switch: `ctrl-w h/l` → `workspace::ActivatePaneLeft/Right` intercepted by
  SplittableEditor (`split.rs:938-976`); the cursor row is mirrored (`split.rs:978-1011`).
- `SplittableEditor` re-emits both sides' events without a side tag → subscribe to `lhs_editor`
  directly for old-side selections.
- **LHS editor is recreated** on every split and dropped on unsplit, including automatic toggles
  below `minimum_split_diff_width` (`split.rs:1391-1418`). Observe the SplittableEditor, re-host
  comment blocks (keep draft editors in the crate's model), re-apply highlights.
- Blocks inserted on either side get automatic balancing spacers on the other side (`block_map.rs:1748-1958`).
- UX-1: the review's overview is a block at `Above(Anchor::Min)` on the right side, drawn across
  both sides by the split view (core patches #5 and #7, §3.10).

### 3.4 Cursor (§2.5)

- No cursor code of the review's own: each side is a plain read-only `Editor` with Zed's caret and
  vim. Split view is upstream's `SplittableEditor`: `ctrl-w h/l` switch sides, the other side's
  cursor follows to the facing row (`sync_cursor_to_other_side`), both sides scroll together. The
  review only puts a new left side's cursor on the row facing the right side's (`face_cursor` in
  `view.rs`: upstream leaves it at the end until the right side's cursor moves, and `ctrl-w h`
  would jump there) and turns the gutter's breakpoint/bookmark hover off on each side
  (`configure_side`; the review's "+" is the gutter's hover button).
- Rows of the newest selection, as the review's actions take them: `context_menu::selected_rows`
  (`selections.newest_adjusted`; an end at column 0 excludes that row).
- Cursor never lands on custom blocks (`BlockSnapshot::clip_point`, `block_map.rs:2504-2558`);
  folded (reviewed) file headers *are* cursor stops.
- Vim counts count buffer lines, not blocks (`vim/src/motion.rs:1537-1606`). Half-page scrolls over
  blocks needed core patch #8 (see "`ctrl-u`/`ctrl-d` over blocks"). Upstream's half-page scroll
  still leaves an off-screen cursor where it is (`newest_selection_on_screen`), so at the top of a
  review whose overview fills the view, `ctrl-u` scrolls without moving the cursor.

### 3.5 Comment blocks & editors

- Blocks: `editor.insert_blocks(BlockProperties{placement: Below(anchor), style: Flex|Sticky, render,
  …})` (`editor.rs:8865`; types `display_map/block_map.rs:159-336`). Blocks auto-size to content
  (`element.rs:3497-3549`). Create the child `Editor` outside the render closure; store it in the
  crate's model. Root div: `.key_context("ReviewComment").block_mouse_except_scroll()` with
  `on_action` handlers, wrapping `EditorElement::new(&child, style)`. Precedents:
  `editor/src/inline_input.rs:131-187, 302-345`; `agent_ui/src/inline_assistant.rs:604-651, 767-822,
  1041-1133`.
- **Vim in the child:** `Editor::auto_height(..)` with `set_use_modal_editing(true)` **inside the
  `cx.new` closure** (vim attaches once at creation, `vim/src/vim.rs:619-627`; precedent
  `git_ui/src/git_panel.rs:1246`). Open in insert mode by dispatching `vim::SwitchToInsertMode`
  (buildable by name: `cx.build_action("vim::SwitchToInsertMode", None)`). Search is disabled in
  auto-height editors (`assets/keymaps/vim.json:1151-1160`).
- **Leaving/saving:** `ctrl-enter` binding in `ReviewComment > Editor`; `ctrl-s` via `on_action::<workspace::Save>`
  on the block root; `:w` → vim's private `VimSave` → `workspace.save_active_item` → our **`Item::save`**
  on ReviewView (report `can_save`, `is_dirty` = has unsaved drafts; save the focused comment) —
  `vim/src/command.rs:349-479`, `workspace/src/pane.rs:2246-2470`. `:wq`/`:x`/`:q` dispatch
  `workspace::CloseActiveItem` → intercept on the block root. `Esc` in normal mode → `editor::Cancel`
  bubbles from the child (`editor.rs:3529-3565`) → handle on block root as "leave, keep text".
  `:q!` → discard unsaved edits.
- **Focus back:** explicitly `window.focus(parent)` and collapse the parent's selection first (vim
  flips to Visual if it regains focus with a non-empty selection, `vim.rs:1581-1588`). Parent
  remembers `last_focused_descendant` across tab switches (`editor.rs:10972-11033`).
- **Edit existing:** "comment at cursor" = crate state (comment whose anchor row is the cursor row);
  `i`/`Enter` focuses its editor (InlineAssistant-style). Nearest-block lookup via
  `editor.row_for_block(id)` (`editor.rs:8926`).
- Rendering saved comments: `markdown::Markdown` + `MarkdownElement` (`crates/markdown/src/markdown.rs:654, 1756`).

### 3.6 Key contexts & keymaps

- Context chain when the comment child is focused:
  `Workspace … > Pane > CodeReview (our div) > Editor{mode=full; no vim keys while child focused} >
  ReviewComment > Editor{mode=auto_height, VimControl, vim_mode=…}` (addon contexts are only added to
  the focused editor, `editor.rs:2842-2848`).
- Bindings that fire only in the review editor's normal/visual mode (not in the child):
  `"CodeReview > Editor && mode == full && vim_mode == normal && !menu"` (and `== visual`).
  `A > B` = some ancestor A, deepest B; `!X` = X nowhere in the stack (`gpui/src/keymap/context.rs:272-320`).
- Our bindings override vim's (`c`, `shift-c`, `r`, `] f`, …) at the same depth, so they must load
  **after** `vim.json`: ship new files `assets/keymaps/code_review.json` (+ `code_review_vim.json`) and
  load them in `crates/zed/src/zed.rs` `load_default_keymap` (:2348), the vim one right after vim.json
  and gated the same way. **Don't** `cx.bind_keys` in init — keymap reload clears bindings
  (`zed.rs:2329-2336`).

### 3.7 Reviewed marks & file headers

- Per-file checkbox in file headers: `Addon::render_buffer_header_controls` (`editor.rs:784-816`,
  rendered `element/header.rs:805-813`, only the **first** addon renders — `.take(1)`); precedent
  `GitPanelAddon` (`git_ui/src/git_panel.rs:7995-8040, 9464-9487`).
- Fold reviewed files: `rhs_editor.fold_buffer(buffer_id)` (`editor/src/fold.rs:618-676`; mirrors to
  LHS in split). Per-hunk controls if wanted: `DiffHunkRenderer` (`editor/src/git.rs:25-41`;
  precedent `agent_ui/src/agent_diff.rs:742-938`).
- As built (M4): range folds are a no-op in split view (`display_map.rs:722`), so reviewed *hunks* are
  dimmed with row highlights instead of collapsed; stock `GoToHunk` walks through folded buffers, so
  `]c`/`[c` use our wrapper; a cursor inside a folded buffer resolves to the whole buffer (vim fix in
  §0 M4 decisions). Folded buffer ids survive excerpt removal (`block_map.rs:1247`), and our fold
  sync re-applies the state when excerpts reappear.

### 3.8 Workspace integration

- Item: template `git_ui/src/project_diff.rs` (register via `observe_new::<Workspace>` :78-115,
  reuse-or-create :146-165, **`act_as_type` returning the inner Editor** :547-571 so vim/search/etc.
  treat it as an editor, `as_searchable` :457). Serialization: `SerializableItem` +
  `workspace::register_serializable_item` (`workspace.rs:1309-1330`).
- Panel: `Panel` trait (`workspace/src/dock.rs:36-107`); add in `zed.rs::initialize_panels` (:778) for
  restore-on-startup. File tree: copy git panel's tree pattern (`git_panel.rs:864-1000, 8050-8160,
  8570`; `ListItem`, `indent_guides`, `FileIcons`, `git_status_icon`); vim-ish panel keys follow
  `vim.json` `ProjectPanel && not_editing` (`:1013-1061`, `menu::*` + `vim::MenuSelectNext`).
- Persistence: own DB domain `db::static_connection!(…, [WorkspaceDb])` (migrations self-register,
  `db/src/db.rs:29-52, 247-280`) or `KeyValueStore` for small state.
- As built (M4): `CodeReviewDb` (`persistence.rs`, no dependencies), migration 0:
  `code_review_sessions(review_key TEXT PRIMARY KEY, created_at, last_opened_at)`,
  `code_review_reviewed_files(review_key FK, path TEXT /*repo-relative unix*/, content_hash INTEGER
  /*NULL = adopt on next reconcile*/, PRIMARY KEY(review_key, path))`,
  `code_review_reviewed_hunks(review_key FK, path, hunk_key, PRIMARY KEY(review_key, path,
  hunk_key))`, all `STRICT`, FKs `ON DELETE CASCADE ON UPDATE CASCADE`. Writes `INSERT OR IGNORE` the
  session row first. Migration 1 (M5 part 2): `code_review_comments(review_key FK, comment_id INTEGER,
  data TEXT /*JSON*/, PRIMARY KEY(review_key, comment_id))`.
- Pickers: `picker::PickerDelegate` with multi-select (`picker/src/picker.rs:259-292`).
- Notifications/toasts: `workspace/src/notifications.rs`.
- Tests: `zed.rs::test_action_namespaces` (:5866-6005) must list `"code_review"`.

### 3.9 CLI glue (§2.3)

Hybrid: CLI flag → `zed://review` URL → interception in `handle_cli_connection` (no `CliRequest`
change, so mismatched CLI/app versions don't break).

1. `crates/cli/src/main.rs`: `Args.review: Option<String>` (must come before paths —
   `trailing_var_arg`, :101-102); when set, push `zed://review?repo=<abs>&target=<urlenc>` into
   `urls`, use the single positional (or cwd) as repo. ~25 lines.
2. `crates/zed/src/zed/open_listener.rs`: `OpenRequestKind::Review{repo, target}` (enum :49-83,
   manual Debug :85-123, parse :157-211 modelled on `git/commit` :263-283); at the top of the URL
   branch of `handle_cli_connection` (:598) route review URLs to `review_cli::handle(..)` which has
   `cwd`, `wait`, open behaviour and `responses`. ~25 lines.
3. `crates/zed/src/main.rs`: `handle_open_request` arm (template: GitCommit :1208-1257) for
   xdg-open/links. ~8 lines.
4. New `crates/zed/src/zed/review_cli.rs`: resolve repo toplevel; repo-aware window lookup (walk
   windows → `MultiWorkspace::workspaces()` → `project.repositories(cx)` matching
   `work_directory_abs_path`; fallback `workspace::open_paths`); await `wait_for_initial_scan`
   (`project.rs:2261`); call `code_review::open_review(..) -> (view, oneshot<ReviewOutcome>)`; `--wait`
   ping loop (copy `open_listener.rs:1061-1083`); `CliResponse::Stdout` + `Exit{status}`.
- Security: zed:// links can come from browsers — handler only reads/opens; never auto-posts.
- **As built (M8)**: step 4 lives in `code_review::cli` (`handle_cli_review`, `open_review_link`),
  not `crates/zed/src/zed/review_cli.rs`, so it is tested with the crate and `zed.rs` is untouched;
  `--review` also sends the repository's path (the trust signal), and step 3's arm calls
  `open_review_link`. Details: §0 "Milestone 8".

### 3.10 Glue footprint in upstream files (keep it this small)

| File | Change |
|---|---|
| `Cargo.toml` | workspace member + `[workspace.dependencies]` entry |
| `crates/zed/Cargo.toml` | dep on `code_review` |
| `crates/zed/src/main.rs` | `code_review::init(cx)` after `git_ui::init` (:776); `handle_open_request` arm (M8, +11) |
| `crates/zed/src/zed.rs` | panel registration in `initialize_panels`; keymap loading in `load_default_keymap`; test namespace list |
| `crates/zed/src/zed/open_listener.rs` | review URL kind + routing (M8, +27: `OpenRequestKind::Review` + Debug arm, `parse`, the CLI request → `code_review::cli::handle_cli_review`) |
| `crates/cli/src/main.rs` | `--review` flag (M8 + polish A, +58/−4: `mod review`, help example, the flag and its help and conflicts, `try_parse` (usage errors exit 1), link + path, the ping marker check, "Zed exited before the review finished") |
| `crates/cli/src/review.rs` | new file (M8): the `--review` request and `check_exit` (95 lines + tests) |
| `crates/editor/src/scroll.rs`, `scroll/autoscroll.rs`, `editor.rs` (`RowHighlightOptions`) | None any more: core patches #2 (`set_forbid_horizontal_autoscroll`) and #3 (`RowHighlightOptions::include_end_row`) went with the line cursor ("Upstream vim in the review tab"), #1 in polish B. `git diff v1.25.0-pre -- crates/editor/src/scroll.rs crates/editor/src/scroll/autoscroll.rs crates/editor/src/config.rs` is empty |
| `crates/editor/src/scroll.rs`, `crates/vim/src/normal/scroll.rs`, `crates/vim/src/motion.rs` | Core patch #8 ("`ctrl-u`/`ctrl-d` over blocks", +11/−1 in `scroll.rs`, +27/−1 in vim's `scroll.rs`, +9/−3 in `motion.rs`): `SharedScrollAnchor::scroll_top_display_point` returns the view's real top row (the anchor's row moved by the offset, `floor(scroll_position.y)`; the anchor's own point when that's its row, so nothing changes without tall blocks); vim's half-page/line scroll leaves a block row for the cursor the way the view scrolled (`Bias::Right` down, `Left` up), taking the other way only when that stays on screen and still goes with the scroll; `window_bottom` (`L`) adds only the offset's fraction. Tests in `code_review` (`test_half_page_scrolls_go_one_way`, `…_in_split_view`); vim's 577 and editor's scroll/split tests pass |
| `crates/editor/src/element/header.rs`, `editor.rs` | Core patch #9 ("Headers read as paths", +24/−15 in `header.rs`, +3 in `editor.rs`): `Editor::set_file_header_path_first` (and its getter): a file header draws the directory before the file name instead of after it; off by default, the review turns it on for both sides (`configure_side`) |
| `crates/editor/src/display_map/block_map.rs` | Core patch #5 (UX-1, +14/−1): `Block::is_above_start` (a custom block at `BlockPlacement::Above(Anchor::Min)`) sorts right after the start comparison in `sort_blocks`, so it comes before the first buffer's header and a folded first buffer (`dedup_by` keeps both), and `balancing_block` keeps `Anchor::Min` for its spacer on the other side (else an excerpt anchor after that side's header). Upstream places no block there (only a test on a headerless singleton). Tests +127 (`test_block_above_multibuffer_start`). "The overview is one block" (+14): `sync` extends an edit landing within a replacement block (a folded buffer) to the start of the replaced rows *before* preserving the transforms ahead of it, so the zero-row blocks right above (the overview) are rebuilt once instead of preserved and rebuilt; the test checks an edit within a folded first buffer |
| `crates/editor/src/split_editor_view.rs`, `element.rs` (`render_blocks`), `editor.rs` | Core patch #7 (UX-1, +149 in `split_editor_view.rs`, +13 in `element.rs`, +3 in `editor.rs`; bugs 2 item 2: +182/−41 in `split_editor_view.rs`, test +104 in `split.rs`): `Editor::set_full_width_block(Option<CustomBlockId>)`. When that editor is the right side of a split diff, its `EditorElement` skips the block (and hands a focused-block record for it on), and `SplitBufferHeadersElement::build_full_width_block` renders it with the block's own render callback across the overlay's width, measures it there (`resize_blocks`, which resizes the left spacer too), paints it under the file headers, and keeps it rendered below the view while it holds the focus. The overlay prepaints after both sides, so it lays the block out in its `request_layout` when it's in view (at the last frame's width, kept in element state) and only falls back to measuring in prepaint plus `request_animation_frame` otherwise. Unsplit, nothing changes. Tests in `split.rs` +217 with #5's (`test_block_above_start_leads_both_sides`, `test_full_width_block_in_split_view`) |
| `crates/editor/src/gutter_row.rs` (new), `element.rs` (prepaint/paint), `element/mouse.rs`, `editor.rs` | Core patch #6 (UX-1, for slice B's gutter "+": 119-line new file, +48 in `element.rs`, +14 in `mouse.rs`, +8 in `editor.rs`): `GutterRow`/`GutterRowRenderer` (`Rc<dyn Fn(&GutterRow, &mut Window, &mut App) -> Option<AnyElement>>`), `Editor::set_gutter_row_renderer`, `hovered_gutter_row`, `start_gutter_row_drag` + `GutterRowDragEnd`. The renderer's elements are prepainted with `Gutter::prepaint_button` for each visible row with a buffer row (both split sides, deleted rows too) and painted with the gutter indicators; `mouse_moved` tracks the hovered row (only while a renderer is set) and a drag's current row, `mouse_up` ends the drag. "Gutter buttons don't overlap" added `GutterRow::right_padding` (+6 in `gutter_row.rs`, +10 in `element.rs`): the gutter's right padding, between the line numbers and the text, as x offsets from the element's origin (`prepaint_button` draws it after the git strip), where the review puts its "+". Tests +152 in `editor_tests.rs` (`test_gutter_row_renderer_hover_and_drag`, which also checks `right_padding`). "Less work per keypress" added `LineNumberClick` / `Editor::set_line_number_click_handler` (+10 in `gutter_row.rs`, +28 in `element/mouse.rs`, +2 in `editor.rs`): a mouse down on a line number's hitbox goes to the handler (deferred past the editor's update) instead of a multibuffer's opening the file, so the review no longer draws an element over every row's line number to catch those clicks |
| `assets/keymaps/code_review*.json` | new files |
| `nix/build.nix` | `releaseChannel ? "nightly"` argument (M9, +30/−7): a `channel` table (stable/preview/nightly), an assert that only Linux builds other channels than nightly, and the channel in `version`, `preBuild`'s `RELEASE_CHANNEL` and the Linux icons and desktop entry. The default derivation is unchanged (same `.drv`); `crates/code_review/INSTALL.md` uses `.override { releaseChannel = "stable"; }` |
| `crates/settings_content/src/settings_content.rs`, `crates/settings/src/vscode_import.rs`, `assets/settings/default.json` | Review Pull Request picker: `SettingsContent::code_review` (`CodeReviewSettingsContent`: `pull_request_search`, `hide_approved_pull_requests`) in the struct and its `flattened_deserialize!` list (+21/−1), `code_review: None` in the VS Code import (+1), the defaults (+11). Editing `settings_content` rebuilds most of Zed |

Measured cost of touching core crates: editing `editor` rebuilds in ~4.6 min at `-j4` vs ~38 s for a
leaf crate (§8.1). Avoid core edits beyond this list.

---

## 4. GitHub integration details

Port behaviour (not code) from tuicr's `gh`-based backend; implement with `HttpClient`.

| Need | tuicr reference | Notes |
|---|---|---|
| PR list (open / review-requested) | `src/forge/github/gh.rs` (`gh pr list --json number,title,author,headRefName,baseRefName,updatedAt,url,state,isDraft`; `--search review-requested:@me`) | GraphQL `search` or REST |
| PR details + info panel | `gh.rs`, `pr_info.rs` (`headRefOid, baseRefOid, body, closed, mergedAt, reviewDecision, mergeable, mergeStateStatus, reviewRequests, latestReviews, statusCheckRollup, comments`) | v1 shows description; checks/reviewers in the top block |
| Fork-PR detection / fork parent | `src/forge/canonical.rs:48` (`repos/o/r .parent`) | decides `refs/pull/N/head` fetch |
| Review threads | `src/forge/github/review_threads.rs:244` (`reviewThreads{isResolved isOutdated path line originalLine diffSide comments{…}}`) | anchors are on the **thread**, not the comment; tuicr doesn't fetch `startLine` (fetch it) |
| Review summaries / bots | `src/forge/github/review_summaries.rs` (`__typename`, `is_bot_login`) | bots hidden by default |
| Viewer + last-review commit | `src/forge/github/review_metadata.rs` | deferred (since-last-review) |
| Create review | `src/forge/github/submit.rs:17` `build_review_payload`; `gh.rs:571` | REST `POST repos/{o}/{r}/pulls/{n}/reviews` `{commit_id, body, event, comments:[{path, line, side, start_line?, start_side?, body}]}`; file comments: not possible there (LV L10): pending review, then `addPullRequestReviewThread(subjectType: FILE)` per file, then `/events` (LV §5.3) |
| Error handling | `gh.rs:1050` (permission / pending-review / commit_id hints) | a user can have only one pending review per PR |

Gotchas carried over: `commit_id` must be the reviewed head (stale-head check before submit); GitHub
renders PR diffs from the merge base (three-dot); >300-file PRs are a non-issue locally.

---

## 5. Data model sketch (implementation choice — adjust freely)

```rust
enum ReviewTarget { Pr { repo: ForgeRepo, number: u64 }, Branch { base: String },
                    Uncommitted, Commits { start: Oid /*parent*/, end: Oid } }
struct ReviewKey(..)   // repo + target identity (PR number / branch+base / worktree / range); not head SHA
struct DraftComment { id, kind: Line{path, side: Old|New, rows: Range<u32>} | File{path} | Review,
                      comment_type: Option<CommentType>, body, line_snapshot: String,
                      state: Draft | Submitted{review_id} | Outdated, anchor: Option<Anchor> /*runtime*/ }
struct ReviewedState { files: HashMap<Path, content_hash>, hunks: HashSet<HunkKey> }
```

tuicr's hunk keys survive line shifts: `hunk-content-v1:{hash}:0`, or a span-qualified key when hunk
content repeats (`src/model/review.rs`).

---

## 6. Milestones

1. **Skeleton:** crate + glue (`init`, empty ReviewView item, empty panel, keymap files loaded); action
   `code_review::ReviewUncommitted` opens a read-only SplittableEditor diff (copied DiffMultibuffer).
2. **Line cursor + navigation:** §3.4; `]f/[f/{}`, `]c/[c`, `]u/[u`, `h/l`, focus mode, `e`.
3. **Targets:** branch vs base (+ base picker), commits (live + historic buffers), StartReview picker.
4. **Panel:** file tree, reviewed marks (+ header checkbox, folding, `r`/`R`, invalidation), filters,
   path search, `.gitattributes` hiding, hide-reviewed.
5. **Comments:** line/range/file/review-level, vim child editors, save/leave semantics, types, `dd`,
   `]t/[t`, comment list, old-side comments in both views, persistence in Zed DB, outdated flagging.
6. **Export:** copy as markdown (`y`), `Y`, visual `y`.
7. **GitHub:** auth, PR list/picker, checkout (same-repo/fork/worktree), description block, threads
   (read-only, filters, bots hidden), submit modal + review creation.
8. **CLI:** `--review`, `zed://review`, `--wait` + exit codes.
9. **Fork ops:** rebase skill, flake install wiring in the user's NixOS config (ask the user where).
   Done: `.claude/skills/rebase-code-review`, `releaseChannel` in `nix/build.nix`,
   `crates/code_review/INSTALL.md` (the `~/nixconf` edits, documented; applying them awaits the
   user).

Post-v1: thread replies/resolve → suggestion blocks → §2.9 list.

---

## 7. Open technical questions / risks (decide during implementation)

- ~~Which context lines GitHub accepts~~ — answered (LV §5.2, `plans/M7-live-verification.md`): only
  rows inside GitHub's own patch hunks (context rows on either side), ranges may span hunks; built
  into `CommentableLines` (M7a).
- ~~Exact GitHub body requirement~~ — answered (LV §5.1): `body_required = comment_count == 0 && event
  != Approve` (slice d).
- Upstream `diff-review` prototype: keep the flag off; watch for upstream turning it on in diff views
  (its gutter "+" would appear in our editor if we used `set_show_diff_review_button`; don't).
- `HighlightKey` is a closed enum (`display_map.rs:160-193`) — use row highlights (TypeId-keyed) only.
- Spacer rows can't receive the mirrored cursor highlight without a core change.
- vim `ctrl-d/u` vs tall blocks (§3.4).
- Zed's `minimum_split_diff_width` auto-toggling recreates the LHS — test comment re-hosting heavily.
- Release channel comes from `crates/zed/RELEASE_CHANNEL` (`stable` on stable tags, `preview` on
  `-pre` tags, `dev` on main). Building from `main` gives a dev-channel app (separate DB, no CLI
  socket) — another reason to base the fork branch on stable tags. The Nix package overwrites it
  (`nightly` unless `releaseChannel` says otherwise, M9).
- Remote (SSH) projects: `Repository::send_job` local-only path; v1 is local-only.

---

## 8. Build, dev loop & benchmarks

### 8.1 Measured (2026-10-16, Ryzen 7 7800X3D, `cargo build -j4 --profile release-fast -p zed`, `nice 19`, `CARGO_PROFILE_RELEASE_FAST_DEBUG=limited`)

| Phase | Time |
|---|---|
| Clean build (v1.24.2) | 20m10s |
| Leaf crate edit (`journal`, stand-in for `code_review`) | 38s |
| `zed` crate edit (glue) | 37s |
| No-op | 1s |
| One-week upstream bump v1.24.2 → v1.25.0-pre (58/249 crates changed incl. util/gpui/editor; 271 crates recompiled; 19 third-party bumps) | 12m23s |
| Core `editor` crate edit | 4m37s |

`target/` was 24 GB. Raw numbers + `--timings` HTML in `research/`. Note these were `-j4` (session
cap); the user's normal builds use all cores.

### 8.2 Environment

- Toolchain: Rust 1.98.1 (`rust-toolchain.toml`); the user's system nixpkgs also has rustc 1.98.1.
- Dev shell: Zed's default `nix develop` shell (`devShells.default`), loaded by direnv via `.envrc`
  (`use flake`; gitignored).
- The devshell's `cargo` is a node wrapper (`script/cargo`) that writes timing files to Zed's data dir;
  the real cargo is `$(rustc --print sysroot)/bin/cargo`.
- User's nix.conf: `cores = 0`, `max-jobs = 16` (all cores by default); trusted user (wheel); has a
  personal Cachix cache `bizmythy-nixconf.cachix.org` (option for CI-built binaries later).
- Zed's upstream CI does **not** push release nix builds to `zed.cachix.org` (nix builds only run on
  labelled PRs), so there's no upstream binary cache to lean on.
- The user's Zed config: `~/.config/zed/` (vim_mode on, `base_keymap: VSCode`, catppuccin, nushell
  terminal, direnv `shell_hook`).

### 8.3 Warm build cache

`~/personal/zed/target/` contains the benchmark's artifacts for `--profile release-fast` with
`CARGO_PROFILE_RELEASE_FAST_DEBUG=limited`, built from a clone of v1.25.0-pre inside the Zed devshell.
Third-party crates (the bulk of the 20-min clean build) should be reused **only if you build with the
same profile, the same env var, and the same devshell**; workspace crates recompiled once on the
`code-review` branch (6m05s at `-j8`, their package IDs include the old checkout path). The
verified, CPU-capped commands are in §0 ("Dev loop").

If you choose a different profile (e.g. `dev`), the cache gives nothing; delete stale dirs with
`cargo clean --profile release-fast` when disk matters.

---

## 9. tuicr UX reference map

Read for behaviour, not code (`~/personal/tuicr`, branch `bizmythy-tweaks`):

- Keys & modes: `docs/KEYBINDINGS.md`, `src/input/keybindings.rs:205-262` (normal mode).
- Document model & rendering order: `src/app/annotations.rs:80-401`; line cursor & decorations
  `src/app/mod.rs:260-327, 525`; fork scroll tweak `a3ac1a3`/`98a1fb4`.
- Reviewed state & hashing: `src/model/review.rs:164-189`; focus-mode advance `f40e649`.
- Comments: `src/model/comment.rs:154-195`; comment editor (vim via embedded nvim in fork `2e40afa`).
- Export format: `src/output/markdown.rs`; AGENTS.md "Review Format".
- Submit: `src/forge/submit.rs` (preflight, `build_review_body` :568); `src/forge/github/submit.rs`.
- Generated/vendored hiding: `src/gitattributes.rs` (fork `2dddaae`, `a1cb3df`; reads `.gitattributes`
  from the PR version).
- Bot filtering: fork `050f0f8`.
- Excluded: review guide/reordering `5ea82dc` (`src/review_guide/`).
