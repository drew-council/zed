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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PullRequestListFilter {
    Open,
    /// Open pull requests that request the viewer's review.
    ReviewRequested,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PullRequestSummary {
    pub number: u64,
    pub title: SharedString,
    pub url: SharedString,
    pub author: Actor,
    pub head_ref: SharedString,
    pub base_ref: SharedString,
    pub is_draft: bool,
    /// Whether the head branch is in another repository (a fork).
    pub is_cross_repository: bool,
    pub updated_at: OffsetDateTime,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PullRequestStatus {
    Open,
    Closed,
    Merged,
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
    pub database_id: u64,
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

impl Hunk {
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
    /// Where the row is in the diff: its hunk, then its index in the hunk. `None` outside the
    /// hunks.
    fn diff_position(&self, side: DiffSide, line: u32) -> Option<(usize, u32)> {
        let hunks = self.hunks.as_ref()?;
        hunks.iter().enumerate().find_map(|(hunk_index, hunk)| {
            if !hunk.contains(side, line) {
                return None;
            }
            // Without the hunk's rows (a patch cut short), deleted rows come before added
            // ones, which is right for a hunk with one change.
            let row = hunk.row_index(side, line).unwrap_or(match side {
                DiffSide::Left => line - hunk.old.start,
                DiffSide::Right => {
                    (line - hunk.new.start).saturating_add(hunk.old.end - hunk.old.start)
                }
            });
            Some((hunk_index, row))
        })
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

/// A forge's pull request API for one repository. Futures are `'static` and `Send`, so callers
/// can run them in the background.
pub trait Forge: Send + Sync + 'static {
    fn context(&self) -> &RepositoryContext;

    /// At most 50 open pull requests of [`RepositoryContext::canonical`], most recently updated
    /// first.
    fn list_pull_requests(
        &self,
        filter: PullRequestListFilter,
    ) -> BoxFuture<'static, Result<Vec<PullRequestSummary>>>;

    fn pull_request(&self, number: u64) -> BoxFuture<'static, Result<PullRequestDetails>>;

    fn pull_request_head(&self, number: u64) -> BoxFuture<'static, Result<HeadStatus>>;

    fn review_threads(&self, number: u64) -> BoxFuture<'static, Result<Vec<RemoteThread>>>;

    fn review_summaries(&self, number: u64)
    -> BoxFuture<'static, Result<Vec<RemoteReviewSummary>>>;

    fn changed_files(&self, number: u64) -> BoxFuture<'static, Result<CommentableLines>>;
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
