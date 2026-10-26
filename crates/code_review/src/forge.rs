//! Pull requests on a forge, independent of the forge: the types reviews work with, and the
//! [`Forge`] trait [`crate::github`] implements.

use crate::github;
pub use crate::github::{AuthFailure, GitHubError};
use anyhow::Result;
use collections::HashMap;
use futures::future::BoxFuture;
use git::Oid;
use gpui::{App, Context, Entity, SharedString, Task};
use project::{Project, git_store::Repository};
use std::{fmt, ops::Range, sync::Arc};
use time::OffsetDateTime;
use workspace::{Toast, Workspace, notifications::NotificationId};

/// A repository on a forge, e.g. `zed-industries/zed` on `github.com`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ForgeRepo {
    pub host: SharedString,
    pub owner: SharedString,
    pub name: SharedString,
}

impl ForgeRepo {
    /// Whether both name the same repository. GitHub ignores the case of owners and names.
    pub fn is_same_repo(&self, other: &ForgeRepo) -> bool {
        self.host.eq_ignore_ascii_case(&other.host)
            && self.owner.eq_ignore_ascii_case(&other.owner)
            && self.name.eq_ignore_ascii_case(&other.name)
    }
}

impl fmt::Display for ForgeRepo {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.owner, self.name)
    }
}

/// A repository the user named, as in `owner/repo#123` (no host) or a pull request URL.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ForgeRepoRef {
    pub host: Option<SharedString>,
    pub owner: SharedString,
    pub name: SharedString,
}

impl ForgeRepoRef {
    pub fn matches(&self, repo: &ForgeRepo) -> bool {
        self.host
            .as_ref()
            .is_none_or(|host| host.eq_ignore_ascii_case(&repo.host))
            && self.owner.eq_ignore_ascii_case(&repo.owner)
            && self.name.eq_ignore_ascii_case(&repo.name)
    }
}

impl fmt::Display for ForgeRepoRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.owner, self.name)
    }
}

/// The author of a pull request, review or comment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Actor {
    /// "ghost" for a deleted account.
    pub login: SharedString,
    pub is_bot: bool,
}

/// Where a checkout's pull requests come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryContext {
    /// The repository of the checkout's remote, as the forge names it.
    pub origin: ForgeRepo,
    /// The repository whose pull requests are listed and opened: the remote's parent when the
    /// remote is a fork (a fork's checkout reviews the upstream's pull requests), or the
    /// repository a reference named.
    pub canonical: ForgeRepo,
    /// The user the token belongs to.
    pub viewer: SharedString,
    pub canonical_https_url: String,
    pub canonical_ssh_url: String,
    /// Whether the checkout's remote is an SSH URL, so fetches from `canonical` use SSH too.
    pub origin_is_ssh: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PullRequestSummary {
    /// The forge's id of the pull request; empty when the forge didn't say.
    pub node_id: SharedString,
    pub number: u64,
    pub title: SharedString,
    pub url: SharedString,
    pub author: Actor,
    pub head_ref: SharedString,
    pub base_ref: SharedString,
    pub status: PullRequestStatus,
    pub is_draft: bool,
    pub review_decision: Option<ReviewDecision>,
    /// Whether the head branch is in another repository (a fork).
    pub is_cross_repository: bool,
    pub updated_at: OffsetDateTime,
    /// The viewer's latest review that approved or requested changes, if any.
    pub viewer_review: Option<ReviewDecision>,
}

impl PullRequestSummary {
    /// Open and not a draft: ready for review.
    pub fn is_ready_for_review(&self) -> bool {
        self.status == PullRequestStatus::Open && !self.is_draft
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PullRequestStatus {
    Open,
    Closed,
    Merged,
}

/// A stack of pull requests (GitHub's stacked pull requests, `gh stack`): each one is based on the
/// head branch of the one below it, and the bottom one on `base_ref`. Merged and closed pull
/// requests stay in their stack.
#[derive(Clone, Debug, PartialEq)]
pub struct PullRequestStack {
    pub number: u64,
    /// The branch the bottom pull request merges into.
    pub base_ref: SharedString,
    /// From the bottom (merged first) to the top.
    pub pull_requests: Vec<PullRequestSummary>,
}

/// Up a stack is away from its base, towards the pull requests that merge last.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StackDirection {
    Up,
    Down,
}

impl PullRequestStack {
    /// The index of pull request `number`, from the bottom.
    pub fn position(&self, number: u64) -> Option<usize> {
        self.pull_requests
            .iter()
            .position(|pull_request| pull_request.number == number)
    }

