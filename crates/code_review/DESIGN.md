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

**Status: milestone 3 (review targets) done.** The long-lived fork branch `code-review`
exists locally (not pushed): upstream tag `v1.25.0-pre` + the fork commits. The fork's `main` is
untouched and equals upstream `main`. Review tabs exist for uncommitted changes, the branch vs a base
(changeable in place), and one commit or a contiguous range (live when it ends at HEAD, a read-only
historic snapshot otherwise), opened from the `code_review::StartReview` picker or the direct actions
`ReviewUncommitted` / `ReviewBranch { base }` / `ReviewCommits { commits }`. Each tab has a
tuicr-style line cursor and file navigation (§2.5 minus comments/reviewed marks/export) and a fixed top
block (branch base, or the commit messages). The review panel in the left dock only shows the active
review's title so far. Next: milestone 4 (§6).

What exists:

| Path | What |
|---|---|
| branch `code-review` | `v1.25.0-pre` + fork commits; all feature work goes here |
| `crates/code_review/DESIGN.md` | This document |
| `crates/code_review/src/code_review.rs` | `init` (registers actions per workspace), all `code_review::` actions (incl. `ReviewBranch`/`ReviewCommits` with fields), keymap loading helpers, test helpers (`init_test`; `test_support`: `init_vim_test`, the review fixture, editor helpers, fake-repo commit/branch helpers like `record_commit`) |
| `crates/code_review/src/target.rs` | `ReviewTarget::{Uncommitted, Branch { base }, Commits(CommitRange)}`, `is_same_review`, empty-state label, `parse_commit_spec` (`rev` / `A..B`, reused by M8's CLI) |
| `crates/code_review/src/commits.rs` | `CommitDiffList` (a commit range's files: live over project buffers, or historic snapshot buffers), `GitBlob`/`build_buffer` copied from `CommitView`, git helpers over `Repository::send_job` (`first_parent_chain`, `resolve_commit_spec`, `validate_ref`) |
| `crates/code_review/src/diff.rs` | `ReviewDiff`: copy of `git_ui`'s `DiffMultibuffer`, driven by a `DiffSource` (`DiffBufferList` for uncommitted/branch, `CommitDiffList` for commits); `ReviewAddon` (header statuses against the review base); loading/error state; `set_branch_base`; file order, `path_at_cursor`, `move_to_path`, single-file mode |
| `crates/code_review/src/top_block.rs` | `ReviewTopBlock`: fixed header above the diff — branch base line with a Change Base button, or the commit messages (collapsible); commit tab titles |
| `crates/code_review/src/picker.rs` | `StartReviewModal` / `StartReviewDelegate`: Local section (uncommitted, branch vs base, recent commits with range multi-select); `CommitSelection` |
| `crates/code_review/src/line_cursor.rs` | `LineCursor`: hides the caret, paints cursor/visual rows (mirrored in split view), horizontal scroll |
| `crates/code_review/src/view.rs` | `ReviewView` workspace item (key context `CodeReview`), reuse-or-create `deploy`/`deploy_in_repo`, titles, Change Base (`open_base_picker`/`select_base`/`change_base`), handlers for the review keys and the direct actions, `FocusDiff`, GPUI tests |
| `crates/code_review/src/panel.rs` | `ReviewPanel` (left dock): follows the active review tab and shows its title; `FocusPanel` / `TogglePanel` / `ToggleFocus` |
| `assets/keymaps/code_review.json`, `code_review_vim.json` | Built-in keymaps (non-vim: only `tab` → `picker::MultiSelectNext` in the Start Review picker; the rest is in the command palette) |
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
applied (`crates/editor/src/config.rs`, `scroll.rs`, `scroll/autoscroll.rs`; §3.10). M3 added no
upstream glue.

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
  ref first (`revparse_batch`, no leading `-`/whitespace), so a typo is an error toast instead of an
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
  CommitView's `--first-parent`); full 40/64-hex shas are used as is, anything else resolves through
  `rev^{commit}`. Root commits diff against git's empty tree. **Live vs snapshot is decided once**, when
  the review opens (range head == HEAD): a live review keeps showing working-tree buffers even if HEAD
  moves later. Live reviews list only the range's files (`diff-tree base head`) over project buffers
  (`GitStore::open_diff_since`), so uncommitted edits to those files show, files changed only in the
  worktree don't.
- **Snapshots**: `Capability::ReadOnly` buffers whose file is a `GitBlob` (`DiskState::Historic`, not
  local → no language server), syntax highlighted. Deviation: both sides load through one
  `git cat-file --batch` (`load_revisions` with `head:path` / `base:path` specs) instead of a
  `load_blob_content` call per old blob. Binary files show a "(binary file not shown)" line as an
  addition or deletion so they stay listed; entries git lists without a blob on one side (submodules)
  are skipped with a warning. All snapshot buffers are built before the first excerpt appears. `e` on
  a snapshot row opens the working-tree file at the same row (clamped), like CommitView's "open file
  at HEAD". The checkout, index and HEAD are never touched.
- **Start Review picker**: Local section = "Uncommitted changes", "Branch vs ‹base›" (secondary
  confirm or no default branch → base picker), then the first-parent chain from HEAD (50 commits).
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

**Known issues / notes for the next slices:**

- Saving: for an active item whose `capability` isn't editable, the workspace doesn't register
  `workspace::Save` (`workspace.rs:8065-8100`) and `save_item` returns early for save intents
  (`pane.rs:2276`). The comments milestone (`:w` / `ctrl-s` saving drafts, §3.5) must make the
  capability editable or route saving differently.
- **Empty-row bleed** (visible in the live check): on an empty row directly followed by a spacer — the
  last empty line of a file before an added/deleted file, or a blank context line before a pure
  addition — the cursor highlight also covers the spacer rows. `highlight_rows` excludes an end at
  column 0 and an empty row has no other column, so its range must reach the next row. Proposed core
  patch #3 (not applied, needs approval): in `Editor::highlighted_display_rows_in_range`
  (`editor.rs:9483` on `code-review`) only exclude the end row when `end.row() > start.row()`, so an
  empty range highlights its own row; `line_cursor::highlight_run` would then pass `start..start` for
  an empty last row. M5 comment blocks right below an empty row would be painted the same way unless
  opaque.
- Upstream quirks seen live: vim relative line numbers apply to the focused side only and count across
  file sections; on the LHS the current line shows 0 instead of its number; after toggling soft wrap in
  split view the LHS is mis-laid-out until the next frame.
- `]c` (`GoToHunk`) unfolds ranges — will fight M4's reviewed-file folding.
- Single-file mode removes excerpts and the LHS is recreated on split toggles: M5 must re-host comment
  blocks for both (`LineCursor::sync_split_state` shows how to catch LHS re-creation). The
  `ToggleSoftWrap` capture will also catch the action inside M5 comment editors — scope it then.
- Multi-cursor commands (`g l`, `g a`) only show the newest selection.
- Not run: `zed`'s own tests (incl. `test_action_namespaces`), clippy. `cargo test -p code_review`:
  27 tests, stable under `ITERATIONS=10`.
- M3 was verified live (harness): picker rows, every target, Change Base (button, palette, reuse),
  snapshot read-only + checkout untouched, live range following edits, top block, titles, reuse,
  `]f`/`]c`/line cursor/`e` in branch, live and snapshot reviews.
- M3 notes: a range `A..B` where A isn't on B's first-parent chain diffs tree(A) → tree(B) and the top
  block walks back to the root or the limit ("N+ commits"). `DiffBufferList` only logs tree-diff
  errors, so a base that disappears after the review opened shows "No changes against ‹base›". Huge
  snapshot ranges load every blob before the first excerpt (switch to per-file `load` futures if that
  is slow). Hover/go-to-definition in snapshots couldn't be checked live (the harness disables
  language servers); snapshot buffers aren't local, so no server attaches by construction.
- M5: drafts keyed per §2.6 must be re-keyed in `ReviewView::change_base` (the only place a target
  changes) and must handle a branch switch while a branch review is open (the branch name is part of
  the key; `RepositoryEvent::HeadChanged` is already subscribed). Review-level comments go into
  `ReviewTopBlock` (an entity rendered above the diff; add content after the commit/branch part).
- M7 slots: `Section` in `picker.rs` (add the pull request section after Local in
  `StartReviewDelegate::set_matches`, where a comment marks it), `StartReviewFocus` (add
  `PullRequests` for `OpenPullRequest { number: None }`), `ReviewTarget` (add `PullRequest`; override
  `is_same_review` so `base_oid` is ignored), `DiffSource::new` (map it to
  `BufferList(DiffBase::Merge { base_ref: base_oid })`), `TopBlockContent` (add the PR description).
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
  - secondary action **"Review in new worktree"** via Zed's worktree machinery
    (`crates/git_ui_core/src/worktree_service.rs`, opens as another workspace in the window).
- **GitHub access:** in-process via Zed's `HttpClient` (GraphQL for reads, REST for review
  creation). Token: `GITHUB_TOKEN` env (Zed's existing convention, cf.
  `crates/http_client/src/github.rs:147`), else one-time `gh auth token` (user's `gh` is logged in
  with `repo` scope). Signing into Zed with GitHub does **not** give a usable token (the OAuth flow
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
  diff content changes (tuicr: FNV-1a over hunk lines, `src/model/review.rs:164-189`). Drafts follow
  code via Zed anchors while running; after reload a draft whose line text no longer matches is
  flagged **outdated** (not dropped).

### 2.7 PR specifics

- Existing review threads render inline, **read-only**; resolved and outdated hidden by default
  (toggle). **Bot review summaries hidden by default** (tuicr `050f0f8`: `is_bot` via
  `__typename == "Bot"` or `[bot]` login, `src/forge/github/review_summaries.rs:138`).
- **Submit modal:** Comment / Approve / Request changes; counts; body preview (from review-level
  comments); stale-head warning (head moved since load); when GitHub requires a body (Request changes,
  and Comment with no inline comments — verify) and none exists, prompt to add a review-level comment.
  On success: drafts become `submitted` (locked), threads refetched.
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

### 3.1 Crate layout (proposed; files up to `picker.rs` exist as of M3)

```
crates/code_review/
  Cargo.toml            # deps: gpui, ui, editor, multi_buffer, buffer_diff, project, git, workspace,
                        #       language, markdown, picker, db, http_client, fs, util, settings, …
  LICENSE-GPL -> ../../LICENSE-GPL   # required by script/check-licenses
  src/code_review.rs    # init(cx): actions, observe_new::<Workspace>, item/panel registration
  src/target.rs         # ReviewTarget (Pr/Branch/Uncommitted/Commits), parsing (CLI + picker)
  src/session.rs        # ReviewSession entity: target, file list, diffs, comments, reviewed state
  src/diff.rs           # building the multibuffer: DiffSource (DiffBufferList / CommitDiffList)
  src/commits.rs        # commit ranges: live (open_diff_since) or historic snapshot buffers; git helpers
  src/view.rs           # ReviewView: workspace Item wrapping a SplittableEditor (+ top block)
  src/top_block.rs      # fixed header above the diff: branch base / commit messages (M5/M7 extend it)
  src/line_cursor.rs    # row highlight, companion mirroring, horizontal scroll
  src/comments/         # model, blocks, CommentEditor (vim child editor), focus/keys
  src/panel.rs          # ReviewPanel (dock): file tree, filters, comment list
  src/picker.rs         # StartReview picker (Local section; PR section in M7)
  src/github/           # provider trait + GitHub impl (GraphQL/REST via HttpClient), auth
  src/checkout.rs       # PR checkout (change_branch / refs/pull fetch / worktree)
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
| Create review | `src/forge/github/submit.rs:17` `build_review_payload`; `gh.rs:571` | REST `POST repos/{o}/{r}/pulls/{n}/reviews` `{commit_id, body, event, comments:[{path, line, side, start_line?, start_side?, body}]}`; file comments: use `subject_type: "file"` (tuicr fakes them) |
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

- Which context lines GitHub accepts for inline comments (only lines inside GitHub's diff hunks?
  expanded lines?) — determines which rows allow `c` in PR reviews. Verify against the API.
- Exact GitHub body requirement for COMMENT with inline comments (docs say required for
  REQUEST_CHANGES/COMMENT) — verify.
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
