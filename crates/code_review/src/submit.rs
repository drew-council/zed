//! Delivering a review. A pull request review is posted to GitHub from a modal that first shows
//! what is sent (its drafts, numbered as GitHub numbers lines) and what would make GitHub refuse
//! it; a local review is finished as Markdown.

use crate::{
    comments::{
        Comment, CommentId, CommentState, CommentTarget, Side, comment_markdown_style,
        open_web_link,
    },
    forge::{
        self, CommentableLines, DiffSide, HeadStatus, PullRequestStatus, ReviewEvent,
        ReviewFileComment, ReviewLineComment, ReviewSubmission,
    },
    pull_request::{
        LineMapping, LocalChanges, LocalChangesEvent, PullRequestState, SubmitReply, SubmitRequest,
        SubmitTarget, check_comment_target, diff_side,
    },
    session::{ReviewOutcome, ReviewSession, ReviewSessionEvent},
    view::{ReviewView, head_oid},
};
use futures::channel::oneshot;
use git::{Oid, repository::RepoPath};
use gpui::{
    ClipboardItem, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Subscription, Task,
    WeakEntity,
};
use language::LanguageRegistry;
use markdown::{Markdown, MarkdownElement};
use project::{
    Project,
    git_store::{Repository, RepositoryEvent},
};
use std::{ops::RangeInclusive, sync::Arc};
use ui::{ElevationIndex, KeyBinding, TintColor, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{DismissDecision, ModalView, Toast, Workspace, notifications::NotificationId};

/// A comment's text as GitHub gets it: a typed comment starts with its type, as in tuicr.
pub(crate) fn comment_body(comment: &Comment) -> String {
    match comment.comment_type {
        Some(comment_type) => format!("[{}] {}", comment_type.label(), comment.body),
        None => comment.body.clone(),
    }
}

/// The review's body: its review comments in the order they were written, a blank line apart
/// (tuicr's `build_review_body`).
pub(crate) fn review_body<'a>(comments: impl IntoIterator<Item = &'a Comment>) -> String {
    comments
        .into_iter()
        .map(comment_body)
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// GitHub refuses a review without a body (whitespace isn't one) that has no line or file
/// comments, unless it approves.
pub(crate) fn needs_body(event: ReviewEvent, comment_count: usize, body: &str) -> bool {
    event != ReviewEvent::Approve && comment_count == 0 && body.trim().is_empty()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    /// Line comments, ranges included.
    pub lines: usize,
    pub ranges: usize,
    pub files: usize,
    pub review: usize,
}

/// A review's drafts as GitHub gets them, and the drafts it doesn't get.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct PlannedReview {
    pub line_comments: Vec<ReviewLineComment>,
    pub file_comments: Vec<ReviewFileComment>,
    pub body: String,
    /// The comments the review posts (line, file and review comments).
    pub comment_ids: Vec<CommentId>,
    pub counts: Counts,
    /// Outdated drafts (where they are): their lines changed after they were written, so they
    /// aren't posted.
    pub outdated: Vec<SharedString>,
    /// Drafts GitHub wouldn't take: where they are, and why.
    pub refused: Vec<(SharedString, SharedString)>,
    /// Line comments waiting for the pull request's head to be checked out: GitHub numbers
    /// lines in that commit.
    pub waiting_for_head: usize,
    pub has_old_lines: bool,
}

/// What a review of the drafts among `comments` sends: line comments numbered as GitHub numbers
/// them (new lines through `working_to_head`, the checkout's changes; old lines as they are,
/// the base's), on renamed files' new paths, inside GitHub's diff (`commentable`), and file
/// comments on files of the pull request. `at_head`: the pull request's head is checked out.
pub(crate) fn plan_review(
    comments: &[Comment],
    commentable: Option<&CommentableLines>,
    at_head: bool,
    working_to_head: impl Fn(&RepoPath, RangeInclusive<u32>) -> LineMapping,
) -> PlannedReview {
    let mut planned = PlannedReview::default();
    let mut review_comments = Vec::new();
    let not_loaded = SharedString::from("GitHub's diff of the pull request hasn't loaded");
    for comment in comments {
        if comment.body.is_empty() || comment.is_submitted() {
            continue;
        }
        let location: SharedString = comment
            .location()
            .unwrap_or_else(|| "Review comment".to_string())
            .into();
        if comment.state == CommentState::Outdated {
            planned.outdated.push(location);
            continue;
        }
        match &comment.target {
            CommentTarget::Review => {
                review_comments.push(comment);
                planned.counts.review += 1;
                planned.comment_ids.push(comment.id);
            }
            CommentTarget::File { path } => {
                let Some(commentable) = commentable else {
                    planned.refused.push((location, not_loaded.clone()));
                    continue;
                };
                if let Err(reason) = check_comment_target(commentable, &comment.target) {
                    planned.refused.push((location, reason));
                    continue;
                }
                planned.file_comments.push(ReviewFileComment {
                    path: commentable.current_path(path.as_unix_str()).to_string(),
                    body: comment_body(comment),
                });
                planned.counts.files += 1;
                planned.comment_ids.push(comment.id);
            }
            CommentTarget::Lines { path, .. } => {
                if !at_head {
                    planned.waiting_for_head += 1;
                    continue;
                }
                let Some(commentable) = commentable else {
                    planned.refused.push((location, not_loaded.clone()));
                    continue;
                };
                let target =
                    match comment.target.map_new_lines(|path, lines| {
                        match working_to_head(path, lines) {
                            LineMapping::Lines(lines) => Ok(lines),
                            mapping => Err(mapping),
                        }
                    }) {
                        Ok(target) => target,
                        Err(mapping) => {
                            planned.refused.push((
                                location,
                                match mapping {
                                    LineMapping::Unknown => {
                                        "Its file is still being compared with HEAD".into()
                                    }
                                    LineMapping::Failed => {
                                        "Its file couldn't be compared with HEAD, so its lines \
                                         can't be numbered as GitHub numbers them"
                                            .into()
                                    }
                                    LineMapping::Changed | LineMapping::Lines(_) => {
                                        "Its lines have local changes, which the pull request \
                                         doesn't have"
                                            .into()
                                    }
                                },
                            ));
                            continue;
                        }
                    };
                if let Err(reason) = check_comment_target(commentable, &target) {
                    planned.refused.push((location, reason));
                    continue;
                }
                let CommentTarget::Lines {
                    side,
                    lines,
                    start_side,
                    ..
                } = &target
                else {
                    continue;
                };
                let (side, start_side) = (diff_side(*side), diff_side(*start_side));
                let is_range = target.is_range();
                planned.line_comments.push(ReviewLineComment {
                    path: commentable.current_path(path.as_unix_str()).to_string(),
                    side,
                    line: *lines.end(),
                    start: is_range.then_some((start_side, *lines.start())),
                    body: comment_body(comment),
                });
                planned.counts.lines += 1;
                if is_range {
                    planned.counts.ranges += 1;
                }
                planned.has_old_lines |= side == DiffSide::Left || start_side == DiffSide::Left;
                planned.comment_ids.push(comment.id);
            }
        }
    }
    planned.body = review_body(review_comments);
    planned
}

/// What the review sends, and how many drafts it doesn't (what keeps them is said below it).
fn counts_line(planned: &PlannedReview) -> String {
    let counts = planned.counts;
    let plural = |count: usize, one: &str, many: &str| {
        format!("{count} {}", if count == 1 { one } else { many })
    };
    let mut parts = Vec::new();
    if counts.lines > 0 {
        let mut lines = plural(counts.lines, "line comment", "line comments");
        if counts.ranges > 0 {
            lines.push_str(&format!(" ({})", plural(counts.ranges, "range", "ranges")));
        }
        parts.push(lines);
    }
    if counts.files > 0 {
        parts.push(plural(counts.files, "file comment", "file comments"));
    }
    if counts.review > 0 {
        parts.push(plural(counts.review, "review comment", "review comments"));
    }
    let blocked = planned.waiting_for_head + planned.refused.len();
    if blocked > 0 {
        parts.push(format!("{blocked} blocked"));
    }
    if !planned.outdated.is_empty() {
        parts.push(format!("{} outdated", planned.outdated.len()));
    }
    if parts.is_empty() {
        "No comments".to_string()
    } else {
        parts.join(" · ")
    }
}

/// The toast saying where a finished local review went.
pub(crate) struct FinishedReviewToast;

/// `; s` in a local review: its comments as Markdown go to `zed --review --wait` when it waits
/// for the review, else to the clipboard. The comments stay drafts.
pub(crate) fn finish_local_review(
    workspace: &mut Workspace,
    review: &Entity<ReviewView>,
    cx: &mut Context<Workspace>,
) {
    let comments = review.read(cx).comments().clone();
    comments.update(cx, |comments, cx| comments.save_open_editors(cx));
    let markdown = review.read(cx).review_markdown(cx);
    let session = review.read(cx).session().clone();
    let message = if session.update(cx, |session, _| session.has_outcome_waiters()) {
        let message = if markdown.is_some() {
            "Review sent to the terminal"
        } else {
            "Review finished (no comments)"
        };
        session.update(cx, |session, _| {
            session.resolve_outcome(ReviewOutcome::Finished { markdown })
        });
        message
    } else {
        match markdown {
            Some(markdown) => {
                cx.write_to_clipboard(ClipboardItem::new_string(markdown));
                "Review finished and copied as Markdown"
            }
            None => "Review finished (no comments)",
        }
    };
    workspace.show_toast(
        Toast::new(NotificationId::unique::<FinishedReviewToast>(), message).autohide(),
        cx,
    );
}