    pub fn contains(&self, number: u64) -> bool {
        self.position(number).is_some()
    }

    /// The index of the pull request a review of the stack starts at: the lowest one ready for
    /// review that the viewer hasn't approved, else the lowest one ready for review, else the
    /// lowest open one (a draft), else the bottom one.
    pub fn review_start(&self) -> usize {
        let lowest = |matches: &dyn Fn(&PullRequestSummary) -> bool| {
            self.pull_requests
                .iter()
                .position(|pull_request| matches(pull_request))
        };
        lowest(&|pull_request| {
            pull_request.is_ready_for_review()
                && pull_request.viewer_review != Some(ReviewDecision::Approved)
        })
        .or_else(|| lowest(&PullRequestSummary::is_ready_for_review))
        .or_else(|| lowest(&|pull_request| pull_request.status == PullRequestStatus::Open))
        .unwrap_or(0)
    }

    /// The nearest open pull request (drafts too) above or below pull request `number`: merged
    /// and closed ones are skipped.
    pub fn neighbor(&self, number: u64, direction: StackDirection) -> Option<&PullRequestSummary> {
        let position = self.position(number)?;
        let is_open =
            |pull_request: &&PullRequestSummary| pull_request.status == PullRequestStatus::Open;
        match direction {
            StackDirection::Up => self.pull_requests[position + 1..].iter().find(is_open),
            StackDirection::Down => self.pull_requests[..position].iter().rev().find(is_open),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

/// The viewer's review that is started but not submitted. While it exists, GitHub refuses every
/// new review of the viewer on the pull request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingReview {
    pub node_id: SharedString,
    /// `None` when GitHub doesn't say: the review still blocks new ones.
    pub database_id: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PullRequestDetails {
    pub node_id: SharedString,
    pub number: u64,
    pub title: SharedString,
    pub url: SharedString,
    /// Markdown.
    pub body: SharedString,
    pub author: Actor,
    pub state: PullRequestStatus,
    pub is_draft: bool,
    pub base_ref: SharedString,
    pub base_oid: Oid,
    pub head_ref: SharedString,
    pub head_oid: Oid,
    /// `None` when the head repository was deleted.
    pub head_repository: Option<ForgeRepo>,
    pub is_cross_repository: bool,
    pub review_decision: Option<ReviewDecision>,
    pub viewer_is_author: bool,
    pub pending_review: Option<PendingReview>,
}

/// What can change on a pull request while it is reviewed, re-read before a submit.
#[derive(Clone, Debug, PartialEq)]
pub struct HeadStatus {
    pub head_oid: Oid,
    pub state: PullRequestStatus,
    pub pending_review: Option<PendingReview>,
}

/// LEFT is the base's side of the diff (deleted rows, and context rows by their old numbers),
/// RIGHT the head's (added rows and context rows).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DiffSide {
    Left,
    Right,
}

/// Where a review thread sits. Line numbers are those of the thread's commit
/// ([`RemoteThread::anchor_commit`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThreadAnchor {
    Line {
        side: DiffSide,
        line: u32,
    },
    /// From `start` on `start_side` to `line` on `side` (the last row, where the thread shows).
    Range {
        start_side: DiffSide,
        start: u32,
        side: DiffSide,
        line: u32,
    },
    File,
    /// The thread's lines changed since it was made; only its original lines are known.
    Outdated {
        original_line: Option<u32>,
        original_start: Option<u32>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct RemoteComment {
    pub id: SharedString,
    pub database_id: Option<u64>,
    pub author: Actor,
    /// Markdown.
    pub body: SharedString,
    pub created_at: OffsetDateTime,
    pub url: SharedString,
}

/// A review thread on the pull request, read-only. Threads of the viewer's pending review are
/// left out.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteThread {
    pub id: SharedString,
    /// The path on the side the thread is on; for a renamed file this may be the old path.
    pub path: String,
    pub anchor: ThreadAnchor,
    /// The commit the line numbers refer to (normally the pull request's head).
    pub anchor_commit: Option<Oid>,
    /// The commit the thread was made on, which an outdated thread's original lines refer to.
    pub original_commit: Option<Oid>,
    pub is_resolved: bool,
    pub is_outdated: bool,
    /// The first comments (up to 100), oldest first.
    pub comments: Vec<RemoteComment>,
    /// All comments of the thread, also those not in `comments`.
    pub total_comments: u32,
}

impl RemoteThread {
    /// The author of the thread's first comment.
    pub fn author(&self) -> Option<&Actor> {
        self.comments.first().map(|comment| &comment.author)
    }

    pub fn is_by_bot(&self) -> bool {
        self.author().is_some_and(|author| author.is_bot)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewState {
    Commented,
    Approved,
    ChangesRequested,
    Dismissed,
}

/// A submitted review with a body. Reviews without one (GitHub makes one per reply or single
/// comment) and pending reviews are left out.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteReviewSummary {
    pub id: SharedString,
    pub author: Actor,
    pub state: ReviewState,
    /// Markdown.
    pub body: SharedString,
    pub submitted_at: Option<OffsetDateTime>,
    pub url: SharedString,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangedFileStatus {
    Added,
    Removed,
    Modified,
    Renamed,
    Copied,
    Changed,
    Unchanged,
}

/// One hunk of a file's diff as the forge has it. GitHub merges hunks that are close together, so
/// these can be larger than the local `git diff`'s. Lines are 1-based, ranges half-open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub old: Range<u32>,
    pub new: Range<u32>,
    /// The hunk's rows in diff order, in runs of one kind.
    pub rows: Vec<HunkRows>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HunkRowKind {
    Context,
    Deleted,
    Added,
}

/// Consecutive rows of one kind. `old_start`/`new_start` are the old and new line numbers at the
/// first row; for deleted rows only the old one counts, for added rows only the new one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HunkRows {
    pub kind: HunkRowKind,
    pub old_start: u32,
    pub new_start: u32,
    pub len: u32,
}

/// How many unchanged lines `git diff` (and GitHub) shows around each change.
pub const DIFF_CONTEXT_LINES: u32 = 3;

impl Hunk {
    /// The hunks `git diff` shows for `changes` (each change's old and new lines, in order) in
    /// files of `old_len` and `new_len` lines: each change with `context` unchanged lines around
    /// it, merged when their context lines would touch.
    pub fn with_context(
        changes: &[(Range<u32>, Range<u32>)],
        context: u32,
        old_len: u32,
        new_len: u32,
    ) -> Vec<Hunk> {
        changes
            .chunk_by(|previous, next| {
                next.1.start.saturating_sub(previous.1.end) <= context.saturating_mul(2)
            })
            .filter_map(|changes| {
                let (first, last) = (changes.first()?, changes.last()?);
                let before = context
                    .min(first.0.start.saturating_sub(1))
                    .min(first.1.start.saturating_sub(1));
                let after = context
                    .min((old_len + 1).saturating_sub(last.0.end))
                    .min((new_len + 1).saturating_sub(last.1.end));
                let old = first.0.start - before..last.0.end + after;
                let new = first.1.start - before..last.1.end + after;
                let mut rows = Vec::new();
                let mut push = |kind, old_start, new_start, len| {
                    if len > 0 {
                        rows.push(HunkRows {
                            kind,
                            old_start,
                            new_start,
                            len,
                        });
                    }
                };
                push(HunkRowKind::Context, old.start, new.start, before);
                for (index, (old_lines, new_lines)) in changes.iter().enumerate() {
                    push(
                        HunkRowKind::Deleted,
                        old_lines.start,
                        new_lines.start,
                        old_lines.end.saturating_sub(old_lines.start),
                    );
                    push(
                        HunkRowKind::Added,
                        old_lines.end,
                        new_lines.start,
                        new_lines.end.saturating_sub(new_lines.start),
                    );
                    let unchanged = match changes.get(index + 1) {
                        Some((next_old, _)) => next_old.start.saturating_sub(old_lines.end),
                        None => after,
                    };
                    push(
                        HunkRowKind::Context,
                        old_lines.end,
                        new_lines.end,
                        unchanged,
                    );
                }
                Some(Hunk { old, new, rows })
            })
            .collect()
    }

    fn contains(&self, side: DiffSide, line: u32) -> bool {
        match side {
            DiffSide::Left => self.old.contains(&line),
            DiffSide::Right => self.new.contains(&line),
        }
    }

    /// The row's index in the hunk, in diff order.
    fn row_index(&self, side: DiffSide, line: u32) -> Option<u32> {
        let mut index: u32 = 0;
        for rows in &self.rows {
            let start = match (side, rows.kind) {
                (DiffSide::Left, HunkRowKind::Context | HunkRowKind::Deleted) => {
                    Some(rows.old_start)
                }
                (DiffSide::Right, HunkRowKind::Context | HunkRowKind::Added) => {
                    Some(rows.new_start)
                }
                _ => None,
            };
            if let Some(start) = start
                && line >= start
                && line - start < rows.len
            {
                return Some(index.saturating_add(line - start));
            }
            index = index.saturating_add(rows.len);
        }
        None
    }
}

/// A file of the pull request's diff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: String,
    /// The old path of a renamed or copied file.
    pub previous_path: Option<String>,
    pub status: ChangedFileStatus,
    /// `None` when the forge shows no diff for the file (a pure rename, a binary or a very large
    /// file): it can only get file comments.
    pub hunks: Option<Vec<Hunk>>,
}

impl ChangedFile {
    fn diff_position(&self, side: DiffSide, line: u32) -> Option<(usize, u32)> {
        diff_position(self.hunks.as_ref()?, side, line)
    }
}

/// Where the row is in the diff: its hunk, then its index in the hunk. `None` outside the hunks.
fn diff_position(hunks: &[Hunk], side: DiffSide, line: u32) -> Option<(usize, u32)> {
    hunks.iter().enumerate().find_map(|(hunk_index, hunk)| {
        if !hunk.contains(side, line) {
            return None;
        }
        // Without the hunk's rows (a patch cut short), deleted rows come before added ones,
        // which is right for a hunk with one change.
        let row = hunk.row_index(side, line).unwrap_or(match side {
            DiffSide::Left => line - hunk.old.start,
            DiffSide::Right => {
                (line - hunk.new.start).saturating_add(hunk.old.end - hunk.old.start)
            }
        });
        Some((hunk_index, row))
    })
}

/// Why lines of a diff can't get a comment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinesRefusal {
    /// The line (or both ends of the range) is outside the diff's hunks.
    Outside,
    StartOutside,
    EndOutside,
    /// The range's ends are in different hunks, so it spans unchanged lines the diff hides.
    AcrossHunks,
    /// The range's start comes after its end in the diff.
    Reversed,
}

/// Whether lines from `start` to `end` can get a comment in a diff of `hunks`: both ends inside
/// the same hunk, the start not after the end. GitHub's API takes a range across hunks too (LV
/// §5.2), but such a range covers unchanged lines the diff doesn't show, so reviews refuse it.
pub fn check_lines(
    hunks: &[Hunk],
    start: (DiffSide, u32),
    end: (DiffSide, u32),
) -> Result<(), LinesRefusal> {
    match (
        diff_position(hunks, start.0, start.1),
        diff_position(hunks, end.0, end.1),
    ) {
        (None, None) => Err(LinesRefusal::Outside),
        (None, Some(_)) => Err(LinesRefusal::StartOutside),
        (Some(_), None) => Err(LinesRefusal::EndOutside),
        (Some(start), Some(end)) if start > end => Err(LinesRefusal::Reversed),
        (Some((start_hunk, _)), Some((end_hunk, _))) if start_hunk != end_hunk => {
            Err(LinesRefusal::AcrossHunks)
        }
        _ => Ok(()),
    }
}

/// The lines of a pull request that can get comments: GitHub only accepts comments on rows
/// inside its own diff hunks (context rows on either side), and file comments on files of the
/// pull request.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CommentableLines {
    files: HashMap<String, ChangedFile>,
    by_previous_path: HashMap<String, String>,
    truncated: bool,
}

impl CommentableLines {
    /// `truncated`: the forge listed only some of the files.
    pub fn new(files: impl IntoIterator<Item = ChangedFile>, truncated: bool) -> Self {
        let mut lines = Self {
            truncated,
            ..Self::default()
        };
        for file in files {
            if let Some(previous_path) = &file.previous_path {
                lines
                    .by_previous_path
                    .insert(previous_path.clone(), file.path.clone());
            }
            lines.files.insert(file.path.clone(), file);
        }
        lines
    }

