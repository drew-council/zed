> Research record, copied from the orchestrator's `plans/FINAL-validation.md`: the checklist the final
> live validation (sessions V1–V8, then the regression sweeps R1/R2) ran. `$STATE` stands for the
> orchestrator's state directory (outside the repository: validation output, sandbox scripts, the
> harness clone); results and screenshots stayed there. Results: `SUMMARY.md` next to this file.

> **Sandbox PR numbers changed (after M7d's reset):** PR A = #1 (read-only fixture, unchanged),
> PR C = **#8** (clean own PR; #2 and #6 are closed), PR B = **#7** (clean bot PR, branch
> `bot/harden-settings-1792229777`; #3 is closed), PR D = #4 (residue). Wherever this document says
> PR C #2 / PR B #3, use #8 / #7. Re-check with `gh pr list -R drew-council/code-review-sandbox`.

# FINAL validation: code_review end to end in the live fork

Goal (user): "complete an actual run of the live zed fork and go through all the features required interactively and
validate that they all function fully as intended". This is the checklist for that run: 8 sessions (V1–V8), one agent
each, covering every user-facing requirement of `crates/code_review/DESIGN.md` §2 and the user decisions in
`AGENT_BRIEF.md` (coverage matrix: §4). Written 2026-10-17 against `code-review` @ `6dc9e55022` (M1–M6, M7a, hardening
1+2); M7 (b)–(d) and M8 were not built yet, so their checks come from `plans/M7-github-v2.md` / `plans/M8-cli.md`.
**DESIGN §0 as built wins** over the plans for names and wording: a documented difference is a PASS with a note, an
undocumented one is a finding.

## 0. Rules

- Read `script/live-harness/README.md`, §1 here, and DESIGN §0 "Known issues" (§3 here) first.
- **Never** edit repo files, run cargo, or run git write commands in `~/personal/zed`. Write only below `$V`
  and in `$S/harness-repo`/`$S/fork-probe`. One harness instance; never while cargo runs (`pgrep -a cargo` empty);
  always `zh stop` at the end.
- GitHub writes **only** to `drew-council/code-review-sandbox`, and only in V5 and V6.18. Never submit to PR A (#1) or
  PR D (#4). Never confirm a row in the `fork-probe` picker. Never print the token (§1.4). Never run `xdg-open`; don't
  click "Open on GitHub"/"Open in browser" in the harness (could start a real browser): check the button exists, check
  URLs with "Copy Pull Request URL".
- Tags: `[B]` built at `6dc9e55`; `[M7b]` PR target/picker/checkout; `[M7c]` description/threads/gate; `[M7d]` submit;
  `[M8]` CLI/links/`--wait`. Unbuilt → `BLOCKED`, never `FAIL`. Built when: M7b `src/pull_request.rs` +
  `src/checkout.rs` exist; M7c DESIGN §0 says slice (c) done; M7d `src/submit.rs`; M8 `src/cli.rs` +
  `target/release-fast/cli`.
- **Default PASS:** every `→` expectation holds, Zed stays alive, no unexpected toast/log error. Extra conditions are
  written `PASS: …`. Results: `PASS`, `FAIL-blocker` (crash, data loss, wrong GitHub post, unexpected checkout move),
  `FAIL-major`, `FAIL-minor` (wording/cosmetic), `BLOCKED`, `N/A` (+reason), `KNOWN` (§3). Evidence for every check:
  screenshot (`shot NN-slug`, then Read it; `zh shot --crop WxH+X+Y` for small text), `zh clip`, or saved shell output.
- Session gates: at the end `zh status` alive (dead = crash: save `zh logs 200`), `zh logs --errors` without panics or
  unexplained `code_review` errors, fixture/sandbox state as the steps say.

Notation: `k[...]` = `zh keys ...` (one token per key; quote `'['` `';'` `'$'`), `t"..."` = `zh type "..."` (newlines
aren't Enter: use `k[Return]`), `p"..."` = palette command (`pal "..."`), `sh:` = shell, `→` = expected. Palette names
are Zed's humanized action names (`code_review::ToggleHideReviewed` → "code_review: toggle hide reviewed").

## 1. Common setup

### 1.1 Environment (each Bash call is a new shell: start every call with `source $V/env.sh`)

```sh
V=$STATE/validation/FINAL; mkdir -p $V/{V1,V2,V3,V4,V5,V6,V7,V8}
cat > $V/env.sh <<'EOF'
V=$STATE/validation/FINAL
S=$STATE/sandbox; R=drew-council/code-review-sandbox
Z=~/personal/zed; W=$V/repo; H=$S/harness-repo
zh() { "$Z/script/live-harness/zh" "$@"; }
shot() { zh shot "$V/${SESSION:?set SESSION=V1..V8}/$1.png" >/dev/null && echo "$V/$SESSION/$1.png"; }
pal() { zh keys ctrl+shift+p && zh type "$1" && zh keys Return; }
wid() { zh status | sed -n 's/.* window \([0-9][0-9]*\).*/\1/p'; }
# Adds a block of bindings (JSON object text) after the harness's own keymap.
addkeys() { { sed '$d' "$Z/script/live-harness/config/keymap.json"; printf ',%s\n]\n' "$1"; } > "$(zh path config)/keymap.json"; }
EOF
```

### 1.2 Preflight (every session; paste the output into the report)

```sh
source $V/env.sh; git -C $Z log --oneline -1; pgrep -a cargo || echo "no cargo"
date -r $Z/target/release-fast/zed '+binary %F %T'; git -C $Z log -1 --format='last commit %ci'
ls $Z/crates/code_review/src/{pull_request,checkout,submit,cli}.rs $Z/target/release-fast/cli 2>&1
sed -n '/^\*\*Status:/,/^$/p' $Z/crates/code_review/DESIGN.md | head -12
```
The binary must be newer than the last commit; otherwise ask the orchestrator for a build (don't build).

### 1.3 Local fixture `$W` (run at the start of V1, V2, V3, V7, V8; it resets everything)

```sh
source $V/env.sh; cat > $V/mkfixture.sh <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
V=$STATE/validation/FINAL; W=$V/repo; ZH=$HOME/personal/zed/script/live-harness/zh
rm -rf "$W" "$V/origin.git" "$V/repo2" "$V/repo two"; mkdir -p "$W/src/util" "$W/docs" "$W/gen" "$W/vendor" "$W/cmd"
cd "$W"; export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
git init -q -b main; git config user.name "Val Idator"; git config user.email val@example.invalid
printf 'gen/** linguist-generated\nvendor/** linguist-vendored\n' > .gitattributes
for i in $(seq 1 60); do echo "pub fn f$i() -> u32 { $i }"; done > src/lib.rs
for i in $(seq 1 40); do echo "strings line $i"; done > src/util/strings.rs
printf 'pub fn add(a: u32, b: u32) -> u32 {\n    a + b\n}\n' > src/util/math.rs
for i in $(seq 1 30); do echo "Guide line $i"; done > docs/guide.md
for i in $(seq 1 10); do echo "note $i"; done > notes.txt
for i in $(seq 1 10); do echo "old $i"; done > old.txt
for i in $(seq 1 20); do echo "// generated $i"; done > gen/schema.rs
for i in $(seq 1 20); do echo "// vendored $i"; done > vendor/lib.js
printf 'module example.com/fixture\n\ngo 1.21\n' > go.mod
printf 'package main\n\nimport "fmt"\n\n// helperValue returns the answer.\nfunc helperValue() int {\n\treturn 42\n}\n\nfunc main() {\n\tfmt.Println("start")\n}\n' > cmd/main.go
git add -A; git commit -qm "init"
git init -q --bare "$V/origin.git"; git remote add origin "$V/origin.git"
git push -q origin main; git remote set-head origin main            # default branch = origin/main
git checkout -qb feature/x
sed -i 's/^strings line 5$/strings line FIVE/' src/util/strings.rs; git commit -qam "c1: Shout strings line five" -m "Body of *c1* with **markdown**."
sed -i 's/^Guide line 10$/Guide line TEN/' docs/guide.md; git commit -qam "c2: Shout guide line ten"
printf 'pub fn sub(a: u32, b: u32) -> u32 {\n    a - b\n}\n' >> src/util/math.rs; git commit -qam "c3: Add sub"
git checkout -q --orphan lonely; git rm -rqf .; echo lonely > lonely.txt; git add lonely.txt; git commit -qm lonely
git checkout -qf feature/x; rm -f lonely.txt
# Uncommitted: lib.rs 3 hunks (modify 5, delete 30, add after 50), pure deletion, staged deletion, untracked 300-row file
sed -i -e 's/^pub fn f5() -> u32 { 5 }$/pub fn five_changed() -> u32 { 5 }/' -e '/^pub fn f30() /d' src/lib.rs
sed -i '/^pub fn f50() /a pub fn added_after_fifty() -> u32 { 5050 }' src/lib.rs
sed -i '/^note 3$/d; /^note 4$/d' notes.txt; git rm -q old.txt
for i in $(seq 1 300); do if [ $i = 7 ]; then echo "big 7 $(printf 'wide%.0s' $(seq 1 60))"; else echo "big $i"; fi; done > src/big.rs
sed -i 's/^strings line 20$/strings line twenty/' src/util/strings.rs
sed -i 's|^// generated 5$|// generated FIVE|' gen/schema.rs; sed -i 's|^// vendored 5$|// vendored FIVE|' vendor/lib.js
sed -i 's|fmt.Println("start")|fmt.Println("start", helperValue())|' cmd/main.go
"$ZH" mkrepo "$V/repo2" >/dev/null; "$ZH" mkrepo "$V/repo two" >/dev/null
git status --short > "$V/fixture-status.txt"; git log --format='%h %s' > "$V/fixture-log.txt"
EOF
chmod +x $V/mkfixture.sh && $V/mkfixture.sh && cat $V/fixture-log.txt
```
Facts: HEAD `feature/x` = c3 → c2 → c1 → init; `origin/main` = init; `lonely` shares no history. Uncommitted review,
review order (directories first): `cmd/main.go`, `src/util/strings.rs`, `src/big.rs` (U, 300 rows; row 7 is 246
chars), `src/lib.rs` (3 hunks), `notes.txt` ("note 3"/"note 4" removed), `old.txt` (D); `gen/schema.rs` and
`vendor/lib.js` hidden → **6 shown**. Branch review adds `docs/guide.md`, `src/util/math.rs` → 8 shown.

### 1.4 GitHub sandbox (V4, V5, V6.15–V6.18)

```sh
source $V/env.sh; $S/reset.sh status | tee $V/sandbox-before.txt   # #1 threads=7; #2, #3 reviews=0; no pending
[ -d $S/repo/.git ] || git clone -q git@github.com:drew-council/code-review-sandbox.git $S/repo
rm -rf $H && git clone -q $S/repo $H && git -C $H remote set-url origin https://github.com/$R.git
git -C $H config credential.https://github.com.helper '!f() { test "$1" = get && printf "username=x-access-token\npassword=%s\n" "$GITHUB_TOKEN"; }; f'
git -C $H fetch -q origin
F=$S/fork-probe; rm -rf $F; git init -q $F; git -C $F -c user.name=h -c user.email=h@invalid commit -q --allow-empty -m init
git -C $F remote add origin https://github.com/drew-council/zed.git
# Start (and every restart) with the token written inline; never echo/export it, no set -x, no `zh env` in this command:
ZH_EXTRA_ENV="GITHUB_TOKEN=$(gh auth token) RUST_LOG=info,code_review=debug" zh start --fork --fresh --workdir $H
# Leak check before `zh stop --clean` (must list nothing, exit=1):
grep -rlF -f <(gh auth token) "$(zh path run)"; echo "exit=$?"
```
`$S/annotate.sh <files.json> <path>` prints a PR file's patch with L/R numbers (`gh api repos/$R/pulls/N/files >
/tmp/f.json`). PR A inventory: `plans/M7-live-verification.md` §3 (engine.rs R134 + reply, filters.rs R20–25,
strings.rs R48 resolved, engine.rs 223 outdated, engine.rs L193, docs/guide.md FILE, filters.rs R38 bot).

### 1.5 Harness habits

- `zh start --fork --fresh --workdir $W`; drop `--fresh` to keep Zed's DB. `zh restart --fork --workdir $W` keeps the
  DB (wait ~2 s after the last change). Graceful quit: `p"zed: quit"`. Review: `p"code_review: review uncommitted"`.
- Prompts ("Delete this comment?"): shot; `Return` = highlighted button, `Escape` = cancel; click if keys don't reach.
- In the review tab `ctrl+q` is visual line, not the user's close: close tabs with `k[: q Return]`.
- Live settings: `$(zh path config)/settings.json` (Zed reloads it; `start` re-copies the defaults).

## 2. Sessions

### V1 — Local targets, picker, line cursor, vim navigation (§2.2, §2.3, §2.4 center, §2.5)

Setup: `SESSION=V1`; fixture; `zh start --fork --fresh --workdir $W`. Picker rows: 0 Uncommitted, 1 Branch, 2 c3, 3
c2, 4 c1, 5 init (shot to confirm the highlight before `Return`).

- [ ] **V1.01** [B] `p"code_review: start review"` → Local section "Uncommitted changes", "Branch vs origin/main",
  commits c3, c2, c1, init (sha, subject); no Pull Requests section [M7b: hidden for a non-GitHub repo, no error].
- [ ] **V1.02** [B] Picker row 0 `Return` → tab "Review: Uncommitted changes", split (default `diff_view_style`), the 6
  files, panel title follows, tab tooltip "Read-Only Tab". PASS: `git status --short` == `$V/fixture-status.txt`.
- [ ] **V1.03** [B] Row 1 → "Review: feature/x vs origin/main", top block "Changes on feature/x since its merge base
  with [origin/main ▾]", 8 files incl. `docs/guide.md` and the uncommitted `src/big.rs`.
- [ ] **V1.04** [B] Click "origin/main ▾" (or `p"code_review: change base"`) → Zed's branch picker → `lonely` → "lonely
  and HEAD have no common history"; back to `origin/main` → files back. New branch tab vs `main` (picker `ctrl+Return`
  on row 1 = choose base); in it Change Base → `origin/main` → the existing tab activates (no third tab).
- [ ] **V1.05** [B] Row 3 (c2) → "Review: ‹sha7› c2: Shout guide line" (subject cut at 20), tooltip "Read-only snapshot
  (no language server)", only `docs/guide.md`, top block collapsed "Read-only snapshot"; `p"code_review: toggle
  header"` → sha, subject, "Val Idator", date. Row 4 (c1): body rendered as markdown. PASS: HEAD unchanged.
- [ ] **V1.06** [B] Row 2, `tab`, `tab` (c3+c2 checked) `Return` → "Review: 2 commits ‹c2›–‹c3›" (live), files
  `docs/guide.md`, `src/util/math.rs` only. `sh: echo extra >> $W/src/util/math.rs` → new row within ~2 s; `sh: git -C
  $W checkout -- src/util/math.rs`.
- [ ] **V1.07** [B] `ctrl+shift+s` on c2, ctrl-click c1 → "2 commits ‹c1›–‹c2›" read-only snapshot (strings.rs,
  guide.md). Selecting c3 then c1 checks c2 too (no gaps). PASS: `git status --short` == fixture.
- [ ] **V1.08** [B] Reopen Uncommitted (palette) and c2 (picker) → existing tabs activate; tab count unchanged.
- [ ] **V1.09** [B/M7b] Direct actions with arguments. Run the block, then press `k[ctrl+alt+shift+a]` … `h`:

  ```sh
  source $V/env.sh; addkeys '{"context":"Workspace","bindings":{"ctrl-alt-shift-a":"code_review::ReviewUncommitted",
  "ctrl-alt-shift-b":["code_review::ReviewBranch",{"base":"lonely"}],"ctrl-alt-shift-c":["code_review::ReviewBranch",{"base":"nope-404"}],
  "ctrl-alt-shift-d":["code_review::ReviewBranch",{"base":"-x"}],"ctrl-alt-shift-e":["code_review::ReviewCommits",{"commits":"HEAD~2..HEAD"}],
  "ctrl-alt-shift-f":["code_review::ReviewCommits",{"commits":"HEAD..HEAD~2"}],"ctrl-alt-shift-g":"code_review::ReviewCommits",
  "ctrl-alt-shift-h":["code_review::OpenPullRequest",{"number":1}]}}'
  ```
  → a activates Uncommitted; b toast "lonely and HEAD have no common history", no tab; c unknown-revision toast; d
  rejected with a toast; e opens "2 commits"; f toast (A not on B's first-parent history); g picker with c3 selected;
  h [M7b] "no GitHub remote" toast.
- [ ] **V1.10** [B] `sh: sed -i 's/"vim_mode": true,/"vim_mode": true, "diff_view_style": "unified",/'
  "$(zh path config)/settings.json"`; close + reopen Uncommitted → unified; `p"editor: toggle split diff"` → split.
  Restore the setting.

Line cursor and vim — Uncommitted, split, `]f` to `src/big.rs` row "big 1":

- [ ] **V1.11** [B] Full-width cursor row incl. gutter, mirrored on the left side (none facing a spacer), no caret,
  status NORMAL; `k[j j k]` moves one row each.
- [ ] **V1.12** [B] `k[5 j]` → "big 6"; `k[3 k]` → "big 3"; `k[2 0 G]` → multibuffer row 20; `k[g g]` → first row of
  the first file; `k[G]` → last row of the last file.
- [ ] **V1.13** [B] On big.rs, shot after each of `ctrl+d` `ctrl+u` (half page, cursor moves), `ctrl+f` `ctrl+b`
  (page), `ctrl+e` `ctrl+y` (view by one line), `z z` `z t` `z b`, `H` `L` → as in a normal Zed vim editor, highlight
  follows the cursor. `M` is PreviousComment here (vim's `M` given up, DESIGN): no move.
- [ ] **V1.14** [B] `k[/]` t"big 15" `k[Return]` → row "big 15"; `n` → "big 150"; `N` → "big 15" (no stuck `n`).
- [ ] **V1.15** [B] `k[w W b B '^' '$' '%' '*' '#' '(' ')' ',']`, `k[f a]`, `k[t a]` → row unchanged, NORMAL, no caret.
- [ ] **V1.16** [B] `k[a A I o O s S g i]`, `k[Insert]` (shot each) → stays NORMAL. PASS: `git status` == fixture.
- [ ] **V1.17** [B] `k[v j j]` → 3 whole rows highlighted on both sides; `Escape`; same with `V`, `ctrl+v`.
- [ ] **V1.18** [B] `k[/]` t"big 7 " `k[Return]`; `k[l l l]` → view scrolls right 4 columns per press; `j k` keep the
  scroll; `h h h` → x = 0; `h` → review panel focused. In visual mode at x = 0 `h` only scrolls. In split, `h` at the
  left edge of either side focuses the panel.
- [ ] **V1.19** [B] From `g g`: `] f` ×5 visits the 6 files in order up to `old.txt`; a 6th press doesn't move; `[ f`
  back to `cmd/main.go` and stops; `}` / `{` the same.
- [ ] **V1.20** [B] On `src/lib.rs`: `] c` → "five_changed" hunk → f30 hunk → "added_after_fifty" hunk → next file;
  from the last hunk `] c` wraps; `[ c` inside a hunk goes to its start.
- [ ] **V1.21** [B] `ctrl+w h` → left side focused, row mirrored; `ctrl+w l` back.
- [ ] **V1.22** [B] `e` on "big 15" → `src/big.rs` in a normal editor at line 15. In the c2 snapshot `e` opens the
  working-tree `docs/guide.md` at that row.
- [ ] **V1.23** [B] On big.rs `k[';' f]` → only big.rs, bar "Single file · 3 of 6  src/big.rs  Show All Files ;f",
  panel status "single file"; `] f` → lib.rs alone ("4 of 6"); `; f` → all files.
- [ ] **V1.24** [B] `; e` closes/opens the panel; `; h` focuses it; `; l` in the panel focuses the diff; `; e` in the
  panel closes it and focus returns to the diff.
- [ ] **V1.25** [B] Read-only guard on "five_changed": `k[d u]`, `k[d O]`, `k[d U]`, `k[alt+y]`, `k[x]`, `k[p]`,
  `k[: e '!' Return]`, `k[: e Return]`. PASS: `git status --short` and `git diff --cached --stat` equal the fixture.
- [ ] **V1.26** [B] LSP: `sh: sed -i 's/"enable_language_server": false/"enable_language_server": true/' "$(zh path
  config)/settings.json"`; reopen Uncommitted; wait until `zh logs 300 | grep -i gopls` shows it (≤60 s).
  `/helperValue` `Return`, `K` → hover "func helperValue() int"; `g d` → `cmd/main.go` line 6 in a normal editor; in
  the review `g A` → references. In the c2 snapshot `K` → nothing (no server).
- [ ] **V1.27** [B] `p"editor: toggle soft wrap"` (diff focused) → both sides wrap; again → no wrap.
- [ ] **V1.28** [B] `k[ctrl+shift+p]` t"code_review:" → every action of `code_review.rs` `actions!` + ReviewBranch,
  ReviewCommits (+ M7/M8 actions once built) listed (scroll + shots). `Escape`.

### V2 — Panel, reviewed marks, filters, mark persistence (§2.4 left dock, §2.5 `r`/`R`/`]u`, §2.6 marks)

Setup: `SESSION=V2`; fixture; `zh start --fork --fresh --workdir $W`; open Uncommitted; `; h`.

- [ ] **V2.01** [B] Panel: "Review: Uncommitted changes", "0/6", status "1 generated hidden · 1 vendored hidden", ⋯
  menu; tree `cmd/main.go` M, `src/` (`util/strings.rs` M, `big.rs` U, `lib.rs` M), `notes.txt` M, `old.txt` D (struck
  through); icons, indent guides, directories first.
- [ ] **V2.02** [B] `j`/`k` move only the selection; `h`/`l` collapse/expand `src/`; `space` toggles; `g g`/`G`; `l` on
  lib.rs shows it in the diff, focus stays; `Return` focuses the diff there; clicking notes.txt selects + shows it. With
  the diff focused, `] f` moves the panel selection (a file in a collapsed dir selects the dir).
- [ ] **V2.03** [B] Diff `g g`, `k[r]` → `cmd/main.go` folds, ✓, "1/6", cursor on strings.rs's first row. On the folded
  header `r` → unfolds, cursor on its first row, "0/6". `r` on `old.txt` (last) → stays on its folded header.
- [ ] **V2.04** [B] The header checkbox (click) and panel `r` toggle the mark without moving the cursor.
- [ ] **V2.05** [B] `k[R]` on "five_changed" → hunk control "✓ Reviewed", rows dimmed (kinds still visible); unified
  (toggle) still dimmed; `R` on a context row → toast; on a folded header → toast; `R` again → normal.
- [ ] **V2.06** [B] `r` big.rs → `] c`/`[ c` skip it; `] u`/`[ u` skip reviewed files and wrap.
- [ ] **V2.07** [B] `p"code_review: toggle hide reviewed"` → big.rs leaves panel and diff, status "1 reviewed hidden",
  progress still "1/6"; `r` on lib.rs → it disappears, cursor to the next shown file (wraps). Mark all → "All files
  reviewed". Toggle off.
- [ ] **V2.08** [B] Panel `k[i]` (prompt prefilled) t"src" `Return` → only src files, status "include src · 3 filtered
  out"; `I` clears. `e` t"\.txt$" → notes/old hidden; `E` clears. `i` t"(" → prompt stays with a regex error. Include
  "zzz" → "All 6 changed files are hidden" + hint. ⋯ "Clear Filters" clears include, exclude, hide-reviewed, not the
  generated/vendored defaults.
- [ ] **V2.09** [B] gen/vendor files absent from panel, diff, `] f`, `] c`. ⋯ "Show Generated" → `gen/schema.rs
  (generated)` in panel + diff, menu item checked; "Show Vendored" likewise; both off again.
- [ ] **V2.10** [B] Panel `k[/]` t"s" `Return` → first match selected, parents expanded; `n`/`N` cycle and wrap.
- [ ] **V2.11** [B] Right-click strings.rs → Mark as Reviewed, Open File, Copy Relative Path, Hide Reviewed / Show
  Generated / Show Vendored, Include/Exclude Filter…, Search Paths…, Clear Filters, Copy Branch Name. Copy Relative Path
  → clip `src/util/strings.rs`; Copy Branch Name (menu and `p"code_review: copy branch name"`) → `feature/x`; after
  `sh: git -C $W checkout -q --detach` it is disabled or toasts (then `git -C $W checkout -q feature/x`); Open File opens.
- [ ] **V2.12** [B] Mark strings.rs; `sh: sed -i 's/^strings line twenty$/strings line 20 again/'
  $W/src/util/strings.rs` → mark clears (unfolds, no ✓); `sed -i 's/^strings line 20 again$/strings line twenty/' …`
  → mark back. `R` on "five_changed", shell-edit the `added_after_fifty` line → five hunk stays dimmed; revert.
- [ ] **V2.13** [B] Unsaved edit: `sh: perl -0pi -e 's/"autosave": \{.*?\n  \},/"autosave": "off",/s' "$(zh path
  config)/settings.json"`; `e` on a notes.txt row, `i` t"X" `Escape` (unsaved), back to the review (`ctrl+pgup`), `r` on
  notes.txt → toast to save first; in the editor `u` (clean) → `r` works. `zh restart --fork --workdir $W` restores
  autosave.
- [ ] **V2.14** [B] Open Branch and the c2 snapshot too; `ctrl+pgdn`/`ctrl+pgup` → panel title/tree follow; a plain file
  tab (`ctrl+p` notes) → panel keeps the last review; `: q` a review → panel shows another; close all → "No review
  open".
- [ ] **V2.15** [B] `k[ctrl+right]` (user's pane::SplitRight) → a second tab of the review; `r` in one → ✓ + fold in
  both.
- [ ] **V2.16** [B] Branch review: mark `docs/guide.md`; Change Base → `main` → unmarked (other key); → `origin/main` →
  marked again.
- [ ] **V2.17** [B] Marks in Uncommitted (2 files + 1 hunk), Branch (1 file), c2 snapshot (guide.md); `zh restart`
  (no `--fresh`); reopen each → same marks, folds, dimming; on a folded header the mode is NORMAL, not Visual.

### V3 — Comments and export (§2.5 comment keys + old side, §2.6, §2.8)

Setup: `SESSION=V3`; fixture; `zh start --fork --fresh --workdir $W`; Uncommitted (split). Save with a quoted heredoc as
`$V/V3/expected-export.md` (exact bytes, one final newline):

```
I reviewed your code and have the following comments. Please address them.

Reviewing staged + unstaged changes

Comment types: ISSUE (problems to fix), SUGGESTION (improvements), NOTE (observations), PRAISE (positive feedback)

## Local tuicr Comments

1. **[PRAISE]** `Review Comment (scope: staged + unstaged changes)` - Overall solid
2. `notes.txt` - file note
3. **[NOTE]** `notes.txt:~3` - deleted note
4. **[SUGGESTION]** `src/big.rs:1-3` - first
   second
5. **[ISSUE]** `src/lib.rs:5` - issue on five
6. `src/lib.rs:~30` - old thirty
```

- [ ] **V3.01** [B] `k[/]` t"five_changed" `k[Return c]` → editor under row 5 in insert mode, placeholder "Leave a
  comment…"; t"issue on five" `k[Escape tab ctrl+Return]` → ISSUE card (error color); focus back on the row, NORMAL.
- [ ] **V3.02** [B] `] f` to big.rs "big 1": `k[V j j c]` → editor under row 3, "Lines 1–3"; t"first" `k[Return]`
  t"second" `k[Escape tab tab : w Return]` → two-line SUGGESTION card.
- [ ] **V3.03** [B] Unified (`p"editor: toggle split diff"`); `] f` to notes.txt; `k[j j]` → red "note 3"; `k[c]` →
  "Old line 3"; t"deleted note" `k[Escape tab tab tab Escape]` (normal-mode Esc leaves) → NOTE card under the red
  row. Back to split.
- [ ] **V3.04** [B] `k[/]` t"f31" `k[Return]`, `k[ctrl+w h]`, `k[k]` → red "pub fn f30" (shot) → `c` → editor on the
  left side, "Old line 30"; t"old thirty" `k[ctrl+s]` → card on the left side; `ctrl+w l`.
- [ ] **V3.05** [B] `] f` to notes.txt, `k[C]` → editor above its first row (below the header); t"file note"
  `k[Escape : x Return]`.
- [ ] **V3.06** [B] `k[';' c]` → editor in the top block's "Review comments"; t"Overall solid" `k[Escape tab tab tab
  tab Z Z]` → PRAISE card (success color).
- [ ] **V3.07** [B] `k[y]` → toast "Review copied to clipboard"; `zh clip > $V/V3/export.md; diff
  $V/V3/expected-export.md $V/V3/export.md && echo SAME`. PASS: `SAME`.
- [ ] **V3.08** [B] `Y` on row 5 → clip `issue on five`. `k[V j j y]` on "big 1" → toast "Copied 3 lines", clip
  `big 1\nbig 2\nbig 3` without trailing newline (`zh clip | od -c | tail -2`), NORMAL. Left side f30: `k[V y]` → clip
  `pub fn f30() -> u32 { 30 }`. `Y` on a row without a comment → "No comment at the cursor".
- [ ] **V3.09** [B] `i` on row 5 → editor, cursor at the start, insert (`Escape Escape`); `Return` on row 5 edits too;
  `Return` elsewhere moves down; `i` elsewhere does nothing.
- [ ] **V3.10** [B] Leave paths. Make G: `/strings line twenty` `Return` `c` t"scratch" `ctrl+Return`. Each round:
  `k[i]` t"X", then the keys below, shot the card (`Esc` = `k[Escape]`):

  | # | Keys | Text | | # | Keys | Text |
  |---|---|---|---|---|---|---|
  | 1 | `Esc : w q Return` | X×1+scratch | | 7 | `Esc :` t"%w /tmp/zhx" `Return` | X×7; alive; no `/tmp/zhx` |
  | 2 | `Esc : q Return` | X×2 | | 8 | `Esc : b d Return` | X×8 |
  | 3 | `Esc ctrl+w q` | X×3 | | 9 | `Esc : q ! Return` | X×8 (discarded) |
  | 4 | `Esc ctrl+q` (user's close) | X×4 | | 10 | `Esc Z Q` | X×8 |
  | 5 | `Esc : w ! Return` | X×5 | | 11 | `Esc : b d ! Return` | X×8 |
  | 6 | `Esc V :` (shows `:'<,'>`) `w Return` | X×6 | | 12 | `p"code_review: discard comment changes"` | X×8 |
- [ ] **V3.11** [B] On G: `i`, `Escape`, `tab` ×5 → Issue, Suggestion, Note, Praise, untyped; `shift+tab` reverses;
  set Issue, `: q ! Return` → still ISSUE. Insert-mode `tab` is text: `k[i]` t"a" `k[tab]` t"b" `k[ctrl+Return]`, `Y`
  → `zh clip | od -c` starts `a \t b`.
- [ ] **V3.12** [B] On G `i`, `Escape`, `d d` (empty), `ctrl+Return` → card keeps its old text. `c` on another row,
  `Escape Escape` → no card left, panel count unchanged.
- [ ] **V3.13** [B] New comment t"]f r y ;c dd" `ctrl+Return` → card shows exactly that, nothing else happened. New
  comment t"**bold** and `code`" → rendered bold + code.
- [ ] **V3.14** [B] `d d` on G → "Delete this comment?" + first line; `Escape` → kept; `d d` + confirm → gone. Delete
  the two V3.13 comments. `d d` on a row without a comment → toast. PASS: `y` + diff → `SAME`.
- [ ] **V3.15** [B] `g g`, `] t` ×6 → big.rs 1–3 → lib.rs 5 → lib.rs ~30 (left side) → notes.txt file → notes.txt ~3 →
  wraps to big.rs; `[ t` reverses; `m`/`M` = `]t`/`[t`; the review comment is skipped. With `; f` on big.rs, `] t`
  switches the shown file. `r` lib.rs, `] t` onto it → it unfolds; unmark.
- [ ] **V3.16** [B] `r` notes.txt → its cards vanish; `c` on the folded header → it unfolds and a *file* comment
  editor opens (`Escape Escape`); `r` until the ✓ is gone. Include filter `src` → notes.txt cards hidden, still in the panel
  list; clear.
- [ ] **V3.17** [B] `; h` → "Comments (6)": Review "Overall solid" (PRAISE), big.rs 1–3, `src/lib.rs:5`,
  `src/lib.rs:~30`, notes.txt (file), `notes.txt:~3` (review order). `j` skips the heading. `Return` on `~30` → diff
  focused on the left row; on Review → its editor opens (`Escape Escape`).
- [ ] **V3.18** [B] Review card's edit button (click) → editor (`Escape Escape`); delete button → prompt → `Escape`.
- [ ] **V3.19** [B] `sh: sed -i -e '1i big 0a' -e '1i big 0b' $W/src/big.rs` → big.rs card "Lines 3–5" under "big
  3", not outdated; `sh: sed -i '1,2d' $W/src/big.rs` → "Lines 1–3".
- [ ] **V3.20** [B] `zh restart --fork --workdir $W`, reopen → 6 cards with types, panel list. PASS: `y` → `SAME`.
- [ ] **V3.21** [B] `i` on lib.rs:5, t"Q" (insert), `p"zed: quit"`; `zh start --fork --workdir $W`, reopen → "Qissue on
  five". Remove the Q (`i`, `Escape`, `x`, `ctrl+Return`).
- [ ] **V3.22** [B] `zh stop`; `sh: sed -i 's/{ 5 }$/{ 55 }/' $W/src/lib.rs`; start, reopen → lib.rs:5 "OUTDATED" badge
  (tooltip), panel "outdated", export line ``5. **[ISSUE]** `src/lib.rs:5` (outdated) - issue on five``, `] t` reaches it.
- [ ] **V3.23** [B] Comment on "strings line twenty", then `sh: sed -i '/^strings line twenty$/d'
  $W/src/util/strings.rs` → OUTDATED at once, still on row 20 (panel `src/util/strings.rs:20`).
- [ ] **V3.24** [B] Banners. Branch review: comment, `y` → line 3 "Reviewing branch feature/x against origin/main (its
  commits since the merge base + staged + unstaged changes)", scope "branch feature/x vs origin/main"; its comments
  aren't Uncommitted's. c2 snapshot comment → "Reviewing commit: ‹sha7›", scope "selected commit range"; c1..c2 →
  "Reviewing commits: ‹c1›, ‹c2›". No comments: `zh clip-set SENTINEL`, `y` → "No comments to copy", clip SENTINEL.

### V4 — Pull requests: picker, checkout, description, threads, gate (§2.2 PR, §2.3, §2.4, §2.7)

Setup: `SESSION=V4`; §1.4; start with the token on `$H` (on `main`). Nothing is posted. PASS for the session:
`$S/reset.sh status` == `$V/sandbox-before.txt` at the end.

- [ ] **V4.01** [M7b] `p"code_review: start review"` → "Loading pull requests…", then "Pull Requests ·
  drew-council/code-review-sandbox" after Local: #1–#4 (not closed #5), `#N`, title, author (`github-actions` for
  #3/#4), `head → base`.
- [ ] **V4.02** [M7b] `k[ctrl+r]` → "No pull requests request your review"; `ctrl+r` → open PRs again (footer button
  too).
- [ ] **V4.03** [M7b] Type `1` → "Open #1" row first; same for `drew-council/code-review-sandbox#1` and
  `https://github.com/drew-council/code-review-sandbox/pull/1/files` (shot, clear). `#999` `Return` → not-found toast;
  `zed-industries/zed#1` → "Open this pull request from a checkout of zed-industries/zed."
- [ ] **V4.04** [M7b] #1 `Return` → `git -C $H rev-parse --abbrev-ref HEAD` = `feature/template-filters` at `3f22f1b`;
  tab "#1 Add template filters…"; panel docs/guide.md, src/engine.rs, src/filters.rs (A), src/legacy.rs (D),
  src/lib.rs, src/render.rs, src/util/strings.rs; gen/vendor hidden. Reopening #1 activates the tab (no fetch).
- [ ] **V4.05** [M7b] `p"code_review: open pull request"` → picker on the PR section, first PR selected.
- [ ] **V4.06** [M7b] `zh stop`; `git -C $H checkout -q main && git -C $H branch -f feature/template-filters
  5c5cdf4`; start (token); open #1 → HEAD fast-forwarded to `3f22f1b`.
- [ ] **V4.07** [M7b] `git -C $H checkout -q main; echo dirty >> $H/src/util/strings.rs`; open #2 → toast with git's
  refusal. PASS: edit still there, HEAD `main`, `git -C $H stash list` empty. Then `git -C $H checkout --
  src/util/strings.rs`.
- [ ] **V4.08** [M7b] Picker #2 `ctrl+Return` ("Review in Worktree") → `git -C $H worktree list` has `pr-2` on
  `fix/truncate-ellipsis`, opened as another workspace with the #2 review; main checkout unchanged. Again → switches to
  it. Mark a file there; open #2 in place later → the mark is there (path-independent key). Cleanup: `git -C $H worktree
  remove --force <path>`.
- [ ] **V4.09** [M7b] Fork PR via `refs/pull/N/head` → `pr/N`: every sandbox PR is same-repo → `N/A` (M7b real-git
  tests cover it).
- [ ] **V4.10** [M7b] User decision. `zh stop`; start (token) on `$S/fork-probe` → "Pull Requests ·
  zed-industries/zed" with upstream PRs. **Don't confirm a row.** Type `drew-council/zed#1` `Return` → "…#1 not found"
  for drew-council/zed (the fork itself is used; it has no PRs; nothing fetched).
- [ ] **V4.11** [M7b] Start on `$H` without `ZH_EXTRA_ENV` → PR status row = no-token hint (`GITHUB_TOKEN` / `gh auth
  login`); with `ZH_EXTRA_ENV="GITHUB_TOKEN=bad"` → rejected-token hint. Local rows work in both.
- [ ] **V4.12** [M7c] PR A in place (split). Top block expanded: "#1 Add template filters and retire the legacy
  renderer", drew-council, `feature/template-filters → main`, markdown body (Summary/Changes headings, list, rust block,
  "Note:" quote), summary "Initial pass…", "1 bot review hidden", toggle row "Resolved (1) · Outdated (1) · Bots (…)",
  then "Review comments". `p"code_review: toggle header"` collapses it to the header line.
- [ ] **V4.13** [M7c] Inline: engine.rs R134 with 2 comments (author · time; reply "Good call — …"); filters.rs card
  under row 25 "Lines 20–25" with a rust block; engine.rs L193 on the left side (unified: under the red row);
  docs/guide.md file thread above its first row; "Open on GitHub" buttons (don't click). Absent: strings.rs 48,
  engine.rs 223, filters.rs 38.
- [ ] **V4.14** [M7c] `p"code_review: toggle resolved threads"` → strings.rs R48 "Resolved"; `p"code_review: toggle
  outdated threads"` → engine.rs "Outdated / moved threads" item, original line 223; `p"code_review: toggle bot
  reviews"` → bot summary ("## Automated review" + table) and the filters.rs R38 suggestion thread. Toggling back hides.
- [ ] **V4.15** [M7c] On R134: `i` opens nothing, `d d` asks nothing, `Y` copies nothing remote. `] t` and the panel
  list include remote threads (muted, GitHub tag), or DESIGN §0 records the deferral (`N/A`).
- [ ] **V4.16** [M7c] Gate. engine.rs R59 (between two local hunks, inside GitHub's merged one): `c` → editor (`Escape
  Escape`). `p"editor: expand excerpts down"` on engine.rs's first excerpt until R8 shows (GitHub hunk `+1,7`) → `c` →
  "GitHub only accepts comments within its diff hunks". `sh: echo x >> $H/README.md` → `C` on README.md → "This file
  isn't part of the pull request on GitHub", `c` on its row → hunk refusal; `git -C $H checkout -- README.md`.
- [ ] **V4.17** [M7c] `git -C $H reset -q --hard 5c5cdf4` → "Checked out 5c5cdf4, PR head 3f22f1b"; threads move to
  per-file "moved: at …, line N" items; `reset --hard 3f22f1b` → inline again.
- [ ] **V4.18** [M7c] ⋯ "Copy Pull Request URL" (after Copy Branch Name) and `p"code_review: copy pull request url"` →
  `https://github.com/drew-council/code-review-sandbox/pull/1`; `p"code_review: reload pull request"` → same state.
  Draft on R134 (`c` t"draft" `ctrl+Return`), `y` → "Reviewing pull request drew-council/code-review-sandbox#1: Add
  template filters…", "URL: …/pull/1", "Head: 3f22f1b", scope "pull request drew-council/code-review-sandbox#1"; no
  remote threads in it. `d d` the draft.
- [ ] **V4.19** [M7c] Mark lib.rs + draft on filters.rs R30; `zh restart` (token); reopen #1 → both back. Clear both.
- [ ] **V4.20** [M7b] Optional: open `5` (closed, branch deleted) → error toast, HEAD unchanged (or opens with a
  "Closed" badge). PASS: no silent failure.
- [ ] **V4.21** [M7b] Optional `gh` token path: start with `ZH_EXTRA_ENV="GH_CONFIG_DIR=$HOME/.config/gh"` and no
  `GITHUB_TOKEN` → PRs load (`N/A` if gh keeps the token in a keyring the harness can't reach).

### V5 — Submit flows, sandbox writes (§2.6 types/body, §2.7 submit modal, `; s`)

Setup: `SESSION=V5`; §1.4; `$S/reset.sh clear-pending`; then (both must have 0 reviews, else `new-pr-c`/`new-bot-pr`):

```sh
PRC=$(gh pr list --repo $R --head fix/truncate-ellipsis --state open --json number --jq '.[0].number')
PRB=$(gh pr list --repo $R --state open --json number,headRefName --jq '[.[]|select(.headRefName|startswith("bot/harden-settings"))][0].number')
gh api repos/$R/pulls/$PRC/files > $V/V5/c-files.json; $S/annotate.sh $V/V5/c-files.json src/util/strings.rs
# after each submit:
gh api repos/$R/pulls/$N/reviews --jq '.[]|[.state,.body,.commit_id[0:7]]'
gh api repos/$R/pulls/$N/comments --jq '.[]|[.path,.subject_type,.start_side,.start_line,.side,.line,.body]'
```
Confirm with annotate: R26 added, R65–R68 spans context, L24 deleted, L23–L25 context → deleted (adjust rows if not).

- [ ] **V5.01** [M7d] `; s` in a local review → "Only pull request reviews can be submitted" (after M8: V6.10).
- [ ] **V5.02** [M7d] Own PR. Open PR C, `k[';' s]` → modal: Comment selected; Approve / Request changes disabled
  with "Can not approve your own pull request" / "…request changes…" tooltips; counts 0; button "Add Review Comment" →
  modal closes, review-comment editor opens; t"Looks fine overall" `k[Escape tab tab tab tab ctrl+Return]`.
- [ ] **V5.03** [M7d] strings.rs: `c` R26 t"one" `k[Escape tab ctrl+Return]` (Issue); `V` R65–R68 `c` t"range"; unified
  `c` on red L24 t"deleted"; split LHS `V` L23–L25 `c` t"old range"; `C` on docs/notes.md t"file" (each `ctrl+Return`).
  `; s` → counts line 4 / file 1 / review 1, preview "[PRAISE] Looks fine overall", no blockers → Submit → spinner,
  toast "Review submitted" + "Open on GitHub", modal closes. PASS (gh): one COMMENTED review, body `[PRAISE] Looks fine
  overall`; RIGHT 26 `[ISSUE] one`, RIGHT 65–68, LEFT 24, LEFT/LEFT 23–25, FILE docs/notes.md; no pending review left.
- [ ] **V5.04** [M7d] Drafts show SUBMITTED (panel "submitted"), `i`/`d d` do nothing on them, inline replaced by the
  refetched remote threads (drew-council); `y` → "No comments to copy"; still submitted after `zh restart`.
- [ ] **V5.05** [M7d] PR B, no drafts: `; s` → Comment → "Add Review Comment"; `right`/`tab`/click → Request changes →
  still "Add Review Comment"; Approve → "Submit". `Escape` → gh: 0 reviews.
- [ ] **V5.06** [M7d] `; s` → Approve → Submit → gh: APPROVED, body "".
- [ ] **V5.07** [M7d] `c` on an added row of src/cache.rs t"needs a test"; `; s` → Request changes → Submit (empty body
  allowed) → CHANGES_REQUESTED with that thread.
- [ ] **V5.08** [M7d] `sh: gh api -X POST repos/$R/pulls/$PRB/reviews -f commit_id=$(git -C $H rev-parse HEAD) --jq
  .state` (PENDING); `p"code_review: reload pull request"` → "You have a pending review on GitHub"; draft + `; s` →
  blocker "You already have a pending review on this PR. Submit or discard it on GitHub, then retry." + link, Submit
  disabled. `$S/reset.sh clear-pending`.
- [ ] **V5.09** [M7d] Draft, `; s` (no blockers), **then** create the pending review as above, then Submit → modal stays
  open with the pending-review hint; gh: no new submitted review; `clear-pending` deletes only yours.
- [ ] **V5.10** [M7d] Stale head (moves PR B's head on GitHub; sandbox only):

  ```sh
  set -- $(gh pr view $PRB --repo $R --json headRefName,headRefOid --jq '.headRefName+" "+.headRefOid')
  T=$(gh api repos/$R/git/commits/$2 --jq .tree.sha)
  N=$(gh api repos/$R/git/commits -f message="empty: move head" -f tree=$T -f "parents[]=$2" --jq .sha)
  gh api -X PATCH repos/$R/git/refs/heads/$1 -f sha=$N --silent
  ```
  Review comment only, `; s` → warning that the head moved and comments are anchored to the local sha7; Submit →
  COMMENTED with `commit_id` = the old head.
- [ ] **V5.11** [M7d] Draft on src/cache.rs; `sh: echo '// x' >> $H/src/cache.rs` → `; s` blocker lists src/cache.rs;
  `git -C $H checkout -- src/cache.rs` → gone. `Escape`.
- [ ] **V5.12** [M7d] Draft on a cache.rs row; `zh stop`; shell-edit that row; start (token), reopen → OUTDATED; `git -C
  $H checkout -- src/cache.rs` (stays outdated); `; s` → "1 outdated draft…" warning, excluded count 1. `Escape`, `d d`.
- [ ] **V5.13** [M7d] `$S/reset.sh new-pr-c` (closes the used PR C); reload the old PR C review → "Closed" badge; `; s`
  → closed warning; `left`/`right`/`tab`/click switch events; `Escape` → nothing posted.
- [ ] **V5.14** [M7d] `$S/reset.sh clear-pending; $S/reset.sh new-bot-pr; $S/reset.sh status | tee $V/sandbox-after.txt`
  → record new PR B/C numbers; `#1 threads=7` unchanged; leak check (§1.4).

### V6 — CLI, `--wait`, `zed://review` links (§2.3 CLI) — all [M8]

Needs `target/release-fast/cli` (orchestrator builds `-p zed -p cli`) **before** `zh start` (the CLI is resolved at
start). `SESSION=V6`; fixture; `zh start --fork --fresh --workdir $W`. `zh open ARGS` runs `cli --zed BIN --existing
ARGS` from `$W`; its exit code is the CLI's. Run `--wait` cases with Bash `run_in_background`, e.g. `zh open --wait
--review uncommitted >$V/V6/out.md 2>$V/V6/err.txt; echo $? >$V/V6/code.txt`. No-flag runs (if M8 added no `zh cli`):

```sh
zcli() { L=${XDG_RUNTIME_DIR:+$XDG_RUNTIME_DIR/zh}; L=${L:-/tmp/zh-$UID}; D=$(zh env | sed -n 's/^export DISPLAY=//p')
  env -i PATH="$PATH" HOME=$L/home XDG_CONFIG_HOME=$L/home/.config XDG_DATA_HOME=$L/home/.local/share \
    XDG_STATE_HOME=$L/home/.local/state XDG_CACHE_HOME=$L/home/.cache XDG_RUNTIME_DIR=$L/runtime DISPLAY=$D \
    $Z/target/release-fast/cli --zed $Z/target/release-fast/zed "$@"; }
enc() { python3 -I -c 'import sys, urllib.parse; print(urllib.parse.quote(sys.argv[1], safe=""))' "$1"; }
```

- [ ] **V6.01** `cli --help > $V/V6/help.txt` → `--review` lists the 8 TARGET forms, "--review must come before PATH",
  `--wait` exit codes 0/2/1; `-h` shows the first line; example `zed --review pr:123`.
- [ ] **V6.02** `zh open --review uncommitted` → exits 0 once the tab is open; still one window.
- [ ] **V6.03** `--review branch`, `branch:main`, `commit:HEAD~1`, `commits:HEAD~2..HEAD` → exit 0, matching tab each.
- [ ] **V6.04** Exit 1 + stderr, no tab: `commit:nope`; `bogus` (lists the forms); `123` ("did you mean pr:123?");
  `branch:lonely`; `--review` without value (1, not 2); `(cd $W && zcli . --review uncommitted)` ("--review must come
  before the repository path"); `--review uncommitted /tmp` (no repository).
- [ ] **V6.05** `--review uncommitted $W/src` → review of `$W`, same window; `… $V/repo2` → repo2 opens per
  `--existing`; `zh open -n --review uncommitted` → second window; `zcli --review uncommitted $V/repo2` → reuses repo2.
- [ ] **V6.06** bg `--wait --review uncommitted` → tab opens, no code after 5 s; `k[: q Return]` → code 2, stderr
  "Review closed without submitting".
- [ ] **V6.07** `zh clip-set SENTINEL`; bg `--wait`; `c` t"cli comment" `ctrl+Return`; `k[';' s]` → toast "Review sent to
  the terminal", code 0. PASS: clip still SENTINEL; then `y` and `zh clip | diff - $V/V6/out.md` empty.
- [ ] **V6.08** bg `--wait`, `; s` without comments → code 0, empty stdout.
- [ ] **V6.09** bg `--wait`, `p"zed: quit"` → code 2. Restart the harness.
- [ ] **V6.10** `; s` with no waiter: with a comment → clip = Markdown, toast "Review finished and copied as Markdown";
  without → "Review finished (no comments)"; drafts and tab stay.
- [ ] **V6.11** Two bg `--wait` (separate files); `ctrl+right` (clone); close the original → both still waiting; `; s`
  → both 0 with the same Markdown. Repeat with closes only (clone last) → both 2.
- [ ] **V6.12** bg `--wait`; `pkill -INT -f 'release-fast/cli.*--wait'` → Zed alive, tab open, no toast.
- [ ] **V6.13** bg `--wait --review branch`; Change Base → `main`; `; s` → code 0 (waiter followed the key switch).
- [ ] **V6.14** `zh open "zed://review?repo=$(enc $W)&target=branch%3Aorigin%2Fmain"` → branch review, exit 0;
  `repo=$(enc "$V/repo two")` works; `repo=relative%2Fpath` → error, nothing opens; unknown query keys ignored.
- [ ] **V6.15** [+M7b] Harness on `$H` with token, `git -C $H checkout -q main`: `zh open
  "zed://review?repo=$(enc $H)&target=pr%3A1"` → prompt "Check out pull request #1?"; Cancel → exit 1 "Review
  cancelled", HEAD `main`. `zh open --review pr:1` (trusted) → no prompt, checkout + tab.
- [ ] **V6.16** [+M7b] `--review drew-council/code-review-sandbox#1` and the PR URL → #1 (at head: no prompt, no fetch).
- [ ] **V6.17** Links never post or finish: no "submitted"/"finished" toast in V6.14–V6.16; `reset.sh status` unchanged.
- [ ] **V6.18** [+M7d] Fresh PR C: bg `--wait --review pr:$PRC`; review comment t"cli submit"; `; s` → Comment → Submit
  → code 0, stdout = `gh api repos/$R/pulls/$PRC/reviews --jq '.[-1].html_url'`. Then `$S/reset.sh new-pr-c`.

### V7 — Robustness: restarts, two windows, split toggles, narrow window, large repo

Setup: `SESSION=V7`; fixture; start on `$W`; recreate V3.01–V3.06 (or continue after V3 without `--fresh`).

- [ ] **V7.01** [B] `i` on lib.rs:5 (insert), `p"editor: toggle split diff"` ×3 → text, insert mode and focus kept;
  old-side cards move left side ↔ under red rows; folds and dimmed hunks survive.
- [ ] **V7.02** [B] `zh xdo windowsize $(wid) 900 1000` → auto-unified (`minimum_split_diff_width`), cards re-hosted, an
  open editor keeps text/focus; `zh xdo windowsize $(wid) 1600 1000` → split again.
- [ ] **V7.03** [B] `p"workspace: new window"`, `p"workspace: open"` → path prompt `$W/src/` → `p"code_review: review
  uncommitted"` → same comments; a comment made in window 2 shows in window 1 (raise with `zh xdo windowraise` /
  `windowfocus` on ids from `zh xdo search --pid <zed pid>`); `sh: sed -i '1i // top' $W/src/lib.rs` moves lib.rs cards
  in both; after 5 s `top -b -n1 -p <zed pid>` < 10 % CPU (no ping-pong). Revert.
- [ ] **V7.04** [B] `zh restart` → comments, marks, types, panel list intact in Uncommitted, Branch, snapshot.
- [ ] **V7.05** [B] Open Uncommitted, Branch, a live range, a snapshot, split one (`ctrl+right`); cycle → panel follows;
  close one by one → panel falls back each time, no crash.
- [ ] **V7.06** [B] Large repo, read-only (never open `$Z`): `rm -rf /tmp/zed-smoke && git clone -q --shared $Z
  /tmp/zed-smoke && git -C /tmp/zed-smoke remote set-head origin main`; save `rev-parse HEAD` + `status --porcelain`;
  start on `/tmp/zed-smoke`. (a) `p"code_review: review branch"` (code-review vs origin/main, ~53 files incl. huge
  `research/*.html`); (b) `addkeys '{"bindings":{"ctrl-alt-shift-s":["code_review::ReviewCommits",{"commits":"v1.25.0-pre~100..v1.25.0-pre"}]}}'`
  (snapshot, ~305 files); (c) live `HEAD~20..HEAD`. Each: time to first excerpt, `] f` / `] c` / `G` / `j` latency,
  `r` on 3 files, a comment + `y`, RSS (`ps -o rss= -p <pid>`). PASS: loads < 60 s, keys < 1 s, logs clean, clone
  HEAD/status unchanged. `rm -rf /tmp/zed-smoke`.
- [ ] **V7.07** [B] `zh logs --errors` over V7: only known noise (`org.freedesktop.secrets`, xinput).
- [ ] **V7.08** [M7c] PR A (token on `$H`): split toggled ×2 → thread cards re-host (L193 left side ↔ unified);
  `zh restart` (token) → threads reload.

### V8 — Non-vim mode (§2.5 "everything in the palette", §2.6 non-vim keys)

Setup: `SESSION=V8`; fixture; `sed 's/"vim_mode": true/"vim_mode": false/' $Z/script/live-harness/config/settings.json
> $V/settings-novim.json`; `zh start --fork --fresh --workdir $W --settings $V/settings-novim.json`.

- [ ] **V8.01** [B] Palette start review; `tab` multi-selects commits; `Return` opens the range.
- [ ] **V8.02** [B] Arrows/PageDown move a full-width line cursor, no caret; typing letters edits nothing (`git status`).
- [ ] **V8.03** [B] Palette: next/previous file, next/previous hunk, toggle file/hunk reviewed, toggle single file mode,
  open file at cursor, focus panel/diff, toggle panel → same effects as the vim keys.
- [ ] **V8.04** [B] `p"code_review: add comment"` → plain editor; `Return` = new line, `ctrl+Return` saves;
  `p"code_review: edit comment"` + `ctrl+s` saves; `Escape` leaves keeping text; `p"code_review: discard comment
  changes"` discards; `p"code_review: cycle comment type"` changes the badge; `p"code_review: delete comment"` →
  prompt → deleted; file and review comments via the palette.
- [ ] **V8.05** [B] Panel keys (`code_review.json`): j k h l space Return r i e I E / n N g g G as in V2.
- [ ] **V8.06** [B] `p"code_review: copy review as markdown"` / `p"code_review: copy comment at cursor"` → clipboard.
- [ ] **V8.07** [M7d/M8] `p"code_review: submit review"`: local → finish toast; PR → modal (Cancel).

## 3. Known issues (report as `KNOWN`) — DESIGN §0

Cursor highlight bleeding onto spacer rows below an empty row; split blocks below a changed row mirrored at the hunk
start; vim relative numbers count across files, the left side's current line shows 0; left side mis-laid-out for one
frame after a soft-wrap toggle; vim status "N lines, M characters" on a folded header; `e` on a folded header opens the
last row; `g l`/`g a` show only the newest selection; multi-line regex errors; file-row tooltip over the context menu;
OUTDATED stays after a stash/pop; two windows with unsaved edits disagree on line numbers; `:qa`/`:wqa` close review
tabs and keep open comment text; cursor colors blend on dimmed rows; generated files may flash before
`.gitattributes` loads; review tabs aren't restored after a restart (reopen them); counts don't multiply review
actions (`3]f` = `]f`).

## 4. Coverage matrix (DESIGN §2 → checks)

| Requirement | Checks |
|---|---|
| §2.2 Uncommitted, Branch + Change Base, commit, live range, snapshot; checkout never moved | V1.02–V1.08, V1.10 |
| §2.2 PR checkout in place, fast-forward, `refs/pull` fork path, dirty tree no stash, new worktree | V4.04, V4.06, V4.09, V4.07, V4.08 |
| §2.2 GitHub token (env, `gh`), host from remote, error hints | V4.11, V4.21, V4.03 |
| §2.3 Picker Local + PR section, review-requested toggle, open by number/URL | V1.01, V4.01–V4.05 |
| §2.3 Direct actions for keymaps | V1.09 |
| §2.3 CLI targets, REPO_DIR, `--help`, window reuse, `-n`, `--wait` exit codes, `zed://review` links | V6.01–V6.18 |
| §2.4 Panel title/status, tree, letters, ✓, filters, search, gitattributes, hide reviewed | V2.01–V2.11 |
| §2.4 Comment list; copy branch / PR URL | V3.17, V2.11, V4.18 |
| §2.4 Read-only real-buffer diff with LSP; split/unified per setting | V1.25, V1.26, V1.10 |
| §2.4 Top block: PR description (collapsible), commit messages, review comments | V4.12, V1.05, V3.06 |
| §2.4 Reviewed files fold; single-file mode; several tabs, panel follows | V2.03, V1.23, V2.14, V7.05 |
| §2.5 Line cursor + mirror; counts, `{N}G`, gg/G, ctrl-d/u/f/b/e/y, zz/zt/zb, H/L, `/ n N` | V1.11–V1.14 |
| §2.5 Dead char motions, no insert, v/V line-wise, h/l + left-edge focus | V1.15–V1.18 |
| §2.5 Key table: `]f [f } { ]c [c ]u [u ]t [t m M c C ;c i Enter dd r R e y Y` vis-`y` `;f ;e ;h ;l` | V1.19–V1.24, V2.03–V2.06, V3.01–V3.15 |
| §2.5 Old-side comments: unified deleted rows, split via `ctrl-w h` | V3.03, V3.04 |
| §2.5 Panel keys; everything in the palette | V2.02, V2.08, V2.10, V1.28, V8 |
| §2.6 Inline cards, insert mode, markdown, every leave key, `:q!`, types + Tab, `dd` | V3.01–V3.14 |
| §2.6 Persistence by target; marks clear on change; drafts follow code; outdated | V2.12, V2.16, V2.17, V3.19–V3.23, V4.19 |
| §2.7 Read-only threads; resolved/outdated hidden + toggles; bot summaries and threads hidden | V4.12–V4.15 |
| §2.7 Fork lists upstream; `owner/repo#N` reaches the fork | V4.10 |
| §2.7 Submit: events, counts, preview, stale head, body prompt, success, refetch, errors | V5.02–V5.13, V6.18 |
| §2.7 Comments only on rows GitHub accepts | V4.16 |
| §2.8 Export format, `Y`, visual `y`, banners, PR preamble | V3.07, V3.08, V3.24, V4.18 |
| User decisions: `; s`, bots hidden, upstream PRs for forks, sandbox-only writes | V5.02, V4.13–V4.14, V4.10, §0 |
| Robustness: restart, quit, two windows, split toggles, narrow window, large repo | V3.20–V3.21, V7.01–V7.06 |

## 5. Results (`$V/results.md`: one block per session, then the summary)

```
## V<n> — <agent> — <date> — commit <sha7> — binary <mtime> — built: B M7b? M7c? M7d? M8?
Preflight: <paste>
| ID | Dep | Result | Evidence | Notes / bug |
|---|---|---|---|---|
| V1.01 | B | PASS | V1/01-picker.png | |
Session gates: Zed alive <y/n> · logs <clean/attached> · fixture/sandbox state <ok>
### Bugs (one per FAIL): **V<n>.<m>** title — severity · exact keys · expected · actual · evidence · log lines

## Summary
| Session | Checks | PASS | FAIL b/M/m | BLOCKED | N/A | KNOWN | Agent | Date |
|---|---|---|---|---|---|---|---|---|
| V1 Local targets & navigation | 28 | | | | | | | |
| V2 Panel & marks | 17 | | | | | | | |
| V3 Comments & export | 24 | | | | | | | |
| V4 PR browse/checkout/threads | 21 | | | | | | | |
| V5 PR submit (sandbox writes) | 14 | | | | | | | |
| V6 CLI & links | 18 | | | | | | | |
| V7 Robustness & large repo | 8 | | | | | | | |
| V8 Non-vim | 7 | | | | | | | |
Sandbox PRs after the run: A #1 (untouched), B #…, C #…, D #4. Leak checks: exit=1 each.
```

Order: V1 → V2 → V3 → V7 → V8 now (local only); V4 → V5 once M7b–d are built; V6 once M8 is (V6.15–V6.18 need the
sandbox). Sessions run one after another (one harness).
