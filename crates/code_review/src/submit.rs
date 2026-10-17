//! Delivering a review. A pull request review is posted to GitHub from a modal that first shows
//! what is sent (its drafts, numbered as GitHub numbers lines) and what would make GitHub refuse
//! it; a local review is finished as Markdown.

use crate::{
    comments::{Comment, CommentId, CommentState, CommentTarget, Side, open_web_link},
    forge::{
        self, CommentableLines, DiffSide, GitHubError, HeadStatus, PullRequestStatus, ReviewEvent,
        ReviewFileComment, ReviewLineComment, ReviewSubmission, SubmittedReview,
    },
    pull_request::{LineMapping, PullRequestState, check_comment_target},
    session::{ReviewOutcome, ReviewSession},
    view::ReviewView,
};
use anyhow::Result;
use editor::hover_markdown_style;
use git::{Oid, repository::RepoPath};
use gpui::{
    ClipboardItem, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Task, WeakEntity,
};
use markdown::{Markdown, MarkdownElement};
use std::ops::RangeInclusive;
use ui::{ElevationIndex, KeyBinding, TintColor, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Toast, Workspace, notifications::NotificationId};

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
    /// Line comments wait for the pull request's head to be checked out: GitHub numbers lines
    /// in that commit.
    pub needs_head: bool,
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
            CommentTarget::Lines { path, side, lines } => {
                if !at_head {
                    planned.needs_head = true;
                    continue;
                }
                let Some(commentable) = commentable else {
                    planned.refused.push((location, not_loaded.clone()));
                    continue;
                };
                let lines = match side {
                    Side::Old => lines.clone(),
                    Side::New => match working_to_head(path, lines.clone()) {
                        LineMapping::Lines(lines) => lines,
                        LineMapping::Changed => {
                            planned.refused.push((
                                location,
                                "Its lines have local changes, which the pull request doesn't \
                                 have"
                                    .into(),
                            ));
                            continue;
                        }
                        LineMapping::Unknown => {
                            planned.refused.push((
                                location,
                                "Its file is still being compared with HEAD".into(),
                            ));
                            continue;
                        }
                    },
                };
                let target = CommentTarget::Lines {
                    path: path.clone(),
                    side: *side,
                    lines: lines.clone(),
                };
                if let Err(reason) = check_comment_target(commentable, &target) {
                    planned.refused.push((location, reason));
                    continue;
                }
                let side = match side {
                    Side::Old => DiffSide::Left,
                    Side::New => DiffSide::Right,
                };
                let (start, end) = (*lines.start(), *lines.end());
                planned.line_comments.push(ReviewLineComment {
                    path: commentable.current_path(path.as_unix_str()).to_string(),
                    side,
                    line: end,
                    start: (start != end).then_some((side, start)),
                    body: comment_body(comment),
                });
                planned.counts.lines += 1;
                if start != end {
                    planned.counts.ranges += 1;
                }
                planned.has_old_lines |= side == DiffSide::Left;
                planned.comment_ids.push(comment.id);
            }
        }
    }
    planned.body = review_body(review_comments);
    planned
}

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
        session.update(cx, |session, _| {
            session.resolve_outcome(ReviewOutcome::Finished { markdown })
        });
        "Review sent to the terminal"
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
        Toast::new(NotificationId::unique::<SubmitReviewModal>(), message).autohide(),
        cx,
    );
}

/// The modal of `; s` in a pull request review: Comment, Approve or Request Changes, what the
/// review sends, and what keeps it from being sent.
pub(crate) struct SubmitReviewModal {
    review: WeakEntity<ReviewView>,
    session: Entity<ReviewSession>,
    pull_request: Entity<PullRequestState>,
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
    head_check: HeadCheck,
    submit_state: SubmitState,
    body: Option<Entity<Markdown>>,
    focus_handle: FocusHandle,
    _head_check_task: Task<()>,
    _submit_task: Task<()>,
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
    Submitting,
    Failed(SharedString),
}

