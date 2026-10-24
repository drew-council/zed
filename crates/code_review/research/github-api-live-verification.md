> Research record, copied from the orchestrator's `plans/M7-live-verification.md` after the final
> validation. `$STATE` stands for the orchestrator's state directory (outside the repository: plans,
> sandbox scripts such as `$STATE/sandbox/reset.sh`, validation output), and `scratchpad/…` for the M7
> agent's scratch directory; neither is kept in the repository. Sandbox PR numbers have changed since
> (see `final-validation/SUMMARY.md`).

# M7: live GitHub verification results

Run on 2026-10-16/17 against **https://github.com/drew-council/code-review-sandbox** (private, owner
`drew-council` = the logged-in `gh` account). Nothing was written to any other repo. This file covers
the M7 plan's §6 cases L1–L14 (`plans/M7-github.md`) plus extra probes. It ends with the resulting
policy changes and a reset recipe.

All REST calls used `X-GitHub-Api-Version: 2022-11-28` (the response confirms
`X-Github-Api-Version-Selected: 2022-11-28`) and the `gh` OAuth token (scopes `repo, read:org, gist, …`).
Raw artifacts are in `scratchpad/sandbox/out/L/`: `<case>.req.json` holds the request body,
`<case>.res.txt` the status line, headers and body, and `log.txt` has one line per call. The helpers
are `scratchpad/sandbox/probe.sh` (`rest`, `gql`, `pending`), `annotate.sh` (prints a PR file's patch
with LEFT/RIGHT numbers) and `reset.sh`.

## 1. Sandbox contents

**Repo settings changed (sandbox only):** `PUT /repos/drew-council/code-review-sandbox/actions/permissions/workflow`
`{"default_workflow_permissions":"write","can_approve_pull_request_reviews":true}` → 204. No account
settings were touched. Visibility is private.

**`main` @ `deea36cf8fa38276ef75801d05773c4c35c48d45`** holds a small Rust crate, "textkit":
`README.md`, `Cargo.toml`, `.gitignore`, `.gitattributes` (`gen/schema.rs linguist-generated`,
`vendor/lib.js linguist-vendored`), `src/lib.rs`, `src/engine.rs` (269 lines), `src/util.rs`,
`src/util/strings.rs`, `src/format.rs`, `src/legacy.rs`, `src/config.rs`, `docs/guide.md`,
`docs/notes.md`, `gen/schema.rs`, `vendor/lib.js`, and two workflows:

- `.github/workflows/bot-pr.yml` (`workflow_dispatch`, inputs `source_ref`, `branch`, `title`, `body`):
  pushes `source_ref` to `branch` with `GITHUB_TOKEN`, then runs `gh pr create`. The PR author is
  `github-actions[bot]`.
- `.github/workflows/bot-review.yml` (`workflow_dispatch`, inputs `pr`, `payload`): pipes `payload` (a
  create-review JSON body) to `gh api POST repos/…/pulls/$PR/reviews` as `github-actions[bot]`.

Branches: `main`, `feature/template-filters` (PR A), `fix/truncate-ellipsis` (PR C),
`fixture/bot-cleanup` (the source for the bot PRs: 2 commits, `665f27b…` then `073e29a…`),
`bot/harden-settings` (PR B), `bot/api-probe` (PR D).

## 2. Pull requests

| PR | URL | Author | Head → base | Purpose |
|---|---|---|---|---|
| **A** #1 | https://github.com/drew-council/code-review-sandbox/pull/1 | drew-council | `feature/template-filters` @ `3f22f1b3bd52ed79a655a055209707e356fa8253` | **Read-only rendering fixture.** Has the prepared thread state (§3). Don't submit reviews to it in tests. |
| **C** #2 | https://github.com/drew-council/code-review-sandbox/pull/2 | drew-council | `fix/truncate-ellipsis` @ `632ee13692b487a7ea71911f473781416f35ff01` | **Clean submit tests (COMMENT only).** 0 reviews, 0 threads. Approve/Request changes fail here (own PR), which also tests the error path. |
| **B** #3 | https://github.com/drew-council/code-review-sandbox/pull/3 | github-actions[bot] | `bot/harden-settings` @ `073e29a647c8d008697867d1b92a0277fba99401` | **Clean Approve / Request changes tests.** 0 reviews, 0 threads. |
| **D** #4 | https://github.com/drew-council/code-review-sandbox/pull/4 | github-actions[bot] | `bot/api-probe` @ `073e29a…` (same content as B) | **API probe PR.** Holds all L-case residue (15 submitted reviews, 19 threads). Free for future API experiments. Don't use it for UI tests. |
| #5 (closed) | https://github.com/drew-council/code-review-sandbox/pull/5 | drew-council | (branch deleted) | One-off test of `reset.sh replay-pr-a`. It reproduced PR A's thread table exactly. Closed. |

Every PR body is markdown with headings, a list, a fenced code block, and a blockquote, task list or
table (B/D).

GraphQL node ids: PR A `PR_kwDOVM9_N88AAAABI92VhA`, C `PR_kwDOVM9_N88AAAABI92aPQ`, B
`PR_kwDOVM9_N88AAAABI92vEQ`, D `PR_kwDOVM9_N88AAAABI924hA`. The bot PRs' GraphQL author is
`{__typename: "Bot", login: "github-actions"}`. `gh pr list` shows them as `app/github-actions`.

### GitHub hunks (`GET pulls/N/files`; these decide which lines are commentable, §5)

**PR A** (4 commits: `1735fd3` filters, `7e8a57c` delete/rename, `5c5cdf4` generated/vendored,
`3f22f1b` outdating push):