/// The modal of `; s` in a pull request review: Comment, Approve or Request Changes, what the
/// review sends, and what keeps it from being sent. It is planned again whenever the drafts,
/// the pull request, the checkout or its changes change, except while it submits: then it shows
/// what is being sent, and can't be closed (the submit itself belongs to the pull request).
pub(crate) struct SubmitReviewModal {
    review: WeakEntity<ReviewView>,
    session: Entity<ReviewSession>,
    pull_request: Entity<PullRequestState>,
    repository: Entity<Repository>,
    project: WeakEntity<Project>,
    workspace: WeakEntity<Workspace>,
    local_changes: Option<Entity<LocalChanges>>,
    language_registry: Arc<LanguageRegistry>,
    planned: PlannedReview,
    event: ReviewEvent,
    /// GitHub doesn't let authors approve their own pull request or request changes on it.
    own_pull_request: bool,
    /// The pull request's head as loaded, which the line comments are numbered in.
    commit: Oid,
    local_head: Option<Oid>,
    title: SharedString,
    url: SharedString,
    status: PullRequestStatus,
    /// The pull request's base on GitHub isn't the one the review compares against.
    base_moved: bool,
    /// The drafts haven't loaded from the database yet.
    session_loading: bool,
    /// HEAD's texts of the files with new-side drafts are being read, to check that their
    /// comparisons with HEAD are up to date.
    reading_head_texts: bool,
    /// Another tab or window of the pull request is submitting a review.
    submitting_elsewhere: bool,
    /// The last submit may have posted the review (see [`PullRequestState::uncertain_submit`]).
    uncertain: Option<SharedString>,
    head_check: HeadCheck,
    submit_state: SubmitState,
    body: Option<Entity<Markdown>>,
    focus_handle: FocusHandle,
    _head_check_task: Task<()>,
    _reply_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

/// What GitHub says about the pull request now, asked when the modal opens.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum HeadCheck {
    Checking,
    Checked(HeadStatus),
    Failed(SharedString),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SubmitState {
    Ready,
    /// This modal's submit (or check of an uncertain one) runs.
    Submitting,
    Failed(SharedString),
}

/// What Enter does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubmitAction {
    Submit,
    /// The review needs a body: write a review comment first.
    AddReviewComment,
    /// The last submit may have posted the review: ask GitHub before sending it again.
    CheckGitHub,
    /// Something keeps the review from being sent.
    Blocked,
    /// Checking the pull request or the files, loading the drafts, or submitting.
    Busy,
}

/// A line of the modal saying what is wrong, with a link to where it can be fixed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Notice {
    pub text: SharedString,
    pub link: Option<SharedString>,
}

impl Notice {
    fn new(text: impl Into<SharedString>) -> Self {
        Self {
            text: text.into(),
            link: None,
        }
    }
}

impl EventEmitter<DismissEvent> for SubmitReviewModal {}

impl ModalView for SubmitReviewModal {
    /// Escape, Cancel, a click outside and another modal (the command palette, say) all ask to
    /// close the modal: not while its submit runs, whose result it shows.
    fn on_before_dismiss(&mut self, _: &mut Window, _: &mut Context<Self>) -> DismissDecision {
        if self.submit_state == SubmitState::Submitting {
            DismissDecision::Pending
        } else {
            DismissDecision::Dismiss(true)
        }
    }
}

impl Focusable for SubmitReviewModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl SubmitReviewModal {
    /// Opens the modal for `review` (a review of `pull_request`), after leaving its open comment
    /// editors, which keep their text.
    pub(crate) fn open(
        workspace: &mut Workspace,
        review: Entity<ReviewView>,
        pull_request: Entity<PullRequestState>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let comments = review.read(cx).comments().clone();
        comments.update(cx, |comments, cx| comments.save_open_editors(cx));
        workspace.toggle_modal(window, cx, |window, cx| {
            Self::new(review, pull_request, window, cx)
        });
    }

    fn new(
        review: Entity<ReviewView>,
        pull_request: Entity<PullRequestState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let view = review.read(cx);
        let session = view.session().clone();
        let repository = view.repository().clone();
        let project = view.project().downgrade();
        let workspace = view.workspace().clone();
        let local_changes = view.local_changes().cloned();
        let language_registry = view.language_registry(cx);
        let details = pull_request.read(cx).details().clone();
        let event = pull_request.read(cx).review_event();
        let mut subscriptions = vec![
            cx.subscribe(
                &session,
                |this, _, event: &ReviewSessionEvent, cx| match event {
                    ReviewSessionEvent::Loaded | ReviewSessionEvent::Comment(_) => this.replan(cx),
                    ReviewSessionEvent::FilesChanged(_)
                    | ReviewSessionEvent::HunksChanged
                    | ReviewSessionEvent::Failed(_) => {}
                },
            ),
            // Loads, and submits from any tab.
            cx.observe(&pull_request, |this, _, cx| this.replan(cx)),
            cx.subscribe(&repository, |this, _, event: &RepositoryEvent, cx| {
                if matches!(event, RepositoryEvent::HeadChanged) {
                    this.replan(cx);
                }
            }),
            // Back from GitHub, where the pending review may have been submitted or discarded
            // (or the network is back).
            cx.observe_window_activation(window, |this, window, cx| {
                let blocked_on_github = match &this.head_check {
                    HeadCheck::Checked(status) => status.pending_review.is_some(),
                    HeadCheck::Failed(_) => true,
                    HeadCheck::Checking => false,
                };
                if window.is_window_active()
                    && blocked_on_github
                    && this.submit_state != SubmitState::Submitting
                {
                    this.check_head(cx);
                }
            }),
        ];
        if let Some(local_changes) = &local_changes {
            subscriptions.push(
                cx.subscribe(local_changes, |this, _, _: &LocalChangesEvent, cx| {
                    this.replan(cx)
                }),
            );
        }
        let mut modal = Self {
            review: review.downgrade(),
            session,
            pull_request,
            repository,
            project,
            workspace,
            local_changes,
            language_registry,
            planned: PlannedReview::default(),
            event,
            own_pull_request: details.viewer_is_author,
            commit: details.head_oid,
            local_head: None,
            title: SharedString::default(),
            url: details.url.clone(),
            status: details.state,
            base_moved: false,
            session_loading: false,
            reading_head_texts: false,
            submitting_elsewhere: false,
            uncertain: None,
            head_check: HeadCheck::Checking,
            submit_state: SubmitState::Ready,
            body: None,
            focus_handle: cx.focus_handle(),
            _head_check_task: Task::ready(()),
            _reply_task: Task::ready(()),
            _subscriptions: subscriptions,
        };
        modal.check_head(cx);
        modal.replan(cx);
        modal
    }

    /// Asks GitHub about the pull request now: whether it moved on, was closed, or has a
    /// pending review of the viewer's (which keeps GitHub from taking another review).
    fn check_head(&mut self, cx: &mut Context<Self>) {
        let pull_request = self.pull_request.read(cx);
        let forge = pull_request.forge().clone();
        let check = cx
            .background_executor()
            .spawn(forge.pull_request_head(pull_request.details().number));
        self.head_check = HeadCheck::Checking;
        cx.notify();
        self._head_check_task = cx.spawn(async move |this, cx| {
            let head_check = match check.await {
                Ok(status) => HeadCheck::Checked(status),
                Err(error) => {
                    log::error!("checking the pull request before submitting: {error:#}");
                    HeadCheck::Failed(forge::error_message(&error))
                }
            };
            this.update(cx, |this, cx| {
                this.head_check = head_check;
                // A refusal the check explains (a pending review made meanwhile, say) shows as
                // the check's blocker, worded as when the modal opened, instead of GitHub's
                // own words.
                if matches!(this.submit_state, SubmitState::Failed(_))
                    && !this.blockers().is_empty()
                {
                    this.submit_state = SubmitState::Ready;
                }
                cx.notify();
            })
            .log_err();
        });
    }

