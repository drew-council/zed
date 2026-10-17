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

**Status: milestones 1–7 done: M4 (panel, reviewed marks, filters), a hardening pass over the M1–M4
code reviews (see "Hardening" below), M5 (comments: part 1 "comments core", part 2 "old side,
persistence, panel list" with the M5 part 1 review fixes), M6 (export, see "Milestone 6") and M7
(GitHub): slice (a), the GitHub client (see "Milestone 7, slice (a)"), is done, followed by a second
hardening pass over the M5 part 2 + M6 review (see "Hardening 2"), then slice (b), pull request
reviews from the picker with checkout in place or in a new worktree (see "Milestone 7, slice (b)"),
and slice (c), the pull request's description and its review threads read-only in the diff, with
GitHub's comment gate (see "Milestone 7, slice (c)"), then a third hardening pass over the M7 (a) +
(b) review: token scoping, safe checkouts, Restricted Mode (see "Hardening 3"), then slice (d),
submitting reviews, with the M7 (c) review's fixes (see "Milestone 7, slice (d)"). M7 is complete;
M8 (CLI) is next.** The
long-lived fork branch `code-review`
exists locally (not pushed): upstream tag `v1.25.0-pre` + the fork commits. The fork's `main` is
untouched and equals upstream `main`. Review tabs exist for uncommitted changes, the branch vs a base
(changeable in place), and one commit or a contiguous range (live when it ends at HEAD, a read-only
historic snapshot otherwise), and a GitHub pull request (checked out in place, Zed's way, or in a new
`pr-N` worktree), opened from the `code_review::StartReview` picker or the direct actions
`ReviewUncommitted` / `ReviewBranch { base }` / `ReviewCommits { commits }` / `OpenPullRequest {
reference }`. Each tab has a
tuicr-style line cursor and file navigation, a fixed top block (branch base, or the commit messages),
per-file and per-hunk reviewed marks persisted in Zed's DB (reviewed files fold to their header,
reviewed hunks are dimmed, marks stop counting while the diff content differs), and per-tab filters (include /
exclude regexes, hide reviewed, generated/vendored files from `.gitattributes` hidden by default). The
review panel shows the active review's file tree with progress, filters, path search and a context
menu, and below the tree the review's comments. Comments on lines and ranges of either side (`c`;
old lines on the left side of a split diff or the removed rows of a unified one), files (`C`) and the
whole review (`;c`) open vim editors in insert mode, render as markdown, have types, and are edited,
deleted and navigated with `i`/`Enter`, `dd` and `]t`/`[t`/`m`/`M`; they are stored in Zed's DB per
review, follow the code while the review is open and are flagged outdated when their lines changed
while it was closed. `y` copies the review as Markdown in tuicr's format, `Y` the comment at the
cursor, visual `y` the selected lines. A GitHub client resolves a checkout's GitHub repository (a
fork's parent), lists and reads its pull requests, threads, reviews and commentable lines; the
picker lists pull requests; a pull request review shows the description and review summaries in its
top block, the existing review threads read-only below their lines (resolved, outdated and bot
threads hidden by default, with toggles; GitHub's line numbers are translated through the
checkout's uncommitted and unsaved changes), and only takes comments on lines inside GitHub's diff
hunks, with the pull request's head checked out. `; s` submits a pull request review (Comment /
Approve / Request Changes, from a modal that shows what is sent and what blocks it) or finishes a
local one (its Markdown to the clipboard, or to a `--wait` waiter). Next: M8 (CLI).

What exists:

