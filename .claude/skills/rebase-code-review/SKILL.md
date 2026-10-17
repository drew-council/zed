---
name: rebase-code-review
description: Rebase Drew's Zed fork (branch `code-review` of drew-council/zed) onto the newest upstream stable tag vX.Y.Z. Backs up the branch, reviews every fork commit and drops what upstream made obsolete, resolves conflicts in the fork's upstream glue, checks the result with range-diff, builds and tests under the agent compute caps, refreshes DESIGN.md, and pushes only after the user confirms. Use when asked to rebase the fork, update or bump the Zed fork to a new stable tag or release, rebase code-review, or move the fork onto the latest Zed stable.
argument-hint: "[target tag, e.g. v1.26.2]"
---

# Rebase `code-review` onto a new upstream stable tag

`code-review` is one long-lived branch: an upstream Zed tag plus the fork's own commits (the
`code_review` crate, its glue in a few upstream files, two small `editor` patches, the `--review`
CLI glue, a `releaseChannel` knob in `nix/build.nix`, docs, the live harness, this skill). The fork
follows upstream **stable** tags (`crates/code_review/DESIGN.md` §2.1), so each new `vX.Y.Z` means
one rebase. Read DESIGN.md §0 (status, decisions, dev loop), §3.10 (upstream footprint) and §8
before you start.

**Always name the exact old tag: `git rebase --onto <new> <old-tag> code-review`. Never use a
merge-base.** Zed tags releases on per-minor release branches (`v1.25.x`): `vX.Y.Z-pre` previews,
then a promotion commit that flips `crates/zed/RELEASE_CHANNEL` to `stable` and gets the `vX.Y.Z`
tag. Fixes reach those branches as cherry-picks from `main`. The next minor's tags don't descend
from the old ones (`v1.24.2` is not an ancestor of `v1.25.0-pre`), so a merge-base would replay the
old branch's cherry-picks as if they were fork commits.

## Rules

- Work only on `code-review` in `/home/drew/personal/zed`. If it isn't checked out, stop. Don't
  switch branches for the user. Never touch `main`: the fork's `main` mirrors upstream.
- Start from a clean tree with no rebase, merge or cherry-pick in progress.
- `upstream` must be `zed-industries/zed`. Use stable tags only (`vX.Y.Z` with no `-pre`), unless
  the user names a preview tag.
- Create a backup branch before you rewrite anything.
- Resolve every conflict according to the fork commit's intent. Never resolve by blanket
  `ours`/`theirs` (the one exception is `Cargo.lock`, below).
- Keep upstream-file edits as small and append-only as DESIGN §3.10 says. Don't "improve" glue
  during a rebase.
- Never remove or change the `> [!IMPORTANT]` banner at the top of `README.md`. It is a `.rules`
  HARD RULE, and only the user removes it.
- Never edit a shipped `CodeReviewDb` migration (`crates/code_review/src/persistence.rs`,
  `MIGRATIONS`). Keep the SQL byte-identical even when upstream changes `db`/`sqlez` APIs around
  it. Zed's databases share one migrator, so a changed migration sends *all* of them to an
  in-memory fallback, which would lose the user's Zed state. New schema goes in a new migration.
- Never run `target/release-fast/zed` or `target/release-fast/cli` directly, and never
  `xdg-open zed://…`: after a stable rebase they are stable-channel builds that talk to the user's
  running Zed and its database. Launch them only through `script/live-harness/zh`.
- If you can't tell whether a commit is still needed, ask the user. If the user is unavailable,
  keep the commit, record the question in the report, and don't push.
- Push only after the user explicitly confirms, and only with `--force-with-lease`.

## Compute budget (the user's PC must stay usable)

- Run cargo only through `direnv exec /home/drew/personal/zed …`, with `-j8` and under
  `nice -n 10`. Allow exactly one cargo job at a time across all agents. Run long commands in the
  background and wait for them to finish.
- Never use `nix develop -c`, which triggers an uncapped Nix rebuild. Step 9 covers the devshell.
  Never `nix build` the Zed package itself (uncapped release LTO build); evaluate it instead.