    /// Plans the review again from the drafts, the pull request and the checkout as they are
    /// now. Not while this modal submits: it shows what is being sent.
    fn replan(&mut self, cx: &mut Context<Self>) {
        let state = self.pull_request.read(cx);
        self.submitting_elsewhere =
            state.is_submitting() && self.submit_state != SubmitState::Submitting;
        self.uncertain = state.uncertain_submit().cloned();
        if self.submit_state == SubmitState::Submitting {
            cx.notify();
            return;
        }
        let details = state.details().clone();
        let canonical = state.forge().context().canonical.to_string();
        let local_head = head_oid(&self.repository, cx);
        let at_head = local_head == Some(details.head_oid);
        let review_base = self
            .review
            .upgrade()
            .and_then(|review| review.read(cx).pull_request_base());
        let session = self.session.read(cx);
        self.session_loading = !session.is_loaded();
        let comments = session.comments().iter().cloned().collect::<Vec<_>>();
        // New lines are numbered in HEAD through each file's comparison with it, which must be
        // a comparison with this commit's text: Zed reloads it some time after a checkout.
        if at_head && let Some(local_changes) = &self.local_changes {
            let paths = comments
                .iter()
                .filter(|comment| !comment.is_submitted() && !comment.body.is_empty())
                .filter_map(|comment| match &comment.target {
                    CommentTarget::Lines {
                        path,
                        side: Side::New,
                        ..
                    } => Some(path.clone()),
                    CommentTarget::Lines { .. }
                    | CommentTarget::File { .. }
                    | CommentTarget::Review => None,
                })
                .collect::<Vec<_>>();
            let repository = self.repository.clone();
            local_changes.update(cx, |local_changes, cx| {
                local_changes.read_head_texts(&repository, details.head_oid, paths, cx)
            });
        }
        let local_changes = self
            .local_changes
            .as_ref()
            .map(|local_changes| local_changes.read(cx));
        self.reading_head_texts =
            local_changes.is_some_and(|local_changes| local_changes.is_reading_head_texts());
        let planned = plan_review(
            &comments,
            self.pull_request.read(cx).commentable(),
            at_head,
            |path, lines| match local_changes {
                Some(local_changes) => {
                    local_changes.working_to_head(path, lines, details.head_oid, cx)
                }
                None => LineMapping::Unknown,
            },
        );
        if planned.body != self.planned.body || self.body.is_none() {
            self.body = (!planned.body.is_empty()).then(|| {
                let body = SharedString::from(planned.body.clone());
                let language_registry = self.language_registry.clone();
                cx.new(|cx| Markdown::new(body, Some(language_registry), None, cx))
            });
        }
        self.planned = planned;
        self.own_pull_request = details.viewer_is_author;
        if !self.is_allowed(self.event) {
            self.event = ReviewEvent::Comment;
        }
        self.commit = details.head_oid;
        self.local_head = local_head;
        self.title = format!("{canonical}#{}: {}", details.number, details.title).into();
        self.url = details.url.clone();
        self.status = details.state;
        self.base_moved = review_base.is_some_and(|base| base != details.base_oid);
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn event(&self) -> ReviewEvent {
        self.event
    }

    #[cfg(test)]
    pub(crate) fn planned(&self) -> &PlannedReview {
        &self.planned
    }

    #[cfg(test)]
    pub(crate) fn submit_state(&self) -> &SubmitState {
        &self.submit_state
    }

    fn is_allowed(&self, event: ReviewEvent) -> bool {
        event == ReviewEvent::Comment || !self.own_pull_request
    }

    pub(crate) fn select_event(&mut self, event: ReviewEvent, cx: &mut Context<Self>) {
        if self.is_allowed(event) && self.submit_state != SubmitState::Submitting {
            self.event = event;
            self.pull_request
                .update(cx, |pull_request, _| pull_request.set_review_event(event));
            if matches!(self.submit_state, SubmitState::Failed(_)) {
                self.submit_state = SubmitState::Ready;
            }
            cx.notify();
        }
    }

    /// The next (or previous) event that may be chosen, wrapping around.
    fn cycle_event(&mut self, forward: bool, cx: &mut Context<Self>) {
        let events = ReviewEvent::ALL;
        let Some(index) = events.iter().position(|event| *event == self.event) else {
            return;
        };
        let count = events.len();
        let next = (1..count)
            .map(|step| {
                let next = if forward {
                    index + step
                } else {
                    index + count - step
                };
                events[next % count]
            })
            .find(|event| self.is_allowed(*event));
        if let Some(event) = next {
            self.select_event(event, cx);
        }
    }

    fn comment_count(&self) -> usize {
        self.planned.line_comments.len() + self.planned.file_comments.len()
    }

    /// What keeps the review from being sent.
    pub(crate) fn blockers(&self) -> Vec<Notice> {
        let mut blockers = Vec::new();
        if let HeadCheck::Checked(status) = &self.head_check
            && status.pending_review.is_some()
        {
            blockers.push(Notice {
                text: "You have a pending review on this pull request on GitHub, which keeps \
                       GitHub from taking another one. Submit or discard it there, then try again."
                    .into(),
                link: Some(self.url.clone()),
            });
        }
        if self.planned.waiting_for_head > 0 {
            let checked_out = match self.local_head {
                Some(local_head) => format!("{} is checked out", local_head.display_short()),
                None => "nothing is checked out".to_string(),
            };
            blockers.push(Notice::new(format!(
                "Check out the pull request's head {} to submit line comments: GitHub numbers \
                 their lines in that commit, and {checked_out}.",
                self.commit.display_short()
            )));
        }
        if !self.planned.refused.is_empty() {
            let comments = self
                .planned
                .refused
                .iter()
                .map(|(location, reason)| format!("{location}: {reason}"))
                .collect::<Vec<_>>()
                .join("; ");
            blockers.push(Notice::new(format!(
                "GitHub wouldn't take these comments (edit or delete them): {comments}"
            )));
        }
        blockers
    }

    /// What may be unexpected about the review, which is sent anyway.
    pub(crate) fn warnings(&self) -> Vec<Notice> {
        let mut warnings = Vec::new();
        if let Some(uncertain) = &self.uncertain {
            warnings.push(Notice {
                text: format!(
                    "The last submit may have posted the review: {uncertain} Enter asks GitHub."
                )
                .into(),
                link: Some(self.url.clone()),
            });
        }
        // What GitHub says now, else what it said when the pull request loaded.
        let mut status = self.status;
        match &self.head_check {
            HeadCheck::Checked(head) => {
                status = head.state;
                if head.head_oid != self.commit {
                    warnings.push(Notice::new(format!(
                        "The pull request moved on to {} since it was loaded: the comments are \
                         made on {}, the commit reviewed here.",
                        head.head_oid.display_short(),
                        self.commit.display_short()
                    )))
                }
            }
            HeadCheck::Failed(error) => warnings.push(Notice::new(format!(
                "Couldn't check the pull request on GitHub: {error}"
            ))),
            HeadCheck::Checking => {}
        }
        match status {
            PullRequestStatus::Open => {}
            PullRequestStatus::Closed => warnings.push(Notice::new("This pull request is closed.")),
            PullRequestStatus::Merged => warnings.push(Notice::new("This pull request is merged.")),
        }
        if !self.planned.outdated.is_empty() {
            warnings.push(Notice::new(format!(
                "Outdated comments aren't submitted: their lines changed after they were \
                 written. Delete them (dd) or write them again: {}",
                self.planned.outdated.join(", ")
            )));
        }
        if self.base_moved && self.planned.has_old_lines {
            warnings.push(Notice::new(
                "The pull request's base moved on GitHub since the review opened: if the pull \
                 request was updated with it, GitHub numbers old lines differently. Reopen the \
                 pull request to compare against its new base.",
            ));
        }
        warnings
    }

    pub(crate) fn action(&self) -> SubmitAction {
        if self.submit_state == SubmitState::Submitting
            || self.submitting_elsewhere
            || self.head_check == HeadCheck::Checking
            || self.session_loading
            || self.reading_head_texts
        {
            SubmitAction::Busy
        } else if self.uncertain.is_some() {
            SubmitAction::CheckGitHub
        } else if !self.blockers().is_empty() {
            SubmitAction::Blocked
        } else if needs_body(self.event, self.comment_count(), &self.planned.body) {
            SubmitAction::AddReviewComment
        } else {
            SubmitAction::Submit
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        match self.action() {
            SubmitAction::Submit => self.submit(cx),
            SubmitAction::AddReviewComment => self.add_review_comment(window, cx),
            SubmitAction::CheckGitHub => self.check_github(cx),
            SubmitAction::Blocked | SubmitAction::Busy => {}
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        if self.submit_state != SubmitState::Submitting {
            cx.emit(DismissEvent);
        }
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.cycle_event(true, cx);
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_event(false, cx);
    }

    /// Closes the modal and starts a review comment in the review tab, which becomes the
    /// review's body.
    fn add_review_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
        let review = self.review.clone();
        // After the modal is gone, which gives the focus back first.
        window.defer(cx, move |window, cx| {
            let Some(review) = review.upgrade() else {
                return;
            };
            if let Some(workspace) = review.read(cx).workspace().upgrade() {
                workspace.update(cx, |workspace, cx| {
                    workspace.activate_item(&review, true, true, window, cx)
                });
            }
            let comments = review.read(cx).comments().clone();
            comments.update(cx, |comments, cx| comments.add_review_comment(window, cx));
        });
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let request = SubmitRequest {
            submission: ReviewSubmission {
                commit_id: self.commit,
                event: self.event,
                body: self.planned.body.clone(),
                line_comments: self.planned.line_comments.clone(),
                file_comments: self.planned.file_comments.clone(),
            },
            target: SubmitTarget {
                session: self.session.clone(),
                comment_ids: self.planned.comment_ids.clone(),
                repository: self.repository.downgrade(),
                project: self.project.clone(),
                workspace: self.workspace.clone(),
            },
        };
        self.submit_state = SubmitState::Submitting;
        cx.notify();
        let reply = self
            .pull_request
            .update(cx, |pull_request, cx| pull_request.submit(request, cx));
        self.await_reply(reply, cx);
    }

    /// Asks GitHub whether the last submit posted the review.
    fn check_github(&mut self, cx: &mut Context<Self>) {
        let reply = self.pull_request.update(cx, |pull_request, cx| {
            pull_request.check_uncertain_submit(cx)
        });
        if let Some(reply) = reply {
            self.submit_state = SubmitState::Submitting;
            cx.notify();
            self.await_reply(reply, cx);
        }
    }

    fn await_reply(&mut self, reply: oneshot::Receiver<SubmitReply>, cx: &mut Context<Self>) {
        self._reply_task = cx.spawn(async move |this, cx| {
            let reply = reply.await;
            this.update(cx, |this, cx| this.replied(reply, cx))
                .log_err();
        });
    }

    fn replied(&mut self, reply: Result<SubmitReply, oneshot::Canceled>, cx: &mut Context<Self>) {
        self.submit_state = SubmitState::Ready;
        match reply {
            // The pull request shows the toast.
            Ok(SubmitReply::Submitted { .. }) => {
                cx.emit(DismissEvent);
                return;
            }
            // What GitHub refused may be what the check finds now.
            Ok(SubmitReply::Failed(message)) => {
                self.submit_state = SubmitState::Failed(message);
                self.check_head(cx);
            }
            // The notice says so.
            Ok(SubmitReply::Uncertain(_)) => {}
            Err(oneshot::Canceled) => {
                self.submit_state = SubmitState::Failed("The submit was interrupted".into())
            }
        }
        self.replan(cx);
    }

    fn render_counts(&self) -> impl IntoElement {
        Label::new(counts_line(&self.planned))
            .size(LabelSize::Small)
            .color(Color::Muted)
    }

    fn render_notice(
        &self,
        id: &'static str,
        index: usize,
        notice: Notice,
        icon: IconName,
        color: Color,
    ) -> impl IntoElement {
        h_flex()
            .w_full()
            .gap_1()
            .items_start()
            .child(Icon::new(icon).size(IconSize::Small).color(color))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(Label::new(notice.text).size(LabelSize::Small).color(color))
                    .when_some(notice.link, |notice, link| {
                        notice.child(
                            Button::new((id, index), link.clone())
                                .label_size(LabelSize::Small)
                                .color(Color::Muted)
                                .tooltip(Tooltip::text("Open on GitHub"))
                                .on_click(move |_, window, cx| open_web_link(&link, window, cx)),
                        )
                    }),
            )
    }
}

impl Render for SubmitReviewModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let action = self.action();
        let event_buttons = ReviewEvent::ALL.map(|event| {
            let allowed = self.is_allowed(event);
            let selected = self.event == event;
            Button::new(("review-event", event as usize), event.label())
                .label_size(LabelSize::Small)
                .layer(ElevationIndex::ModalSurface)
                .toggle_state(selected)
                .style(if selected {
                    ButtonStyle::Tinted(TintColor::Accent)
                } else {
                    ButtonStyle::Outlined
                })
                .disabled(!allowed || self.submit_state == SubmitState::Submitting)
                .when(!allowed, |button| {
                    button.tooltip(Tooltip::text(match event {
                        ReviewEvent::Approve => "Can not approve your own pull request",
                        ReviewEvent::RequestChanges | ReviewEvent::Comment => {
                            "Can not request changes on your own pull request"
                        }
                    }))
                })
                .on_click(cx.listener(move |this, _, _, cx| this.select_event(event, cx)))
        });
        let body = match &self.body {
            Some(body) => div()
                .w_full()
                .max_h(window.line_height() * 10.)
                .overflow_hidden()
                .text_sm()
                .child(MarkdownElement::new(
                    body.clone(),
                    comment_markdown_style(window, cx),
                ))
                .into_any_element(),
            None => Label::new("No review comment")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
        };
        let blockers = self.blockers();
        let warnings = self.warnings();
        let muted =
            |text: &'static str| Some(Label::new(text).size(LabelSize::Small).color(Color::Muted));
        let status = match (&self.submit_state, &self.head_check, action) {
            (SubmitState::Submitting, _, _) if self.uncertain.is_some() => {
                muted("Asking GitHub whether the review was posted…")
            }
            (SubmitState::Submitting, _, _) => muted("Submitting…"),
            (SubmitState::Failed(error), _, _) => Some(
                Label::new(error.clone())
                    .size(LabelSize::Small)
                    .color(Color::Error),
            ),
            _ if self.submitting_elsewhere => {
                muted("A review of this pull request is being submitted from another tab…")
            }
            (_, HeadCheck::Checking, _) => muted("Checking the pull request on GitHub…"),
            _ if self.session_loading => muted("Loading the review's comments…"),
            _ if self.reading_head_texts => muted("Comparing the files with HEAD…"),
            (_, _, SubmitAction::AddReviewComment) => Some(
                Label::new(match self.event {
                    ReviewEvent::RequestChanges => {
                        "Requesting changes needs a review comment or a line or file comment."
                    }
                    ReviewEvent::Comment | ReviewEvent::Approve => {
                        "A review without line or file comments needs a review comment."
                    }
                })
                .size(LabelSize::Small)
                .color(Color::Warning),
            ),
            _ => None,
        };
        let confirm_label = match action {
            SubmitAction::AddReviewComment => "Add Review Comment".to_string(),
            SubmitAction::CheckGitHub => "Check GitHub".to_string(),
            SubmitAction::Submit | SubmitAction::Blocked | SubmitAction::Busy => {
                format!("Submit: {}", self.event.label())
            }
        };
        let submitting = self.submit_state == SubmitState::Submitting;
        let focus_handle = self.focus_handle.clone();