| File | Status | Hunks |
|---|---|---|
| docs/guide.md | modified | `-23,6 +23,26` `-32,8 +52,3` |
| gen/schema.rs (linguist-generated) | modified | `-1,7 +1,7` `-26,4 +26,9` |
| src/engine.rs | modified | `-1,6 +1,7` `-11,6 +12,7` `-25,6 +27,9` `-43,15 +48,19` `-121,9 +130,20` `-145,7 +165,9` `-178,20 +200,17` `-200,11 +219,15` `-266,4 +289,24` |
| src/filters.rs | added | `-0,0 +1,71` |
| src/legacy.rs | removed | `-1,22 +0,0` |
| src/lib.rs | modified | `-1,7 +1,7` |
| src/render.rs (from src/format.rs) | renamed + 1 line | `-17,6 +17,7` |
| src/util/strings.rs | modified | `-42,6 +42,16` `-69,4 +79,9` |
| vendor/lib.js (linguist-vendored) | modified | `-1,4 +1,4` `-12,22 +12,24` |

**PR B / PR D** (2 commits: `665f27b` main change, `073e29a` "Reset cache statistics on clear"):

| File | Status | Hunks |
|---|---|---|
| README.md | modified | `-7,7 +7,8` |
| docs/design-notes.md (from docs/notes.md) | **pure rename, no `patch`** | — |
| docs/guide.md | modified | `-25,7 +25,7` |
| src/cache.rs | added | `-0,0 +1,60` |
| src/engine.rs | modified | `-3,8 +3,9` `-188,9 +189,10` `-266,4 +268,19` (hunks ~180 lines apart) |
| src/lib.rs | modified | `-1,7 +1,8` |
| src/settings.rs (from src/config.rs) | renamed + edit | `-37,7 +37,9` |
| src/util/strings.rs | modified, deletions only | `-26,22 +26,6` `-64,9 +48,4` |

**PR C:** `docs/notes.md` `-4,3 +4,4`; `src/util/strings.rs` `-21,8 +21,9` `-61,8 +62,10`.

## 3. PR A prepared state (inventory)

Commits: the threads were created at `5c5cdf4`. Then `3f22f1b` rewrote `src/engine.rs:223`
(`is_truthy`), which outdated thread 4 only.

| # | Thread id | Path | subjectType | diffSide | startDiffSide | line | startLine | originalLine | originalStartLine | resolved | outdated | comments | author | comment databaseIds |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 current + reply | `PRRT_kwDOVM9_N86tcJxI` | `src/engine.rs` | LINE | RIGHT | – | 134 | 134 | 134 | null | false | false | 2 | User:drew-council | 4301011954, 4301012457 |
| 2 multi-line | `PRRT_kwDOVM9_N86tcJxN` | `src/filters.rs` | LINE | RIGHT | RIGHT | 25 | 20 | 25 | 20 | false | false | 1 | User:drew-council | 4301011958 |
| 3 resolved | `PRRT_kwDOVM9_N86tcJxO` | `src/util/strings.rs` | LINE | RIGHT | – | 48 | 48 | 48 | null | **true** | false | 1 | User:drew-council | 4301011959 |
| 4 outdated | `PRRT_kwDOVM9_N86tcJxR` | `src/engine.rs` | LINE | RIGHT | – | **null** | null | 223 | null | false | **true** | 1 | User:drew-council | 4301011963 |
| 5 LEFT (deleted line) | `PRRT_kwDOVM9_N86tcJxU` | `src/engine.rs` | LINE | **LEFT** | – | 193 | 193 | 193 | null | false | false | 1 | User:drew-council | 4301011968 |
| 6 file-level | `PRRT_kwDOVM9_N86tcJ3I` | `docs/guide.md` | **FILE** | RIGHT | – | 1 | null | 1 | null | false | false | 1 | User:drew-council | 4301012553 |
| 7 bot inline | `PRRT_kwDOVM9_N86tcKM3` | `src/filters.rs` | LINE | RIGHT | – | 38 | 38 | 38 | null | false | false | 1 | **Bot:github-actions** | 4301014871 |