- Watch disk space. `target/` is over 40 GB and grows after every rebase. Don't create extra
  target dirs or worktrees.

## Workflow

### 1. Preflight

```sh
cd /home/drew/personal/zed
git branch --show-current                     # must be code-review
git status --porcelain                        # must be empty
for p in rebase-merge rebase-apply MERGE_HEAD CHERRY_PICK_HEAD; do
  test -e "$(git rev-parse --git-path $p)" && echo "in progress: $p"
done
pgrep -a -x 'cargo|rustc'                     # another agent building? wait for it
git remote get-url upstream                   # zed-industries/zed
df -h /home
```

### 2. Fetch

```sh
git fetch upstream --tags
git fetch origin                              # origin/code-review, for the push lease
```

### 3. Find the old base

```sh
old_base=$(git describe --tags --abbrev=0 --match 'v[0-9]*' code-review)
git tag --points-at "$old_base"               # if several tags share the commit, take the stable one
git rev-list --count "$old_base..code-review"
git log --format='%an' "$old_base..code-review" | sort | uniq -c
```

`$old_base` must match the "Base:" decision in DESIGN.md §0. The authors must all be the fork's
(today only `drew-council`). Upstream authors or bots such as `zed-zippy[bot]` in that range mean
the base is wrong: stop.

### 4. Pick the target

```sh
git tag -l 'v[0-9]*' | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | sort -V | tail -5
```

Take the newest tag whose `X.Y.Z` is greater than the old base's `X.Y.Z`. An equal `X.Y.Z` also
counts when the old base is that version's `-pre` (`v1.25.0` is newer than `v1.25.0-pre`).
Compare the versions yourself: `sort -V` orders `v1.25.0` *before* `v1.25.0-pre`. **A minor's
first stable is often not `.0`**: 1.23 shipped stable as `v1.23.1` (after `v1.23.0-pre`,
`v1.23.1-pre`), 1.24 as `v1.24.2` (after `v1.24.0-pre` … `v1.24.2-pre`), so the fork's next base
after `v1.25.0-pre` may be any `v1.25.N`. If the user named a tag, use it, but run the same checks.
If no newer stable tag exists, say so and stop.

```sh
git show "$target:crates/zed/RELEASE_CHANNEL"; echo          # must print: stable
git rev-parse "$target^{commit}"
git ls-remote upstream "refs/tags/$target" "refs/tags/$target^{}"   # same commit
git merge-base --is-ancestor "$old_base" "$target" && echo "same release branch" || echo "new release branch"
```

On the same release branch (a promotion or patch release), expect a few small conflicts. A new
release branch brings in about a week of `main`. Skipping a whole minor is fine, but mention it in
the report.

### 5. Survey what upstream changed (read-only)

```sh
git log --oneline "$old_base..$target" | wc -l

# Upstream files the fork touches (DESIGN §3.10 is the intent; this diff is the truth):
footprint=$(git diff --name-only "$old_base" code-review -- . \
  ':(exclude)crates/code_review' ':(exclude)assets/keymaps/code_review*.json' \
  ':(exclude)script/live-harness' ':(exclude).claude')
echo "$footprint"
git diff --stat "$old_base" "$target" -- $footprint      # the likely conflicts

# Did upstream absorb a core patch?
git grep -n 'fn set_number_deleted_lines' "$target" -- crates/editor/src   # `pub fn` => patch #1 obsolete
git grep -n 'forbid_horizontal_autoscroll\|fn set_.*horizontal_autoscroll' "$target" -- crates/editor/src

# Name collisions with the fork:
git ls-tree "$target" crates/code_review crates/cli/src/review.rs .claude/skills/rebase-code-review   # must be empty
git grep -n -w 'code_review\|releaseChannel' "$target" -- ':(exclude)crates/code_review' | head

# Upstream sources the crate copied (drift to port later, in a separate commit):
git log --oneline "$old_base..$target" -- crates/git_ui/src/diff_multibuffer.rs \
  crates/git_ui/src/commit_view.rs crates/git_ui/src/staged_diff.rs \
  crates/git_ui_core/src/worktree_service.rs

# Upstream's in-editor review prototype (DESIGN §1, §7). Its flag must stay off:
git log --oneline "$old_base..$target" -- crates/feature_flags/src/flags.rs crates/editor/src/git.rs

# Devshell and toolchain impact (step 9):
git diff --stat "$old_base" "$target" -- flake.nix flake.lock nix rust-toolchain.toml
```