        v_flex()
            .key_context("SubmitReviewModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .elevation_3(cx)
            .w(rems(36.))
            .p_3()
            .gap_2()
            .child(
                v_flex()
                    .gap_0p5()
                    .child(Headline::new("Submit Review").size(HeadlineSize::Small))
                    .child(
                        Label::new(self.title.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    ),
            )
            .child(h_flex().gap_1().children(event_buttons))
            .when(self.own_pull_request, |modal| {
                modal.child(
                    Label::new(
                        "You wrote this pull request: GitHub doesn't let you approve it or \
                         request changes on it.",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
            })
            .child(self.render_counts())
            .child(
                v_flex()
                    .gap_1()
                    .p_2()
                    .rounded_sm()
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        Label::new("Review comment")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(body),
            )
            .children(blockers.into_iter().enumerate().map(|(index, blocker)| {
                self.render_notice(
                    "submit-blocker",
                    index,
                    blocker,
                    IconName::XCircle,
                    Color::Error,
                )
            }))
            .children(warnings.into_iter().enumerate().map(|(index, warning)| {
                self.render_notice(
                    "submit-warning",
                    index,
                    warning,
                    IconName::Warning,
                    Color::Warning,
                )
            }))
            .children(status)
            .child(
                h_flex()
                    .justify_end()
                    .gap_1()
                    .child(
                        Button::new("submit-review-cancel", "Cancel")
                            .label_size(LabelSize::Small)
                            .layer(ElevationIndex::ModalSurface)
                            .disabled(submitting)
                            .key_binding(KeyBinding::for_action_in(
                                &menu::Cancel,
                                &focus_handle,
                                cx,
                            ))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.cancel(&menu::Cancel, window, cx)
                            })),
                    )
                    .child(
                        Button::new("submit-review-confirm", confirm_label)
                            .label_size(LabelSize::Small)
                            .layer(ElevationIndex::ModalSurface)
                            .style(ButtonStyle::Filled)
                            .disabled(matches!(action, SubmitAction::Blocked | SubmitAction::Busy))
                            .key_binding(KeyBinding::for_action_in(
                                &menu::Confirm,
                                &focus_handle,
                                cx,
                            ))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&menu::Confirm, window, cx)
                            })),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        comments::CommentType,
        github::test_support::{PR1_FILES, changed_files_fixture},
    };

    fn comment(id: u64, target: CommentTarget, body: &str) -> Comment {
        Comment {
            id: CommentId(id),
            target,
            comment_type: None,
            body: body.into(),
            line_text: None,
            state: CommentState::Draft,
        }
    }

    fn lines(path: &str, side: Side, lines: RangeInclusive<u32>) -> CommentTarget {
        CommentTarget::lines(RepoPath::new(path).unwrap(), side, lines)
    }

    fn file(path: &str) -> CommentTarget {
        CommentTarget::File {
            path: RepoPath::new(path).unwrap(),
        }
    }

    fn mixed(start_side: Side, start: u32, side: Side, end: u32) -> CommentTarget {
        CommentTarget::Lines {
            path: RepoPath::new("src/engine.rs").unwrap(),
            side,
            lines: start..=end,
            start_side,
        }
    }

    #[test]
    fn test_counts_line() {
        let mut planned = PlannedReview::default();
        assert_eq!(counts_line(&planned), "No comments");
        planned.waiting_for_head = 1;
        assert_eq!(counts_line(&planned), "1 blocked");
        planned.waiting_for_head = 0;
        planned.outdated = vec!["a.rs:3".into()];
        assert_eq!(counts_line(&planned), "1 outdated");
        planned.counts = Counts {
            lines: 2,
            ranges: 1,
            files: 1,
            review: 0,
        };
        planned.refused = vec![("b.rs:1".into(), "outside the diff".into())];
        planned.waiting_for_head = 1;
        assert_eq!(
            counts_line(&planned),
            "2 line comments (1 range) · 1 file comment · 2 blocked · 1 outdated"
        );
    }

    #[test]
    fn test_review_body_and_when_it_is_needed() {
        let mut overall = comment(0, CommentTarget::Review, "Overall\nfine");
        overall.comment_type = Some(CommentType::Issue);
        let plain = comment(1, CommentTarget::Review, "Second");
        assert_eq!(comment_body(&overall), "[ISSUE] Overall\nfine");
        assert_eq!(
            review_body([&overall, &plain]),
            "[ISSUE] Overall\nfine\n\nSecond"
        );
        assert_eq!(review_body([]), "");

        // GitHub: COMMENT and REQUEST_CHANGES need a body or a line or file comment; APPROVE
        // never does; whitespace isn't a body.
        for (event, comment_count, body, needed) in [
            (ReviewEvent::Comment, 0, "", true),
            (ReviewEvent::Comment, 0, "  \n ", true),
            (ReviewEvent::Comment, 0, "x", false),
            (ReviewEvent::Comment, 1, "", false),
            (ReviewEvent::RequestChanges, 0, "", true),
            (ReviewEvent::RequestChanges, 0, "\t", true),
            (ReviewEvent::RequestChanges, 2, "", false),
            (ReviewEvent::Approve, 0, "", false),
        ] {
            assert_eq!(
                needs_body(event, comment_count, body),
                needed,
                "{event:?} {comment_count} {body:?}"
            );
        }
    }

    /// Drafts become GitHub's comments: new lines numbered in HEAD, old lines as they are (LEFT),
    /// ranges with their start, renamed files on their new path, typed bodies prefixed. Outdated
    /// and submitted drafts, and drafts GitHub wouldn't take, aren't sent.
    #[test]
    fn test_plan_review() {
        let commentable = changed_files_fixture(PR1_FILES);
        let engine = RepoPath::new("src/engine.rs").unwrap();
        // The working tree has two lines more than HEAD at the top of `src/engine.rs`.
        let working_to_head = |path: &RepoPath, lines: RangeInclusive<u32>| {
            if *path != engine {
                LineMapping::Lines(lines)
            } else if *lines.start() <= 2 {
                LineMapping::Changed
            } else {
                LineMapping::Lines(lines.start() - 2..=lines.end() - 2)
            }
        };
        let mut overall = comment(0, CommentTarget::Review, "Overall");
        overall.comment_type = Some(CommentType::Issue);
        let mut fifty_nine = comment(2, lines("src/engine.rs", Side::New, 61..=61), "59");
        fifty_nine.comment_type = Some(CommentType::Suggestion);
        let mut outdated = comment(12, lines("src/engine.rs", Side::New, 7..=7), "old");
        outdated.state = CommentState::Outdated;
        let mut submitted = comment(13, file("docs/guide.md"), "sent");
        submitted.state = CommentState::Submitted {
            review_id: "1".into(),
        };
        let comments = [
            overall,
            comment(1, CommentTarget::Review, "Second"),
            fifty_nine,
            comment(3, lines("src/engine.rs", Side::New, 132..=136), "range"),
            comment(4, lines("src/engine.rs", Side::Old, 193..=193), "deleted"),
            comment(
                5,
                lines("src/engine.rs", Side::Old, 191..=193),
                "deleted range",
            ),
            comment(6, lines("src/format.rs", Side::Old, 18..=18), "renamed"),
            comment(7, file("docs/guide.md"), "file"),
            comment(8, lines("src/engine.rs", Side::New, 102..=102), "outside"),
            comment(9, lines("src/engine.rs", Side::New, 1..=1), "local"),
            comment(10, file("README.md"), "not in the pull request"),
            comment(11, CommentTarget::Review, ""),
            outdated,
            submitted,
            // From removed lines to the added line replacing them, and from an added line to a
            // removed one below it: GitHub's `start_side` (LV MX).
            comment(
                14,
                mixed(Side::Old, 191, Side::New, 215),
                "removed and added",
            ),
            comment(
                15,
                mixed(Side::New, 205, Side::Old, 185),
                "added and removed",
            ),
            // Old line 185 comes before new line 207 in GitHub's diff.
            comment(16, mixed(Side::New, 209, Side::Old, 185), "reversed"),
        ];
        let planned = plan_review(&comments, Some(&commentable), true, working_to_head);
        let line = |path: &str, side, line, start, body: &str| ReviewLineComment {
            path: path.into(),
            side,
            line,
            start,
            body: body.into(),
        };
        assert_eq!(
            planned.line_comments,
            [
                line(
                    "src/engine.rs",
                    DiffSide::Right,
                    59,
                    None,
                    "[SUGGESTION] 59"
                ),
                line(
                    "src/engine.rs",
                    DiffSide::Right,
                    134,
                    Some((DiffSide::Right, 130)),
                    "range"
                ),
                line("src/engine.rs", DiffSide::Left, 193, None, "deleted"),
                line(
                    "src/engine.rs",
                    DiffSide::Left,
                    193,
                    Some((DiffSide::Left, 191)),
                    "deleted range"
                ),
                line("src/render.rs", DiffSide::Left, 18, None, "renamed"),
                line(
                    "src/engine.rs",
                    DiffSide::Right,
                    213,
                    Some((DiffSide::Left, 191)),
                    "removed and added"
                ),
                line(
                    "src/engine.rs",
                    DiffSide::Left,
                    185,
                    Some((DiffSide::Right, 203)),
                    "added and removed"
                ),
            ]
        );
        assert_eq!(
            planned.file_comments,
            [ReviewFileComment {
                path: "docs/guide.md".into(),
                body: "file".into(),
            }]
        );
        assert_eq!(planned.body, "[ISSUE] Overall\n\nSecond");
        assert_eq!(
            planned.counts,
            Counts {
                lines: 7,
                ranges: 4,
                files: 1,
                review: 2,
            }
        );
        assert_eq!(
            planned.comment_ids,
            (0..=7).chain([14, 15]).map(CommentId).collect::<Vec<_>>()
        );
        assert_eq!(planned.outdated, ["src/engine.rs:7"]);
        assert_eq!(
            planned.refused,
            [
                (
                    "src/engine.rs:102".into(),
                    "GitHub only accepts comments within its diff hunks".into()
                ),
                (
                    "src/engine.rs:1".into(),
                    "Its lines have local changes, which the pull request doesn't have".into()
                ),
                (
                    "README.md".into(),
                    "This file isn't part of the pull request on GitHub".into()
                ),
                (
                    "src/engine.rs:209-~185".into(),
                    "The selection's start comes after its end in GitHub's diff".into()
                ),
            ]
        );
        assert_eq!(planned.waiting_for_head, 0);
        assert!(planned.has_old_lines);

        // Another commit checked out: line comments wait for the pull request's head, the rest
        // can go.
        let planned = plan_review(&comments, Some(&commentable), false, working_to_head);
        assert_eq!(planned.waiting_for_head, 10);
        assert!(planned.line_comments.is_empty());
        assert_eq!(planned.file_comments.len(), 1);
        assert_eq!(planned.counts.review, 2);

        // GitHub's diff didn't load: nothing on files or lines can be checked.
        let planned = plan_review(&comments, None, true, working_to_head);
        assert!(planned.line_comments.is_empty());
        assert!(planned.file_comments.is_empty());
        assert_eq!(planned.refused.len(), 12);

        // A file that couldn't be compared with HEAD: its working tree's numbers may not be
        // HEAD's, so its new lines aren't sent.
        let planned = plan_review(&comments[2..3], Some(&commentable), true, |_, _| {
            LineMapping::Failed
        });
        assert!(planned.line_comments.is_empty());
        assert_eq!(
            planned.refused,
            [(
                "src/engine.rs:61".into(),
                "Its file couldn't be compared with HEAD, so its lines can't be numbered as \
                 GitHub numbers them"
                    .into()
            )]
        );
    }
}