| Path | What |
|---|---|
| branch `code-review` | `v1.25.0-pre` + fork commits; all feature work goes here |
| `crates/code_review/DESIGN.md` | This document |
| `crates/code_review/src/code_review.rs` | `init` (registers actions per workspace), all `code_review::` actions (incl. `ReviewBranch`/`ReviewCommits` with fields), keymap loading helpers, test helpers (`init_test`; `test_support`: `init_vim_test`, the review fixture, editor helpers, fake-repo commit/branch helpers like `record_commit`) |
| `crates/code_review/src/target.rs` | `ReviewTarget::{Uncommitted, Branch { base: BranchBase }, Commits(CommitRange), PullRequest(PullRequestTarget)}`, `BranchBase` (the only way to build a branch base: syntax check), `is_same_review`, `review_key`, empty-state label, `parse_commit_spec` (`rev` / `A..B`, reused by M8's CLI), `PullRequestReference` + `parse_pull_request_reference` (M8 reuses it), `validate_branch_name` (forge branch names) |
| `crates/code_review/src/commits.rs` | `CommitDiffList` (a commit range's files: live over project buffers, or historic snapshot buffers), `GitBlob`/`build_buffer` copied from `CommitView`, git helpers over `Repository::send_job` (`first_parent_chain`, `resolve_commit_spec`, `check_branch_base`, `resolve_commits`) |
| `crates/code_review/src/diff.rs` | `ReviewDiff`: copy of `git_ui`'s `DiffMultibuffer`, driven by a `DiffSource` (`DiffBufferList` for uncommitted/branch, `CommitDiffList` for commits); `ReviewAddon` (header statuses against the review base + the reviewed checkbox: every header control goes here); loading/error state; `set_branch_base`; file order, `path_at_cursor`, `move_to_path`, single-file mode; per-file `FileDigest`s, the filter-hidden set (`set_hidden`), `ReviewDiffEvent` |
| `crates/code_review/src/session.rs` | `ReviewKey`, `ReviewSession` (reviewed files/hunks and the `CommentStore` of one review, shared by its tabs through the `ReviewSessions` global; marks and comments loaded from/saved to the DB, positions flushed on close/quit, old reviews pruned; M7d: `mark_submitted`, `ReviewOutcome` + `wait_for_outcome`/`has_outcome_waiters`/`resolve_outcome` for M8's `--wait`), FNV-1a `file_digest` (content hash + hunk keys), `reconcile` |
| `crates/code_review/src/comments.rs` | Comment model: `CommentId`, `Side`, `CommentType` (cycle, labels, colors, legend definitions), `CommentTarget` (`Review`/`File`/`Lines`), `CommentState`, `Comment` (`location()`, JSON for the DB), `CommentStore`, `CommentEvent`; unit tests |
| `crates/code_review/src/comments/blocks.rs` | `CommentBlocks` (one per review tab): a `CommentView` per comment, the comment and thread blocks on both sides of the diff (`ItemKey::{Remote(RemoteKey), Comment(CommentId)}`, `set_remote_items`/`RemoteItem`, `reconcile`, `placement`, `line_row_anchor`, `old_row_in_unified_diff`, `block_below_row`, re-hosting when the left side is recreated), the tab's own anchors (`TabAnchors`/`LineAnchor`, parked per session on a key switch) with the outdated check, deleted-line detection and live line numbers (`update_anchors`), comment editors (incl. the `vim::VimSave` capture) (`begin_edit`, `leave`, focus return + guard), the comment keys (`c`/`C`/`;c`/`i`/`dd`/`]t`), comment at cursor, `Y`/visual `y` text; comment GPUI tests |
| `crates/code_review/src/comments/view.rs` | `CommentView`: the comment card (type and OUTDATED/SUBMITTED badges, location, edit/delete buttons, markdown or the editor) and its `ReviewComment` key context with the leave/keep actions |
| `crates/code_review/src/comments/remote.rs` | M7c: `RemoteThreadView` (a pull request's review thread, or a file's outdated/moved threads, read-only: badges, location, comments with author/time/markdown, "…N more on GitHub", Open on GitHub), `RemoteThreadContent`, `thread_location`; M7d: `open_web_link` (only http(s) opens), `HoveredLink` + `untrusted_markdown` (links show their destination while hovered) |
| `crates/code_review/src/submit.rs` | M7d: `comment_body`/`review_body` (`[TYPE] ` prefixes), `needs_body` (LV §5.1), `plan_review` → `PlannedReview` (the payload mapping, counts, outdated and refused drafts), `SubmitReviewModal` (events, blockers, warnings, body prompt, submit + auth retry, success), `finish_local_review`; payload/flow/modal tests |
| `crates/code_review/src/export.rs` | M6: `review_markdown` (tuicr's Markdown export), `ExportPreamble`; golden tests |
| `crates/code_review/src/forge.rs` | M7: forge-neutral pull request types (`ForgeRepo`/`ForgeRepoRef`, `RepositoryContext`, `PullRequestSummary`/`Details`, `HeadStatus`, `RemoteThread` + `ThreadAnchor`, `RemoteReviewSummary`, `ChangedFile` + `Hunk`, `CommentableLines` with `allows`/`allows_range`/`current_path`; M7d: `ReviewEvent`, `ReviewSubmission`, `ReviewLineComment`/`ReviewFileComment`, `SubmittedReview`), the `Forge` trait (with `submit_review`), `resolve_forge`, `error_message` / `github_error` / `show_error_toast` |
| `crates/code_review/src/github.rs` | M7: `GitHubError` + hints, endpoints, host rules (`GitHubHost::{Cloud, Configured, Unverified}`, `GitHubHosts`: user-configured providers, registry hosts, SSH aliases via `ssh -G`), remote parsing (`remotes`: the checkout's GitHub remotes, preferred first), token lookup (gh's rules) and cache per (host, work dir), `GitHubClient` (GraphQL/REST over `HttpClient`, paging), response parsing (threads, summaries, files/patch hunks), `GitHubForge`, the forge cache (keyed by every remote URL); `test_support` (fake GitHub `HttpClient`, fixtures, `FakeLookups`) and tests, an ignored live test |
| `crates/code_review/src/pull_request.rs` | M7b: `PullRequestState` (forge + details, one per review key through the `PullRequests` weak global), `open_pull_request` / `OpenPullRequestOptions` (the entry point M8 calls; one at a time per repository, `OpeningPullRequest`), the in-place flow, `plan_worktree_fetch`, `trusts_head_repository` and the new-worktree flow (own `git worktree add --detach`, `trust_worktree`, `repository_at`). M7c: threads, summaries, `CommentableLines`, `ThreadFilters`, `load`/`reload` (+ forge re-resolution after auth errors, from any open checkout: `add_checkout`, `resolve_forge`, `use_forge`), `PullRequestEvent`, `place_thread`/`ThreadPlacement`, `check_comment` (the per-tab gate)/`check_comment_target`; M7d: `LocalChanges` (per tab: each file's diff against HEAD, `head_to_working`/`working_to_head` → `LineMapping`); PR review GPUI tests and their `Fixture` |
| `crates/code_review/src/checkout.rs` | M7b: `CheckoutStep`, `LocalState` (`read_local_state`/`local_state`: HEAD, base present, local branches + upstreams, read from git), the pure `plan_checkout`, `head_remote`, `RestrictedMode` + `ensure_trusted`, `run_checkout` (Zed's fetch and branch switch, plus our own git jobs: `fast_forward` with the branch check, `ensure_fast_forward`, detach; `run_git` with Zed's safety flags and askpass), the askpass delegate; planner and real-git tests |
| `crates/code_review/src/github/queries.graphql` | M7: every GraphQL operation in one document (sent whole with `operationName`) |
| `crates/code_review/test_data/github/*.json` | M7: trimmed live responses from the sandbox repo (PR A #1 threads/summaries/details/head/files, PR D #4 files, the open list, the `drew-council/zed` fork context, a NOT_FOUND) |
| `crates/code_review/src/persistence.rs` | `CodeReviewDb` DB domain (migration 0: `code_review_sessions`, `code_review_reviewed_files`, `code_review_reviewed_hunks`; migration 1: `code_review_comments`), queued writes, `prune`, `sync` barrier |
| `crates/code_review/src/gitattributes.rs` | `.gitattributes` parser/loader for `linguist-generated`/`gitlab-generated`/`linguist-vendored` (tuicr's rules, via `ignore`'s gitignore matcher) |
| `crates/code_review/src/reviewed.rs` | Header checkbox, `ReviewHunkRenderer` (hunk control "Mark Reviewed"/"✓ Reviewed"), dimmed reviewed-hunk painting; reviewed-mark GPUI tests |
| `crates/code_review/src/top_block.rs` | `ReviewTopBlock`: fixed header above the diff — branch base line with a Change Base button, the commit messages (collapsible), or the pull request's block (header, warnings, description and review summaries, thread toggles, load errors; collapsible, expanded by default); then the review-level comments; commit tab titles |
| `crates/code_review/src/picker.rs` | `StartReviewModal` / `StartReviewDelegate`: Local section (uncommitted, branch vs base, recent commits with range multi-select), Pull Requests section (open / review requested, open by reference, "Review in Worktree"); `CommitSelection`; the `OpenPullRequest` action |
| `crates/code_review/src/line_cursor.rs` | `LineCursor`: hides the caret, paints cursor/visual rows (mirrored in split view), horizontal scroll |
| `crates/code_review/src/view.rs` | `ReviewView` workspace item (key context `CodeReview`), reuse-or-create `deploy`/`deploy_in_repo`, titles, Change Base (`open_base_picker`/`select_base`/`change_base`), handlers for the review keys and the direct actions, `FocusDiff`; its session, filters (`ReviewFilters`), attributes, fold transitions, `r`/`R`/`]c`/`[c`, the single-file bar, the panel's read API (`panel_files`, `summary`); GPUI tests |
| `crates/code_review/src/panel.rs` | `ReviewPanel` (left dock): follows the active review tab; file tree (compacted directories, status letters, ✓, icons, indent guides), header with progress/status line/menu, filter and search prompts, context menu (pull request entries in PR reviews), empty states, the comment and thread list; workspace-level filter/copy actions (`CopyPullRequestUrl` too); GPUI tests |
| `assets/keymaps/code_review.json`, `code_review_vim.json` | Built-in keymaps (non-vim: `tab` → `picker::MultiSelectNext` and `ctrl-r` → `TogglePullRequestFilter` in the Start Review picker, the review panel's keys, `ctrl-enter`/`ctrl-s` in comment editors; the review tab's keys are vim-only) |
| `crates/code_review/research/benchmark-results.txt` | Measured build times (see §8) |
| `crates/code_review/research/cargo-timing-*.html` | `cargo --timings` reports for the clean build and the one-week bump |
| `script/live-harness/README.md` | `zh`: headless real-GUI Zed harness (Xvfb, input, screenshots) for agent verification; costs CPU, use only when a slice asks |
| `target/` (gitignored) | **Warm build cache** (see §8.3): `release-fast` for `-p zed` plus `debug` test artifacts, 43 GB |
| `.rules` ("Drew's Zed fork" section) | Pointer to this file for agent sessions (`CLAUDE.md`/`AGENTS.md` symlink to it) |
| `README.md` first two lines | `> [!IMPORTANT]` banner required by the `.rules` HARD RULE; never remove it (the user does) |

Upstream glue so far (all append-style, §3.10): root `Cargo.toml` (member + workspace dep),
`crates/zed/Cargo.toml` (dep), `crates/zed/src/main.rs` (`code_review::init` after `git_ui::init`),
`crates/zed/src/zed.rs` (`ReviewPanel::load` in `initialize_panels`; `code_review::key_bindings`
after the base keymap and `code_review::vim_key_bindings` right after `vim.json` in
`load_default_keymap`; `"code_review"` in `test_action_namespaces`). Core patches #1 and #2 are
applied (`crates/editor/src/config.rs`, `scroll.rs`, `scroll/autoscroll.rs`; §3.10). M3 and M4 added
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
  language settings first, which may change nothing).
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
  tab title, plus "Read-only snapshot"), expanded = up to 100 commits newest first with sha, subject,
  author, date and the markdown body (16 lines, then it scrolls; "…and more commits" past the limit).
  Commit messages can't be commented on. `ToggleHeader` has no default key.
- **Titles**: "Review: Uncommitted changes", "Review: ‹branch› vs ‹base›", "Review: ‹sha7› ‹subject (20
  chars)›", "Review: ‹n› commits ‹oldest7›–‹newest7›" ("‹n›+ commits up to ‹newest7›" when truncated;
  before the commits load, the head's short sha). Snapshot tabs add "Read-only snapshot (no language
  server)" to the tooltip. The panel shows the same title and follows changes.
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
  "✓ Reviewed" with the `R` binding.
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
  ;f") plus "single file" in the panel's status line.
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
  new comment and at the start for `i`/`Enter` (tuicr's `i`). UI font, like the rendered markdown
  (`hover_markdown_style`, as the top block).
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
- **Types**: `tab`/`shift-tab` in the editor's vim normal mode (insert-mode `tab` is text), untyped →
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
  line and file comments in diff order (file, file comments first, then last line, then id), wrapping;
  review-level comments, filtered files and lines outside every excerpt are skipped; single-file mode
  switches files; folded files unfold. From a plain row, a comment below that row is the next one.
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
  over removed and added rows still comments on the added ones. Card labels "Old line N" / "Old lines
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
  exist and their text equal `line_text`, else the comment becomes `Outdated` (stored; sticky until `dd`)
  on the clamped rows: an OUTDATED badge with a tooltip, "outdated" in the panel, ` (outdated)` in the
  export, still navigable. Other tabs anchor it on its lines without comparing. A comment whose lines
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
  enough); ranges may span hunks. Paths: a renamed file is found by its old path too;
  `current_path` gives the new path for submit. `contains_file` is true for files of the PR, and for
  any file when the list is `truncated` (they might be in the PR; lines stay refused).
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
  line. A `git stash` while the file stays in the review outdates its comments (sticky after the pop).
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
  scroll area of 12 lines** (deviation from the plan's 16: the diff kept too little room in a
  1000-pixel window) with the markdown description ("No description provided." when empty) and the
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
  delete is logged; the first error is returned). 422s of a review become `GitHubError::
  ReviewRefused`, whose hints add the pull request's URL (pending review) and the short commit
  (`commitOID`). A 401 resolves the forge again (`PullRequestState::resolve_forge` from any open
  checkout, `use_forge`) and retries once.
- **Success**: `ReviewSession::mark_submitted(ids, review id)` (stored), `PullRequestState::reload`,
  `ReviewOutcome::Submitted { url }` for waiters, a toast "Review submitted: ‹url›" with **Copy URL**
  (not auto-hiding). Submitted comments leave the diff, `]t` and the top block
  (`CommentBlocks::is_shown`; their threads come back with the reload); the panel still lists them,
  "submitted", and a click says the thread is on GitHub.
- **Outcome hooks for M8** (`session.rs`): `ReviewOutcome::{Submitted { url }, Finished { markdown
  }}` (pub), `wait_for_outcome() -> oneshot::Receiver` (canceled = the session was released = every
  tab closed: M8's `Closed`), `has_outcome_waiters`, `resolve_outcome`. A session re-key (Change
  Base, branch switch) doesn't move waiters yet: M8 must.
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

**Known issues / notes for the next slices:**

- **Notes for M8 (CLI)**:
  - `--wait` outcomes: `ReviewSession::wait_for_outcome()` (crate-visible) resolves with
    `ReviewOutcome::Submitted { url }` after a pull request review is posted, or `Finished { markdown }`
    after `; s` in a local review (`None` without comments); a canceled receiver means every tab of
    the review closed (`Closed`, exit 2). `finish_local_review` already sends the Markdown to a waiter
    instead of the clipboard. Move waiters to the new session when a tab's key changes
    (`ReviewView::update_session`), or a branch review whose base changes reports Closed.
  - The modal never opens from a link (`zed://review` only opens and reads): posting needs `; s` and
    Enter.
  - Sandbox for the final validation: PR A #1 (read-only), PR C **#6** (own, clean), PR B **#7**
    (bot, clean), PR D #4 (API probes).
- **Comment editors, known gaps**: every `:w` form leaves the comment keeping its text (see "Hardening
  2"); none writes a file. `:qa`, `:qa!`, `:wqa`, `:xa` close the review tab(s); open comment text is
  saved even by `:qa!`. Two tabs editing one saved comment: last writer wins. Two windows whose buffers
  differ (unsaved edits) disagree on a comment's line numbers: each places it by its own buffer, the
  stored numbers are the last mover's.
- Split view: a block below a *changed* row is still mirrored below the start of its hunk, so rows inside
  a multi-row hunk don't face each other until the hunk ends; a block below the last unchanged row of an
  excerpt is mirrored above the facing row (no next row to anchor to). Both are upstream
  `balancing_block` behaviour.
- Harness: `zh type` turns newlines into a key that isn't Enter for Zed; use `zh keys Return`.
- **Empty-row bleed** (visible in the live check): on an empty row directly followed by a spacer — the
  last empty line of a file before an added/deleted file, or a blank context line before a pure
  addition — the cursor highlight also covers the spacer rows. `highlight_rows` excludes an end at
  column 0 and an empty row has no other column, so its range must reach the next row. Proposed core
  patch #3 (not applied, needs approval): in `Editor::highlighted_display_rows_in_range`
  (`editor.rs:9483` on `code-review`) only exclude the end row when `end.row() > start.row()`, so an
  empty range highlights its own row; `line_cursor::highlight_run` would then pass `start..start` for
  an empty last row. Comment blocks are opaque, so they hide it.
- Upstream quirks seen live: vim relative line numbers apply to the focused side only and count across
  file sections; on the LHS the current line shows 0 instead of its number; after toggling soft wrap in
  split view the LHS is mis-laid-out until the next frame.
- M4 cosmetics: the line cursor's colors blend the full diff color, also on dimmed rows; the file row
  tooltip can overlap the panel's context menu; regex errors show the regex crate's multi-line message;
  `e` on a folded header opens the file at its last row. Header controls go only through `ReviewAddon`
  in `diff.rs`.
- Multi-cursor commands (`g l`, `g a`) only show the newest selection.
- Not run: `zed`'s own tests (incl. `test_action_namespaces`). `cargo test -p code_review`: 152 tests
  + 1 ignored live GitHub test (after M7 slice d; 136 after hardening 3, 127 after M7 slice c, 120 after slice b, 113 after hardening 2, 110 after
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
  onto `v1.25.0` once that stable tag ships; until then `crates/zed/RELEASE_CHANNEL` is `preview`.
- Agent build cap: **`-j8` under `nice -n 10`** for every cargo invocation. Never run cargo without
  these limits. This applies to agent sessions only; the Nix package build stays uncapped (§2.1).
- Core patch #2 (horizontal-autoscroll opt-out in `editor`) accepted; batch core edits so `editor`
  rebuilds once.
- M2 (orchestrator): `]f`/`[f` stop at the ends and `]u`/`[u` wrap, as in tuicr; `h` at the left edge of
  the right side in split view focuses the panel; no counted `3}`-style jumps (no vim patch).
- M7 (user): a fork's checkout lists the upstream parent's pull requests; bot threads hide behind the
  bot-summaries toggle; live GitHub writes only in `drew-council/code-review-sandbox`; submit is `; s`
  in the review tab (plus the palette).
- M7 slice (d) (agent, from M8's plan D3, can be vetoed): `; s` in a local review finishes it, its
  Markdown to the clipboard or to a `--wait` waiter.

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
- **Build & run** (the warm-cache profile):

  ```sh
  direnv exec /home/drew/personal/zed env CARGO_PROFILE_RELEASE_FAST_DEBUG=limited \
    nice -n 10 cargo build -j8 --profile release-fast -p zed
  ./target/release-fast/zed
  ```

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
  (backup branch, review each commit, drop obsolete ones, `range-diff`, run checks).
- **Install:** the fork's own Nix flake (Zed's standard crane build, `packages.default`) replaces
  nixpkgs' `zed-editor` as the user's only Zed. Use the normal Nix build settings — no `-j` caps.
  Built from a stable tag, `crates/zed/RELEASE_CHANNEL` is `stable`, so it shares the user's
  existing settings/keymap/DB and the CLI single-instance socket works (dev-channel builds do not
  start the socket listener — `crates/zed/src/main.rs:360-364`).
- Settings: no `code_review` settings block until the first setting needs to persist; then add it
  (requires editing `crates/settings_content`, accepted).

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
  a collapsible block with the **PR description** rendered with Zed's markdown (or the commit
  message(s) for commit reviews — not commentable), followed by the **review-level comments**
  section. Reviewed files fold to their header. **Single-file focus mode** toggle (continuous view is
  default). Multiple review tabs may be open; the panel follows the active one.

### 2.5 Movement & keys (review tab)

**Line cursor, not a character cursor** (tuicr feel). The user was emphatic: in the review tab you
move line by line; there is no column.

- Visual: no caret, no char-level selection; the current line is a strong **full-width row highlight
  incl. gutter**, mirrored on the other side in split view; visual mode highlights whole rows.
- Under the hood: **Zed's real vim**; the column exists but is never shown (pinning it breaks `n`, §0). Counts, `{N}G`, `gg`/`G`,
  `ctrl-d/u/f/b/e/y`, `zz/zt/zb`, `H/M/L`, `/` + `n`/`N` (lands on the row) all behave exactly as in
  Zed. Character/word motions (`w b e f t 0 $ ^ …`) do nothing in the review tab.
- `h`/`l` scroll the view horizontally (tuicr: 4 columns); `h` at the left edge focuses the panel.
- `v` and `V` both start line-wise visual.

| Keys | Action |
|---|---|
| `]f` / `[f`, `}` / `{` | next / previous file |
| `]c` / `[c` | next / previous hunk (Zed vim default) |
| `]t` / `[t`, `m` / `M` | next / previous comment thread (gives up vim marks in the review tab) |
| `]u` / `[u` | next / previous unreviewed file |
| `c` (normal / visual) | comment on line / range |
| `C` | file comment |
| `;c` | review-level comment (tuicr leader `;`, dead with a line cursor) |
| `i` / `Enter` | edit the comment at the cursor line |
| `dd` | delete comment at cursor (with confirmation) |
| `r` | toggle file reviewed and advance to next file |
| `R` | toggle hunk reviewed |
| `e` | open file at cursor line in a normal editor |
| `y` / `Y` / visual `y` | copy review as markdown / copy comment at cursor / copy selected source lines |
| `;f` / `;e` / `;h` / `;l` | focus mode / toggle panel / focus panel / focus diff |
| `a`, `o`, … | no-ops (would otherwise enter a useless insert mode on a read-only buffer) |

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

### 3.1 Crate layout (proposed; as of M7 `session.rs`, `persistence.rs`, `gitattributes.rs`, `reviewed.rs`, `comments.rs` + `comments/{blocks,remote,view}.rs`, `export.rs`, `forge.rs`, `github.rs` + `github/queries.graphql`, `pull_request.rs`, `checkout.rs`, `submit.rs` exist too; `cli.rs` doesn't yet)

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
  src/line_cursor.rs    # row highlight, companion mirroring, horizontal scroll
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
  src/cli.rs            # target from zed://review URLs; called from crates/zed glue
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
- **Core patch #1:** old-side line numbers on unified deleted rows need `set_number_deleted_lines`
  made `pub` (`crates/editor/src/config.rs:373`).

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

### 3.4 Line cursor (§2.5)

- Hide Zed's caret/selection painting: set `editor.show_local_selections = false` (public field,
  `editor.rs:1013`) on both RHS and LHS editors. Vim keeps working; only painting stops.
- Paint our own: `editor.highlight_rows::<ReviewCursorRow>(row, color_fn, RowHighlightOptions{
  include_gutter: true, autoscroll: false})` (`editor.rs:9284-9362`) on the cursor row; separate marker
  types for visual-range rows. Refresh on `SelectionsChanged`. Keep `autoscroll: false` (else autoscroll
  chases the highlight). Mirror onto the companion editor only when display rows match (spacer rows
  can't be highlighted without a core change — skip).
- ~~Column pinning~~: dropped in M2 — it breaks search repeat (`n`); see §0.
- `v` → `vim::ToggleVisualLine` in the review contexts (normal + visual). Selection → row range via
  `selections.newest_adjusted` + the `spanned_rows` rule (end column 0 excludes last row).
- `h`/`l` → horizontal scroll actions; `h` at scroll-x 0 → focus panel. Needs core patch #2 (autoscroll
  would snap x back on every `j`/`k`) and no soft wrap (the review turns it off by default).
- Rebind `i`/`a`/`o` etc. (vim would enter insert mode on a read-only buffer).
- Cursor never lands on custom blocks (`BlockSnapshot::clip_point`, `block_map.rs:2504-2558`) — good;
  folded (reviewed) file headers *are* cursor stops (matches tuicr fork `a3ac1a3`).
- Vim counts count buffer lines, not blocks (`vim/src/motion.rs:1537-1606`); `zz/zt/zb` count display
  rows (correct visually). Known quirk to watch: vim `ctrl-d/u` may clip the cursor off-screen when the
  scroll boundary lands inside a tall block (`vim/src/normal/scroll.rs:102-280`).

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

### 3.10 Glue footprint in upstream files (keep it this small)

| File | Change |
|---|---|
| `Cargo.toml` | workspace member + `[workspace.dependencies]` entry |
| `crates/zed/Cargo.toml` | dep on `code_review` |
| `crates/zed/src/main.rs` | `code_review::init(cx)` after `git_ui::init` (:776); `handle_open_request` arm |
| `crates/zed/src/zed.rs` | panel registration in `initialize_panels`; keymap loading in `load_default_keymap`; `mod review_cli`; test namespace list |
| `crates/zed/src/zed/open_listener.rs` | review URL kind + routing |
| `crates/cli/src/main.rs` | `--review` flag |
| `crates/editor/src/config.rs` | `set_number_deleted_lines` → `pub` (core patch #1) |
| `crates/editor/src/scroll.rs`, `scroll/autoscroll.rs` | `Editor::set_forbid_horizontal_autoscroll` (core patch #2): a `ScrollManager` flag checked first in `autoscroll_horizontally`, so the review tab's `h`/`l` scroll survives `j`/`k` |
| `assets/keymaps/code_review*.json` | new files |
| (later) `crates/settings_content` + `assets/settings/default.json` | settings block |

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
  socket) — another reason to base the fork branch on stable tags.
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