/// What Enter does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubmitAction {
    Submit,
    /// The review needs a body: write a review comment first.
    AddReviewComment,
    /// Something keeps the review from being sent.
    Blocked,
    /// Checking the pull request, or submitting.
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
impl ModalView for SubmitReviewModal {}

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
        workspace.toggle_modal(window, cx, |_, cx| Self::new(review, pull_request, cx));
    }

    fn new(
        review: Entity<ReviewView>,
        pull_request: Entity<PullRequestState>,
        cx: &mut Context<Self>,
    ) -> Self {
        let view = review.read(cx);
        let session = view.session().clone();
        let local_head = view.head_oid(cx);
        let local_changes = view.local_changes().cloned();
        let review_base = view.pull_request_base();
        let language_registry = view.language_registry(cx);
        let state = pull_request.read(cx);
        let details = state.details().clone();
        let forge = state.forge().clone();
        let comments = session
            .read(cx)
            .comments()
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        let planned = plan_review(
            &comments,
            state.commentable(),
            local_head == Some(details.head_oid),
            |path, lines| match &local_changes {
                Some(local_changes) => local_changes.read(cx).working_to_head(path, lines, cx),
                None => LineMapping::Unknown,
            },
        );
        let body = (!planned.body.is_empty()).then(|| {
            let body = SharedString::from(planned.body.clone());
            cx.new(|cx| Markdown::new(body, Some(language_registry), None, cx))
        });
        let check = cx
            .background_executor()
            .spawn(forge.pull_request_head(details.number));
        let head_check_task = cx.spawn(async move |this, cx| {
            let head_check = match check.await {
                Ok(status) => HeadCheck::Checked(status),
                Err(error) => {
                    log::error!("checking the pull request before submitting: {error:#}");
                    HeadCheck::Failed(forge::error_message(&error))
                }
            };
            this.update(cx, |this, cx| {
                this.head_check = head_check;
                cx.notify();
            })
            .log_err();
        });
        let canonical = forge.context().canonical.to_string();
        Self {
            review: review.downgrade(),
            session,
            planned,
            event: ReviewEvent::Comment,
            own_pull_request: details.viewer_is_author,
            commit: details.head_oid,
            local_head,
            title: format!("{canonical}#{}: {}", details.number, details.title).into(),
            url: details.url.clone(),
            status: details.state,
            base_moved: review_base.is_some_and(|base| base != details.base_oid),
            head_check: HeadCheck::Checking,
            submit_state: SubmitState::Ready,
            body,
            focus_handle: cx.focus_handle(),
            pull_request,
            _head_check_task: head_check_task,
            _submit_task: Task::ready(()),
        }
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
        if self.planned.needs_head {
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
        match &self.head_check {
            HeadCheck::Checked(status) if status.head_oid != self.commit => {
                warnings.push(Notice::new(format!(
                    "The pull request moved on to {} since it was loaded: the comments are \
                     made on {}, the commit reviewed here.",
                    status.head_oid.display_short(),
                    self.commit.display_short()
                )))
            }
            HeadCheck::Failed(error) => warnings.push(Notice::new(format!(
                "Couldn't check the pull request on GitHub: {error}"
            ))),
            HeadCheck::Checking | HeadCheck::Checked(_) => {}
        }
        match self.status {
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
        if self.submit_state == SubmitState::Submitting || self.head_check == HeadCheck::Checking {
            SubmitAction::Busy
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
            SubmitAction::Submit => self.submit(window, cx),
            SubmitAction::AddReviewComment => self.add_review_comment(window, cx),
            SubmitAction::Blocked | SubmitAction::Busy => {}
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
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

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let submission = ReviewSubmission {
            commit_id: self.commit,
            event: self.event,
            body: self.planned.body.clone(),
            line_comments: self.planned.line_comments.clone(),
            file_comments: self.planned.file_comments.clone(),
        };
        let pull_request = self.pull_request.clone();
        let (forge, number) = {
            let pull_request = pull_request.read(cx);
            (pull_request.forge().clone(), pull_request.details().number)
        };
        let url = self.url.clone();
        let executor = cx.background_executor().clone();
        self.submit_state = SubmitState::Submitting;
        cx.notify();
        self._submit_task = cx.spawn_in(window, async move |this, cx| {
            let mut submitted = executor
                .spawn(forge.submit_review(number, url.clone(), submission.clone()))
                .await;
            // A token GitHub rejected was marked revoked: the forge is resolved again, with
            // whatever token there is now, and the review tried once more (nothing was made).
            let rejected_token = submitted.as_ref().err().is_some_and(|error| {
                matches!(forge::github_error(error), Some(GitHubError::Auth { .. }))
            });
            if rejected_token {
                let resolve =
                    pull_request.update(cx, |pull_request, cx| pull_request.resolve_forge(cx));
                if let Some(resolve) = resolve
                    && let Some(forge) = resolve.await.log_err()
                {
                    pull_request
                        .update(cx, |pull_request, _| pull_request.use_forge(forge.clone()));
                    submitted = executor
                        .spawn(forge.submit_review(number, url, submission))
                        .await;
                }
            }
            this.update_in(cx, |this, window, cx| this.submitted(submitted, window, cx))
                .log_err();
        });
    }

    fn submitted(
        &mut self,
        submitted: Result<SubmittedReview>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let review = match submitted {
            Ok(review) => review,
            Err(error) => {
                log::error!("submitting a review of {}: {error:#}", self.title);
                self.submit_state = SubmitState::Failed(forge::error_message(&error));
                cx.notify();
                return;
            }
        };
        let ids = self.planned.comment_ids.clone();
        let url = review.url.clone();
        self.session.update(cx, |session, cx| {
            session.mark_submitted(&ids, review.id.to_string().into(), cx);
            session.resolve_outcome(ReviewOutcome::Submitted {
                url: url.to_string(),
            });
        });
        // Its threads come back from GitHub, in place of the drafts.
        self.pull_request
            .update(cx, |pull_request, cx| pull_request.reload(cx));
        if let Some(workspace) = Workspace::for_window(window, cx) {
            let toast = Toast::new(
                NotificationId::unique::<SubmitReviewModal>(),
                format!("Review submitted: {url}"),
            )
            .on_click("Copy URL", move |_, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(url.to_string()))
            });
            workspace.update(cx, |workspace, cx| workspace.show_toast(toast, cx));
        }
        cx.emit(DismissEvent);
    }

    fn render_counts(&self) -> impl IntoElement {
        let counts = self.planned.counts;
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
        let text = if parts.is_empty() {
            "No comments".to_string()
        } else {
            parts.join(" · ")
        };
        Label::new(text).size(LabelSize::Small).color(Color::Muted)
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
                    hover_markdown_style(window, cx),
                ))
                .into_any_element(),
            None => Label::new("No review comment")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
        };
        let blockers = self.blockers();
        let warnings = self.warnings();
        let status = match (&self.submit_state, &self.head_check, action) {
            (SubmitState::Submitting, _, _) => Some(
                Label::new("Submitting…")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            ),
            (SubmitState::Failed(error), _, _) => Some(
                Label::new(error.clone())
                    .size(LabelSize::Small)
                    .color(Color::Error),
            ),
            (_, HeadCheck::Checking, _) => Some(
                Label::new("Checking the pull request on GitHub…")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            ),
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
            SubmitAction::Submit | SubmitAction::Blocked | SubmitAction::Busy => {
                format!("Submit: {}", self.event.label())
            }
        };
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
                            .key_binding(KeyBinding::for_action_in(
                                &menu::Cancel,
                                &focus_handle,
                                cx,
                            ))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
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
        CommentTarget::Lines {
            path: RepoPath::new(path).unwrap(),
            side,
            lines,
        }
    }

    fn file(path: &str) -> CommentTarget {
        CommentTarget::File {
            path: RepoPath::new(path).unwrap(),
        }
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
                lines: 5,
                ranges: 2,
                files: 1,
                review: 2,
            }
        );
        assert_eq!(
            planned.comment_ids,
            (0..=7).map(CommentId).collect::<Vec<_>>()
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
            ]
        );
        assert!(!planned.needs_head);
        assert!(planned.has_old_lines);

        // Another commit checked out: line comments wait for the pull request's head, the rest
        // can go.
        let planned = plan_review(&comments, Some(&commentable), false, working_to_head);
        assert!(planned.needs_head);
        assert!(planned.line_comments.is_empty());
        assert_eq!(planned.file_comments.len(), 1);
        assert_eq!(planned.counts.review, 2);

        // GitHub's diff didn't load: nothing on files or lines can be checked.
        let planned = plan_review(&comments, None, true, working_to_head);
        assert!(planned.line_comments.is_empty());
        assert!(planned.file_comments.is_empty());
        assert_eq!(planned.refused.len(), 9);
    }
}

#[cfg(test)]
mod review_tests {
    use super::*;
    use crate::{
        github::test_support::{PR1_DETAILS, PR1_HEAD, PR1_HEAD_OID, Reply, SentRequest},
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
        let respond: Arc<dyn Fn(&SentRequest) -> Option<Reply> + Send + Sync> =
            Arc::new(|request| match request.operation() {
                Some("PullRequestHead") => Some(Reply::json(PR1_HEAD)),
                Some("AddFileThread") => Some(Reply::json(
                    r#"{"data":{"addPullRequestReviewThread":{"thread":{"id":"PRRT_new"}}}}"#,
                )),
                None if request.uri.ends_with("/pulls/1/reviews/9/events") => {
                    Some(review_reply(9, "COMMENTED"))
                }
                None if request.uri.ends_with("/pulls/1/reviews") => {
                    Some(review_reply(9, "PENDING"))
                }
                _ => None,
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
}