#[cfg(test)]
mod review_tests {
    use super::*;
    use crate::{
        github::test_support::{
            PR1_DETAILS, PR1_HEAD, PR1_HEAD_OID, PR1_SUMMARIES, Reply, SentRequest,
        },
        pull_request::tests::review::{
            Fixture, has_open_comment_editor, notification_count, open_review, requests_of,
        },
        test_support::{
            C1, dot_git, init_vim_test, move_to_row_with_text, open_review_fixture, rhs_editor,
        },
    };
    use futures::FutureExt as _;
    use gpui::{TestAppContext, VisualTestContext};
    use http_client::Method;
    use settings::DiffViewStyle;
    use std::sync::{Arc, Mutex};

    const PR1_URL: &str = "https://github.com/drew-council/code-review-sandbox/pull/1";

    /// Another modal, which the modal layer would open instead of the submit modal.
    struct OtherModal(FocusHandle);

    impl EventEmitter<DismissEvent> for OtherModal {}
    impl ModalView for OtherModal {}

    impl Focusable for OtherModal {
        fn focus_handle(&self, _: &App) -> FocusHandle {
            self.0.clone()
        }
    }

    impl Render for OtherModal {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().track_focus(&self.0)
        }
    }

    fn post_count(requests: &crate::github::test_support::SentRequests) -> usize {
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| {
                request.method == Method::POST && request.uri.ends_with("/pulls/1/reviews")
            })
            .count()
    }

    fn review_reply(id: u64, state: &str) -> Reply {
        Reply::json(
            serde_json::json!({
                "id": id,
                "node_id": format!("PRR_{id}"),
                "html_url": format!("{PR1_URL}#pullrequestreview-{id}"),
                "state": state,
            })
            .to_string(),
        )
    }

    fn modal(
        review: &Entity<ReviewView>,
        cx: &mut VisualTestContext,
    ) -> Option<Entity<SubmitReviewModal>> {
        let workspace = review.read_with(cx, |review, _| review.workspace().upgrade().unwrap());
        workspace.read_with(cx, |workspace, cx| workspace.active_modal(cx))
    }

    fn comment(keys: &str, text: &str, cx: &mut VisualTestContext) {
        cx.simulate_keystrokes(keys);
        cx.simulate_input(text);
        cx.simulate_keystrokes("ctrl-enter");
    }

    fn posted(requests: &crate::github::test_support::SentRequests) -> Vec<String> {
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.method != Method::GET)
            .filter_map(|request| match request.operation() {
                Some("AddFileThread") => Some("AddFileThread".to_string()),
                Some(_) => None,
                None => Some(format!("{} {}", request.method, request.uri)),
            })
            .collect()
    }

    /// `; s` posts the drafts as one review (a pending one here: a file comment needs GraphQL),
    /// which marks them submitted, hides them in the diff (their threads come back from
    /// GitHub) and tells whoever waits for the review.
    #[gpui::test]
    async fn test_submit_a_pull_request_review(cx: &mut TestAppContext) {
        use std::sync::atomic::{AtomicBool, Ordering};

        // Once posted, the review's summary comes last among the pull request's reviews.
        let posted_review = Arc::new(AtomicBool::new(false));
        let summaries_with_posted_review = {
            let mut summaries: serde_json::Value = serde_json::from_str(PR1_SUMMARIES).unwrap();
            summaries["data"]["repository"]["pullRequest"]["reviews"]["nodes"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({
                    "id": "PRR_9",
                    "state": "COMMENTED",
                    "body": "Overall",
                    "submittedAt": "2026-10-17T12:00:00Z",
                    "url": format!("{PR1_URL}#pullrequestreview-9"),
                    "author": { "__typename": "User", "login": "drew-council" },
                }));
            summaries.to_string()
        };
        let respond: Arc<dyn Fn(&SentRequest) -> Option<Reply> + Send + Sync> = Arc::new({
            let posted_review = posted_review.clone();
            move |request| match request.operation() {
                Some("PullRequestHead") => Some(Reply::json(PR1_HEAD)),
                Some("AddFileThread") => Some(Reply::json(
                    r#"{"data":{"addPullRequestReviewThread":{"thread":{"id":"PRRT_new"}}}}"#,
                )),
                Some("ReviewSummaries") if posted_review.load(Ordering::SeqCst) => {
                    Some(Reply::json(summaries_with_posted_review.clone()))
                }
                None if request.uri.ends_with("/pulls/1/reviews/9/events") => {
                    posted_review.store(true, Ordering::SeqCst);
                    Some(review_reply(9, "COMMENTED"))
                }
                None if request.uri.ends_with("/pulls/1/reviews") => {
                    Some(review_reply(9, "PENDING"))
                }
                _ => None,
            }
        });
        let fixture = Fixture {
            respond: Some(respond),
            ..Fixture::default()
        };
        let (_fs, workspace, review, requests, cx) = open_review(fixture, cx).await;
        let rhs = rhs_editor(&review, cx);
        move_to_row_with_text(&rhs, "ENGINE 59", cx);
        comment("c", "fifty-nine", cx);
        comment("shift-c", "the engine", cx);
        comment("; c", "Overall", cx);
        let session = review.read_with(cx, |review, _| review.session().clone());
        let mut outcome = session.update(cx, |session, _| session.wait_for_outcome());
        let threads_loaded = requests_of(&requests, "ReviewThreads");
        let top_block = review.read_with(cx, |review, _| review.top_block().clone());
        let shows_posted_review = |cx: &mut VisualTestContext| {
            top_block.read_with(cx, |top_block, _| {
                top_block.markdown_keys().contains(&"PRR_9".into())
            })
        };
        assert!(!shows_posted_review(cx));

        cx.simulate_keystrokes("; s");
        let modal = modal(&review, cx).expect("the submit modal opens");
        modal.read_with(cx, |modal, _| {
            assert_eq!(modal.event(), ReviewEvent::Comment);
            assert_eq!(
                modal.planned().counts,
                Counts {
                    lines: 1,
                    ranges: 0,
                    files: 1,
                    review: 1,
                }
            );
            assert_eq!(modal.planned().body, "Overall");
            assert!(modal.blockers().is_empty());
            assert!(modal.warnings().is_empty());
            assert_eq!(modal.action(), SubmitAction::Submit);
        });
        // The viewer wrote the pull request: no approving or requesting changes.
        for keys in ["right", "tab", "left"] {
            cx.simulate_keystrokes(keys);
            modal.read_with(cx, |modal, _| {
                assert_eq!(modal.event(), ReviewEvent::Comment)
            });
        }

        cx.simulate_keystrokes("enter");
        assert!(super::review_tests::modal(&review, cx).is_none());
        assert_eq!(
            posted(&requests),
            [
                "POST https://api.github.com/repos/drew-council/code-review-sandbox/pulls/1/reviews"
                    .to_string(),
                "AddFileThread".to_string(),
                "POST https://api.github.com/repos/drew-council/code-review-sandbox/pulls/1/reviews/9/events"
                    .to_string(),
            ]
        );
        let review_post = requests
            .lock()
            .unwrap()
            .iter()
            .find(|request| request.uri.ends_with("/pulls/1/reviews"))
            .map(|request| request.body.clone())
            .unwrap();
        assert_eq!(
            review_post,
            serde_json::json!({
                "commit_id": PR1_HEAD_OID,
                "comments": [{
                    "path": "src/engine.rs",
                    "line": 59,
                    "side": "RIGHT",
                    "body": "fifty-nine",
                }],
            })
        );

        // Submitted, stored as such, and out of the diff and the top block.
        session.read_with(cx, |session, _| {
            for comment in session.comments().iter() {
                assert_eq!(
                    comment.state,
                    CommentState::Submitted {
                        review_id: "9".into()
                    },
                    "{comment:?}"
                );
            }
        });
        let blocks = review.read_with(cx, |review, _| review.comments().clone());
        assert!(
            cx.update(|_, cx| blocks.update(cx, |blocks, cx| blocks.hosted_rows(cx)))
                .is_empty()
        );
        assert!(
            blocks
                .read_with(cx, |blocks, cx| blocks.review_comment_views(cx))
                .is_empty()
        );
        assert!(requests_of(&requests, "ReviewThreads") > threads_loaded);
        assert_eq!(
            outcome.now_or_never(),
            Some(Ok(ReviewOutcome::Submitted {
                url: format!("{PR1_URL}#pullrequestreview-9")
            }))
        );
        outcome = session.update(cx, |session, _| session.wait_for_outcome());
        assert!(notification_count(&workspace, cx) > 0);
        // Once the threads are loaded again, the panel lists them, not the submitted drafts.
        let rows = review.read_with(cx, |review, cx| review.panel_comments(cx));
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|row| row.remote), "{:?}", rows.len());
        // The posted review's summary is in the overview, after the description (which
        // doesn't scroll on its own any more: the overview scrolls with the diff).
        assert!(shows_posted_review(cx));

        // Nothing is left to submit: a comment review then needs a body.
        cx.simulate_keystrokes("; s");
        let modal = super::review_tests::modal(&review, cx).unwrap();
        modal.read_with(cx, |modal, _| {
            assert_eq!(modal.planned().comment_ids, []);
            assert_eq!(modal.action(), SubmitAction::AddReviewComment);
        });
        cx.simulate_keystrokes("escape");
        assert!(super::review_tests::modal(&review, cx).is_none());
        assert_eq!(outcome.now_or_never(), None);
    }

    /// A pending review on GitHub blocks every new review; a pull request that moved on is a
    /// warning; a review without comments needs a review comment, unless it approves.
    #[gpui::test]
    async fn test_what_keeps_a_review_from_being_submitted(cx: &mut TestAppContext) {
        let moved_on_with_pending_review = PR1_HEAD
            .replace(PR1_HEAD_OID, "4444444444444444444444444444444444444444")
            .replace(
                r#""nodes": []"#,
                r#""nodes": [{"id": "PRR_pending", "databaseId": 77, "viewerDidAuthor": true}]"#,
            );
        let head = Arc::new(Mutex::new(moved_on_with_pending_review));
        // Someone else's pull request.
        let details =
            PR1_DETAILS.replace(r#""viewerDidAuthor": true"#, r#""viewerDidAuthor": false"#);
        let respond: Arc<dyn Fn(&SentRequest) -> Option<Reply> + Send + Sync> = Arc::new({
            let head = head.clone();
            move |request| match request.operation() {
                Some("PullRequestHead") => Some(Reply::json(head.lock().unwrap().clone())),
                Some("PullRequestDetails") => Some(Reply::json(details.clone())),
                None if request.method == Method::POST => Some(Reply::status(500, "{}")),
                _ => None,
            }
        });
        let fixture = Fixture {
            respond: Some(respond),
            ..Fixture::default()
        };
        let (_fs, _workspace, review, requests, cx) = open_review(fixture, cx).await;

        cx.simulate_keystrokes("; s");
        let modal = modal(&review, cx).unwrap();
        modal.read_with(cx, |modal, _| {
            let blockers = modal.blockers();
            assert_eq!(blockers.len(), 1, "{blockers:?}");
            assert!(blockers[0].text.contains("pending review"));
            assert_eq!(blockers[0].link.as_deref(), Some(PR1_URL));
            let warnings = modal.warnings();
            assert_eq!(warnings.len(), 1, "{warnings:?}");
            assert!(
                warnings[0]
                    .text
                    .starts_with("The pull request moved on to 4444444 since it was loaded")
            );
            assert_eq!(modal.action(), SubmitAction::Blocked);
        });
        cx.simulate_keystrokes("enter");
        assert!(super::review_tests::modal(&review, cx).is_some());
        assert_eq!(posted(&requests), Vec::<String>::new());
        cx.simulate_keystrokes("escape");

        // Without the pending review: approving needs nothing, commenting a review comment.
        *head.lock().unwrap() = PR1_HEAD.to_string();
        cx.simulate_keystrokes("; s");
        let modal = super::review_tests::modal(&review, cx).unwrap();
        modal.read_with(cx, |modal, _| {
            assert_eq!(modal.action(), SubmitAction::AddReviewComment)
        });
        cx.simulate_keystrokes("right");
        modal.read_with(cx, |modal, _| {
            assert_eq!(modal.event(), ReviewEvent::Approve);
            assert_eq!(modal.action(), SubmitAction::Submit);
        });
        cx.simulate_keystrokes("right");
        modal.read_with(cx, |modal, _| {
            assert_eq!(modal.event(), ReviewEvent::RequestChanges);
            assert_eq!(modal.action(), SubmitAction::AddReviewComment);
        });
        // Enter writes the review comment.
        cx.simulate_keystrokes("enter");
        assert!(super::review_tests::modal(&review, cx).is_none());
        assert!(has_open_comment_editor(&review, cx));
        assert_eq!(posted(&requests), Vec::<String>::new());
        // The modal opens on Request Changes again, with the review comment it asked for.
        cx.simulate_input("Please split this");
        cx.simulate_keystrokes("ctrl-enter");
        cx.simulate_keystrokes("; s");
        let modal = super::review_tests::modal(&review, cx).unwrap();
        modal.read_with(cx, |modal, _| {
            assert_eq!(modal.event(), ReviewEvent::RequestChanges);
            assert_eq!(modal.action(), SubmitAction::Submit);
        });
        cx.simulate_keystrokes("escape");
    }

    /// A review GitHub refuses because of a pending review made since the modal opened shows
    /// the same blocker as one found when it opens, and can't be sent again.
    #[gpui::test]
    async fn test_refused_for_a_pending_review(cx: &mut TestAppContext) {
        let with_pending_review = PR1_HEAD.replace(
            r#""nodes": []"#,
            r#""nodes": [{"id": "PRR_pending", "databaseId": 77, "viewerDidAuthor": true}]"#,
        );
        let head = Arc::new(Mutex::new(PR1_HEAD.to_string()));
        let respond: Arc<dyn Fn(&SentRequest) -> Option<Reply> + Send + Sync> = Arc::new({
            let head = head.clone();
            move |request| match request.operation() {
                Some("PullRequestHead") => Some(Reply::json(head.lock().unwrap().clone())),
                None if request.uri.ends_with("/pulls/1/reviews") => Some(Reply::status(
                    422,
                    r#"{"message":"Unprocessable Entity","errors":["User can only have one pending review per pull request"]}"#,
                )),
                _ => None,
            }
        });
        let fixture = Fixture {
            respond: Some(respond),
            ..Fixture::default()
        };
        let (_fs, _workspace, review, _requests, cx) = open_review(fixture, cx).await;
        let rhs = rhs_editor(&review, cx);
        move_to_row_with_text(&rhs, "ENGINE 59", cx);
        comment("c", "fifty-nine", cx);

        cx.simulate_keystrokes("; s");
        let modal = modal(&review, cx).unwrap();
        modal.read_with(cx, |modal, _| {
            assert_eq!(modal.action(), SubmitAction::Submit)
        });
        *head.lock().unwrap() = with_pending_review;
        cx.simulate_keystrokes("enter");
        modal.read_with(cx, |modal, _| {
            assert_eq!(modal.submit_state(), &SubmitState::Ready);
            let blockers = modal.blockers();
            assert_eq!(blockers.len(), 1, "{blockers:?}");
            assert!(blockers[0].text.starts_with("You have a pending review"));
            assert_eq!(modal.action(), SubmitAction::Blocked);
        });

        // Back from GitHub, where it was discarded: the modal asks again.
        *head.lock().unwrap() = PR1_HEAD.to_string();
        cx.deactivate_window();
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        modal.read_with(cx, |modal, _| {
            assert!(modal.blockers().is_empty());
            assert_eq!(modal.action(), SubmitAction::Submit);
        });
    }

    /// After GitHub rejected the token, the forge (and so the token) is resolved again and the
    /// review tried once more; a refused review keeps the modal open with GitHub's reason.
    #[gpui::test]
    async fn test_refused_review_stays_open(cx: &mut TestAppContext) {
        let posts = Arc::new(Mutex::new(0));
        let respond: Arc<dyn Fn(&SentRequest) -> Option<Reply> + Send + Sync> = Arc::new({
            let posts = posts.clone();
            move |request| match request.operation() {
                Some("PullRequestHead") => Some(Reply::json(PR1_HEAD)),
                None if request.uri.ends_with("/pulls/1/reviews") => {
                    let mut posts = posts.lock().unwrap();
                    *posts += 1;
                    Some(if *posts == 1 {
                        Reply::status(401, r#"{"message":"Bad credentials"}"#)
                    } else {
                        Reply::status(
                            422,
                            r#"{"message":"Unprocessable Entity","errors":["The commitOID is not part of the pull request"]}"#,
                        )
                    })
                }
                _ => None,
            }
        });
        let fixture = Fixture {
            respond: Some(respond),
            ..Fixture::default()
        };
        let (_fs, _workspace, review, requests, cx) = open_review(fixture, cx).await;
        let rhs = rhs_editor(&review, cx);
        move_to_row_with_text(&rhs, "ENGINE 59", cx);
        comment("c", "fifty-nine", cx);
        let resolutions = requests_of(&requests, "RepositoryContext");

        cx.simulate_keystrokes("; s");
        cx.simulate_keystrokes("enter");
        let modal = modal(&review, cx).expect("the modal stays open");
        modal.read_with(cx, |modal, _| {
            assert_eq!(
                modal.submit_state(),
                &SubmitState::Failed(
                    "GitHub doesn't know the reviewed commit 3f22f1b. Reload the review (drafts \
                     are kept)."
                        .into()
                )
            );
            assert_eq!(modal.action(), SubmitAction::Submit);
        });
        assert_eq!(*posts.lock().unwrap(), 2);
        assert_eq!(requests_of(&requests, "RepositoryContext"), resolutions + 1);
        let session = review.read_with(cx, |review, _| review.session().clone());
        session.read_with(cx, |session, _| {
            assert!(
                session
                    .comments()
                    .iter()
                    .all(|comment| comment.state == CommentState::Draft)
            )
        });
    }

    /// Line comments are numbered in the pull request's head: with another commit checked out
    /// they can't be submitted.
    #[gpui::test]
    async fn test_line_comments_need_the_pull_requests_head(cx: &mut TestAppContext) {
        let respond: Arc<dyn Fn(&SentRequest) -> Option<Reply> + Send + Sync> =
            Arc::new(|request| match request.operation() {
                Some("PullRequestHead") => Some(Reply::json(PR1_HEAD)),
                _ => None,
            });
        let fixture = Fixture {
            respond: Some(respond),
            ..Fixture::default()
        };
        let (fs, _workspace, review, _requests, cx) = open_review(fixture, cx).await;
        let rhs = rhs_editor(&review, cx);
        move_to_row_with_text(&rhs, "ENGINE 59", cx);
        comment("c", "fifty-nine", cx);
        fs.with_git_state(dot_git(), true, |state| {
            state.refs.insert("HEAD".into(), C1.into());
        })
        .unwrap();
        cx.run_until_parked();

        cx.simulate_keystrokes("; s");
        let modal = modal(&review, cx).unwrap();
        modal.read_with(cx, |modal, _| {
            let blockers = modal.blockers();
            assert_eq!(blockers.len(), 1, "{blockers:?}");
            assert_eq!(
                blockers[0].text,
                "Check out the pull request's head 3f22f1b to submit line comments: GitHub \
                 numbers their lines in that commit, and 1111111 is checked out."
            );
            assert_eq!(modal.action(), SubmitAction::Blocked);
        });
    }

    /// `; s` in a local review finishes it: its Markdown goes to the clipboard, or to whoever
    /// waits for the review (`zed --review --wait`).
    #[gpui::test]
    async fn test_finish_a_local_review(cx: &mut TestAppContext) {
        init_vim_test(cx, DiffViewStyle::Unified);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);
        let clipboard = |cx: &mut VisualTestContext| {
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .unwrap_or_default()
        };
        move_to_row_with_text(&rhs, "line 3", cx);
        comment("c", "fix this", cx);

        cx.simulate_keystrokes("; s");
        let markdown = review
            .read_with(cx, |review, cx| review.review_markdown(cx))
            .unwrap();
        assert_eq!(clipboard(cx), markdown);
        assert!(markdown.contains("`a.rs:3` - fix this"));

        let session = review.read_with(cx, |review, _| review.session().clone());
        let outcome = session.update(cx, |session, _| session.wait_for_outcome());
        cx.write_to_clipboard(ClipboardItem::new_string("untouched".into()));
        cx.simulate_keystrokes("; s");
        assert_eq!(
            outcome.now_or_never(),
            Some(Ok(ReviewOutcome::Finished {
                markdown: Some(markdown)
            }))
        );
        assert_eq!(clipboard(cx), "untouched");
        // The comments stay drafts.
        session.read_with(cx, |session, _| {
            assert!(
                session
                    .comments()
                    .iter()
                    .all(|comment| comment.state == CommentState::Draft)
            )
        });
    }

    /// The submit belongs to the pull request, not the modal: while it runs, the modal doesn't
    /// close (Escape, a click outside, another modal), and no other tab or window can submit the
    /// drafts again. Once GitHub answers, the drafts are submitted and the modal closes.
    #[gpui::test]
    async fn test_closing_the_modal_while_submitting(cx: &mut TestAppContext) {
        let (answer, gate) = oneshot::channel::<()>();
        let gate = gate.shared();
        let respond: Arc<dyn Fn(&SentRequest) -> Option<Reply> + Send + Sync> =
            Arc::new(move |request| match request.operation() {
                Some("PullRequestHead") => Some(Reply::json(PR1_HEAD)),
                None if request.uri.ends_with("/pulls/1/reviews") => {
                    Some(review_reply(9, "COMMENTED").after(gate.clone()))
                }
                _ => None,
            });
        let fixture = Fixture {
            respond: Some(respond),
            ..Fixture::default()
        };
        let (fs, workspace, review, requests, cx) = open_review(fixture, cx).await;
        let rhs = rhs_editor(&review, cx);
        move_to_row_with_text(&rhs, "ENGINE 59", cx);
        comment("c", "fifty-nine", cx);
        let session = review.read_with(cx, |review, _| review.session().clone());
        let mut outcome = session.update(cx, |session, _| session.wait_for_outcome());
        let pull_request = review.read_with(cx, |review, _| review.pull_request().unwrap().clone());

        cx.simulate_keystrokes("; s");
        cx.simulate_keystrokes("enter");
        let submitting = modal(&review, cx).expect("the modal stays open while submitting");
        submitting.read_with(cx, |modal, _| {
            assert_eq!(modal.submit_state(), &SubmitState::Submitting);
            assert_eq!(modal.action(), SubmitAction::Busy);
        });
        assert!(pull_request.read_with(cx, |pull_request, _| pull_request.is_submitting()));

        cx.simulate_keystrokes("escape");
        assert_eq!(modal(&review, cx), Some(submitting.clone()));
        let hidden =
            workspace.update_in(cx, |workspace, window, cx| workspace.hide_modal(window, cx));
        assert!(
            !hidden,
            "a click outside the modal hides it through the modal layer"
        );
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.toggle_modal(window, cx, |_, cx| OtherModal(cx.focus_handle()))
        });
        assert_eq!(modal(&review, cx), Some(submitting.clone()));
        assert_eq!(outcome.try_recv(), Ok(None));

        // Another window on the same pull request: its modal waits, and its tab can't submit.
        let project = project::Project::test(
            fs.clone(),
            [std::path::Path::new(util::path!("/project"))],
            &mut cx.cx,
        )
        .await;
        let other_window =
            cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project, window, cx));
        let mut other_cx = VisualTestContext::from_window(other_window.into(), cx);
        let other_workspace = other_window
            .read_with(&other_cx, |multi_workspace, _| {
                multi_workspace.workspace().clone()
            })
            .unwrap();
        other_cx.run_until_parked();
        crate::test_support::dispatch(
            crate::OpenPullRequest {
                reference: Some("1".into()),
            },
            &mut other_cx,
        );
        let other_review = crate::test_support::active_review(&other_workspace, &mut other_cx);
        assert_eq!(
            other_review.read_with(&other_cx, |review, _| review.pull_request().cloned()),
            Some(pull_request.clone())
        );
        other_workspace.update_in(&mut other_cx, |workspace, window, cx| {
            SubmitReviewModal::open(
                workspace,
                other_review.clone(),
                pull_request.clone(),
                window,
                cx,
            )
        });
        other_cx.run_until_parked();
        let other_modal = modal(&other_review, &mut other_cx).unwrap();
        other_modal.read_with(&other_cx, |modal, _| {
            assert_eq!(modal.action(), SubmitAction::Busy)
        });
        let refused = pull_request.update(cx, |pull_request, cx| {
            pull_request.submit(
                SubmitRequest {
                    submission: ReviewSubmission {
                        commit_id: PR1_HEAD_OID.parse().unwrap(),
                        event: ReviewEvent::Comment,
                        body: "again".into(),
                        line_comments: Vec::new(),
                        file_comments: Vec::new(),
                    },
                    target: SubmitTarget {
                        session: session.clone(),
                        comment_ids: Vec::new(),
                        repository: WeakEntity::new_invalid(),
                        project: WeakEntity::new_invalid(),
                        workspace: WeakEntity::new_invalid(),
                    },
                },
                cx,
            )
        });
        assert!(
            matches!(refused.now_or_never(), Some(Ok(SubmitReply::Failed(_)))),
            "a second submit runs while the first does"
        );
        assert_eq!(post_count(&requests), 1);

        // GitHub answers.
        answer.send(()).unwrap();
        cx.run_until_parked();
        assert_eq!(modal(&review, cx), None);
        session.read_with(cx, |session, _| {
            assert!(
                session
                    .comments()
                    .iter()
                    .all(|comment| comment.is_submitted())
            )
        });
        assert_eq!(
            outcome.try_recv(),
            Ok(Some(ReviewOutcome::Submitted {
                url: format!("{PR1_URL}#pullrequestreview-9")
            }))
        );
        // The other window's modal has nothing left to submit.
        other_cx.run_until_parked();
        other_modal.read_with(&other_cx, |modal, _| {
            assert_eq!(modal.planned().comment_ids, []);
            assert_eq!(modal.action(), SubmitAction::AddReviewComment);
        });
        assert_eq!(post_count(&requests), 1);
        assert!(notification_count(&workspace, cx) > 0);
    }

    /// A submit that fails without saying whether GitHub made the review (a 5xx here) and whose
    /// lookup fails too isn't offered again: Enter asks GitHub. Finding nothing, the drafts can
    /// be sent again; a review GitHub has from the attempt is the success, without a second post.
    #[gpui::test]
    async fn test_uncertain_submit_is_checked_before_sending_again(cx: &mut TestAppContext) {
        // What `GET …/reviews` answers: an error, no review, then the posted one.
        let listed = Arc::new(Mutex::new(Reply::status(500, "{}")));
        let respond: Arc<dyn Fn(&SentRequest) -> Option<Reply> + Send + Sync> = Arc::new({
            let listed = listed.clone();
            move |request| match request.operation() {
                Some("PullRequestHead") => Some(Reply::json(PR1_HEAD)),
                None if request.method == Method::POST => Some(Reply::status(502, "{}")),
                None if request.uri.contains("/reviews/77/comments") => {
                    Some(Reply::json(r#"[{"body": "fifty-nine"}]"#))
                }
                None if request.uri.contains("/pulls/1/reviews") => {
                    let listed = listed.lock().unwrap();
                    Some(Reply::status(listed.status, listed.body.clone()))
                }
                _ => None,
            }
        });
        let fixture = Fixture {
            respond: Some(respond),
            ..Fixture::default()
        };
        let (_fs, _workspace, review, requests, cx) = open_review(fixture, cx).await;
        let rhs = rhs_editor(&review, cx);
        move_to_row_with_text(&rhs, "ENGINE 59", cx);
        comment("c", "fifty-nine", cx);
        let session = review.read_with(cx, |review, _| review.session().clone());

        cx.simulate_keystrokes("; s");
        cx.simulate_keystrokes("enter");
        let modal = modal(&review, cx).unwrap();
        modal.read_with(cx, |modal, _| {
            assert_eq!(modal.action(), SubmitAction::CheckGitHub);
            let warnings = modal.warnings();
            assert!(
                warnings[0]
                    .text
                    .starts_with("The last submit may have posted the review"),
                "{warnings:?}"
            );
        });
        // The pull request remembers it, for every modal.
        cx.simulate_keystrokes("escape");
        cx.simulate_keystrokes("; s");
        let modal = super::review_tests::modal(&review, cx).unwrap();
        modal.read_with(cx, |modal, _| {
            assert_eq!(modal.action(), SubmitAction::CheckGitHub)
        });
        assert_eq!(post_count(&requests), 1);

        // GitHub has no review from it: nothing was posted, and the drafts can be sent again.
        *listed.lock().unwrap() = Reply::json("[]");
        cx.simulate_keystrokes("enter");
        modal.read_with(cx, |modal, _| {
            assert!(
                matches!(modal.submit_state(), SubmitState::Failed(message) if message.starts_with("GitHub has no review from the last attempt")),
                "{:?}",
                modal.submit_state()
            );
            assert_eq!(modal.action(), SubmitAction::Submit);
        });
        assert_eq!(post_count(&requests), 1);

        // The second post fails the same way, but GitHub made the review this time.
        let submitted_at = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        *listed.lock().unwrap() = Reply::json(
            serde_json::json!([{
                "id": 77,
                "user": { "login": "drew-council" },
                "body": "",
                "state": "COMMENTED",
                "html_url": format!("{PR1_URL}#pullrequestreview-77"),
                "commit_id": PR1_HEAD_OID,
                "submitted_at": submitted_at,
            }])
            .to_string(),
        );
        cx.simulate_keystrokes("enter");
        assert!(super::review_tests::modal(&review, cx).is_none());
        assert_eq!(post_count(&requests), 2);
        session.read_with(cx, |session, _| {
            for comment in session.comments().iter() {
                assert_eq!(
                    comment.state,
                    CommentState::Submitted {
                        review_id: "77".into()
                    }
                );
            }
        });
    }

    /// A submitted comment can't change: an editor of it open in another tab closes without
    /// saving, and the session refuses to change or delete it.
    #[gpui::test]
    async fn test_submitted_comments_cant_change(cx: &mut TestAppContext) {
        let respond: Arc<dyn Fn(&SentRequest) -> Option<Reply> + Send + Sync> =
            Arc::new(|request| match request.operation() {
                Some("PullRequestHead") => Some(Reply::json(PR1_HEAD)),
                None if request.uri.ends_with("/pulls/1/reviews") => {
                    Some(review_reply(9, "COMMENTED"))
                }
                _ => None,
            });
        let fixture = Fixture {
            respond: Some(respond),
            ..Fixture::default()
        };
        let (_fs, workspace, review, _requests, cx) = open_review(fixture, cx).await;
        let rhs = rhs_editor(&review, cx);
        move_to_row_with_text(&rhs, "ENGINE 59", cx);
        comment("c", "fifty-nine", cx);
        let session = review.read_with(cx, |review, _| review.session().clone());

        // A split clone edits the comment, without leaving the editor.
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        let split = workspace.update_in(cx, |workspace, window, cx| {
            workspace.split_and_clone(pane, workspace::SplitDirection::Right, window, cx)
        });
        cx.run_until_parked();
        drop(split);
        let clone = crate::test_support::active_review(&workspace, cx);
        assert_ne!(clone, review);
        let clone_rhs = rhs_editor(&clone, cx);
        move_to_row_with_text(&clone_rhs, "ENGINE 59", cx);
        cx.simulate_keystrokes("i");
        cx.simulate_input("not sent ");
        assert!(has_open_comment_editor(&clone, cx));

        // The original tab submits.
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&review, true, true, window, cx)
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("; s");
        cx.simulate_keystrokes("enter");
        assert!(modal(&review, cx).is_none());
        assert!(!has_open_comment_editor(&clone, cx));
        let id = session.read_with(cx, |session, _| {
            let comment = session.comments().iter().next().unwrap();
            assert_eq!(comment.body, "fifty-nine");
            assert!(comment.is_submitted());
            comment.id
        });
        session.update(cx, |session, cx| {
            session.set_comment_body(id, "changed", cx);
            session.set_comment_type(id, Some(crate::comments::CommentType::Issue), cx);
            session.delete_comment(id, cx);
        });
        session.read_with(cx, |session, _| {
            let comment = session.comments().get(id).unwrap();
            assert_eq!(comment.body, "fifty-nine");
            assert_eq!(comment.comment_type, None);
        });
    }

    /// GitHub rejected the token after the pending review was made, so the rejected token
    /// couldn't delete it either: the forge is resolved again, the pending review deleted with
    /// the new token, and the review posted once more.
    #[gpui::test]
    async fn test_rejected_token_after_the_pending_review(cx: &mut TestAppContext) {
        let events = Arc::new(Mutex::new(0));
        let deletes = Arc::new(Mutex::new(0));
        let respond: Arc<dyn Fn(&SentRequest) -> Option<Reply> + Send + Sync> = Arc::new({
            let events = events.clone();
            let deletes = deletes.clone();
            move |request| {
                let bad_credentials = || Reply::status(401, r#"{"message":"Bad credentials"}"#);
                match request.operation() {
                    Some("PullRequestHead") => Some(Reply::json(PR1_HEAD)),
                    Some("AddFileThread") => Some(Reply::json(
                        r#"{"data":{"addPullRequestReviewThread":{"thread":{"id":"PRRT_new"}}}}"#,
                    )),
                    None if request.uri.ends_with("/pulls/1/reviews/9/events") => {
                        let mut events = events.lock().unwrap();
                        *events += 1;
                        Some(if *events == 1 {
                            bad_credentials()
                        } else {
                            review_reply(9, "COMMENTED")
                        })
                    }
                    None if request.uri.ends_with("/pulls/1/reviews/9")
                        && request.method == Method::DELETE =>
                    {
                        let mut deletes = deletes.lock().unwrap();
                        *deletes += 1;
                        Some(if *deletes == 1 {
                            bad_credentials()
                        } else {
                            Reply::status(204, "")
                        })
                    }
                    None if request.uri.ends_with("/pulls/1/reviews/9") => Some(bad_credentials()),
                    None if request.uri.ends_with("/pulls/1/reviews") => {
                        Some(review_reply(9, "PENDING"))
                    }
                    _ => None,
                }
            }
        });
        let fixture = Fixture {
            respond: Some(respond),
            ..Fixture::default()
        };
        let (_fs, _workspace, review, requests, cx) = open_review(fixture, cx).await;
        comment("shift-c", "the engine", cx);
        let resolutions = requests_of(&requests, "RepositoryContext");

        cx.simulate_keystrokes("; s");
        cx.simulate_keystrokes("enter");
        assert!(modal(&review, cx).is_none(), "submitted with the new token");
        assert_eq!(requests_of(&requests, "RepositoryContext"), resolutions + 1);
        let reviews =
            "https://api.github.com/repos/drew-council/code-review-sandbox/pulls/1/reviews";
        assert_eq!(
            posted(&requests),
            [
                format!("POST {reviews}"),
                "AddFileThread".to_string(),
                format!("POST {reviews}/9/events"),
                format!("DELETE {reviews}/9"),
                format!("DELETE {reviews}/9"),
                format!("POST {reviews}"),
                "AddFileThread".to_string(),
                format!("POST {reviews}/9/events"),
            ]
        );
        let session = review.read_with(cx, |review, _| review.session().clone());
        session.read_with(cx, |session, _| {
            assert!(
                session
                    .comments()
                    .iter()
                    .all(|comment| comment.is_submitted())
            )
        });
    }

    /// `; s` (and the palette's Submit Review) act on the review tab in front only: from another
    /// tab, nothing is finished.
    #[gpui::test]
    async fn test_submit_needs_the_review_tab(cx: &mut TestAppContext) {
        init_vim_test(cx, DiffViewStyle::Unified);
        let (_fs, workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);
        move_to_row_with_text(&rhs, "line 3", cx);
        comment("c", "fix this", cx);
        let session = review.read_with(cx, |review, _| review.session().clone());
        let mut outcome = session.update(cx, |session, _| session.wait_for_outcome());

        let project_path = workspace.read_with(cx, |workspace, cx| {
            workspace
                .project()
                .read(cx)
                .find_project_path(util::path!("/project/a.rs"), cx)
                .unwrap()
        });
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_path(project_path, None, true, window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();
        crate::test_support::dispatch(crate::SubmitReview, cx);
        assert_eq!(outcome.try_recv(), Ok(None));
        assert!(workspace.read_with(cx, |workspace, cx| {
            workspace.active_modal::<SubmitReviewModal>(cx).is_none()
        }));

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&review, true, true, window, cx)
        });
        crate::test_support::dispatch(crate::SubmitReview, cx);
        assert!(matches!(
            outcome.try_recv(),
            Ok(Some(ReviewOutcome::Finished { markdown: Some(_) }))
        ));
    }
}
