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

**Status: branch bootstrapped, no feature code yet.** The long-lived fork branch `code-review`
exists locally (not pushed): upstream tag `v1.25.0-pre` + the fork commits (`.rules` pointer, this
directory, the README banner). The fork's `main` is untouched and equals upstream `main`. The dev
loop below is verified. Next: milestone 1 (§6).

What exists:

| Path | What |
|---|---|
| branch `code-review` | `v1.25.0-pre` + fork commits; all feature work goes here |
| `crates/code_review/DESIGN.md` | This document |
| `crates/code_review/research/benchmark-results.txt` | Measured build times (see §8) |
| `crates/code_review/research/cargo-timing-*.html` | `cargo --timings` reports for the clean build and the one-week bump |
| `target/` (gitignored) | **Warm build cache** (see §8.3): `release-fast` for `-p zed` plus `debug` test artifacts, 43 GB |
| `.rules` ("Drew's Zed fork" section) | Pointer to this file for agent sessions (`CLAUDE.md`/`AGENTS.md` symlink to it) |
| `README.md` first two lines | `> [!IMPORTANT]` banner required by the `.rules` HARD RULE; never remove it (the user does) |

**Decisions** (made by the user, 2026-10-16; don't ask again):

- Branch: **`code-review`**, one long-lived branch rebased onto each upstream stable tag (§2.1).
- Base: **`v1.25.0-pre`** (`aa9c16803d`) for now, because it matches the warm `target/` cache. Rebase
  onto `v1.25.0` once that stable tag ships; until then `crates/zed/RELEASE_CHANNEL` is `preview`.
- Agent build cap: **`-j8` under `nice -n 10`** for every cargo invocation. Never run cargo without
  these limits. This applies to agent sessions only; the Nix package build stays uncapped (§2.1).

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
- Under the hood: **Zed's real vim**, column pinned to 0. Counts, `{N}G`, `gg`/`G`,
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
  by head SHA, so new pushes don't start a new review. Reviewed marks clear when that file's/hunk's
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

### 3.1 Crate layout (proposed)

```
crates/code_review/
  Cargo.toml            # deps: gpui, ui, editor, multi_buffer, buffer_diff, project, git, workspace,
                        #       language, markdown, picker, db, http_client, fs, util, settings, …
  LICENSE-GPL -> ../../LICENSE-GPL   # required by script/check-licenses
  src/code_review.rs    # init(cx): actions, observe_new::<Workspace>, item/panel registration
  src/target.rs         # ReviewTarget (Pr/Branch/Uncommitted/Commits), parsing (CLI + picker)
  src/session.rs        # ReviewSession entity: target, file list, diffs, comments, reviewed state
  src/diff.rs           # building the multibuffer: DiffBufferList / open_diff_since / historic buffers
  src/view.rs           # ReviewView: workspace Item wrapping a SplittableEditor (+ top block)
  src/line_cursor.rs    # row highlight, column pinning, companion mirroring
  src/comments/         # model, blocks, CommentEditor (vim child editor), focus/keys
  src/panel.rs          # ReviewPanel (dock): file tree, filters, comment list
  src/picker.rs         # StartReview picker
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
- Column pinning: in normal mode, after selection changes, collapse to column 0 of the row.
- `v` → `vim::ToggleVisualLine` in the review contexts (normal + visual). Selection → row range via
  `selections.newest_adjusted` + the `spanned_rows` rule (end column 0 excludes last row).
- `h`/`l` → horizontal scroll actions; `h` at scroll-x 0 → focus panel.
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