Thread 1's body has inline code. Thread 2's body has a fenced `rust` block. Thread 7's body has a
GitHub ```` ```suggestion ```` block.

Reviews (GraphQL `reviews`):

| Review | databaseId | State | Author | Body | Comments |
|---|---|---|---|---|---|
| `PRR_kwDOVM9_N88AAAABSyT2YQ` | 5555680865 | COMMENTED | User drew-council | "Initial pass. Mostly questions…" + bullet list (markdown) | 5 (threads 1–5) |
| `PRR_kwDOVM9_N88AAAABSyT4VA` | 5555681364 | COMMENTED | User drew-council | `""` (the reply's implicit review) | 1 |
| `PRR_kwDOVM9_N88AAAABSyT4nA` | 5555681436 | COMMENTED | User drew-council | `""` (the file comment's implicit review) | 1 |
| `PRR_kwDOVM9_N88AAAABSyUC4w` | 5555684067 | COMMENTED | **Bot github-actions** (REST `github-actions[bot]`, `type: Bot`) | "## Automated review" + table + `<details>` | 1 (thread 7) |

Top-level conversation comment: `IC_kwDOVM9_N88AAAABc0PS5A` (databaseId 6228792036, drew-council).
`reviewDecision` is null. After dropping empty bodies (the plan's tuicr rule), the summaries left are 1 user
summary and 1 bot summary, so the bot one can be hidden by default.

## 4. Live cases (requests and observed responses)

Unless noted, the request is `POST /repos/drew-council/code-review-sandbox/pulls/{n}/reviews`, and
"pending" means the same call without `event`. Each pending probe was then fetched through GraphQL
and deleted (`DELETE …/reviews/{id}` → 200), so none remain. Line notation: `R10` = RIGHT/new line
10, `L9` = LEFT/old line 9. PR D head at test time: `H2 = 073e29a647c8d008697867d1b92a0277fba99401`;
older PR commit `H1 = 665f27ba9d571eecc9697c33eed11db617162bb3`.

### L1–L4: body requirement (PR D, submitted)

| Case | Request body (besides `commit_id: H2`) | Status | Response |
|---|---|---|---|
| L1 | `{"event":"COMMENT","body":"","comments":[{path:"src/cache.rs",line:20,side:"RIGHT",body:"…"}]}` | **200** | `state: COMMENTED` |
| L1b | COMMENT, `body` omitted, 1 inline | **200** | COMMENTED |
| L2 | `{"event":"COMMENT","body":""}` | **422** | `{"message":"Unprocessable Entity","errors":[""],"documentation_url":"…#create-a-review-for-a-pull-request","status":"422"}` (**the error string is empty**) |
| L2b | COMMENT, body omitted, 0 comments | 422 | same `errors:[""]` |
| L2c | COMMENT, `body:"   \n "`, 0 comments | 422 | same `errors:[""]` (whitespace counts as empty) |
| L3 | `{"event":"REQUEST_CHANGES","body":"","comments":[1 inline]}` | **200** | CHANGES_REQUESTED |
| L3e | REQUEST_CHANGES, body omitted, 1 inline | 200 | CHANGES_REQUESTED |
| L3b | `{"event":"REQUEST_CHANGES","body":""}`, 0 comments | **200 (!)** | CHANGES_REQUESTED, `body:""` |
| L3b2/L3c/L3d/L3b4/L3f | REQUEST_CHANGES with 0 comments and body `""`, omitted, or `"  "`, retried in several orders, and once after dismissing L3b's review | **422** | `errors:["Review You need to leave a comment indicating the requested changes."]` |
| L3b5 | REQUEST_CHANGES `body:""`, 0 comments, sent right after L1 (a COMMENT review with an inline comment) | **200** | CHANGES_REQUESTED |
| L4 | `{"event":"APPROVE","body":""}` | 200 | APPROVED |
| L4b | APPROVE, body omitted | 200 | APPROVED |
| L4c | APPROVE, `body:""`, 1 inline | 200 | APPROVED (the inline comment is kept) |
| L4d | `event:"APPROVED"` (invalid) | 422 | `errors:["Variable $event of type PullRequestReviewEvent was provided invalid value"]` |

**Interpretation:** REQUEST_CHANGES with an empty body and no comments succeeds only when the
reviewer has review comments left over that no earlier CHANGES_REQUESTED review has "used up": L3b
followed the A1 COMMENT review, and L3b5 followed L1. Otherwise it fails. That depends on history, so
don't rely on it.

### Own-PR checks (PR C, nothing was created)

| Case | Request | Status | errors[0] |
|---|---|---|---|
| O1 | APPROVE `body:""` | 422 | `Review Can not approve your own pull request` |
| O2 | REQUEST_CHANGES `body:"needs work"` | 422 | `Review Can not request changes on your own pull request` |
| O3 | REQUEST_CHANGES `body:""` | 422 | `Review You need to leave a comment indicating the requested changes.` (the body check runs **before** the own-PR check) |
| O4 | APPROVE `body:"lgtm"` + 1 inline | 422 | `Review Can not approve your own pull request` |

### L5–L9, L14: which lines are commentable (PR D, pending + delete)

Every 422 here is `{"message":"Unprocessable Entity","errors":["<text>"],…}`.

| Case | Comment | Status | errors / stored anchor (GraphQL) |
|---|---|---|---|
| L5a | `src/engine.rs` R10, a context row 2 below the change, in hunk `+3,9` (R3–R11) | 200 | RIGHT line 10 |
| L5b | R3 (first context row of the hunk) | 200 | RIGHT 3 |
| L5d | R11 (last context row of the hunk) | 200 | RIGHT 11 |
| L5c | R12 (1 row below the hunk) | 422 | `Line could not be resolved` |
| L5e | R2 (1 row above the hunk) | 422 | `Line could not be resolved` |
| L6 | **LEFT** L9, a context row (old 9 = new 10) | **200** | diffSide LEFT, line 9, **startLine 10**, startDiffSide null |
| L6b | LEFT L192 (deleted row) | 200 | LEFT 192 |
| L6c | `src/util/strings.rs` RIGHT 35 (exists in the new file but outside the new hunks) | 422 | `Line could not be resolved` |
| L7 | engine.rs R100 (unchanged, more than 80 rows from any hunk) | **422** | `Line could not be resolved` |
| L7b | R999 (past EOF) | 422 | `Line could not be resolved` |
| L7c | `src/render.rs` (not in the PR) | 422 | `Path could not be resolved` |
| L7c2 | `vendor/lib.js` (in the repo, unchanged by the PR) | 422 | `Path could not be resolved` |
| L7e | LEFT L100 (outside hunks) | 422 | `Line could not be resolved` |
| L7f | `src/cache.rs` R10 (added file) | 200 | RIGHT 10 |
| L7g | `docs/design-notes.md` R1 (**pure rename, no patch**) | 422 | `Line could not be resolved` |
| L7h | R8 with **`side` omitted** | 200 | defaults to RIGHT |
| L7i | line 0 | 422 | `Line could not be resolved` |
| L7j | comment `body:""` | 200 (pending) | accepted at creation; not submitted |
| L8 | range R8 → R192 (**across** hunks 1 and 2) | **200** | RIGHT, startDiffSide RIGHT, startLine 8, line 192 |
| L8b | range R8 → R15 (inside hunk to outside) | 422 | `Line could not be resolved` |
| L8c | range R100 → R105 (both ends outside) | 422 | `Line could not be resolved` |
| L8d | range R5 → R10 (one hunk, context included) | 200 | startLine 5, line 10 |
| L8e | range R1 → R6 (starts outside) | 422 | `Line could not be resolved` |
| L9 | `start_line:191,start_side:"LEFT"` (deleted) → `line:195,side:"RIGHT"` (added) | **200** | diffSide RIGHT, startDiffSide LEFT, startLine 191, line 195 |
| L9b | start RIGHT 192 → end LEFT 193 (the end is above the start in diff order) | 422 | `Start position could not be resolved` |
| L9c | start R10 → end R8 (start after end) | 422 | `Start position could not be resolved` |
| L9d | start R8 = end R8, both RIGHT | 200 | startDiffSide **RIGHT**, startLine 8 = line 8 |
| L9e | `start_line:6`, **`start_side` omitted**, `line:8,side:RIGHT` | 200 | startDiffSide RIGHT (defaults to `side`) |
| L9f | `src/util/strings.rs` LEFT 29 → LEFT 43 (deleted block) | 200 | LEFT/LEFT 29–43 |
| L9g | start **LEFT 189 (context)** → RIGHT 194 (added) | 200 | REST: `start_line:189,start_side:"LEFT"`. GraphQL: **startLine 190** (the RIGHT number of the same row), originalStartLine 189 |
| L14 | `src/settings.rs` (new path) LEFT 40 (deleted row of the renamed file) | **200** | LEFT 40, path `src/settings.rs` |
| L14b | `src/config.rs` (**old path**) LEFT 40 | 200 | stored with path `src/config.rs` |
| L14c | new path RIGHT 41 | 200 | RIGHT 41 |
| L14d | old path RIGHT 41 | 200 | stored with path `src/config.rs` |
| L14e | new path LEFT 38 (context) | 200 | LEFT 38 |
| L14f | new path R10 (outside the hunk) | 422 | `Line could not be resolved` |
| L14g | `docs/notes.md` (old path of the pure rename) LEFT 1 | 422 | `Line could not be resolved` |
| G1 | PR A `src/engine.rs` **R59**: outside the local `git diff` hunks (`-43,7 +48,11`, `-51,7 +60,7`) but inside GitHub's merged hunk `-43,15 +48,19` | **200** | RIGHT 59 |
| D1 | PR A `src/legacy.rs` (deleted file) LEFT 7 | 200 | LEFT 7 |
| D2 | deleted file, RIGHT 7 | 422 | `Line could not be resolved` |
| D3 | deleted file, LEFT 3 → LEFT 12 | 200 | LEFT/LEFT 3–12 |
| D4 | `gen/schema.rs` (linguist-generated) R30 | 200 | normal |
| D5 | `vendor/lib.js` (linguist-vendored) R17 | 200 | normal |

**Atomicity** (PR D head `cc9b236`, see L13):
- P1: COMMENT with one valid and one invalid comment → 422 `Line could not be resolved`, and **nothing
  is created** (the review count did not change).
- P2: a bad path plus a bad line → 422 `errors:["Path could not be resolved and Line could not be resolved"]`.
  Errors are merged into one string, and neither names the failing comment.

**A1 (shape check, submitted on PR D):** one COMMENT review with the L6, L9g, L9 and L8 comments plus R10.
- REST `GET /pulls/4/comments` returns clean anchors. For example L6 gives
  `line:9, side:"LEFT", start_line:null`, and L9g gives `start_line:189, start_side:"LEFT", line:194, side:"RIGHT"`.
- **REST `GET /pulls/4/reviews/{id}/comments` returns `line/side/start_line/start_side: null`** (only
  `position`), both for pending and for submitted reviews. Don't use that endpoint for anchors.

### MX: ranges from one side to the other (2026-10-24, PR B #14, pending + delete)

Run for bugs 2's follow-up on PR B #14 (head `073e29a…`, same diff as PR D; see §2 for its hunks:
`src/engine.rs` `-3,8 +3,9` = L3–L5/R3–R5 context, L6–L7 deleted, R6–R8 added, then context;
`-188,9 +189,10` = context, L191–L193 deleted, R192–R195 added, context). Each comment was posted
alone as a pending review (`start_line`, `start_side`, `line`, `side` all sent), read back through
GraphQL `reviewThreads`, then deleted. Nothing is left on the PR.

| Case | Comment (start → end) | Status | Stored (GraphQL) |
|---|---|---|---|
| MX-1 | LEFT 6 (deleted) → RIGHT 8 (added), one hunk | 200 | startDiffSide LEFT, startLine 6, diffSide RIGHT, line 8 |
| MX-2 | RIGHT 5 (context) → LEFT 7 (deleted below it) | **200** | startDiffSide RIGHT, startLine 5, diffSide LEFT, line 7 |
| MX-3 | RIGHT 6 (added, hunk 1) → LEFT 192 (deleted, hunk 2) | **200** | RIGHT 6 → LEFT 192 |
| MX-4 | RIGHT 8 (added) → LEFT 7 (deleted, **above** it in the diff) | 422 | `Start position could not be resolved` |
| MX-5 | LEFT 6 → LEFT 192 (across hunks, added rows between) | 200 | LEFT/LEFT 6–192 |
| MX-6 | LEFT 4 (context) → RIGHT 8 | 200 | LEFT 4 → RIGHT 8 |
| MX-7 | LEFT 7 (deleted, hunk 1) → RIGHT 196 (context, hunk 2) | 200 | LEFT 7 → RIGHT 196 |
| MX-8 | RIGHT 10 (context, hunk 1) → LEFT 191 (deleted, hunk 2) | 200 | RIGHT 10 → LEFT 191 |
| MX-9 | LEFT 5 (context) → LEFT 7 | 200 | LEFT/LEFT 5–7 |
| MX-10 | RIGHT 3 → RIGHT 9 (over the deleted and added rows) | 200 | RIGHT/RIGHT 3–9 |
| MX-11 | `README.md` RIGHT 9 (context) → LEFT 10 (deleted) | 200 | RIGHT 9 → LEFT 10 |
| MX-12 | LEFT 192 → RIGHT 192 (deleted row before the added one) | 200 | LEFT 192 → RIGHT 192 |
| MX-13 | RIGHT 192 → LEFT 193 (end above the start) | 422 | `Start position could not be resolved` |
| MX-14 | RIGHT 6 (added) → LEFT 7 (deleted, above it) | 422 | `Start position could not be resolved` |
| MX-15 | RIGHT 8 (added, hunk 1) → LEFT 191 (deleted, hunk 2) | 200 | RIGHT 8 → LEFT 191 |

So GitHub takes **every combination of sides**, LEFT → RIGHT and RIGHT → LEFT, in one hunk or
across hunks, as long as both ends are in its hunks and the start comes no later than the end in
unified-diff order (deleted rows before the added rows of the same change). It refuses only a
start after the end (MX-4/13/14, as L9b/L9c). Threads keep the sides they were posted with
(`startDiffSide` / `diffSide`), and §5.5's parsing reads them back (for a RIGHT start
`startLine` is the start; MX-2/3/8/11/15 had `originalStartLine` equal to it).

### L10: `subject_type` inside create-review `comments[]` (PR D)

| Case | Comment item | Status | errors[0] |
|---|---|---|---|
| L10a | `{path, subject_type:"file", body}` (pending) | 422 | `Variable $comments of type [DraftPullRequestReviewComment] was provided invalid value for 0.subjectType (Field is not defined on DraftPullRequestReviewComment), 0.position (Expected value to not be null)` |
| L10b | `{path, subject_type:"file", line:1, side:"RIGHT", body}` | 422 | `… [DraftPullRequestReviewThread] … 0.subjectType (Field is not defined on DraftPullRequestReviewThread)` |
| L10c | `{path, body}`, no line and no position | 422 | `… 0.position (Expected value to not be null)` |
| L10d | as L10a but with `event:"COMMENT"` | 422 | same as L10a |

So REST create-review cannot make file comments. The pending flow is required.

### L11: pending review + `addPullRequestReviewThread(subjectType: FILE)` + submit (PR D)

1. `POST …/pulls/4/reviews {"commit_id":H2,"comments":[{path:"src/cache.rs",line:30,side:"RIGHT",body:"…"}]}`
   (no `event`, no `body`) → 200 `{"id":5555705425,"node_id":"PRR_kwDOVM9_N88AAAABSyVWUQ","state":"PENDING","body":""}`.
2. GraphQL
   `mutation($reviewId: ID!, $path: String!, $body: String!) { addPullRequestReviewThread(input: {pullRequestReviewId: $reviewId, path: $path, body: $body, subjectType: FILE}) { thread { id subjectType path line diffSide isOutdated comments(first: 1) { nodes { id state } } } } }`:
   - `src/cache.rs` (added file) → 200 `thread {subjectType: FILE, line: 1, diffSide: RIGHT, comments[0].state: PENDING}`
   - `docs/design-notes.md` (**pure rename, no patch**) → 200, thread created
   - `src/render.rs` (**not in the PR**) → HTTP 200 `{"data":{"addPullRequestReviewThread":{"thread":null}}}`
     with **no `errors`**. Treat `thread == null` as a failure.
   - `src/config.rs` (old path of the renamed file) → 200, thread created. It was **outdated by the
     next push** (see L13), while the same comment on the new path stayed current.
3. `POST …/pulls/4/reviews/5555705425/events {"event":"COMMENT","body":""}` → **200** COMMENTED,
   `submitted_at` set. Every thread (1 line + 3 file) became SUBMITTED.

Variants:
- L11-6a: events call without `event` → 422 `{"message":"Invalid request.\n\n\"event\" wasn't supplied."}`
  (no `errors`).
- L11-7: events on an already submitted review → 422 `{"message":"Validation Failed","errors":["Could not comment pull request review."]}`.
- L11-8: `DELETE` of a submitted review → 422 `{"message":"Can not delete a non-pending pull request review"}`.
- L11f/L11c: a pending review created with **no comments** and `body:"L11f …"` → 200. It then got one
  FILE thread and was submitted with `{"event":"REQUEST_CHANGES","body":""}` → **200 CHANGES_REQUESTED,
  body `""`**. So the events `body` **replaces** the body set at creation, and a file comment counts
  as "a comment" for REQUEST_CHANGES.
- L11b: `addPullRequestReviewThread(input:{pullRequestId: "PR_kwDOVM9_N88AAAABI924hA", path, body, subjectType: FILE})`
  with no pending review → 200. **A pending review is created automatically** (commit = head). It was
  deleted afterwards.
- L11g: a GraphQL **line** thread outside the hunks (`line: 100, side: RIGHT`) → 200 `thread: null`,
  no errors (silent). L11h: line 8 → thread created.
- L11i: a bad `pullRequestReviewId` → 200 with
  `errors:[{"type":"NOT_FOUND","path":["addPullRequestReviewThread"],"message":"Could not resolve to a node with the global id of 'PRR_doesnotexist'"}]`.
- D6 (PR A, pending then deleted): a FILE thread on the **deleted** file `src/legacy.rs` → created.

### L12: a second review while one is pending (PR D, during L11)

| Case | Request | Status | Response |
|---|---|---|---|
| L12a | another pending create | 422 | `errors:["User can only have one pending review per pull request"]` |
| L12b | COMMENT + inline | 422 | same |
| L12c | APPROVE, 0 comments | 422 | same (APPROVE is blocked as well) |
| L12d | `POST /pulls/4/comments` (single comment) | 422 | `{"message":"Validation Failed","errors":[{"resource":"PullRequestReview","code":"custom","field":"user_id","message":"user_id can only have one pending review per pull request"}]}` (**errors are objects here**) |
| L12e | single comment with `subject_type:"file"` | 422 | same as L12d |
| L12f | GraphQL `pullRequest(number:4){ reviews(states:[PENDING], first:1){ nodes{ id databaseId viewerDidAuthor comments{totalCount} } } }` | 200 | `[{id:"PRR_kwDOVM9_N88AAAABSyVWUQ", databaseId:5555705425, viewerDidAuthor:true, comments:{totalCount:4}}]` |
| L12g | REST `GET /pulls/4/reviews` | 200 | lists the pending review (`state:"PENDING"`, no `submitted_at`) |

The viewer's pending threads **also appear in `pullRequest.reviewThreads`**, with comment
`state: PENDING`. The `pending` helper relied on this to read back each probe's anchor.

### L13: `commit_id` handling (PR D)

| Case | `commit_id` | Status | Result |
|---|---|---|---|
| L13a | `0123456789abcdef0123456789abcdef01234567` (unknown) | 422 | `The commitOID is not part of the pull request` |
| L13g | the same, submitted COMMENT with no comments | 422 | same |
| L13e | short SHA `073e29a` | 422 | `Variable $commitOID of type GitObjectID was provided invalid value` |
| L13b | the base commit `deea36c` (on main) | 422 | `Path could not be resolved` (diff of base vs base) |
| L13c | PR A's head `3f22f1b` (a different branch), engine.rs R8 | 422 | `Line could not be resolved` |
| L13c2 | PR A's head, engine.rs **R4** (an added line in *PR A's* diff) | **200** | pending review created on PR D with `commit_id:3f22f1b…` (**any known commit is accepted; lines are checked against that commit's diff**) |
| L13f | `commit_id` omitted | 200 | uses the current head |
| L13h | older PR commit H1, COMMENT with no inline comments | 200 | `commit_id: H1` |
| L13d | older PR commit H1, COMMENT + inline on engine.rs R8, cache.rs 41/42/43 (H1 numbers; these lines move to 42/43/46 in H2) | 200 | Threads stay anchored to **H1**: GraphQL `line:41/42/43`, `isOutdated:false`, `comments[0].commit.oid = 665f27b`, while `startLine` showed the head-mapped 42/43/46. REST `line:41…`, `commit_id:665f27b`. Polling for ~1 min did not change this. |
| push | `cc9b236` pushed to `bot/api-probe` (adds 2 lines at the top of cache.rs) | – | Within seconds **every current thread was remapped to the new head**: cache.rs 20→22, …, `commit.oid = cc9b236`, `originalLine` unchanged. The L13d threads moved from H1 numbering to the new head (41→44, 42→45, 43→48), still not outdated. The old-path FILE thread on `src/config.rs` became **outdated**. |
| P3 | after that push, pending with `commit_id: H2` (the now-previous head), cache.rs 20 | 200 | accepted (deleted) |
| force-push | `bot/api-probe` force-pushed back to H2 (cc9b236 orphaned) | – | all threads were remapped back to H2 numbering |
| L13i | the **force-pushed-away** head `cc9b236`, cache.rs R3 (exists only in that commit) | **200** | pending accepted, `commit_id: cc9b236` (deleted) |
| L13j | the same, submitted COMMENT with no inline comments | **200** | COMMENTED on an orphaned commit |

### Error shapes (other)

- `POST /pulls/999/reviews` → 404 `{"message":"Not Found","documentation_url":…,"status":"404"}`.
- A bad token on REST and on GraphQL → `HTTP/2.0 401`, `{"message":"Bad credentials",…,"status":"401"}`.
- GraphQL PR not found → 200 `{"data":{"repository":{"pullRequest":null}},"errors":[{"type":"NOT_FOUND","message":"Could not resolve to a PullRequest with the number of 999.",…}]}`.
- 422 bodies from create-review: `{"message":"Unprocessable Entity","errors":["<string>"],"documentation_url":"…","status":"422"}`.
  `errors` items are **strings** there but **objects** (`{resource, code, field, message}`, `message:"Validation Failed"`)
  from `POST /pulls/{n}/comments`. Some 422s (the events call) have no `errors`, only `message`.
- `PullRequestChangedFile` (GraphQL) has only `additions, changeType, deletions, path, viewerViewedState`.
  Neither API has a generated/vendored flag.

## 5. Policy decisions for the implementer

These replace the provisional rules in `M7-github.md` §0, §2 (parsing), §4(c/d) and §5.

### 5.1 `body_required(event, comment_count)` (comment_count = line + file comments in this submission)

- **COMMENT:** require a non-empty `body.trim()` **iff `comment_count == 0`** (L1/L1b vs L2/L2b/L2c).
- **REQUEST_CHANGES:** require a non-empty `body.trim()` **iff `comment_count == 0`** (L3/L3e/L11c pass
  with an empty body; L3c/L3d/L3f fail without comments). GitHub sometimes accepts the 0-comment case
  (L3b, L3b5), depending on history, so require the body anyway.
- **APPROVE:** never (L4/L4b/L4c).
- In short, `body_required = comment_count == 0 && event != Approve`. Treat a whitespace-only body as
  empty. Always send `body` as a string (`""` is fine) in the one-shot create. In the pending flow,
  send the body only on the `/events` call, which overwrites the pending body (L11c).

### 5.2 Commentable lines (`CommentableLines::allows`)

- A position `(side, line)` is commentable **iff it falls inside one of GitHub's patch hunks for that
  file**: RIGHT → `[+start, +start+count)`, LEFT → `[-start, -start+count)`. Context rows inside a
  hunk can be commented on either side (L5a/b/d, L6). Even one row outside a hunk gives 422 (L5c/L5e/L7).
  This keeps the plan's v1 rule; the API does **not** accept arbitrary lines of a changed file.
- Use **GitHub's `patch`** from `GET pulls/N/files`, **not** the local `git diff`. GitHub merges nearby
  hunks: in PR A, engine.rs `-43,15 +48,19` is two hunks locally, and R59 between them is accepted (G1).
- Files without a `patch` (pure rename L7g; presumably binary or too large, not tested) allow **file
  comments only**. Added files are RIGHT only. Deleted files are LEFT only (D1/D2/D3). Generated and
  vendored files behave like any other file (D4/D5).
- **Ranges:** both ends must be commentable on their own sides, and the start must come **no later
  than the end in unified-diff order** (within a hunk, deleted rows come before added rows). LEFT→RIGHT
  works (L9, L9g). An end above the start fails with `Start position could not be resolved` (L9b/L9c).
  A range **may span hunks** (L8 accepted), so the plan's "must stay inside one hunk" rule is stricter
  than GitHub. Recommendation: allow any range whose two ends are commentable (simpler, matches the
  API). Keeping the same-hunk rule is a valid UX choice, but it is not required. The ends may be
  on different sides either way round, RIGHT → LEFT too (MX).
- Mapping (unchanged from the plan): added row → RIGHT new line, deleted row → LEFT old line, context
  row → RIGHT new line. Always send `side` and `start_side` explicitly; the defaults are RIGHT and
  `side` (L7h/L9e). Omit `start_line`/`start_side` when start == end; otherwise GitHub stores
  `startDiffSide` even for a one-line "range" (L9d).
- **Renamed files:** always use the **new path** (`filename`) for both sides. The old path is accepted
  at creation (L14b/L14d) but went outdated on the next push (L11-5, after the push). When reading
  threads, also match `path == previous_filename` back to the renamed file.

### 5.3 File-comment flow (confirmed; plan §4(d) stands, with these changes)

1. `POST …/reviews {commit_id, comments:[line comments…]}` with **no `event` and no `body`** → PENDING.
   Zero line comments is fine (L11f).
2. For each file comment: `addPullRequestReviewThread(input:{pullRequestReviewId: <node_id>, path: <new path>, body, subjectType: FILE})`.
   **Check `data.addPullRequestReviewThread.thread != null`**: GitHub returns `thread: null` with no
   `errors` for a path that is not in the PR (L11-4). GraphQL `errors[]` (e.g. NOT_FOUND) also fail.
3. `POST …/reviews/{id}/events {"event": "COMMENT"|"APPROVE"|"REQUEST_CHANGES", "body": "…"}`.
4. On failure in step 2 or 3: `DELETE …/reviews/{id}` (→ 200 with the review JSON), log a failed delete,
   and report the original error.

Don't create line threads with GraphQL. `addPullRequestReviewThread` with a bad line also returns
`thread: null` silently (L11g), while REST create-review is atomic (P1) and gives clear errors.

### 5.4 Pending reviews and preflight

- `PullRequestHead`'s `reviews(states:[PENDING], first:1){nodes{id viewerDidAuthor}}` finds the
  viewer's pending review (L12f). Only the viewer's own pending review is ever visible. While it
  exists, **every** create fails, APPROVE included, and so do single comments (L12a–e). Make this a
  **blocking** preflight error, not a warning.
- **Threads from the viewer's pending review show up in `reviewThreads`.** Add `state` to the comments
  selection and drop (or label "pending on GitHub") threads whose first comment has `state: PENDING`.
- **Head moved:** GitHub accepts any `commit_id` it knows, including older PR commits, commits from
  other branches, and force-pushed-away heads (L13c2/L13d/L13i/L13j). Only unknown SHAs are refused
  (L13a). The "PR head moved since load" preflight is therefore the only guard against a stale
  submit, and a warning there is correct. Comments posted against the loaded head are anchored to it
  until the next push to the PR, when GitHub remaps them (L13d + push).

### 5.5 Thread parsing (replaces the plan's §0 "Thread anchors" rule and §2 parsing rules)

- Check `subjectType == "FILE"` **first**. FILE threads have `line: 1, diffSide: RIGHT, originalLine: 1,
  startLine: null` (PR A thread 6, L11). REST shows them as `subject_type:"file", line:1, side:"RIGHT"`.
- `line == null` → `Outdated { original_line: originalLine }` (PR A thread 4).
- **A thread is single-line iff `startDiffSide == null`; ignore `startLine` in that case.** For
  single-line threads GraphQL's `startLine` is a head-mapped RIGHT-side number: L6 had `line:9 (LEFT)`
  with `startLine:10`, and L13d had `line:41` with `startLine:42`. The plan's
  `startLine.filter(|s| Some(*s) != line)` would produce bogus ranges, so **don't use it**.
- Multi-line iff `startDiffSide != null`. The start side is `startDiffSide`. The start line is
  `startLine` when that side is RIGHT, and **`originalStartLine` when it is LEFT**, because for a LEFT
  context row `startLine` is the RIGHT number (L9g: startLine 190 vs REST and originalStartLine 189).
  If `(startDiffSide, start) == (diffSide, line)`, treat it as single-line (L9d).
- Line numbers are relative to `comments.nodes[0].commit.oid`, which is normally `headRefOid`. Add
  `commit { oid }` to the comments selection. Place a thread inline only when that oid equals the
  reviewed (local) head. Otherwise put it in the per-file "outdated/moved" block with `originalLine`.
  This covers a local checkout that is behind the PR, and fresh comments made against an older commit.
- Bots: in GraphQL, `author.__typename == "Bot"` with login `github-actions` (no suffix). REST gives
  `github-actions[bot]` with `type:"Bot"`. Keep both checks.
- Read anchors from GraphQL threads (or REST `GET /pulls/N/comments`), never from
  `GET /pulls/N/reviews/{id}/comments` (it returns nulls).

### 5.6 Error hints (§5 strings, now confirmed)

Collect `errors[]` (items may be strings, or objects with `message`), else use `message`, and match
substrings:

| GitHub text (observed) | Hint |
|---|---|
| `one pending review per pull request` (create: `User can only have…`; comments: `user_id can only have…`) | "You already have a pending review on this PR. Submit or discard it on GitHub, then retry." + PR link |
| `Line could not be resolved` / `Path could not be resolved` / `Start position could not be resolved` (possibly several joined by ` and `; the failing comment is not named) | "GitHub rejected a comment's position (outside the PR diff). Reload the review and check comments on lines outside GitHub's diff hunks." |
| `The commitOID is not part of the pull request` (only for SHAs GitHub doesn't know) | "GitHub doesn't know the reviewed commit {sha}. Reload the review (drafts are kept)." |
| `Can not approve your own pull request` / `Can not request changes on your own pull request` (prefixed with `Review `) | pass through without the `Review ` prefix |
| `You need to leave a comment indicating the requested changes.` | "Request changes needs a review comment or at least one inline comment." (shouldn't happen given §5.1) |
| COMMENT with no body and no comments → `errors:[""]` (empty!) | "GitHub needs a review comment when there are no inline comments." (shouldn't happen given §5.1) |
| `Variable $… was provided invalid value` | internal bug: log it and show the raw message |
| events: `"event" wasn't supplied.` / `Could not comment pull request review.`; delete: `Can not delete a non-pending pull request review` | internal / race: show the raw message |
| GraphQL `addPullRequestReviewThread.thread == null` with no errors | "GitHub rejected the file comment on {path} (file not in the PR?)." |