At `v1.25.0-pre` + M9 the footprint is `.rules`, `Cargo.lock`, `Cargo.toml`, `README.md`,
`crates/cli/src/main.rs`, `crates/cli/src/review.rs` (a fork file in the upstream crate),
`crates/editor/src/config.rs`, `crates/editor/src/scroll.rs`,
`crates/editor/src/scroll/autoscroll.rs`, `crates/zed/Cargo.toml`, `crates/zed/src/main.rs`,
`crates/zed/src/zed.rs`, `crates/zed/src/zed/open_listener.rs` and `nix/build.nix`. A settings
block would add `crates/settings_content` and `assets/settings/default.json`. Flag any footprint
file that §3.10 doesn't list.

### 6. Review each fork commit

```sh
git log --reverse --format='%h %s' "$old_base..code-review"
git show --stat <sha>        # for each commit that touches files outside crates/code_review
```

For each commit, decide:

- **keep**: the bootstrap and docs commits (the `.rules` section, the README banner, DESIGN.md,
  INSTALL.md, `research/`), the live harness, this skill, the `nix/build.nix` knob, the CLI glue,
  and every `crates/code_review` commit. Upstream can't make the crate's own code obsolete.
- **adapt**: glue whose surrounding upstream code moved. Resolve it during the rebase.
- **drop** (or drop the obsolete hunk): a core patch that upstream absorbed, such as
  `set_number_deleted_lines` becoming `pub` or upstream gaining a horizontal-autoscroll opt-out.
  If upstream's version differs, switch `code_review` to it in the follow-up commit. Likewise the
  `nix/build.nix` knob if upstream adds its own channel parameter (then update INSTALL.md).

Also flag any core patch with no caller in the crate, because each one is a rebase hot spot that
buys nothing: `git grep -n set_number_deleted_lines code-review -- crates/code_review`. At
`v1.25.0-pre` + M9, core patch #1 still has no caller. Ask before dropping it.

Upstream API changes that break the crate's code show up as build errors, not conflicts. Fix them
after the rebase in one commit, `code_review: Adapt to Zed vX.Y.Z`, rather than editing many
replayed commits. Glue conflicts get resolved inside their own commit.

### 7. Back up

```sh
backup="code-review-backup-$(date +%F)"
git show-ref --verify --quiet "refs/heads/$backup" && backup="$backup-$(date +%H%M)"
git branch "$backup" code-review
lease=$(git rev-parse --verify --quiet origin/code-review || true)   # empty: never pushed
```

### 8. Rebase

With nothing to drop:

```sh
git rebase --onto "$target" "$old_base" code-review
```

With drops or edits from step 6, edit the todo list non-interactively (no editor opens):

```sh
GIT_SEQUENCE_EDITOR="sed -i -e 's/^pick a586dae/drop a586dae/'" \
  git rebase -i --onto "$target" "$old_base" code-review
```

Use `edit` instead of `drop` for a commit that is only partly obsolete. For example, `a586dae4e9`
carries both editor patches plus a DESIGN.md line. When the rebase stops at it, restore the
absorbed file from the new base (`git checkout "$target" -- crates/editor/src/config.rs`), run
`git commit --amend` (fix the message too), then `git rebase --continue`. Git drops commits that
become empty on its own; list them in the report. To drop a commit after the fact, run
`git rebase --onto <commit>^ <commit> code-review`.

### Resolving conflicts

During a rebase, `--ours`/HEAD is the new base plus the fork commits replayed so far, and
`--theirs` is the fork commit being applied. After resolving, run `git add …` and then
`GIT_EDITOR=true git rebase --continue`. If a commit turns out to be obsolete halfway through, run
`git rebase --skip`. `git rebase --abort` goes back to the start at any point.