    /// Whether the forge listed only some of the files. Files it didn't list can't get line
    /// comments.
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    pub fn files(&self) -> impl Iterator<Item = &ChangedFile> {
        self.files.values()
    }

    /// The file at `path`, also when `path` is a renamed file's old path.
    pub fn file(&self, path: &str) -> Option<&ChangedFile> {
        self.files.get(path).or_else(|| {
            self.by_previous_path
                .get(path)
                .and_then(|current| self.files.get(current))
        })
    }

    /// Whether `path` can get a file comment. When the file list is truncated, files not in it
    /// might still be part of the pull request.
    pub fn contains_file(&self, path: &str) -> bool {
        self.truncated || self.file(path).is_some()
    }

    pub fn allows(&self, path: &str, side: DiffSide, line: u32) -> bool {
        self.file(path)
            .and_then(|file| file.diff_position(side, line))
            .is_some()
    }

    /// Whether a comment can span from `start` to `end`: both ends must be commentable and the
    /// start mustn't come after the end in the diff. A range may span hunks.
    pub fn allows_range(&self, path: &str, start: (DiffSide, u32), end: (DiffSide, u32)) -> bool {
        let Some(file) = self.file(path) else {
            return false;
        };
        match (
            file.diff_position(start.0, start.1),
            file.diff_position(end.0, end.1),
        ) {
            (Some(start), Some(end)) => start <= end,
            _ => false,
        }
    }