## 6. Reset recipe for later validation runs

`$STATE/sandbox/reset.sh`
only touches the sandbox repo:

```sh
R=$STATE/sandbox/reset.sh
$R status          # open PRs: author, head, #reviews, #threads, pending review ids
$R clear-pending   # DELETE the viewer's PENDING reviews on every open PR (run after every test)
$R new-pr-c        # close PR C and open a fresh, review-free PR from fix/truncate-ellipsis (new number)
$R new-bot-pr [branch]   # dispatch bot-pr.yml from fixture/bot-cleanup -> a fresh github-actions[bot] PR (PR B content)
$R replay-pr-a [branch]  # recreate PR A's prepared state (all 7 threads, reviews, reply, resolve, outdating push, bot review) on a new PR
```

Exercised in this run: `status`, `clear-pending` and `replay-pr-a`. `new-bot-pr` runs the same
`gh workflow run bot-pr.yml …` command that created PRs B and D, but the script's wait loop has not
been run. `new-pr-c` (`gh pr close` + `gh pr create` from the same branch) has not been run.

Rules of thumb:
- Submitted reviews can't be deleted. After a UI test submits to PR C or PR B, run `new-pr-c` or
  `new-bot-pr` and use the new PR number. Approvals and change requests can also be neutralised with
  `PUT /repos/drew-council/code-review-sandbox/pulls/{n}/reviews/{id}/dismissals {"message":"reset","event":"DISMISS"}`
  (worked on PR D; the branch isn't protected). Their threads stay.
- A pending review left by a crashed submit blocks every later submit (§5.4). Run `clear-pending`.
- Never submit to PR A. If it gets polluted, `replay-pr-a` makes an identical copy (verified with #5,
  which then matched PR A's thread table exactly and was closed). The replay starts the branch at
  `5c5cdf4…`, posts `scratchpad/sandbox/out/pr1-review1.json`, adds the reply, the file comment and the
  issue comment, resolves the `strings.rs:48` thread, fast-forwards the branch to `3f22f1b…` (which
  outdates `engine.rs:223`), then dispatches `bot-review.yml` with `scratchpad/sandbox/out/bot-review-pr1.json`.
- Bot review by hand: `gh workflow run bot-review.yml --repo drew-council/code-review-sandbox -f pr=<n> -f payload='{"event":"COMMENT","body":"…","comments":[…]}'`.
- PR D's head was force-pushed back to `073e29a…`. The orphaned commit `cc9b236…` is still on GitHub.
  PR D can take any further API experiment, and `probe.sh`'s `pending` helper creates, inspects and
  deletes a pending review in one call.
- Local clone: `scratchpad/sandbox/repo` (remote `git@github.com:drew-council/code-review-sandbox.git`,
  SSH; pushes to `.github/workflows/*` need SSH because the `gh` token has no `workflow` scope).

State at the end of this run: no pending reviews on any PR. PR B and PR C have 0 reviews and 0 threads.
PR A matches §3. PR D holds the probe residue.