`*.rs` files merge through the user's global weave driver (`~/.config/git/attributes`). Weave
marks the conflicts it can't resolve with `refused_by:`; use the `weave` skill for those.

| File | Fork change | Resolution |
|---|---|---|
| `Cargo.toml` | `"crates/code_review"` in `members`; `code_review = { path = "crates/code_review" }` in `[workspace.dependencies]` | keep both, in alphabetical order |
| `Cargo.lock` | the `code_review` package, its deps, `"code_review"` in `zed`'s deps | regenerate (below) |
| `crates/zed/Cargo.toml` | `code_review.workspace = true` | keep, in alphabetical order |
| `crates/zed/src/main.rs` | `code_review::init(cx);` right after `git_ui::init(cx);`; in `handle_open_request`, the `OpenRequestKind::Review { url }` arm (`open_options_for_request` + `code_review::cli::open_review_link`) | keep `init` right after `git_ui::init`; re-apply the arm next to upstream's other `OpenRequestKind` arms, using upstream's current way of building open options |
| `crates/zed/src/zed.rs` | `initialize_panels`: `code_review::ReviewPanel::load(…)` and `add_panel_when_ready(review_panel, …)`; `load_default_keymap`: `cx.bind_keys(code_review::key_bindings(cx))` after the base keymap, and `cx.bind_keys(code_review::vim_key_bindings(cx))` right after `vim.json` inside the vim/helix condition; `"code_review"` in `test_action_namespaces` | re-apply each intent to upstream's new shape. Order matters: the vim bindings override `vim.json`, so they load after it, under the same condition |
| `crates/zed/src/zed/open_listener.rs` | `OpenRequestKind::Review { url }` + its `Debug` arm; in `OpenRequest::parse`, `code_review::cli::is_review_url(&url)` before the `ssh://` branch; in `handle_cli_connection`, a request whose only URL is a review link goes to `code_review::cli::handle_cli_review` (with `OpenOptions { env, ..open_options_for_behavior(…) }`) and returns | re-apply; the CLI branch must stay before upstream's generic `urls` handling |
| `crates/cli/src/main.rs` | `mod review;`; the `zed --review pr:123` help example; the `review: Option<String>` arg (`conflicts_with = "diff"`); `Args::try_parse()` with exit 1 for usage errors when `review::requested()`; `review::request(…)` pushing the link and the repo path, paths skipped while reviewing; `ensure!(!is_review, "Zed exited before the review finished")` | re-apply each; `crates/cli/src/review.rs` is a fork file and won't conflict, but adapt it if upstream's CLI types moved |
| `crates/editor/src/config.rs` | `set_number_deleted_lines`: `pub(super)` → `pub` (core patch #1) | drop it if upstream made the function public |
| `crates/editor/src/scroll.rs`, `scroll/autoscroll.rs` | `ScrollManager::forbid_horizontal_autoscroll` + `Editor::set_forbid_horizontal_autoscroll`; early `return None` in `autoscroll_horizontally` (core patch #2) | re-apply so the flag is checked before any horizontal autoscroll work; drop it if upstream offers an equivalent opt-out |
| `nix/build.nix` | `releaseChannel ? "nightly"` argument, a `channel` table in `let` (stable/preview/nightly: icon suffix, app name, app id), a Darwin-only-nightly assert, and `channel.*` in `version`, `preBuild`'s `echo … > crates/zed/RELEASE_CHANNEL` and the Linux install phase's icons and desktop entry | re-apply to upstream's new lines. With the default `nightly` the derivation must stay exactly upstream's (step 11 checks) |
| `.rules` | the "Drew's Zed fork" section at the top | keep it at the top; take upstream's rest |
| `README.md` | the 3-line `> [!IMPORTANT]` banner at the top | keep it verbatim; never remove it |

**`Cargo.lock`:** take upstream's side and re-resolve for this commit's manifests (nothing gets
compiled):

```sh
git checkout --ours -- Cargo.lock
nice -n 10 cargo metadata --format-version 1 --offline > /dev/null   # drop --offline if a crate isn't cached
git add Cargo.lock
```

Use plain `cargo` here. If `flake.lock` changed, a `direnv exec` in the middle of the rebase would
start the uncapped devshell rebuild that step 9 avoids. 13 fork commits touch `Cargo.lock`, but
usually only the first one conflicts.

### 9. Refresh the devshell (only if step 5 showed flake, nix or toolchain changes)

nix-direnv re-evaluates when `flake.nix`, `flake.lock` or `.envrc` change. The new devshell
includes a crane build of `docs_preprocessor` over all of `crates/`, and after a `Cargo.lock`
change it also rebuilds all of Zed's third-party deps. The Nix daemon does that build, uncapped,
inside the next `direnv exec`. `nice` doesn't reach daemon builds. Build the devshell explicitly
with caps first (the user is a trusted Nix user, so these settings reach the daemon):

```sh
nice -n 10 nix build /home/drew/personal/zed#devShells.x86_64-linux.default --no-link \
  --option cores 8 --option max-jobs 1
direnv exec /home/drew/personal/zed true          # re-evaluates from the store, quickly
```

If only `rust-toolchain.toml` changed, nix-direnv won't notice, and the devshell silently keeps
the old rustc. After the capped build, force a reload with
`touch /home/drew/personal/zed/.envrc && direnv exec /home/drew/personal/zed true`, then check that
`direnv exec /home/drew/personal/zed rustc --version` matches `rust-toolchain.toml`. A toolchain
bump also invalidates all of `target/`, so the next build is a clean build (about 20 minutes at
`-j4`, DESIGN §8.1).

`ZED_COMMIT_SHA`: the devshell exports the flake's `self.rev` from its last evaluation (empty if
the tree was dirty then), so dev builds report a stale SHA. This is cosmetic: the Nix product
build gets the right one. To stamp a dev build correctly, add
`ZED_COMMIT_SHA=$(git -C /home/drew/personal/zed rev-parse HEAD)` to the build's `env`. Only the
few crates that read it recompile.

### 10. Review the result

```sh
git range-diff "$old_base..$backup" "$target..code-review"
git diff --stat "$target" code-review -- . ':(exclude)crates/code_review' \
  ':(exclude)assets/keymaps/code_review*.json' ':(exclude)script/live-harness' \
  ':(exclude).claude'                                                            # same footprint as before, minus drops
git diff "$target" code-review -- crates/zed/RELEASE_CHANNEL                      # empty; the file says stable
git diff "$backup" code-review -- crates/code_review/src/persistence.rs           # MIGRATIONS untouched
git diff --check "$target" code-review
git log --format='%an' "$target..code-review" | sort -u                          # still only the fork's author(s)
head -3 README.md                                                                # banner intact
```

Read every range-diff pair marked `!` and make sure each change is a deliberate adaptation.

### 11. Build and test (one job at a time, in the background)

```sh
direnv exec /home/drew/personal/zed env CARGO_PROFILE_RELEASE_FAST_DEBUG=limited \
  nice -n 10 cargo build -j8 --profile release-fast -p zed -p cli
direnv exec /home/drew/personal/zed nice -n 10 cargo test -j8 -p code_review
direnv exec /home/drew/personal/zed nice -n 10 cargo test -j8 -p zed --bin zed test_action_namespaces
direnv exec /home/drew/personal/zed nice -n 10 ./script/clippy -p code_review -j8
direnv exec /home/drew/personal/zed cargo fmt --all -- --check
direnv exec /home/drew/personal/zed cargo metadata --locked --format-version 1 > /dev/null   # the Nix build uses --locked
```

Always build `-p zed -p cli` together, never `-p cli` alone (feature resolution differs per `-p`
set and would recompile third-party crates). Expected cost:

- After a one-week bump, the build recompiles most workspace crates plus the bumped deps
  (12m23s at `-j4` for v1.24.2 → v1.25.0-pre, DESIGN §8.1).
- `cargo test -p zed` is cold the first time: its feature set differs, so it takes many minutes
  and adds GBs to `target/`. The same goes for `cargo test -p cli` (the `review.rs` tests; M8 ran
  them with a standalone `rustc --test` instead, DESIGN §0 "Milestone 8").
- `code_review`'s `test_keymaps_load_without_errors` catches upstream action renames that the
  fork's keymaps use.

Check that the Nix package still evaluates, without building it (seconds; `allow-import-from-
derivation false` makes Nix fail instead of building anything):

```sh
nice -n 10 nix eval --option allow-import-from-derivation false --option max-jobs 1 \
  --json /home/drew/personal/zed#packages.x86_64-linux.default --apply \
  'p: let s = p.override { releaseChannel = "stable"; }; in
      { nightly = p.name; stable = s.name; echo = s.preBuild; }'
```

`nightly` ends in `-nightly+<sha7>` (empty sha while the tree is dirty), `stable` has no channel
word, and `echo` contains `echo stable > crates/zed/RELEASE_CHANNEL`.

Fix failures in the `code_review: Adapt to Zed vX.Y.Z` commit. Every new commit must build and
pass its tests.

After a rebase onto a stable tag, `./target/release-fast/zed` is a stable-channel build. Launched
directly, it shares the installed Zed's database (`~/.local/share/zed/db/0-stable`) and CLI socket
(`zed-stable.sock`), and it hands its paths to a running Zed instead of starting its own window.
Run it only through `script/live-harness/zh` (isolated HOME; only when asked and never while cargo
runs). A user running it by hand must pass `--user-data-dir <tmpdir>`.

### 12. Refresh DESIGN.md

- In the header blockquote, set the base tag and say which pointers you re-verified.
- In §0, update the "upstream tag `…` + the fork commits" sentence, the `branch code-review` row
  of "What exists" and the "Base:" decision (drop "for now … until then RELEASE_CHANNEL is
  `preview`" once the base is stable). Add one dated line: "Rebased onto vX.Y.Z (YYYY-MM-DD):
  dropped …, adapted …".
- Spot-check the line pointers into the upstream files the fork touches (§3.8, §3.10) and fix the
  ones you checked. For example:
  `git grep -n 'git_ui::init(cx)' -- crates/zed/src/main.rs`,
  `git grep -n 'fn test_action_namespaces' -- crates/zed/src/zed.rs`,
  `git grep -n 'fn set_number_deleted_lines' -- crates/editor/src/config.rs`.
  Don't rewrite the other pointers wholesale; they refer to main@2d4c483 as the header says.
  `grep -n "$old_base" crates/code_review/DESIGN.md` finds the base mentions. Keep the historical
  ones in §8.
- On the first stable rebase: §0's dev loop already warns that dev builds become stable-channel
  and must run isolated; make sure it now says they *are*. `crates/code_review/INSTALL.md`'s "Not
  installable yet" note loses its rebase half.
- Commit as `code_review: Update design notes for the rebase onto vX.Y.Z`.

### 13. Push (only after the user confirms)

Show the user the summary below first. Then:

```sh
git push -u origin code-review                                         # first push: origin has no code-review yet
git push --force-with-lease=code-review:"$lease" origin code-review   # every later push
```

Never push `main`, tags or the backup branch, unless the user asks. After the push, the user
updates the installed fork from their NixOS config (`~/nixconf`):
`nix flake update zed-fork && nh os switch` (`crates/code_review/INSTALL.md`). Don't run it or edit
`~/nixconf` yourself.

## Report

- old base → new tag (same or new release branch, minors skipped), upstream commit count
- backup branch name and the lease SHA (or "never pushed")
- kept, adapted and dropped commits, with the reason for each drop (absorbed upstream, emptied)
- conflict resolutions per file
- devshell refresh: done (capped) or not needed; toolchain bump yes or no
- validation: build, `code_review` tests (count), `test_action_namespaces`, clippy, fmt,
  `cargo metadata --locked`, the Nix evaluation, with durations
- follow-ups: drift in the copied upstream sources, core patches with no caller, upstream
  `diff-review` changes
- DESIGN.md commit, push status, final `git status`