    /// Whether a new comment can span from `start` to `end` ([`check_lines`]). Stricter than
    /// [`Self::allows_range`], which places existing threads.
    pub fn check_range(
        &self,
        path: &str,
        start: (DiffSide, u32),
        end: (DiffSide, u32),
    ) -> Result<(), LinesRefusal> {
        let hunks = self
            .file(path)
            .and_then(|file| file.hunks.as_deref())
            .unwrap_or_default();
        check_lines(hunks, start, end)
    }

    /// The path a comment on `path` is submitted with: a renamed file's new path. Comments on the
    /// old path are accepted but go outdated with the next push.
    pub fn current_path<'a>(&'a self, path: &'a str) -> &'a str {
        if self.files.contains_key(path) {
            return path;
        }
        self.by_previous_path
            .get(path)
            .map(String::as_str)
            .unwrap_or(path)
    }
}

/// What a submitted review says about the pull request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReviewEvent {
    Comment,
    Approve,
    RequestChanges,
}

impl ReviewEvent {
    pub const ALL: [Self; 3] = [Self::Comment, Self::Approve, Self::RequestChanges];

    pub fn label(self) -> &'static str {
        match self {
            Self::Comment => "Comment",
            Self::Approve => "Approve",
            Self::RequestChanges => "Request Changes",
        }
    }
}

/// A comment on lines of the pull request's diff: on `line` of `side`, starting at `start` when
/// it spans several lines. New lines are numbered in the reviewed commit, old ones in the base.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewLineComment {
    /// The file's path in the reviewed commit (a renamed file's new path).
    pub path: String,
    pub side: DiffSide,
    pub line: u32,
    pub start: Option<(DiffSide, u32)>,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewFileComment {
    pub path: String,
    pub body: String,
}

/// A review to post, all at once: GitHub creates it with its comments, or not at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewSubmission {
    /// The commit the comments' new lines are numbered in.
    pub commit_id: Oid,
    pub event: ReviewEvent,
    pub body: String,
    pub line_comments: Vec<ReviewLineComment>,
    pub file_comments: Vec<ReviewFileComment>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmittedReview {
    pub id: u64,
    pub url: SharedString,
}

/// A submit that failed without saying whether GitHub made the review (a timeout, a dropped
/// connection, a 5xx, a response that couldn't be read), to find that out later instead of
/// posting the review twice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmitAttempt {
    pub submission: ReviewSubmission,
    /// When it started, in Unix seconds of this machine's clock.
    pub started: i64,
    /// Its pending review (a review with file comments is made pending first), once GitHub made
    /// it.
    pub pending_review: Option<u64>,
}

/// A forge's pull request API for one repository. Futures are `'static` and `Send`, so callers
/// can run them in the background.
pub trait Forge: Send + Sync + 'static {
    fn context(&self) -> &RepositoryContext;

    /// At most 50 pull requests of [`RepositoryContext::canonical`] that a search in the forge's
    /// syntax finds, most recently updated first unless the search sorts them.
    fn search_pull_requests(
        &self,
        search: String,
    ) -> BoxFuture<'static, Result<Vec<PullRequestSummary>>>;

    /// The stacks the pull requests with these [`PullRequestSummary::node_id`]s are in, each
    /// once, in the order of the first pull request in each. Fails where the forge has no
    /// stacks.
    fn pull_request_stacks(
        &self,
        node_ids: Vec<SharedString>,
    ) -> BoxFuture<'static, Result<Vec<PullRequestStack>>>;

    /// The stack pull request `number` is in, if any. Fails where the forge has no stacks.
    fn pull_request_stack(
        &self,
        number: u64,
    ) -> BoxFuture<'static, Result<Option<PullRequestStack>>>;

    fn pull_request(&self, number: u64) -> BoxFuture<'static, Result<PullRequestDetails>>;

    fn pull_request_head(&self, number: u64) -> BoxFuture<'static, Result<HeadStatus>>;

    fn review_threads(&self, number: u64) -> BoxFuture<'static, Result<Vec<RemoteThread>>>;

    fn review_summaries(&self, number: u64)
    -> BoxFuture<'static, Result<Vec<RemoteReviewSummary>>>;

    fn changed_files(&self, number: u64) -> BoxFuture<'static, Result<CommentableLines>>;

    /// Posts a review of the pull request whose URL is `url`. A failure made nothing, except
    /// when it is a [`GitHubError::PendingReviewLeft`] (a pending review couldn't be deleted) or
    /// a [`GitHubError::ReviewUncertain`] (the review may have been posted).
    fn submit_review(
        &self,
        number: u64,
        url: SharedString,
        submission: ReviewSubmission,
    ) -> BoxFuture<'static, Result<SubmittedReview>>;

    /// The review a submit that failed with [`GitHubError::ReviewUncertain`] posted, if GitHub
    /// has it. The attempt's pending review, when it is still pending, is deleted: then nothing
    /// was posted.
    fn find_submitted_review(
        &self,
        number: u64,
        attempt: SubmitAttempt,
    ) -> BoxFuture<'static, Result<Option<SubmittedReview>>>;

    /// Deletes the viewer's pending review `id`, which a failed submit left behind.
    fn discard_pending_review(&self, number: u64, id: u64) -> BoxFuture<'static, Result<()>>;
}

/// The forge of `repository`'s GitHub remote (the current branch's upstream remote, else
/// `origin`, else `upstream`, else any). `reference` picks the repository of a pull request the
/// user named: it must be the one the checkout's pull requests come from, or a remote's.
///
/// Resolved forges are reused while their token works. Errors are [`GitHubError`]s
/// ([`error_message`] has the text to show); [`GitHubError::Unsupported`] means the repository
/// has no GitHub remote.
pub fn resolve_forge(
    repository: &Entity<Repository>,
    reference: Option<ForgeRepoRef>,
    project: &Entity<Project>,
    cx: &mut App,
) -> Task<Result<Arc<dyn Forge>>> {
    github::resolve(repository, reference, project, cx)
}

/// The text to show for an error of a [`Forge`]: what to do about it, when that is known.
pub fn error_message(error: &anyhow::Error) -> SharedString {
    match github_error(error) {
        Some(error) => error.hint(),
        None => format!("{error:#}").into(),
    }
}

/// The [`GitHubError`] behind `error`, if it has one.
pub fn github_error(error: &anyhow::Error) -> Option<&GitHubError> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<GitHubError>())
}

struct ForgeErrorToast;

/// Shows an error of a [`Forge`] or of opening a pull request in a toast. A single sign-on
/// error links to the page that authorizes the token; Restricted Mode offers to trust the project.
pub fn show_error_toast(
    workspace: &mut Workspace,
    error: &anyhow::Error,
    cx: &mut Context<Workspace>,
) {
    log::error!("pull request: {error:#}");
    let mut toast = Toast::new(
        NotificationId::unique::<ForgeErrorToast>(),
        error_message(error).to_string(),
    );
    if let Some(GitHubError::Sso { url: Some(url) }) = github_error(error) {
        let url = url.clone();
        toast = toast.on_click("Authorize", move |_, cx| cx.open_url(&url));
    } else if error
        .chain()
        .any(|cause| cause.is::<crate::checkout::RestrictedMode>())
    {
        toast = toast.on_click("Trust Project…", |window, cx| {
            window.dispatch_action(Box::new(workspace::ToggleWorktreeSecurity), cx)
        });
    }
    workspace.show_toast(toast, cx);
}

/// Hides the toast of [`show_error_toast`], once what failed has worked.
pub fn dismiss_error_toast(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    workspace.dismiss_toast(&NotificationId::unique::<ForgeErrorToast>(), cx);
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A pull request "Layer N" for stack tests.
    pub(crate) fn layer(
        number: u64,
        status: PullRequestStatus,
        is_draft: bool,
        viewer_review: Option<ReviewDecision>,
    ) -> PullRequestSummary {
        PullRequestSummary {
            node_id: format!("PR_{number}").into(),
            number,
            title: format!("Layer {number}").into(),
            url: format!("https://github.com/drew-council/code-review-sandbox/pull/{number}")
                .into(),
            author: Actor {
                login: "drew-council".into(),
                is_bot: false,
            },
            head_ref: format!("stack/{number}").into(),
            base_ref: "main".into(),
            status,
            is_draft,
            review_decision: None,
            is_cross_repository: false,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            viewer_review,
        }
    }

    pub(crate) fn stack_of(
        number: u64,
        pull_requests: Vec<PullRequestSummary>,
    ) -> PullRequestStack {
        PullRequestStack {
            number,
            base_ref: "main".into(),
            pull_requests,
        }
    }

    #[test]
    fn test_stack_review_start_and_neighbors() {
        use PullRequestStatus::{Closed, Merged, Open};
        let approved = Some(ReviewDecision::Approved);
        let changes_requested = Some(ReviewDecision::ChangesRequested);

        // Merged, closed, a draft and one the viewer approved come before the first one to
        // review; one with changes requested by the viewer is still to review.
        let stack = stack_of(
            9,
            vec![
                layer(1, Merged, false, approved),
                layer(2, Closed, false, None),
                layer(3, Open, true, None),
                layer(4, Open, false, approved),
                layer(5, Open, false, changes_requested),
                layer(6, Open, false, None),
            ],
        );
        assert_eq!(stack.review_start(), 4);
        assert_eq!(stack.position(6), Some(5));
        assert!(stack.contains(2) && !stack.contains(7));
        let neighbor = |number, direction| {
            stack
                .neighbor(number, direction)
                .map(|pull_request| pull_request.number)
        };
        // Approved and draft pull requests are stepped onto, merged and closed ones skipped.
        assert_eq!(neighbor(5, StackDirection::Down), Some(4));
        assert_eq!(neighbor(4, StackDirection::Down), Some(3));
        assert_eq!(neighbor(3, StackDirection::Down), None);
        assert_eq!(neighbor(1, StackDirection::Up), Some(3));
        assert_eq!(neighbor(5, StackDirection::Up), Some(6));
        assert_eq!(neighbor(6, StackDirection::Up), None);
        assert_eq!(neighbor(7, StackDirection::Up), None);

        // Everything to review approved: the lowest one ready for review.
        let stack = stack_of(
            9,
            vec![
                layer(1, Merged, false, None),
                layer(2, Open, true, None),
                layer(3, Open, false, approved),
                layer(4, Open, false, approved),
            ],
        );
        assert_eq!(stack.review_start(), 2);
        // Only drafts open: the lowest draft.
        let stack = stack_of(
            9,
            vec![
                layer(1, Merged, false, None),
                layer(2, Open, true, None),
                layer(3, Open, true, None),
            ],
        );
        assert_eq!(stack.review_start(), 1);
        // Nothing open: the bottom.
        let stack = stack_of(
            9,
            vec![layer(1, Merged, false, None), layer(2, Closed, false, None)],
        );
        assert_eq!(stack.review_start(), 0);
    }

    fn rows(kind: HunkRowKind, old_start: u32, new_start: u32, len: u32) -> HunkRows {
        HunkRows {
            kind,
            old_start,
            new_start,
            len,
        }
    }

    #[test]
    fn test_hunks_with_context() {
        use HunkRowKind::{Added, Context, Deleted};

        // 40 lines; line 2 replaced, one line inserted before old line 35, old line 38 deleted.
        let changes = [(2..3, 2..3), (35..35, 35..36), (38..39, 39..39)];
        assert_eq!(
            Hunk::with_context(&changes, 3, 40, 40),
            [
                Hunk {
                    old: 1..6,
                    new: 1..6,
                    rows: vec![
                        rows(Context, 1, 1, 1),
                        rows(Deleted, 2, 2, 1),
                        rows(Added, 3, 2, 1),
                        rows(Context, 3, 3, 3),
                    ],
                },
                // Three unchanged lines between two changes: one hunk, as in `git diff`.
                Hunk {
                    old: 32..41,
                    new: 32..41,
                    rows: vec![
                        rows(Context, 32, 32, 3),
                        rows(Added, 35, 35, 1),
                        rows(Context, 35, 36, 3),
                        rows(Deleted, 38, 39, 1),
                        rows(Context, 39, 39, 2),
                    ],
                },
            ]
        );
        // Seven unchanged lines between changes keep them apart; six don't.
        assert_eq!(
            Hunk::with_context(&[(5..6, 5..6), (13..14, 13..14)], 3, 20, 20).len(),
            2
        );
        assert_eq!(
            Hunk::with_context(&[(5..6, 5..6), (12..13, 12..13)], 3, 20, 20).len(),
            1
        );
        // A new file: everything is one hunk without context.
        assert_eq!(
            Hunk::with_context(&[(1..1, 1..4)], 3, 0, 3),
            [Hunk {
                old: 1..1,
                new: 1..4,
                rows: vec![rows(Added, 1, 1, 3)],
            }]
        );
        assert!(Hunk::with_context(&[], 3, 10, 10).is_empty());
    }

    #[test]
    fn test_check_lines() {
        use DiffSide::{Left, Right};

        // Lines 2 and 35 of 40 changed: hunks over lines 1–5 and 32–38 on both sides.
        let hunks = Hunk::with_context(&[(2..3, 2..3), (35..36, 35..36)], 3, 40, 40);
        let check = |start, end| check_lines(&hunks, start, end);
        assert_eq!(check((Right, 2), (Right, 2)), Ok(()));
        // Unchanged lines inside a hunk take comments, as on GitHub.
        assert_eq!(check((Right, 3), (Right, 5)), Ok(()));
        assert_eq!(check((Left, 1), (Left, 5)), Ok(()));
        assert_eq!(check((Right, 6), (Right, 6)), Err(LinesRefusal::Outside));
        assert_eq!(check((Right, 8), (Right, 12)), Err(LinesRefusal::Outside));
        assert_eq!(check((Right, 4), (Right, 8)), Err(LinesRefusal::EndOutside));
        assert_eq!(
            check((Right, 30), (Right, 33)),
            Err(LinesRefusal::StartOutside)
        );
        assert_eq!(
            check((Right, 3), (Right, 33)),
            Err(LinesRefusal::AcrossHunks)
        );
        assert_eq!(check((Right, 5), (Right, 3)), Err(LinesRefusal::Reversed));
        // An old line comes before the new line that replaced it, not after.
        assert_eq!(check((Left, 2), (Right, 2)), Ok(()));
        assert_eq!(check((Right, 2), (Left, 2)), Err(LinesRefusal::Reversed));
        assert_eq!(
            check_lines(&[], (Right, 1), (Right, 1)),
            Err(LinesRefusal::Outside)
        );
    }
}
