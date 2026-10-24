//! Opening a pull request for review: its details from the forge, checking it out (in place or
//! in a new worktree), and the state the review tabs of one pull request share: its review
//! threads, review summaries and the lines GitHub accepts comments on.

use crate::{
    checkout::{self, CheckoutStep, canonical_source, plan_checkout, run_checkout},
    comments::{CommentId, CommentTarget, Side},
    forge::{
        self, CommentableLines, DiffSide, Forge, ForgeRepoRef, GitHubError, LinesRefusal,
        PullRequestDetails, PullRequestStack, RemoteReviewSummary, RemoteThread, RepositoryContext,
        ReviewEvent, ReviewSubmission, StackDirection, SubmitAttempt, SubmittedReview,
        ThreadAnchor,
    },
    github::{self, GitHubRemote},
    opening::OpeningReview,
    session::{ReviewKey, ReviewOutcome, ReviewSession},
    target::{PullRequestReference, PullRequestTarget, ReviewTarget, validate_branch_name},
    view::ReviewView,
};
use anyhow::{Context as _, Result, anyhow, bail};
use buffer_diff::{BufferDiff, BufferDiffEvent, BufferDiffSnapshot};
use collections::{HashMap, HashSet};
use futures::channel::oneshot;
use git::{
    Oid,
    repository::{CreateWorktreeTarget, RepoPath},
};
use gpui::{
    App, AppContext as _, AsyncApp, AsyncWindowContext, BackgroundExecutor, ClipboardItem, Context,
    Entity, EntityId, EventEmitter, Global, PromptLevel, SharedString, Subscription, Task,
    TaskExt as _, WeakEntity, Window,
};
use language::{Buffer, BufferSnapshot, Point};
use project::{
    Project,
    git_store::Repository,
    project_settings::ProjectSettings,
    trusted_worktrees::{PathTrust, TrustedWorktrees},
};
use settings::Settings as _;
use std::{
    cell::RefCell,
    ops::RangeInclusive,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::Duration,
};
use util::{ResultExt as _, path_list::PathList};
use workspace::{OpenMode, SaveIntent, Toast, Workspace, notifications::NotificationId};

/// What the review tabs of one pull request share: the forge it is on, its details, and what it
/// has on GitHub besides the code (threads, summaries, GitHub's own diff).
pub struct PullRequestState {
    forge: Arc<dyn Forge>,
    details: PullRequestDetails,
    /// The checkouts reviewing the pull request (the latest last), to resolve the forge again
    /// from one that is still open after GitHub rejected its token.
    checkouts: Vec<(WeakEntity<Repository>, WeakEntity<Project>)>,
    threads: Vec<Arc<RemoteThread>>,
    summaries: Vec<RemoteReviewSummary>,
    /// GitHub's diff hunks: comments are only accepted inside them.
    commentable: Option<CommentableLines>,
    /// The stack the pull request is in, once loaded.
    stack: Option<PullRequestStack>,
    filters: ThreadFilters,
    is_loading: bool,
    load_errors: Vec<SharedString>,
    /// GitHub rejected the token: the next load resolves the forge, and so the token, again.
    forge_is_stale: bool,
    /// A review is being posted (or an uncertain one checked): one at a time, from any tab or
    /// window of the pull request, so that two can't post the same drafts.
    submitting: bool,
    /// The last submit may have posted the review: GitHub is asked before the drafts can be
    /// sent again.
    uncertain_submit: Option<UncertainSubmit>,
    /// What the last submit modal was set to send (Comment, Approve, Request Changes), which the
    /// next one starts on, until a review is posted: closing the modal to write the review
    /// comment it asked for mustn't lose it.
    review_event: ReviewEvent,
    /// The threads loaded since the last review was posted: the posted drafts are among them.
    threads_include_submitted: bool,
    _load_task: Task<()>,
    /// Owned here, not by the submit modal: closing the modal (or every tab) mustn't cancel a
    /// review GitHub may already be making.
    _submit_task: Task<()>,
}

/// What a review tab sends to post its review.
pub(crate) struct SubmitRequest {
    pub(crate) submission: ReviewSubmission,
    pub(crate) target: SubmitTarget,
}

/// Where a submit's result goes.
#[derive(Clone)]
pub(crate) struct SubmitTarget {
    /// The session whose drafts are posted, marked submitted once they are.
    pub(crate) session: Entity<ReviewSession>,
    pub(crate) comment_ids: Vec<CommentId>,
    /// The submitting tab's checkout: after GitHub rejected the token, the forge is resolved
    /// again from it first (tokens are per project).
    pub(crate) repository: WeakEntity<Repository>,
    pub(crate) project: WeakEntity<Project>,
    /// Where the success toast shows.
    pub(crate) workspace: WeakEntity<Workspace>,
}

/// How a submit (or the check of an uncertain one) ended, for the modal that started it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SubmitReply {
    Submitted {
        url: SharedString,
    },
    /// Nothing was posted: the drafts can be sent again.
    Failed(SharedString),
    /// The review may have been posted: [`PullRequestState::check_uncertain_submit`] finds out.
    Uncertain(SharedString),
}

/// A submit that may have posted the review.
struct UncertainSubmit {
    attempt: SubmitAttempt,
    message: SharedString,
    target: SubmitTarget,
}

/// The toast saying a review was posted.
pub(crate) struct ReviewSubmittedToast;

/// Where a pull request is in its stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StackPosition {
    /// The stack's number.
    pub(crate) stack: u64,
    /// From the bottom, 0-based.
    pub(crate) position: usize,
    pub(crate) count: usize,
    /// The numbers of the nearest open pull requests below and above it.
    pub(crate) below: Option<u64>,
    pub(crate) above: Option<u64>,
}

/// Which review threads and review summaries of a pull request show, for all its tabs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ThreadFilters {
    pub show_resolved: bool,
    pub show_outdated: bool,
    /// Threads and review summaries written by bots.
    pub show_bots: bool,
}

/// How many threads (and bot summaries) each filter hides or shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ThreadCounts {
    pub resolved: usize,
    pub outdated: usize,
    /// Bot threads and bot review summaries.
    pub bots: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PullRequestEvent {
    /// The details, threads, summaries or commentable lines were (re)loaded.
    Loaded,
    FiltersChanged,
}

impl EventEmitter<PullRequestEvent> for PullRequestState {}

/// The pull request of each open pull request review, by review key, so that split tabs and a
/// tab reopened while another one is open share one.
#[derive(Default)]
struct PullRequests(HashMap<ReviewKey, WeakEntity<PullRequestState>>);

impl Global for PullRequests {}

impl PullRequestState {
    pub(crate) fn for_key(key: &ReviewKey, cx: &mut App) -> Option<Entity<Self>> {
        let pull_requests = cx.default_global::<PullRequests>();
        pull_requests
            .0
            .retain(|_, pull_request| pull_request.upgrade().is_some());
        pull_requests.0.get(key).and_then(WeakEntity::upgrade)
    }

    /// Records what was just loaded for the pull request under `key`, updating the tabs that
    /// review it, and loads its threads, summaries and commentable lines.
    pub(crate) fn set(
        key: ReviewKey,
        forge: Arc<dyn Forge>,
        details: PullRequestDetails,
        repository: &Entity<Repository>,
        project: &Entity<Project>,
        cx: &mut App,
    ) -> Entity<Self> {
        if let Some(pull_request) = Self::for_key(&key, cx) {
            pull_request.update(cx, |pull_request, cx| {
                pull_request.forge = forge;
                pull_request.details = details;
                pull_request.add_checkout(repository, project);
                pull_request.forge_is_stale = false;
                pull_request.load(false, cx);
            });
            return pull_request;
        }
        let pull_request = cx.new(|cx| {
            let mut pull_request = Self {
                forge,
                details,
                checkouts: vec![(repository.downgrade(), project.downgrade())],
                threads: Vec::new(),
                summaries: Vec::new(),
                commentable: None,
                stack: None,
                filters: ThreadFilters::default(),
                is_loading: false,
                load_errors: Vec::new(),
                forge_is_stale: false,
                submitting: false,
                uncertain_submit: None,
                review_event: ReviewEvent::Comment,
                threads_include_submitted: false,
                _load_task: Task::ready(()),
                _submit_task: Task::ready(()),
            };
            pull_request.load(false, cx);
            pull_request
        });
        cx.default_global::<PullRequests>()
            .0
            .insert(key, pull_request.downgrade());
        pull_request
    }

    pub fn details(&self) -> &PullRequestDetails {
        &self.details
    }

    /// Records a checkout that reviews the pull request (a tab's), which the forge can be
    /// resolved from again.
    pub(crate) fn add_checkout(
        &mut self,
        repository: &Entity<Repository>,
        project: &Entity<Project>,
    ) {
        self.checkouts.retain(|(known, _)| {
            known.upgrade().is_some() && known.entity_id() != repository.entity_id()
        });
        self.checkouts
            .push((repository.downgrade(), project.downgrade()));
    }

    pub fn forge(&self) -> &Arc<dyn Forge> {
        &self.forge
    }

    pub fn threads(&self) -> &[Arc<RemoteThread>] {
        &self.threads
    }

    pub fn stack(&self) -> Option<&PullRequestStack> {
        self.stack.as_ref()
    }

    /// Where the pull request is in its stack, once that is loaded.
    pub(crate) fn stack_position(&self) -> Option<StackPosition> {
        let stack = self.stack.as_ref()?;
        let number = self.details.number;
        let neighbor = |direction| {
            stack
                .neighbor(number, direction)
                .map(|neighbor| neighbor.number)
        };
        Some(StackPosition {
            stack: stack.number,
            position: stack.position(number)?,
            count: stack.pull_requests.len(),
            below: neighbor(StackDirection::Down),
            above: neighbor(StackDirection::Up),
        })
    }

    pub(crate) fn commentable(&self) -> Option<&CommentableLines> {
        self.commentable.as_ref()
    }

    pub fn filters(&self) -> ThreadFilters {
        self.filters
    }

    pub fn is_loading(&self) -> bool {
        self.is_loading
    }

    /// Why the last load failed, one message per distinct failure.
    pub fn load_errors(&self) -> &[SharedString] {
        &self.load_errors
    }

    /// The review summaries the filters show: bots' only while bots are shown.
    pub fn visible_summaries(&self) -> impl Iterator<Item = &RemoteReviewSummary> {
        let show_bots = self.filters.show_bots;
        self.summaries
            .iter()
            .filter(move |summary| show_bots || !summary.author.is_bot)
    }

    pub fn hidden_bot_summaries(&self) -> usize {
        if self.filters.show_bots {
            return 0;
        }
        self.summaries
            .iter()
            .filter(|summary| summary.author.is_bot)
            .count()
    }

    pub fn thread_counts(&self) -> ThreadCounts {
        ThreadCounts {
            resolved: self
                .threads
                .iter()
                .filter(|thread| thread.is_resolved)
                .count(),
            outdated: self
                .threads
                .iter()
                .filter(|thread| thread.is_outdated)
                .count(),
            bots: self
                .threads
                .iter()
                .filter(|thread| thread.is_by_bot())
                .count()
                + self
                    .summaries
                    .iter()
                    .filter(|summary| summary.author.is_bot)
                    .count(),
        }
    }

    pub fn set_filters(&mut self, filters: ThreadFilters, cx: &mut Context<Self>) {
        if self.filters != filters {
            self.filters = filters;
            cx.emit(PullRequestEvent::FiltersChanged);
            cx.notify();
        }
    }

    pub(crate) fn review_event(&self) -> ReviewEvent {
        self.review_event
    }

    pub(crate) fn set_review_event(&mut self, event: ReviewEvent) {
        self.review_event = event;
    }

    /// Whether the threads shown include those of the drafts marked submitted: then the
    /// drafts needn't be listed next to them.
    pub(crate) fn threads_include_submitted(&self) -> bool {
        self.threads_include_submitted
    }

    /// Loads the details, threads, summaries and GitHub's diff again (on request, after a
    /// submit, and when HEAD moves).
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.load(true, cx);
    }

    /// Whether GitHub would take a comment on `target`, made in a checkout at `local_head` whose
    /// files differ from that commit as `local` says: only on files of the pull request, and on
    /// lines inside GitHub's diff hunks, which are numbered in the pull request's head commit.
    /// So line comments need that commit checked out, and lines the checkout didn't change.
    pub(crate) fn check_comment(
        &self,
        target: &CommentTarget,
        local_head: Option<Oid>,
        local: &LocalChanges,
        cx: &App,
    ) -> Result<(), SharedString> {
        if *target == CommentTarget::Review {
            return Ok(());
        }
        let Some(commentable) = &self.commentable else {
            return Err(if self.is_loading {
                "Loading the pull request's diff from GitHub…".into()
            } else {
                "The pull request's diff couldn't be loaded from GitHub: reload the pull request \
                 to comment on its files and lines"
                    .into()
            });
        };
        let CommentTarget::Lines { path, side, lines } = target else {
            return check_comment_target(commentable, target);
        };
        if local_head != Some(self.details.head_oid) {
            return Err(format!(
                "Check out the pull request's head {} to comment on its lines: GitHub numbers \
                 them in that commit",
                self.details.head_oid.display_short()
            )
            .into());
        }
        let lines = match side {
            Side::Old => lines.clone(),
            Side::New => {
                match local.working_to_head(path, lines.clone(), self.details.head_oid, cx) {
                    LineMapping::Lines(lines) => lines,
                    LineMapping::Changed => {
                        return Err(
                            "These lines have local changes, which the pull request doesn't have"
                                .into(),
                        );
                    }
                    LineMapping::Unknown => {
                        return Err("Comparing the file with HEAD… Try again in a moment.".into());
                    }
                    LineMapping::Failed => {
                        return Err(
                        "Couldn't compare the file with HEAD, so its lines can't be numbered as \
                         GitHub numbers them"
                            .into(),
                    );
                    }
                }
            }
        };
        check_comment_target(
            commentable,
            &CommentTarget::Lines {
                path: path.clone(),
                side: *side,
                lines,
            },
        )
    }

    fn load(&mut self, with_details: bool, cx: &mut Context<Self>) {
        let number = self.details.number;
        let mut forge = self.forge.clone();
        let resolve = if self.forge_is_stale {
            self.resolve_forge(cx)
        } else {
            None
        };
        let executor = cx.background_executor().clone();
        self.is_loading = true;
        cx.notify();
        self._load_task = cx.spawn(async move |this, cx| {
            let mut resolved = false;
            if let Some(resolve) = resolve {
                match resolve.await {
                    Ok(new_forge) => {
                        forge = new_forge;
                        resolved = true;
                    }
                    Err(error) => {
                        this.update(cx, |this, cx| this.fail(&error, cx)).log_err();
                        return;
                    }
                }
            }
            let mut loaded = fetch(&forge, number, with_details, &executor).await;
            // A token GitHub rejected was marked revoked: the forge is resolved again, with
            // whatever token there is now, and the load tried once more.
            if !resolved && loaded.auth_failed() {
                let resolve = this
                    .update(cx, |this, cx| this.resolve_forge(cx))
                    .ok()
                    .flatten();
                if let Some(resolve) = resolve
                    && let Some(new_forge) = resolve.await.log_err()
                {
                    forge = new_forge;
                    loaded = fetch(&forge, number, with_details, &executor).await;
                }
            }
            this.update(cx, |this, cx| this.apply(forge, loaded, cx))
                .log_err();
        });
    }

    /// Uses a forge resolved again (after GitHub rejected the token of the one before).
    pub(crate) fn use_forge(&mut self, forge: Arc<dyn Forge>) {
        self.forge = forge;
        self.forge_is_stale = false;
    }

    /// Resolves the forge again from a checkout reviewing the pull request, which looks its
    /// token up again; `None` when none is open.
    pub(crate) fn resolve_forge(&self, cx: &mut App) -> Option<Task<Result<Arc<dyn Forge>>>> {
        self.resolve_forge_from(None, cx)
    }

    /// [`Self::resolve_forge`], from `preferred` while it is open (the checkout of the tab that
    /// asks: its project may have its own token), else from the latest checkout still open.
    fn resolve_forge_from(
        &self,
        preferred: Option<(WeakEntity<Repository>, WeakEntity<Project>)>,
        cx: &mut App,
    ) -> Option<Task<Result<Arc<dyn Forge>>>> {
        let (repository, project) = preferred
            .into_iter()
            .chain(self.checkouts.iter().rev().cloned())
            .find_map(|(repository, project)| Some((repository.upgrade()?, project.upgrade()?)))?;
        let canonical = &self.forge.context().canonical;
        let reference = ForgeRepoRef {
            host: Some(canonical.host.clone()),
            owner: canonical.owner.clone(),
            name: canonical.name.clone(),
        };
        Some(forge::resolve_forge(
            &repository,
            Some(reference),
            &project,
            cx,
        ))
    }

    /// Whether a review of the pull request is being posted (or an uncertain one checked).
    pub(crate) fn is_submitting(&self) -> bool {
        self.submitting
    }

    /// Why the last submit may have posted the review, while GitHub hasn't been asked.
    pub(crate) fn uncertain_submit(&self) -> Option<&SharedString> {
        self.uncertain_submit
            .as_ref()
            .map(|uncertain| &uncertain.message)
    }

    /// Posts a review of the pull request. The work belongs to the pull request, shared by its
    /// tabs, so it finishes (marking the drafts submitted, telling `--wait` waiters, the toast)
    /// whatever happens to the modal or the tab that asked. Refused while another submit runs
    /// or the last one may have posted the review.
    pub(crate) fn submit(
        &mut self,
        request: SubmitRequest,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<SubmitReply> {
        let (reply, receiver) = oneshot::channel();
        if self.submitting || self.uncertain_submit.is_some() {
            reply
                .send(SubmitReply::Failed(
                    "Another review of this pull request is being submitted".into(),
                ))
                .ok();
            return receiver;
        }
        let number = self.details.number;
        let url = self.details.url.clone();
        let forge = self.forge.clone();
        let executor = cx.background_executor().clone();
        // Held until the review is posted, even when every tab of the pull request closes
        // meanwhile: its drafts must be marked submitted.
        let pull_request = cx.entity();
        self.submitting = true;
        cx.notify();
        self._submit_task = cx.spawn(async move |_, cx| {
            let SubmitRequest { submission, target } = request;
            let mut result = executor
                .spawn(forge.submit_review(number, url.clone(), submission.clone()))
                .await;
            if let Err(error) = result {
                result = retry_with_new_token(
                    &pull_request,
                    &target,
                    number,
                    url,
                    submission,
                    error,
                    cx,
                )
                .await;
            }
            let answer =
                pull_request.update(cx, |this, cx| this.submit_finished(result, target, cx));
            reply.send(answer).ok();
        });
        receiver
    }

    fn submit_finished(
        &mut self,
        result: Result<SubmittedReview>,
        target: SubmitTarget,
        cx: &mut Context<Self>,
    ) -> SubmitReply {
        self.submitting = false;
        cx.notify();
        let error = match result {
            Ok(review) => return self.posted(review, &target, cx),
            Err(error) => error,
        };
        log::error!(
            "submitting a review of pull request #{}: {error:#}",
            self.details.number
        );
        let message = forge::error_message(&error);
        match forge::github_error(&error) {
            Some(GitHubError::ReviewUncertain { attempt, .. }) => {
                self.uncertain_submit = Some(UncertainSubmit {
                    attempt: (**attempt).clone(),
                    message: message.clone(),
                    target,
                });
                SubmitReply::Uncertain(message)
            }
            _ => SubmitReply::Failed(message),
        }
    }

    /// Asks GitHub whether the last submit, which failed without saying, posted the review:
    /// then it is a success; else nothing was posted and the drafts can be sent again. `None`
    /// when there is nothing to check, or a submit runs.
    pub(crate) fn check_uncertain_submit(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<oneshot::Receiver<SubmitReply>> {
        if self.submitting {
            return None;
        }
        let uncertain = self.uncertain_submit.take()?;
        let (reply, receiver) = oneshot::channel();
        let number = self.details.number;
        let forge = self.forge.clone();
        let executor = cx.background_executor().clone();
        let pull_request = cx.entity();
        self.submitting = true;
        cx.notify();
        self._submit_task = cx.spawn(async move |_, cx| {
            let found = executor
                .spawn(forge.find_submitted_review(number, uncertain.attempt.clone()))
                .await;
            let answer = pull_request.update(cx, |this, cx| {
                this.submitting = false;
                cx.notify();
                match found {
                    Ok(Some(review)) => this.posted(review, &uncertain.target, cx),
                    Ok(None) => SubmitReply::Failed(
                        "GitHub has no review from the last attempt, so nothing was posted: \
                         submit again."
                            .into(),
                    ),
                    Err(error) => {
                        log::error!("checking whether a review was posted: {error:#}");
                        let message: SharedString = format!(
                            "Couldn't ask GitHub whether the last review was posted: {}",
                            forge::error_message(&error)
                        )
                        .into();
                        this.uncertain_submit = Some(UncertainSubmit {
                            message: message.clone(),
                            ..uncertain
                        });
                        SubmitReply::Uncertain(message)
                    }
                }
            });
            reply.send(answer).ok();
        });
        Some(receiver)
    }

    /// The review was posted: its drafts are submitted, whoever waits for the review hears of
    /// it, its threads come back from GitHub (in place of the drafts), and a toast says so.
    fn posted(
        &mut self,
        review: SubmittedReview,
        target: &SubmitTarget,
        cx: &mut Context<Self>,
    ) -> SubmitReply {
        let url = review.url.clone();
        target.session.update(cx, |session, cx| {
            session.mark_submitted(&target.comment_ids, review.id.to_string().into(), cx);
            session.resolve_outcome(ReviewOutcome::Submitted {
                url: url.to_string(),
            });
        });
        self.review_event = ReviewEvent::Comment;
        self.threads_include_submitted = false;
        self.reload(cx);
        let toast = Toast::new(
            NotificationId::unique::<ReviewSubmittedToast>(),
            format!("Review submitted: {url}"),
        )
        .on_click("Copy URL", {
            let url = url.clone();
            move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(url.to_string()))
        });
        match target.workspace.upgrade() {
            Some(workspace) => {
                workspace.update(cx, |workspace, cx| workspace.show_toast(toast, cx))
            }
            None => log::info!("review submitted: {url}"),
        }
        SubmitReply::Submitted { url }
    }

    fn fail(&mut self, error: &anyhow::Error, cx: &mut Context<Self>) {
        log::error!("loading pull request #{}: {error:#}", self.details.number);
        self.is_loading = false;
        self.load_errors = vec![forge::error_message(error)];
        cx.emit(PullRequestEvent::Loaded);
        cx.notify();
    }

    /// What failed keeps what was loaded before, next to the error.
    fn apply(&mut self, forge: Arc<dyn Forge>, loaded: Loaded, cx: &mut Context<Self>) {
        let number = self.details.number;
        let mut errors = Vec::<SharedString>::new();
        let mut auth_failed = false;
        let mut record = |error: anyhow::Error| {
            log::error!("loading pull request #{number}: {error:#}");
            auth_failed |= matches!(forge::github_error(&error), Some(GitHubError::Auth { .. }));
            let message = forge::error_message(&error);
            if !errors.contains(&message) {
                errors.push(message);
            }
        };
        if let Some(details) = loaded.details {
            match details {
                Ok(details) => self.details = details,
                Err(error) => record(error),
            }
        }
        match loaded.threads {
            Ok(threads) => {
                self.threads = threads.into_iter().map(Arc::new).collect();
                self.threads_include_submitted = true;
            }
            Err(error) => record(error),
        }
        match loaded.summaries {
            Ok(summaries) => self.summaries = summaries,
            Err(error) => record(error),
        }
        match loaded.files {
            Ok(files) => self.commentable = Some(files),
            Err(error) => record(error),
        }
        // Not a load error: a GitHub without stacks (GitHub Enterprise Server) fails it every
        // time, and the review works without it.
        match loaded.stack {
            Ok(stack) => self.stack = stack,
            Err(error) => log::warn!("loading the stack of pull request #{number}: {error:#}"),
        }
        self.forge = forge;
        self.forge_is_stale = auth_failed;
        self.load_errors = errors;
        self.is_loading = false;
        cx.emit(PullRequestEvent::Loaded);
        cx.notify();
    }
}

/// After GitHub rejected the token of a submit (`error`; the token was marked revoked): the
/// forge is resolved again, which looks the token up again, from the submitting tab's checkout
/// first, and the review is tried once more. A pending review the rejected token couldn't
/// delete is deleted with the new one first. Any other error is returned as it is.
async fn retry_with_new_token(
    pull_request: &Entity<PullRequestState>,
    target: &SubmitTarget,
    number: u64,
    url: SharedString,
    submission: ReviewSubmission,
    error: anyhow::Error,
    cx: &mut AsyncApp,
) -> Result<SubmittedReview> {
    let executor = cx.background_executor().clone();
    let left_behind = match forge::github_error(&error) {
        Some(GitHubError::Auth { .. }) => None,
        Some(GitHubError::PendingReviewLeft {
            cause,
            review_id: Some(id),
            ..
        }) if matches!(**cause, GitHubError::Auth { .. }) => Some(*id),
        _ => return Err(error),
    };
    let preferred = (target.repository.clone(), target.project.clone());
    let Some(resolve) =
        pull_request.update(cx, |this, cx| this.resolve_forge_from(Some(preferred), cx))
    else {
        return Err(error);
    };
    let forge = match resolve.await {
        Ok(forge) => forge,
        Err(resolve_error) => {
            log::error!("resolving the forge again after a rejected token: {resolve_error:#}");
            return Err(error);
        }
    };
    pull_request.update(cx, |this, _| this.use_forge(forge.clone()));
    if let Some(id) = left_behind
        && let Err(discard_error) = executor
            .spawn(forge.discard_pending_review(number, id))
            .await
    {
        log::error!("deleting the pending review {id} with a new token: {discard_error:#}");
        return Err(error);
    }
    executor
        .spawn(forge.submit_review(number, url, submission))
        .await
}

/// How a review tab's working tree differs from its HEAD commit, file by file. GitHub numbers a
/// pull request's lines in its head commit, while the review shows the working tree with its
/// uncommitted and unsaved changes: this translates between the two.
pub(crate) struct LocalChanges {
    project: WeakEntity<Project>,
    files: HashMap<RepoPath, LocalFile>,
    /// HEAD's text of some files, read from git: after HEAD moves, Zed reloads the diffs' base
    /// texts some time later, so until a file's base text is this one its new lines can't be
    /// numbered in that commit.
    head_texts: Option<HeadTexts>,
    /// The hash of each file's diff base text, by that text's version: the gutter checks the
    /// cursor's line on every frame, and hashing a whole file each time is too slow.
    base_text_hashes: RefCell<HashMap<RepoPath, (language::BufferId, clock::Global, Option<u64>)>>,
}

struct HeadTexts {
    head: Oid,
    /// A hash of each file's text in `head`; `None`: the commit doesn't have the file.
    hashes: HashMap<RepoPath, Option<u64>>,
    reading: HashSet<RepoPath>,
    _tasks: Vec<Task<()>>,
}

struct LocalFile {
    buffer: Entity<Buffer>,
    state: LocalFileState,
}

enum LocalFileState {
    Loading {
        _task: Task<()>,
    },
    /// The file's diff against HEAD, recalculated as the buffer changes.
    Loaded {
        diff: Entity<BufferDiff>,
        _subscription: Subscription,
    },
    /// The file couldn't be compared. Threads count its lines as HEAD's; comments can't be
    /// numbered.
    Failed,
}

/// Lines of one numbering (HEAD's or the working tree's) in the other.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LineMapping {
    Lines(RangeInclusive<u32>),
    /// Some of the lines changed in the working tree (or the file isn't in HEAD).
    Changed,
    /// The file hasn't been compared with HEAD yet.
    Unknown,
    /// The file couldn't be compared with HEAD.
    Failed,
}

/// A file was compared with HEAD, or its comparison changed.
pub(crate) struct LocalChangesEvent;

impl EventEmitter<LocalChangesEvent> for LocalChanges {}

impl LocalChanges {
    pub(crate) fn new(project: &Entity<Project>) -> Self {
        Self {
            project: project.downgrade(),
            files: HashMap::default(),
            head_texts: None,
            base_text_hashes: RefCell::default(),
        }
    }

    /// Reads the text of `paths` in `head` from git (once per commit and file), which the
    /// files' comparisons must be based on before their new lines map to that commit.
    pub(crate) fn read_head_texts(
        &mut self,
        repository: &Entity<Repository>,
        head: Oid,
        paths: Vec<RepoPath>,
        cx: &mut Context<Self>,
    ) {
        if self
            .head_texts
            .as_ref()
            .is_none_or(|texts| texts.head != head)
        {
            self.head_texts = Some(HeadTexts {
                head,
                hashes: HashMap::default(),
                reading: HashSet::default(),
                _tasks: Vec::new(),
            });
        }
        let Some(texts) = self.head_texts.as_mut() else {
            return;
        };
        let paths = paths
            .into_iter()
            .filter(|path| !texts.hashes.contains_key(path) && !texts.reading.contains(path))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        if paths.is_empty() {
            return;
        }
        texts.reading.extend(paths.iter().cloned());
        let revisions = paths
            .iter()
            .map(|path| format!("{head}:{}", path.as_unix_str()))
            .collect::<Vec<_>>();
        let read = crate::commits::local_job(
            repository,
            "code review: read HEAD",
            move |backend| async move { backend.load_revisions(revisions).await },
            cx,
        );
        let task = cx.spawn(async move |this, cx| {
            let read = read.await;
            this.update(cx, |this, cx| {
                let Some(texts) = this.head_texts.as_mut().filter(|texts| texts.head == head)
                else {
                    return;
                };
                for path in &paths {
                    texts.reading.remove(path);
                }
                match read {
                    Ok(blobs) => {
                        for (path, blob) in paths.into_iter().zip(blobs) {
                            let hash =
                                blob.map(|bytes| text_hash(&String::from_utf8_lossy(&bytes)));
                            texts.hashes.insert(path, hash);
                        }
                    }
                    // Unchecked, the comparisons are taken as they are.
                    Err(error) => log::error!("reading files at {head}: {error:#}"),
                }
                cx.emit(LocalChangesEvent);
            })
            .log_err();
        });
        texts._tasks.push(task);
    }

    /// Whether HEAD's texts are still being read from git.
    pub(crate) fn is_reading_head_texts(&self) -> bool {
        self.head_texts
            .as_ref()
            .is_some_and(|texts| !texts.reading.is_empty())
    }

    /// Whether the file's comparison is based on its text in `head`, as far as known.
    fn is_based_on(&self, path: &RepoPath, head: Oid, diff: &BufferDiffSnapshot) -> bool {
        let Some(texts) = self.head_texts.as_ref().filter(|texts| texts.head == head) else {
            return true;
        };
        if texts.reading.contains(path) {
            return false;
        }
        let Some(expected) = texts.hashes.get(path) else {
            return true;
        };
        let base_text = diff.base_text();
        let key = (base_text.remote_id(), base_text.version().clone());
        let mut hashes = self.base_text_hashes.borrow_mut();
        let base = match hashes.get(path) {
            Some((buffer_id, version, hash)) if (buffer_id, version) == (&key.0, &key.1) => *hash,
            _ => {
                let hash = diff
                    .base_text_exists()
                    .then(|| text_hash(&base_text.text()));
                hashes.insert(path.clone(), (key.0, key.1, hash));
                hash
            }
        };
        base == *expected
    }

    /// Compares the review's files (with the buffers showing them) with HEAD, each buffer once,
    /// and forgets the files the review no longer has.
    pub(crate) fn sync(&mut self, files: Vec<(RepoPath, Entity<Buffer>)>, cx: &mut Context<Self>) {
        let paths = files
            .iter()
            .map(|(path, _)| path.clone())
            .collect::<HashSet<_>>();
        self.files.retain(|path, _| paths.contains(path));
        let Some(project) = self.project.upgrade() else {
            return;
        };
        for (path, buffer) in files {
            if self
                .files
                .get(&path)
                .is_some_and(|file| file.buffer == buffer)
            {
                continue;
            }
            let open = project.update(cx, |project, cx| {
                project.open_uncommitted_diff(buffer.clone(), cx)
            });
            let task = cx.spawn({
                let path = path.clone();
                async move |this, cx| {
                    let diff = open.await;
                    this.update(cx, |this, cx| this.compared(path, diff, cx))
                        .log_err();
                }
            });
            self.files.insert(
                path,
                LocalFile {
                    buffer,
                    state: LocalFileState::Loading { _task: task },
                },
            );
        }
    }

    fn compared(
        &mut self,
        path: RepoPath,
        diff: Result<Entity<BufferDiff>>,
        cx: &mut Context<Self>,
    ) {
        let Some(file) = self.files.get_mut(&path) else {
            return;
        };
        file.state = match diff {
            Ok(diff) => {
                let subscription = cx.subscribe(&diff, |_, _, _: &BufferDiffEvent, cx| {
                    cx.emit(LocalChangesEvent)
                });
                LocalFileState::Loaded {
                    diff,
                    _subscription: subscription,
                }
            }
            Err(error) => {
                log::error!("comparing {} with HEAD: {error:#}", path.as_unix_str());
                LocalFileState::Failed
            }
        };
        cx.emit(LocalChangesEvent);
    }

    /// Where HEAD's `lines` (1-based) of the file are in the working tree, to show threads. A
    /// file that couldn't be compared counts as unchanged.
    pub(crate) fn head_to_working(
        &self,
        path: &RepoPath,
        lines: RangeInclusive<u32>,
        cx: &App,
    ) -> LineMapping {
        let Some(file) = self.files.get(path) else {
            return LineMapping::Unknown;
        };
        match &file.state {
            LocalFileState::Loading { .. } => LineMapping::Unknown,
            LocalFileState::Failed => LineMapping::Lines(lines),
            LocalFileState::Loaded { diff, .. } => {
                let diff = diff.read(cx).snapshot(cx);
                let buffer = file.buffer.read(cx).snapshot();
                head_lines_in_working_tree(&diff, &buffer, lines)
            }
        }
    }

    /// Where the working tree's `lines` (1-based) of the file are in `head`, the checked-out
    /// commit, to number comments.
    pub(crate) fn working_to_head(
        &self,
        path: &RepoPath,
        lines: RangeInclusive<u32>,
        head: Oid,
        cx: &App,
    ) -> LineMapping {
        let Some(file) = self.files.get(path) else {
            return LineMapping::Unknown;
        };
        match &file.state {
            LocalFileState::Loading { .. } => LineMapping::Unknown,
            LocalFileState::Failed => LineMapping::Failed,
            LocalFileState::Loaded { diff, .. } => {
                let diff = diff.read(cx).snapshot(cx);
                if !self.is_based_on(path, head, &diff) {
                    return LineMapping::Unknown;
                }
                let buffer = file.buffer.read(cx).snapshot();
                working_lines_in_head(&diff, &buffer, lines)
            }
        }
    }
}

/// Compares texts the way git and Zed may store them differently (line endings, a BOM).
fn text_hash(text: &str) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.trim_start_matches('\u{feff}')
        .chars()
        .filter(|character| *character != '\r')
        .for_each(|character| character.hash(&mut hasher));
    hasher.finish()
}

/// Where HEAD's `lines` (the diff's base text, 1-based) are in the buffer, unless the buffer
/// changed any of them.
fn head_lines_in_working_tree(
    diff: &BufferDiffSnapshot,
    buffer: &BufferSnapshot,
    lines: RangeInclusive<u32>,
) -> LineMapping {
    if !diff.base_text_exists() {
        return LineMapping::Changed;
    }
    let base = diff.base_text();
    let max_row = base.max_point().row;
    for line in lines.clone() {
        let Some(row) = line.checked_sub(1).filter(|row| *row <= max_row) else {
            return LineMapping::Changed;
        };
        let start = base.point_to_offset(Point::new(row, 0));
        let end = if row < max_row {
            base.point_to_offset(Point::new(row + 1, 0))
        } else {
            base.len()
        };
        // Lines inserted right before or after the row leave it as it is.
        if diff
            .hunks_intersecting_base_text_range(start..end, buffer)
            .any(|hunk| {
                hunk.diff_base_byte_range.start < end && hunk.diff_base_byte_range.end > start
            })
        {
            return LineMapping::Changed;
        }
    }
    let line_in_buffer = |line: u32| {
        diff.base_text_point_to_buffer_point(Point::new(line - 1, 0), buffer)
            .row
            + 1
    };
    LineMapping::Lines(line_in_buffer(*lines.start())..=line_in_buffer(*lines.end()))
}

/// Where the buffer's `lines` (1-based) are in HEAD (the diff's base text), unless the buffer
/// changed any of them.
fn working_lines_in_head(
    diff: &BufferDiffSnapshot,
    buffer: &BufferSnapshot,
    lines: RangeInclusive<u32>,
) -> LineMapping {
    if !diff.base_text_exists() {
        return LineMapping::Changed;
    }
    let max_row = buffer.max_point().row;
    for line in lines.clone() {
        let Some(row) = line.checked_sub(1).filter(|row| *row <= max_row) else {
            return LineMapping::Changed;
        };
        let start = Point::new(row, 0);
        let end = if row < max_row {
            Point::new(row + 1, 0)
        } else {
            buffer.max_point()
        };
        let range = buffer.anchor_before(start)..buffer.anchor_after(end);
        if diff
            .hunks_intersecting_range(range, buffer)
            .any(|hunk| hunk.range.start < end && hunk.range.end > start)
        {
            return LineMapping::Changed;
        }
    }
    let line_in_head = |line: u32| {
        diff.buffer_point_to_base_text_point(Point::new(line - 1, 0), buffer)
            .row
            + 1
    };
    LineMapping::Lines(line_in_head(*lines.start())..=line_in_head(*lines.end()))
}

struct Loaded {
    details: Option<Result<PullRequestDetails>>,
    threads: Result<Vec<RemoteThread>>,
    summaries: Result<Vec<RemoteReviewSummary>>,
    files: Result<CommentableLines>,
    stack: Result<Option<PullRequestStack>>,
}

impl Loaded {
    fn auth_failed(&self) -> bool {
        let is_auth = |error: &anyhow::Error| {
            matches!(forge::github_error(error), Some(GitHubError::Auth { .. }))
        };
        self.details
            .as_ref()
            .is_some_and(|details| details.as_ref().err().is_some_and(is_auth))
            || self.threads.as_ref().err().is_some_and(is_auth)
            || self.summaries.as_ref().err().is_some_and(is_auth)
            || self.files.as_ref().err().is_some_and(is_auth)
    }
}

/// Everything at once; one failure doesn't stop the others.
async fn fetch(
    forge: &Arc<dyn Forge>,
    number: u64,
    with_details: bool,
    executor: &BackgroundExecutor,
) -> Loaded {
    let details = with_details.then(|| executor.spawn(forge.pull_request(number)));
    let threads = executor.spawn(forge.review_threads(number));
    let summaries = executor.spawn(forge.review_summaries(number));
    let files = executor.spawn(forge.changed_files(number));
    let stack = executor.spawn(forge.pull_request_stack(number));
    let details = match details {
        Some(details) => Some(details.await),
        None => None,
    };
    Loaded {
        details,
        threads: threads.await,
        summaries: summaries.await,
        files: files.await,
        stack: stack.await,
    }
}

/// Where a review thread shows in a review whose checkout is at `local_head`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ThreadPlacement {
    /// Below its lines, or at its file, in the diff.
    Inline(CommentTarget),
    /// In its file's item of outdated and moved threads: its lines changed since it was made, or
    /// they are numbered for another commit than the one checked out.
    Moved { path: RepoPath },
}

/// Where `thread` shows, if the filters show it. Line numbers count in the thread's commit, so
/// only a thread made on the checked-out commit goes below its lines. `files` maps a renamed
/// file's paths: the local diff shows its old lines under the old path and its new lines under
/// the new one.
pub(crate) fn place_thread(
    thread: &RemoteThread,
    local_head: Option<Oid>,
    files: Option<&CommentableLines>,
    filters: ThreadFilters,
) -> Option<ThreadPlacement> {
    if (thread.is_resolved && !filters.show_resolved)
        || (thread.is_outdated && !filters.show_outdated)
        || (thread.is_by_bot() && !filters.show_bots)
    {
        return None;
    }
    let side = match &thread.anchor {
        ThreadAnchor::Line { side, .. } | ThreadAnchor::Range { side, .. } => *side,
        ThreadAnchor::File | ThreadAnchor::Outdated { .. } => DiffSide::Right,
    };
    let file = files.and_then(|files| files.file(&thread.path));
    let path = match (side, file) {
        (DiffSide::Left, Some(file)) => file.previous_path.as_deref().unwrap_or(&file.path),
        (DiffSide::Right, Some(file)) => file.path.as_str(),
        (_, None) => thread.path.as_str(),
    };
    let path = RepoPath::new(path).log_err()?;
    let is_moved = thread
        .anchor_commit
        .zip(local_head)
        .is_some_and(|(anchor_commit, local_head)| anchor_commit != local_head);
    Some(match &thread.anchor {
        ThreadAnchor::File => ThreadPlacement::Inline(CommentTarget::File { path }),
        ThreadAnchor::Outdated { .. } => ThreadPlacement::Moved { path },
        _ if is_moved => ThreadPlacement::Moved { path },
        ThreadAnchor::Line { side, line } => ThreadPlacement::Inline(CommentTarget::Lines {
            path,
            side: comment_side(*side),
            lines: *line..=*line,
        }),
        // A range from an old line to a new one sits below its new line, like its last row; so
        // does one whose start GitHub's diff doesn't have before its end (a left start is
        // numbered in the commit the thread was made on).
        ThreadAnchor::Range {
            start_side,
            start,
            side,
            line,
        } => ThreadPlacement::Inline(CommentTarget::Lines {
            path,
            side: comment_side(*side),
            lines: if start_side == side
                && start <= line
                && files.is_none_or(|files| {
                    files.allows_range(&thread.path, (*start_side, *start), (*side, *line))
                }) {
                *start..=*line
            } else {
                *line..=*line
            },
        }),
    })
}

fn comment_side(side: DiffSide) -> Side {
    match side {
        DiffSide::Left => Side::Old,
        DiffSide::Right => Side::New,
    }
}

pub(crate) fn diff_side(side: Side) -> DiffSide {
    match side {
        Side::Old => DiffSide::Left,
        Side::New => DiffSide::Right,
    }
}

/// Whether GitHub takes a comment on `target`, by its diff: lines (old ones on the left side)
/// inside its hunks ([`check_lines`]), files of the pull request.
pub(crate) fn check_comment_target(
    commentable: &CommentableLines,
    target: &CommentTarget,
) -> Result<(), SharedString> {
    let (path, lines) = match target {
        CommentTarget::Review => return Ok(()),
        CommentTarget::File { path } => {
            return if commentable.contains_file(path.as_unix_str()) {
                Ok(())
            } else {
                Err("This file isn't part of the pull request on GitHub".into())
            };
        }
        CommentTarget::Lines { path, side, lines } => (path.as_unix_str(), (*side, lines)),
    };
    let Some(file) = commentable.file(path) else {
        return Err(if commentable.is_truncated() {
            "GitHub didn't list this file's diff (the pull request has too many files), so it \
             only takes a file comment"
                .into()
        } else {
            "This file isn't part of the pull request on GitHub".into()
        });
    };
    if file.hunks.is_none() {
        return Err("GitHub shows no diff for this file, so it only takes a file comment".into());
    }
    let (side, lines) = lines;
    let side = diff_side(side);
    commentable
        .check_range(path, (side, *lines.start()), (side, *lines.end()))
        .map_err(|refusal| {
            match refusal {
                LinesRefusal::Outside => "GitHub only accepts comments within its diff hunks",
                LinesRefusal::StartOutside => {
                    "The selection starts outside GitHub's diff hunks, and GitHub only accepts \
                     comments within them"
                }
                LinesRefusal::EndOutside => {
                    "The selection ends outside GitHub's diff hunks, and GitHub only accepts \
                     comments within them"
                }
                LinesRefusal::Reversed => {
                    "The selection's start comes after its end in GitHub's diff"
                }
            }
            .into()
        })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpenPullRequestOptions {
    /// Review in a new linked worktree (`pr-N`, opened as another workspace) instead of checking
    /// the pull request out in this one.
    pub in_new_worktree: bool,
    /// Ask before moving the checkout, for requests from outside Zed.
    pub confirm_checkout: bool,
}

/// Opens a review of the pull request `reference` names: loads it from the forge of
/// `repository`'s GitHub remote, checks it out (Zed's way: see [`crate::checkout`]) and opens or
/// activates its review tab. The checkout only moves here, after the user chose the pull request.
pub fn open_pull_request(
    workspace: &mut Workspace,
    repository: Entity<Repository>,
    reference: PullRequestReference,
    options: OpenPullRequestOptions,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<Entity<ReviewView>>> {
    let Some(opening) = OpeningPullRequest::begin(repository.entity_id(), cx) else {
        return Task::ready(Err(anyhow!(
            "Another pull request is being opened in this repository. Try again once it is open."
        )));
    };
    let project = workspace.project().clone();
    let resolve = forge::resolve_forge(&repository, reference.repo.clone(), &project, cx);
    let workspace_handle = cx.weak_entity();
    cx.spawn_in(window, async move |_, cx| {
        let _opening = opening;
        let forge = resolve.await?;
        let details = cx
            .background_executor()
            .spawn(forge.pull_request(reference.number))
            .await?;
        let pull_request = LoadedPullRequest { forge, details };
        if options.in_new_worktree {
            open_in_new_worktree(workspace_handle, repository, pull_request, cx).await
        } else {
            check_out_and_open(
                workspace_handle,
                repository,
                pull_request,
                options.confirm_checkout,
                cx,
            )
            .await
        }
    })
}

/// The repositories a pull request is being opened in.
#[derive(Default)]
struct OpeningPullRequests(Rc<RefCell<HashSet<EntityId>>>);

impl Global for OpeningPullRequests {}

/// Held while a pull request is opened in a repository: two at once would interleave their
/// branch switches and fast-forwards on the repository's job queue.
struct OpeningPullRequest {
    opening: Rc<RefCell<HashSet<EntityId>>>,
    repository: EntityId,
}

impl OpeningPullRequest {
    fn begin(repository: EntityId, cx: &mut App) -> Option<Self> {
        let opening = cx.default_global::<OpeningPullRequests>().0.clone();
        let is_new = opening.borrow_mut().insert(repository);
        is_new.then_some(Self {
            opening,
            repository,
        })
    }
}

impl Drop for OpeningPullRequest {
    fn drop(&mut self) {
        self.opening.borrow_mut().remove(&self.repository);
    }
}

/// [`open_pull_request`], with a failure shown as a toast.
pub(crate) fn open_pull_request_and_notify(
    workspace: &mut Workspace,
    repository: Entity<Repository>,
    reference: PullRequestReference,
    options: OpenPullRequestOptions,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let task = open_pull_request(workspace, repository, reference, options, window, cx);
    cx.spawn(async move |workspace, cx| match task.await {
        // The review may be in another workspace (a new worktree's), which may have replaced
        // this one in the window.
        Ok(view) => {
            let review_workspace = view.read_with(cx, |view, _| view.workspace().clone());
            review_workspace
                .update(cx, |workspace, cx| {
                    forge::dismiss_error_toast(workspace, cx)
                })
                .log_err();
        }
        Err(error) => {
            let shown = workspace.update(cx, |workspace, cx| {
                forge::show_error_toast(workspace, &error, cx)
            });
            if shown.is_err() {
                log::error!("opening a pull request: {error:#}");
            }
        }
    })
    .detach();
}

/// The toast saying where a stack can't be stepped through.
struct StackNavigationToast;

/// Reviews the nearest open pull request above or below `review`'s in its stack instead (see
/// [`open_in_stack`]), or says in a toast why there is none.
pub(crate) fn open_stack_neighbor(
    workspace: &mut Workspace,
    review: Entity<ReviewView>,
    direction: StackDirection,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let message = match review.read(cx).pull_request() {
        None => "Open a pull request review first".to_string(),
        Some(pull_request) => {
            let pull_request = pull_request.read(cx);
            let number = pull_request.details().number;
            match pull_request.stack() {
                None if pull_request.is_loading() => {
                    format!("Loading the stack of pull request #{number}…")
                }
                None => format!("Pull request #{number} isn't in a stack"),
                Some(stack) => match stack.neighbor(number, direction) {
                    Some(neighbor) => {
                        let neighbor = neighbor.number;
                        open_in_stack(workspace, review, neighbor, window, cx);
                        return;
                    }
                    None => match direction {
                        StackDirection::Up => {
                            format!("No open pull request above #{number} in its stack")
                        }
                        StackDirection::Down => {
                            format!("No open pull request below #{number} in its stack")
                        }
                    },
                },
            }
        }
    };
    workspace.show_toast(
        Toast::new(NotificationId::unique::<StackNavigationToast>(), message).autohide(),
        cx,
    );
}

/// Reviews pull request `number`, of the stack of `review`'s pull request, instead of that one:
/// checked out in the review's repository (in place). A tab showing what the stack says about it
/// takes the focus at once; once its review is open, that review replaces both tabs. The review's
/// drafts stay in its session, for when it is opened again.
pub(crate) fn open_in_stack(
    workspace: &mut Workspace,
    review: Entity<ReviewView>,
    number: u64,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let (repository, canonical, summary) = {
        let view = review.read(cx);
        let Some(pull_request) = view.pull_request() else {
            return;
        };
        let pull_request = pull_request.read(cx);
        let summary = pull_request.stack().and_then(|stack| {
            stack
                .pull_requests
                .iter()
                .find(|pull_request| pull_request.number == number)
                .cloned()
        });
        (
            view.repository().clone(),
            pull_request.forge().context().canonical.clone(),
            summary,
        )
    };
    let opening = summary.and_then(|summary| {
        let pane = workspace.pane_for(&review)?;
        let opening = cx.new(|cx| OpeningReview::new(summary, cx));
        pane.update(cx, |pane, cx| {
            let index = pane.index_for_item(&review).map(|index| index + 1);
            pane.add_item(Box::new(opening.clone()), true, true, index, window, cx);
        });
        Some(opening)
    });
    let reference = PullRequestReference {
        repo: Some(ForgeRepoRef {
            host: Some(canonical.host),
            owner: canonical.owner,
            name: canonical.name,
        }),
        number,
    };
    let task = open_pull_request(
        workspace,
        repository,
        reference,
        OpenPullRequestOptions::default(),
        window,
        cx,
    );
    cx.spawn_in(window, async move |workspace, cx| {
        let result = task.await;
        workspace
            .update_in(cx, |workspace, window, cx| match result {
                Ok(opened) => {
                    forge::dismiss_error_toast(workspace, cx);
                    let mut replaced = vec![review.entity_id()];
                    replaced.extend(opening.map(|opening| opening.entity_id()));
                    for item_id in replaced {
                        if item_id == opened.entity_id() {
                            continue;
                        }
                        let Some(pane) = workspace.panes().iter().find(|pane| {
                            pane.read(cx).items().any(|item| item.item_id() == item_id)
                        }) else {
                            continue;
                        };
                        pane.update(cx, |pane, cx| {
                            pane.close_item_by_id(item_id, SaveIntent::Skip, window, cx)
                        })
                        .detach_and_log_err(cx);
                    }
                }
                Err(error) => {
                    // The tab says what went wrong; closing it goes back to the review.
                    if let Some(opening) = &opening {
                        let message = forge::error_message(&error);
                        opening.update(cx, |opening, cx| opening.fail(message, cx));
                    }
                    forge::show_error_toast(workspace, &error, cx);
                }
            })
            .log_err();
    })
    .detach();
}

struct LoadedPullRequest {
    forge: Arc<dyn Forge>,
    details: PullRequestDetails,
}

/// The user answered Cancel when asked before a checkout.
#[derive(Debug)]
pub(crate) struct ReviewCancelled;

impl std::fmt::Display for ReviewCancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Review cancelled")
    }
}

impl std::error::Error for ReviewCancelled {}

async fn check_out_and_open(
    workspace: WeakEntity<Workspace>,
    repository: Entity<Repository>,
    pull_request: LoadedPullRequest,
    confirm_checkout: bool,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<ReviewView>> {
    let LoadedPullRequest { forge, details } = pull_request;
    let target = PullRequestTarget::new(
        forge.context().canonical.clone(),
        details.number,
        details.base_oid,
    )?;
    let remotes = cx.update(|_, cx| github::remotes(&repository, cx))?.await?;
    let local = cx
        .update(|_, cx| checkout::read_local_state(&repository, details.base_oid, cx))?
        .await?;
    let steps = plan_checkout(&details, forge.context(), &remotes, &local)?;
    if !steps.is_empty() {
        cx.update(|_, cx| checkout::ensure_trusted(&repository, cx))??;
    }
    if confirm_checkout && !steps.is_empty() {
        let work_dir = repository.read_with(cx, |repository, _| {
            repository.work_directory_abs_path.clone()
        });
        let what = match checkout::checked_out_branch(&steps) {
            Some(branch) => format!("checks out {branch}"),
            None => format!("fetches its base branch {}", details.base_ref),
        };
        let mut detail = format!(
            "Reviewing {}#{} {what} in {}.",
            forge.context().canonical,
            details.number,
            work_dir.display()
        );
        if !trusts_head_repository(&details, forge.context()) {
            detail.push_str(&format!(
                " It comes from {}: like this project's own code, its project settings and \
                 tasks apply and language servers may run its build scripts.",
                match &details.head_repository {
                    Some(head_repository) => format!("the fork {head_repository}"),
                    None => "a deleted fork".to_string(),
                }
            ));
        }
        let answer = cx.update(|window, cx| {
            window.prompt(
                PromptLevel::Warning,
                &format!("Check out pull request #{}?", details.number),
                Some(&detail),
                &["Check Out", "Cancel"],
                cx,
            )
        })?;
        if answer.await? != 0 {
            return Err(ReviewCancelled.into());
        }
    }
    run_checkout(&repository, steps, &workspace, cx).await?;

    let target = ReviewTarget::PullRequest(target);
    workspace.update_in(cx, |workspace, window, cx| {
        let key = target.review_key(&repository.read(cx));
        // Held until the tab holds it.
        let project = workspace.project().clone();
        let _pull_request = PullRequestState::set(key, forge, details, &repository, &project, cx);
        ReviewView::deploy_in_repo(workspace, target, repository, window, cx)
    })
}

/// Whether the pull request's code is the repository's own: from the repository itself or the
/// viewer's own fork. Any other fork's code is a stranger's.
fn trusts_head_repository(details: &PullRequestDetails, context: &RepositoryContext) -> bool {
    if !details.is_cross_repository {
        return true;
    }
    details
        .head_repository
        .as_ref()
        .is_some_and(|head_repository| head_repository.owner.eq_ignore_ascii_case(&context.viewer))
}

/// What brings a pull request's head and base into the repository for a review worktree, which
/// is detached at the head (`git worktree add --detach` takes any commit): `refs/pull/N/head`
/// into `FETCH_HEAD`, whichever remote has the head branch. A branch would be in the way: git
/// can't fetch into a branch another worktree has checked out.
pub(crate) fn plan_worktree_fetch(
    details: &PullRequestDetails,
    context: &RepositoryContext,
    remotes: &[GitHubRemote],
) -> Result<CheckoutStep> {
    validate_branch_name(&details.head_ref).context("The pull request's head branch")?;
    validate_branch_name(&details.base_ref).context("The pull request's base branch")?;
    Ok(CheckoutStep::FetchRefspec {
        source: canonical_source(context, remotes),
        refspecs: vec![
            format!("refs/pull/{}/head", details.number),
            format!("refs/heads/{}", details.base_ref),
        ],
    })
}

/// Reviews the pull request in a linked worktree `pr-N` of the repository, detached at the pull
/// request's head and opened as another workspace of the window; the original checkout stays as
/// it is. A worktree of that name made before is reused, and only moved to the pull request's
/// head when the user agrees.
async fn open_in_new_worktree(
    workspace: WeakEntity<Workspace>,
    repository: Entity<Repository>,
    pull_request: LoadedPullRequest,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<ReviewView>> {
    let details = &pull_request.details;
    let context = pull_request.forge.context();
    let head_oid = details.head_oid;
    let worktree_name = format!("pr-{}", details.number);
    let worktree_directory = cx.update(|_, cx| {
        ProjectSettings::get_global(cx)
            .git
            .worktree_directory
            .clone()
    })?;
    let path = repository.read_with(cx, |repository, _| {
        repository.path_for_new_linked_worktree(&worktree_name, &worktree_directory)
    })?;
    let worktrees = repository.update(cx, |repository, _| repository.worktrees());
    let worktrees = worktrees.await.context("the git job was canceled")??;
    let existing = worktrees.into_iter().find(|worktree| worktree.path == path);
    let moves_existing = existing
        .as_ref()
        .is_some_and(|existing| existing.sha.parse::<Oid>().ok() != Some(head_oid));

    if let Some(existing) = existing.as_ref().filter(|_| moves_existing) {
        let checked_out = match existing
            .ref_name
            .as_deref()
            .map(|ref_name| ref_name.strip_prefix("refs/heads/").unwrap_or(ref_name))
        {
            Some(branch) => format!("has {branch} checked out"),
            None => match existing.sha.parse::<Oid>() {
                Ok(sha) => format!("is at {}", sha.display_short()),
                Err(_) => format!("is at {}", existing.sha),
            },
        };
        let answer =
            cx.update(|window, cx| {
                window.prompt(
                PromptLevel::Warning,
                &format!("Move the worktree {worktree_name} to pull request #{}?", details.number),
                Some(&format!(
                    "{} {checked_out}. Reviewing {}#{} there checks out the pull request's head \
                     {} (detached); branches stay as they are.",
                    path.display(),
                    context.canonical,
                    details.number,
                    head_oid.display_short(),
                )),
                &["Check Out", "Cancel"],
                cx,
            )
            })?;
        if answer.await? != 0 {
            return Err(ReviewCancelled.into());
        }
    }
    let creates = existing.is_none();
    if creates || moves_existing {
        cx.update(|_, cx| checkout::ensure_trusted(&repository, cx))??;
        let remotes = cx.update(|_, cx| github::remotes(&repository, cx))?.await?;
        let fetch = plan_worktree_fetch(details, context, &remotes)?;
        run_checkout(&repository, vec![fetch], &workspace, cx).await?;
    }
    if creates {
        let create = repository.update(cx, |repository, _| {
            repository.create_worktree(
                CreateWorktreeTarget::Detached {
                    base_sha: Some(head_oid.to_string()),
                },
                path.clone(),
            )
        });
        create
            .await
            .context("the git job was canceled")?
            .with_context(|| format!("Couldn't create the worktree {}", path.display()))?;
    }

    let new_workspace = open_worktree(&workspace, path.clone(), cx).await?;
    // A worktree of the repository's own code is trusted like the checkout it was made from
    // (Zed's worktree creation does the same). Any other fork's gets Zed's Restricted Mode prompt:
    // its project settings, tasks and build scripts would run otherwise.
    if creates && trusts_head_repository(details, context) {
        cx.update(|window, cx| trust_worktree(&new_workspace, &path, window, cx))?;
    }
    let new_repository = repository_at(&new_workspace, &path, cx).await?;
    if moves_existing {
        run_checkout(
            &new_repository,
            vec![CheckoutStep::Detach {
                to: head_oid.to_string(),
            }],
            &new_workspace.downgrade(),
            cx,
        )
        .await?;
    }
    check_out_and_open(
        new_workspace.downgrade(),
        new_repository,
        pull_request,
        false,
        cx,
    )
    .await
}

async fn open_worktree(
    workspace: &WeakEntity<Workspace>,
    path: PathBuf,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<Workspace>> {
    let multi_workspace = workspace
        .read_with(cx, |workspace, _| workspace.multi_workspace().cloned())?
        .context("the window has no workspaces")?;
    let open = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.find_or_create_local_workspace(
            PathList::new(&[path]),
            None,
            None,
            OpenMode::Activate,
            Some(workspace.clone()),
            window,
            cx,
        )
    })?;
    open.await
}

fn trust_worktree(workspace: &Entity<Workspace>, path: &Path, window: &mut Window, cx: &mut App) {
    if !ProjectSettings::get_global(cx).session.trust_all_worktrees
        && let Some(trusted_worktrees) = TrustedWorktrees::try_get_global(cx)
    {
        let worktree_store = workspace.read(cx).project().read(cx).worktree_store();
        let worktree = worktree_store.read(cx).find_worktree(path, cx);
        if let Some((worktree, _)) = worktree {
            let worktree = PathTrust::Worktree(worktree.read(cx).id());
            trusted_worktrees.update(cx, |trusted_worktrees, cx| {
                trusted_worktrees.trust(&worktree_store, HashSet::from_iter([worktree]), cx);
            });
        }
    }
    // The prompt that opened with the workspace closes when nothing is restricted any more.
    workspace.update(cx, |workspace, cx| {
        workspace.show_worktree_trust_security_modal(false, window, cx);
    });
}

/// The repository of a workspace just opened on the worktree at `path`, once Zed found it.
pub(crate) async fn repository_at(
    workspace: &Entity<Workspace>,
    path: &Path,
    cx: &mut AsyncApp,
) -> Result<Entity<Repository>> {
    let scan = workspace.read_with(cx, |workspace, cx| {
        workspace.project().read(cx).wait_for_initial_scan(cx)
    });
    scan.await;
    // Repositories are discovered from the scan's results shortly after it ends.
    for _ in 0..50 {
        let repository = workspace.read_with(cx, |workspace, cx| {
            let repositories = workspace.project().read(cx).repositories(cx);
            repositories
                .values()
                .find(|repository| *repository.read(cx).work_directory_abs_path == *path)
                // The path may be a symlink's, and the workspace has nothing else.
                .or_else(|| {
                    (repositories.len() == 1)
                        .then(|| repositories.values().next())
                        .flatten()
                })
                .cloned()
        });
        if let Some(repository) = repository {
            return Ok(repository);
        }
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
    }
    bail!("No git repository found in the worktree {}", path.display())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::forge::{Actor, ForgeRepo, PullRequestStatus};

    fn repo(owner: &str) -> ForgeRepo {
        ForgeRepo {
            host: "github.com".into(),
            owner: owner.to_string().into(),
            name: "r".into(),
        }
    }

    fn remote(name: &str, repo: ForgeRepo) -> GitHubRemote {
        GitHubRemote {
            name: name.into(),
            repo,
            is_ssh: true,
            host_kind: crate::github::GitHubHost::Cloud,
        }
    }

    #[test]
    fn test_plan_worktree() {
        let canonical = repo("o");
        let context = RepositoryContext {
            origin: repo("me"),
            canonical: canonical.clone(),
            viewer: "me".into(),
            canonical_https_url: "https://github.com/o/r".into(),
            canonical_ssh_url: "git@github.com:o/r.git".into(),
            origin_is_ssh: true,
        };
        let details = |head_repository: Option<ForgeRepo>| PullRequestDetails {
            node_id: "PR_2".into(),
            number: 2,
            title: "Fix".into(),
            url: "https://github.com/o/r/pull/2".into(),
            body: "".into(),
            author: Actor {
                login: "me".into(),
                is_bot: false,
            },
            state: PullRequestStatus::Open,
            is_draft: false,
            base_ref: "main".into(),
            base_oid: "deea36cf8fa38276ef75801d05773c4c35c48d45".parse().unwrap(),
            head_ref: "fix/truncate".into(),
            head_oid: "632ee13692b487a7ea71911f473781416f35ff01".parse().unwrap(),
            is_cross_repository: false,
            head_repository,
            review_decision: None,
            viewer_is_author: true,
            pending_review: None,
        };
        let remotes = [remote("origin", canonical.clone())];
        let fork_remotes = [remote("origin", repo("me")), remote("upstream", canonical)];
        let fetch = |source: &str| CheckoutStep::FetchRefspec {
            source: source.into(),
            refspecs: vec!["refs/pull/2/head".into(), "refs/heads/main".into()],
        };

        // The head always comes from the canonical repository's refs/pull, into FETCH_HEAD: the
        // worktree is detached at it.
        assert_eq!(
            plan_worktree_fetch(&details(Some(repo("o"))), &context, &remotes).unwrap(),
            fetch("origin")
        );
        assert_eq!(
            plan_worktree_fetch(&details(Some(repo("me"))), &context, &fork_remotes).unwrap(),
            fetch("upstream")
        );
        assert_eq!(
            plan_worktree_fetch(
                &details(Some(repo("someone"))),
                &context,
                &[remote("origin", repo("me"))]
            )
            .unwrap(),
            fetch("git@github.com:o/r.git")
        );
        let mut hostile = details(None);
        hostile.head_ref = "-x".into();
        assert!(plan_worktree_fetch(&hostile, &context, &remotes).is_err());

        // Only the repository's own code and the viewer's fork are trusted like the checkout.
        let mut same_repository = details(Some(repo("o")));
        same_repository.is_cross_repository = false;
        assert!(trusts_head_repository(&same_repository, &context));
        let mut own_fork = details(Some(repo("Me")));
        own_fork.is_cross_repository = true;
        assert!(trusts_head_repository(&own_fork, &context));
        let mut stranger = details(Some(repo("someone")));
        stranger.is_cross_repository = true;
        assert!(!trusts_head_repository(&stranger, &context));
        let mut deleted = details(None);
        deleted.is_cross_repository = true;
        assert!(!trusts_head_repository(&deleted, &context));
    }

    #[gpui::test]
    fn test_one_pull_request_opens_per_repository(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let (repository, other_repository) = (EntityId::from(1), EntityId::from(2));
            let first = OpeningPullRequest::begin(repository, cx).unwrap();
            assert!(OpeningPullRequest::begin(repository, cx).is_none());
            let other = OpeningPullRequest::begin(other_repository, cx).unwrap();
            drop(first);
            assert!(OpeningPullRequest::begin(repository, cx).is_some());
            drop(other);
        });
    }

    fn new_lines(path: &str, lines: std::ops::RangeInclusive<u32>) -> ThreadPlacement {
        ThreadPlacement::Inline(CommentTarget::Lines {
            path: RepoPath::new(path).unwrap(),
            side: Side::New,
            lines,
        })
    }

    fn old_lines(path: &str, lines: std::ops::RangeInclusive<u32>) -> ThreadPlacement {
        ThreadPlacement::Inline(CommentTarget::Lines {
            path: RepoPath::new(path).unwrap(),
            side: Side::Old,
            lines,
        })
    }

    fn moved(path: &str) -> ThreadPlacement {
        ThreadPlacement::Moved {
            path: RepoPath::new(path).unwrap(),
        }
    }

    #[test]
    fn test_place_thread() {
        use crate::github::test_support::{
            PR1_FILES, PR1_HEAD_OID, changed_files_fixture, pr1_threads,
        };

        let threads = pr1_threads();
        let thread = |id: &str| {
            threads
                .iter()
                .find(|thread| thread.id.as_ref() == id)
                .unwrap()
                .clone()
        };
        let head: Oid = PR1_HEAD_OID.parse().unwrap();
        let files = changed_files_fixture(PR1_FILES);
        let place = |thread: &RemoteThread, filters: ThreadFilters| {
            place_thread(thread, Some(head), Some(&files), filters)
        };
        let shown = ThreadFilters::default();
        let all = ThreadFilters {
            show_resolved: true,
            show_outdated: true,
            show_bots: true,
        };

        // A line on the right side, with a reply; a range; a deleted line; a file.
        let single = thread("PRRT_kwDOVM9_N86tcJxI");
        assert_eq!(
            place(&single, shown),
            Some(new_lines("src/engine.rs", 134..=134))
        );
        assert_eq!(
            place(&thread("PRRT_kwDOVM9_N86tcJxN"), shown),
            Some(new_lines("src/filters.rs", 20..=25))
        );
        assert_eq!(
            place(&thread("PRRT_kwDOVM9_N86tcJxU"), shown),
            Some(old_lines("src/engine.rs", 193..=193))
        );
        let file = thread("PRRT_kwDOVM9_N86tcJ3I");
        assert_eq!(
            place(&file, shown),
            Some(ThreadPlacement::Inline(CommentTarget::File {
                path: RepoPath::new("docs/guide.md").unwrap()
            }))
        );

        // Resolved, outdated and bot threads only show when asked for. An outdated thread goes
        // to its file's item.
        for (id, filters, placement) in [
            (
                "PRRT_kwDOVM9_N86tcJxO",
                ThreadFilters {
                    show_resolved: true,
                    ..shown
                },
                new_lines("src/util/strings.rs", 48..=48),
            ),
            (
                "PRRT_kwDOVM9_N86tcJxR",
                ThreadFilters {
                    show_outdated: true,
                    ..shown
                },
                moved("src/engine.rs"),
            ),
            (
                "PRRT_kwDOVM9_N86tcKM3",
                ThreadFilters {
                    show_bots: true,
                    ..shown
                },
                new_lines("src/filters.rs", 38..=38),
            ),
        ] {
            assert_eq!(place(&thread(id), shown), None, "{id}");
            assert_eq!(place(&thread(id), filters), Some(placement.clone()), "{id}");
            assert_eq!(place(&thread(id), all), Some(placement), "{id}");
        }

        // Another commit checked out: line threads are numbered for another file, files aren't.
        let other_head: Oid = "1111111111111111111111111111111111111111".parse().unwrap();
        assert_eq!(
            place_thread(&single, Some(other_head), Some(&files), shown),
            Some(moved("src/engine.rs"))
        );
        assert_eq!(
            place_thread(&file, Some(other_head), Some(&files), shown),
            place(&file, shown)
        );
        // Without a known HEAD or GitHub's file list.
        assert_eq!(
            place_thread(&single, None, None, shown),
            Some(new_lines("src/engine.rs", 134..=134))
        );

        // A renamed file: old lines are under the old path in the local diff, new ones under the
        // new path, whichever path the thread has.
        let mut renamed = single.clone();
        for (path, side, placement) in [
            (
                "src/render.rs",
                DiffSide::Left,
                old_lines("src/format.rs", 18..=18),
            ),
            (
                "src/render.rs",
                DiffSide::Right,
                new_lines("src/render.rs", 18..=18),
            ),
            (
                "src/format.rs",
                DiffSide::Right,
                new_lines("src/render.rs", 18..=18),
            ),
            (
                "src/format.rs",
                DiffSide::Left,
                old_lines("src/format.rs", 18..=18),
            ),
        ] {
            renamed.path = path.into();
            renamed.anchor = ThreadAnchor::Line { side, line: 18 };
            assert_eq!(place(&renamed, shown), Some(placement), "{path} {side:?}");
        }

        // A range from an old line to a new one sits below the new one; a context row's old
        // number is an old line.
        let mut range = single;
        range.anchor = ThreadAnchor::Range {
            start_side: DiffSide::Left,
            start: 191,
            side: DiffSide::Right,
            line: 195,
        };
        assert_eq!(
            place(&range, shown),
            Some(new_lines("src/engine.rs", 195..=195))
        );
        range.anchor = ThreadAnchor::Line {
            side: DiffSide::Left,
            line: 9,
        };
        assert_eq!(
            place(&range, shown),
            Some(old_lines("src/engine.rs", 9..=9))
        );
    }

    #[test]
    fn test_thread_locations() {
        use crate::comments::thread_location;
        use crate::github::test_support::pr1_threads;

        let threads = pr1_threads();
        let location = |id: &str, is_moved: bool| {
            let thread = threads
                .iter()
                .find(|thread| thread.id.as_ref() == id)
                .unwrap();
            thread_location(thread, is_moved).map(|location| location.to_string())
        };
        assert_eq!(location("PRRT_kwDOVM9_N86tcJxI", false), None);
        assert_eq!(
            location("PRRT_kwDOVM9_N86tcJxI", true).as_deref(),
            Some("Line 134 at 3f22f1b")
        );
        assert_eq!(
            location("PRRT_kwDOVM9_N86tcJxN", false).as_deref(),
            Some("Lines 20–25")
        );
        assert_eq!(
            location("PRRT_kwDOVM9_N86tcJxU", false).as_deref(),
            Some("Old line 193")
        );
        assert_eq!(
            location("PRRT_kwDOVM9_N86tcJxR", true).as_deref(),
            Some("Originally line 223 at 5c5cdf4")
        );
        assert_eq!(
            location("PRRT_kwDOVM9_N86tcJ3I", false).as_deref(),
            Some("File comment")
        );

        // An outdated thread's original line is in the commit it was made on, which pushes
        // since didn't change (they moved its current commit along).
        let mut outdated = threads
            .iter()
            .find(|thread| thread.id.as_ref() == "PRRT_kwDOVM9_N86tcJxR")
            .unwrap()
            .clone();
        outdated.anchor_commit = Some("2222222222222222222222222222222222222222".parse().unwrap());
        outdated.original_commit =
            Some("1111111111111111111111111111111111111111".parse().unwrap());
        assert_eq!(
            thread_location(&outdated, true).as_deref(),
            Some("Originally line 223 at 1111111")
        );
    }

    #[test]
    fn test_comment_gate() {
        use crate::github::test_support::{PR1_FILES, PR4_FILES, changed_files_fixture};

        let files = changed_files_fixture(PR1_FILES);
        let lines =
            |path: &str, side: Side, lines: std::ops::RangeInclusive<u32>| CommentTarget::Lines {
                path: RepoPath::new(path).unwrap(),
                side,
                lines,
            };
        let file = |path: &str| CommentTarget::File {
            path: RepoPath::new(path).unwrap(),
        };
        let check = |target: &CommentTarget| check_comment_target(&files, target);
        let outside: Result<(), SharedString> =
            Err("GitHub only accepts comments within its diff hunks".into());
        let not_in_pull_request: Result<(), SharedString> =
            Err("This file isn't part of the pull request on GitHub".into());

        // R59 is between two local hunks but inside GitHub's merged one; R100 is far from any.
        assert_eq!(check(&lines("src/engine.rs", Side::New, 59..=59)), Ok(()));
        assert_eq!(
            check(&lines("src/engine.rs", Side::New, 100..=100)),
            outside
        );
        assert_eq!(check(&lines("src/engine.rs", Side::Old, 193..=193)), Ok(()));
        // A range's ends are in GitHub's hunks (`+1,7` and `+12,7` here), which may hold
        // unchanged lines only, and may be in different hunks, as GitHub takes them.
        assert_eq!(check(&lines("src/engine.rs", Side::New, 2..=7)), Ok(()));
        assert_eq!(check(&lines("src/engine.rs", Side::New, 5..=7)), Ok(()));
        assert_eq!(check(&lines("src/engine.rs", Side::New, 2..=13)), Ok(()));
        assert_eq!(
            check(&lines("src/engine.rs", Side::New, 2..=10)),
            Err(
                "The selection ends outside GitHub's diff hunks, and GitHub only accepts \
                 comments within them"
                    .into()
            )
        );
        assert_eq!(
            check(&lines("src/engine.rs", Side::New, 9..=13)),
            Err(
                "The selection starts outside GitHub's diff hunks, and GitHub only accepts \
                 comments within them"
                    .into()
            )
        );
        assert_eq!(check(&lines("src/engine.rs", Side::New, 8..=11)), outside);
        // The renamed file's old lines, under its old path.
        assert_eq!(check(&lines("src/format.rs", Side::Old, 18..=18)), Ok(()));
        // Files.
        assert_eq!(check(&file("docs/guide.md")), Ok(()));
        assert_eq!(check(&file("README.md")), not_in_pull_request);
        assert_eq!(
            check(&lines("README.md", Side::New, 1..=1)),
            not_in_pull_request
        );
        assert_eq!(check(&CommentTarget::Review), Ok(()));

        // A pure rename has no diff on GitHub: file comments only.
        let files = changed_files_fixture(PR4_FILES);
        assert_eq!(
            check_comment_target(&files, &lines("docs/design-notes.md", Side::New, 1..=1)),
            Err("GitHub shows no diff for this file, so it only takes a file comment".into())
        );
        assert_eq!(
            check_comment_target(&files, &file("docs/design-notes.md")),
            Ok(())
        );
    }

    pub(crate) mod review {
        use super::*;
        use crate::{
            CopyBranchName, CopyPullRequestUrl, OpenPullRequest, ReloadPullRequest,
            ToggleBotReviews, ToggleOutdatedThreads, ToggleResolvedThreads,
            comments::{ItemKey, RemoteKey},
            github::{
                set_fake_token_source,
                test_support::{
                    PR1_BASE_OID, PR1_DETAILS, PR1_FILES, PR1_HEAD_OID, PR1_SUMMARIES, PR1_THREADS,
                    Reply, SentRequest, SentRequests, fake_github, repository_context_reply,
                },
            },
            test_support::{
                C1, active_review, commit_data, cursor_line, cursor_point, dispatch, dot_git,
                init_vim_test, insert_commit_ref, lhs_editor, move_to_row_with_text,
                open_workspace, path_at_cursor, rhs_editor, row_text, splittable,
            },
            top_block::TopBlockContent,
            view::PanelCommentKey,
        };
        use editor::{Bias, Editor};
        use gpui::{Focusable as _, TestAppContext, VisualContext as _, VisualTestContext};
        use language::Point;
        use project::{FakeFs, Project};
        use serde_json::json;
        use settings::DiffViewStyle;
        use std::path::Path;
        use std::sync::atomic::{AtomicBool, Ordering};
        use util::path;

        const SINGLE: &str = "PRRT_kwDOVM9_N86tcJxI";
        const RANGE: &str = "PRRT_kwDOVM9_N86tcJxN";
        const RESOLVED: &str = "PRRT_kwDOVM9_N86tcJxO";
        const DELETED_LINE: &str = "PRRT_kwDOVM9_N86tcJxU";
        const FILE: &str = "PRRT_kwDOVM9_N86tcJ3I";
        const BOT: &str = "PRRT_kwDOVM9_N86tcKM3";
        const USER_SUMMARY: &str = "PRR_kwDOVM9_N88AAAABSyT2YQ";
        const BOT_SUMMARY: &str = "PRR_kwDOVM9_N88AAAABSyUC4w";

        fn thread(id: &str) -> RemoteKey {
            RemoteKey::Thread(id.into())
        }

        fn numbered(prefix: &str, count: u32) -> Vec<String> {
            (1..=count).map(|line| format!("{prefix} {line}")).collect()
        }

        fn text(lines: &[String]) -> String {
            lines.join("\n") + "\n"
        }

        /// What [`open_review`] varies.
        #[derive(Default)]
        pub(crate) struct Fixture {
            pub(crate) split: bool,
            /// The threads answer 401 while set.
            pub(crate) fail_threads: Arc<AtomicBool>,
            /// The working tree differs from HEAD: two lines are inserted at the top of
            /// `src/engine.rs`, and `src/filters.rs` line 22 is edited.
            pub(crate) local_edits: bool,
            /// Thread nodes (GraphQL) after PR A's.
            pub(crate) extra_threads: Vec<serde_json::Value>,
            /// Answers requests before the fake GitHub's own answers.
            pub(crate) respond: Option<Arc<dyn Fn(&SentRequest) -> Option<Reply> + Send + Sync>>,
        }

        async fn open_pull_request_review(
            diff_view_style: DiffViewStyle,
            fail_threads: Arc<AtomicBool>,
            cx: &mut TestAppContext,
        ) -> (
            Arc<FakeFs>,
            Entity<Workspace>,
            Entity<ReviewView>,
            SentRequests,
            &mut VisualTestContext,
        ) {
            let fixture = Fixture {
                split: diff_view_style == DiffViewStyle::Split,
                fail_threads,
                ..Fixture::default()
            };
            open_review(fixture, cx).await
        }

        /// A checkout of PR A at its head, with files shaped so that its threads' lines exist:
        /// `src/engine.rs` changes lines 59, 100 and 134 and deletes old line 193,
        /// `src/filters.rs` is added (71 lines), `src/util/strings.rs` changes line 48 and
        /// `docs/guide.md` line 30. The fake GitHub has PR A's threads, summaries and files.
        pub(crate) async fn open_review(
            fixture: Fixture,
            cx: &mut TestAppContext,
        ) -> (
            Arc<FakeFs>,
            Entity<Workspace>,
            Entity<ReviewView>,
            SentRequests,
            &mut VisualTestContext,
        ) {
            let Fixture {
                split,
                fail_threads,
                local_edits,
                extra_threads,
                respond,
            } = fixture;
            init_vim_test(
                cx,
                if split {
                    DiffViewStyle::Split
                } else {
                    DiffViewStyle::Unified
                },
            );
            let engine_old = numbered("engine", 269);
            let mut engine_new = engine_old.clone();
            for line in [59, 100, 134] {
                engine_new[line - 1] = format!("ENGINE {line}");
            }
            engine_new.remove(192);
            let strings_old = numbered("strings", 60);
            let mut strings_new = strings_old.clone();
            strings_new[47] = "STRINGS 48".into();
            let guide_old = numbered("guide", 40);
            let mut guide_new = guide_old.clone();
            guide_new[29] = "GUIDE 30".into();
            let filters = text(&numbered("filter", 71));
            let (engine_local, filters_local) = if local_edits {
                let mut engine = engine_new.clone();
                engine.splice(0..0, ["local 1".to_string(), "local 2".to_string()]);
                let mut filters = numbered("filter", 71);
                filters[21] = "filter 22 edited".into();
                (text(&engine), text(&filters))
            } else {
                (text(&engine_new), filters.clone())
            };

            let fs = FakeFs::new(cx.executor());
            fs.insert_tree(
                path!("/project"),
                json!({
                    ".git": {},
                    "docs": { "guide.md": text(&guide_new) },
                    "src": {
                        "engine.rs": engine_local,
                        "filters.rs": filters_local,
                        "util": { "strings.rs": text(&strings_new) },
                    },
                }),
            )
            .await;
            fs.set_merge_base_content_for_repo(
                dot_git(),
                &[
                    ("docs/guide.md", text(&guide_old)),
                    ("src/engine.rs", text(&engine_old)),
                    ("src/util/strings.rs", text(&strings_old)),
                ],
            );
            fs.set_head_and_index_for_repo(
                dot_git(),
                &[
                    ("docs/guide.md", text(&guide_new)),
                    ("src/engine.rs", text(&engine_new)),
                    ("src/filters.rs", filters),
                    ("src/util/strings.rs", text(&strings_new)),
                ],
            );
            fs.set_remote_for_repo(
                dot_git(),
                "origin",
                "https://github.com/drew-council/code-review-sandbox.git",
            );
            fs.set_branch_name(dot_git(), Some("feature/template-filters"));
            fs.with_git_state(dot_git(), true, |state| {
                state.refs.insert("HEAD".into(), PR1_HEAD_OID.into());
            })
            .unwrap();
            insert_commit_ref(&fs, PR1_BASE_OID, PR1_BASE_OID);
            fs.set_commit_data(
                dot_git(),
                [(commit_data(PR1_HEAD_OID, None, "Outdate is_truthy"), false)],
            );
            let threads = if extra_threads.is_empty() {
                PR1_THREADS.to_string()
            } else {
                let mut threads: serde_json::Value = serde_json::from_str(PR1_THREADS).unwrap();
                threads["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"]
                    .as_array_mut()
                    .unwrap()
                    .extend(extra_threads);
                threads.to_string()
            };
            let (http, requests) = fake_github(move |request| {
                if let Some(reply) = respond.as_ref().and_then(|respond| respond(request)) {
                    return reply;
                }
                match request.operation() {
                    Some("RepositoryContext") => repository_context_reply(request),
                    Some("PullRequestDetails") => Reply::json(PR1_DETAILS),
                    Some("ReviewThreads") if fail_threads.load(Ordering::SeqCst) => {
                        Reply::status(401, r#"{"message":"Bad credentials"}"#)
                    }
                    Some("ReviewThreads") => Reply::json(threads.clone()),
                    Some("ReviewSummaries") => Reply::json(PR1_SUMMARIES),
                    None if request.uri.contains("/pulls/1/files") => Reply::json(PR1_FILES),
                    _ => Reply::status(500, "{}"),
                }
            });
            cx.update(|cx| {
                cx.set_http_client(http);
                set_fake_token_source(cx, |_| Some("token".into()));
            });
            let (workspace, cx) = open_workspace(fs.clone(), cx).await;
            dispatch(
                OpenPullRequest {
                    reference: Some("1".into()),
                },
                cx,
            );
            let review = active_review(&workspace, cx);
            cx.focus(&review);
            cx.run_until_parked();
            (fs, workspace, review, requests, cx)
        }

        fn remote_rows(
            review: &Entity<ReviewView>,
            cx: &mut VisualTestContext,
        ) -> Vec<(RemoteKey, bool, u32)> {
            let blocks = review.read_with(cx, |review, _| review.comments().clone());
            cx.update(|_, cx| blocks.update(cx, |blocks, cx| blocks.hosted_remote_rows(cx)))
        }

        fn remote_keys(review: &Entity<ReviewView>, cx: &mut VisualTestContext) -> Vec<RemoteKey> {
            remote_rows(review, cx)
                .into_iter()
                .map(|(key, _, _)| key)
                .collect()
        }

        fn display_row(editor: &Entity<Editor>, row: u32, cx: &mut VisualTestContext) -> u32 {
            editor.update(cx, |editor, cx| {
                editor
                    .display_snapshot(cx)
                    .point_to_display_point(Point::new(row, 0), Bias::Left)
                    .row()
                    .0
            })
        }

        /// The block row of a thread, and whether it is on the left side.
        fn thread_row(
            review: &Entity<ReviewView>,
            id: &str,
            cx: &mut VisualTestContext,
        ) -> (bool, u32) {
            remote_rows(review, cx)
                .into_iter()
                .find(|(key, _, _)| *key == thread(id))
                .map(|(_, on_lhs, row)| (on_lhs, row))
                .unwrap_or_else(|| panic!("{id} isn't shown"))
        }

        pub(crate) fn requests_of(requests: &SentRequests, operation: &str) -> usize {
            requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request.operation() == Some(operation))
                .count()
        }

        pub(crate) fn notification_count(
            workspace: &Entity<Workspace>,
            cx: &mut VisualTestContext,
        ) -> usize {
            workspace.update(cx, |workspace, cx| {
                let ids = workspace.notification_ids();
                for id in &ids {
                    workspace.dismiss_notification(id, cx);
                }
                ids.len()
            })
        }

        pub(crate) fn has_open_comment_editor(
            review: &Entity<ReviewView>,
            cx: &VisualTestContext,
        ) -> bool {
            review.read_with(cx, |review, cx| review.comments().read(cx).is_editing(cx))
        }

        /// HEAD's text of a file is reloaded some time after HEAD moves: until the file's
        /// comparison is based on the checked-out commit's text (read from git), its new lines
        /// aren't numbered in that commit. A file that couldn't be compared at all isn't either.
        #[gpui::test]
        async fn test_new_lines_wait_for_the_heads_text(cx: &mut TestAppContext) {
            let fixture = Fixture {
                local_edits: true,
                ..Fixture::default()
            };
            let (fs, _workspace, review, _requests, cx) = open_review(fixture, cx).await;
            let local_changes =
                review.read_with(cx, |review, _| review.local_changes().unwrap().clone());
            let repository = review.read_with(cx, |review, _| review.repository().clone());
            let engine = RepoPath::new("src/engine.rs").unwrap();
            let head: Oid = PR1_HEAD_OID.parse().unwrap();
            let map = |cx: &mut VisualTestContext| {
                local_changes.read_with(cx, |local_changes, cx| {
                    local_changes.working_to_head(&engine, 61..=61, head, cx)
                })
            };
            let read_head = |cx: &mut VisualTestContext| {
                local_changes.update(cx, |local_changes, cx| {
                    local_changes.read_head_texts(&repository, head, vec![engine.clone()], cx)
                });
                cx.run_until_parked();
            };
            // The working tree has two lines more at the top than HEAD.
            assert_eq!(map(cx), LineMapping::Lines(59..=59));
            read_head(cx);
            assert_eq!(map(cx), LineMapping::Lines(59..=59));

            // HEAD's text in git gains a line at the top (as a checkout of a commit that has it
            // would), before Zed reloads the comparison's base.
            fs.with_git_state(dot_git(), false, |state| {
                let text = state.head_contents.get(&engine).cloned().unwrap();
                state
                    .head_contents
                    .insert(engine.clone(), [b"local 1\n".as_slice(), &text].concat());
            })
            .unwrap();
            local_changes.update(cx, |local_changes, _| local_changes.head_texts = None);
            read_head(cx);
            assert_eq!(map(cx), LineMapping::Unknown);

            // Zed reloads it: the first local line is HEAD's now.
            fs.with_git_state(dot_git(), true, |_| {}).unwrap();
            cx.run_until_parked();
            assert_eq!(map(cx), LineMapping::Lines(60..=60));
            local_changes.read_with(cx, |local_changes, _| {
                assert!(!local_changes.is_reading_head_texts())
            });

            local_changes.update(cx, |local_changes, _| {
                if let Some(file) = local_changes.files.get_mut(&engine) {
                    file.state = LocalFileState::Failed;
                }
            });
            assert_eq!(map(cx), LineMapping::Failed);
            assert_eq!(
                local_changes.read_with(cx, |local_changes, cx| {
                    local_changes.head_to_working(&engine, 59..=59, cx)
                }),
                LineMapping::Lines(59..=59),
                "threads still show"
            );
        }

        /// A pull request review in a stack shows the stack, and steps up and down it to the
        /// open pull requests, each replacing the tab of the one before.
        #[gpui::test]
        async fn test_stack_navigation(cx: &mut TestAppContext) {
            use crate::{
                NextPullRequestInStack, PreviousPullRequestInStack, ReviewPanel,
                github::test_support::{stack_node, stacked_pull_request},
                test_support::{add_review_panel, click_selector, redraw, review_count, tab_title},
            };

            // #1 is between a merged #5 and a closed #8, with the draft #4 on top. #4's head is
            // #1's, so that it opens without a checkout.
            let stack = stack_node(
                7,
                vec![
                    stacked_pull_request(5, "MERGED", false, false),
                    stacked_pull_request(1, "OPEN", false, false),
                    stacked_pull_request(8, "CLOSED", false, false),
                    stacked_pull_request(4, "OPEN", true, false),
                ],
            );
            let mut pr4_details: serde_json::Value = serde_json::from_str(PR1_DETAILS).unwrap();
            let details = &mut pr4_details["data"]["repository"]["pullRequest"];
            details["number"] = json!(4);
            details["title"] = json!("Layer 4");
            details["isDraft"] = json!(true);
            let fail_details = Arc::new(AtomicBool::new(false));
            let respond = {
                let fail_details = fail_details.clone();
                move |request: &SentRequest| {
                    let number = request.variables()["number"].as_u64();
                    match request.operation() {
                    Some("PullRequestDetails")
                        if number == Some(4) && fail_details.load(Ordering::SeqCst) =>
                    {
                        Some(Reply::status(500, "{}"))
                    }
                    Some("PullRequestStackOf") => Some(Reply::json(
                        json!({ "data": { "repository": { "pullRequest": { "stack": stack } } } })
                            .to_string(),
                    )),
                    Some("PullRequestDetails") if number == Some(4) => {
                        Some(Reply::json(pr4_details.to_string()))
                    }
                    None if request.uri.contains("/pulls/4/files") => Some(Reply::json(PR1_FILES)),
                    _ => None,
                }
                }
            };
            let fixture = Fixture {
                respond: Some(Arc::new(respond)),
                ..Fixture::default()
            };
            let (_fs, workspace, review, _requests, cx) = open_review(fixture, cx).await;
            add_review_panel(&workspace, cx);
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.open_panel::<ReviewPanel>(window, cx)
            });
            cx.run_until_parked();
            let position = |review: &Entity<ReviewView>, cx: &mut VisualTestContext| {
                review.read_with(cx, |review, cx| {
                    review.pull_request().unwrap().read(cx).stack_position()
                })
            };
            assert_eq!(
                position(&review, cx),
                Some(StackPosition {
                    stack: 7,
                    position: 1,
                    count: 4,
                    below: None,
                    above: Some(4),
                })
            );
            redraw(cx);
            for selector in [
                "review-pull-request-stack",
                "review-toolbar-next-in-stack",
                "review-panel-stack",
            ] {
                assert!(
                    cx.debug_bounds(selector).is_some(),
                    "{selector} isn't drawn"
                );
            }

            // Nothing open below #1: a toast, and the review stays.
            dispatch(PreviousPullRequestInStack, cx);
            assert_eq!(notification_count(&workspace, cx), 1);
            assert_eq!(active_review(&workspace, cx), review);

            let opening_tab = |cx: &mut VisualTestContext| {
                workspace.read_with(cx, |workspace, cx| {
                    workspace.active_item_as::<crate::opening::OpeningReview>(cx)
                })
            };

            // Up skips the closed #8. #4's tab shows at once, before anything loaded; when #4
            // can't be loaded it says so, and closing it goes back to #1's review.
            fail_details.store(true, Ordering::SeqCst);
            cx.update(|window, cx| window.dispatch_action(Box::new(NextPullRequestInStack), cx));
            let opening = opening_tab(cx).expect("#4's tab shows at once");
            assert_eq!(
                workspace.read_with(cx, |workspace, cx| workspace
                    .active_item(cx)
                    .map(|item| item.tab_content_text(0, cx))),
                Some("PR #4: Layer 4".into())
            );
            redraw(cx);
            assert!(cx.debug_bounds("review-opening-status").is_some());
            cx.run_until_parked();
            assert!(opening.read_with(cx, |opening, _| opening.error().is_some()));
            assert_eq!(review_count(&workspace, cx), 1);
            let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
            let opening_id = opening.entity_id();
            pane.update_in(cx, |pane, window, cx| {
                pane.close_item_by_id(opening_id, SaveIntent::Skip, window, cx)
            })
            .await
            .unwrap();
            cx.run_until_parked();
            assert_eq!(active_review(&workspace, cx), review);
            fail_details.store(false, Ordering::SeqCst);

            // Loaded, #4's review replaces both tabs.
            cx.update(|window, cx| window.dispatch_action(Box::new(NextPullRequestInStack), cx));
            assert!(opening_tab(cx).is_some());
            cx.run_until_parked();
            assert!(opening_tab(cx).is_none());
            let open_items = workspace.read_with(cx, |workspace, cx| {
                workspace.active_pane().read(cx).items_len()
            });
            assert_eq!(open_items, 1);
            let next = active_review(&workspace, cx);
            assert_ne!(next, review);
            assert_eq!(review_count(&workspace, cx), 1);
            assert_eq!(tab_title(&next, cx), "PR #4: Layer 4");
            assert_eq!(
                position(&next, cx).map(|position| (position.position, position.below)),
                Some((3, Some(1)))
            );

            // `[ s` steps back down, and the toolbar's button back up.
            cx.focus(&next);
            cx.simulate_keystrokes("[ s");
            cx.run_until_parked();
            let back = active_review(&workspace, cx);
            assert_eq!(
                tab_title(&back, cx),
                "PR #1: Add template filters and retire the legacy renderer"
            );
            assert_eq!(review_count(&workspace, cx), 1);
            click_selector("review-toolbar-next-in-stack", cx);
            assert_eq!(
                tab_title(&active_review(&workspace, cx), cx),
                "PR #4: Layer 4"
            );
            assert_eq!(review_count(&workspace, cx), 1);
        }

        #[gpui::test]
        async fn test_description_and_threads_in_unified_view(cx: &mut TestAppContext) {
            let (_fs, workspace, review, requests, cx) =
                open_pull_request_review(DiffViewStyle::Unified, Default::default(), cx).await;
            let rhs = rhs_editor(&review, cx);
            let pull_request =
                review.read_with(cx, |review, _| review.pull_request().unwrap().clone());

            // The description block: expanded, with the description and the user's summary; the
            // bot's summary is hidden.
            let top_block = review.read_with(cx, |review, _| review.top_block().clone());
            top_block.read_with(cx, |top_block, cx| {
                assert!(matches!(
                    top_block.content(),
                    TopBlockContent::PullRequest { state: Some(_), .. }
                ));
                assert!(top_block.is_expanded());
                assert_eq!(
                    top_block.markdown_keys(),
                    ["PRR_kwDOVM9_N88AAAABSyT2YQ", "description"]
                );
                let description = top_block.markdown_source("description", cx).unwrap();
                assert_eq!(description, pull_request.read(cx).details().body);
                assert!(
                    top_block
                        .markdown_source(USER_SUMMARY, cx)
                        .unwrap()
                        .starts_with("Initial pass.")
                );
            });
            pull_request.read_with(cx, |pull_request, _| {
                assert_eq!(pull_request.hidden_bot_summaries(), 1);
                assert!(pull_request.load_errors().is_empty());
                assert_eq!(
                    pull_request.thread_counts(),
                    ThreadCounts {
                        resolved: 1,
                        outdated: 1,
                        bots: 2,
                    }
                );
            });

            // Resolved, outdated and bot threads are hidden.
            let mut shown = vec![
                thread(SINGLE),
                thread(RANGE),
                thread(DELETED_LINE),
                thread(FILE),
            ];
            shown.sort();
            let mut keys = remote_keys(&review, cx);
            keys.sort();
            assert_eq!(keys, shown);

            // Below their last line: a new line, a range of an added file, a deleted line.
            let row = move_to_row_with_text(&rhs, "ENGINE 134", cx);
            assert_eq!(
                thread_row(&review, SINGLE, cx),
                (false, display_row(&rhs, row, cx) + 1)
            );
            let row = move_to_row_with_text(&rhs, "filter 25", cx);
            assert_eq!(
                thread_row(&review, RANGE, cx),
                (false, display_row(&rhs, row, cx) + 1)
            );
            let row = move_to_row_with_text(&rhs, "engine 193", cx);
            assert_eq!(
                thread_row(&review, DELETED_LINE, cx),
                (false, display_row(&rhs, row, cx) + 1)
            );
            // The file thread is at the top of its file.
            let guide_row = move_to_row_with_text(&rhs, "GUIDE 30", cx);
            assert!(thread_row(&review, FILE, cx).1 < display_row(&rhs, guide_row, cx));
            let blocks = review.read_with(cx, |review, _| review.comments().clone());
            blocks.read_with(cx, |blocks, cx| {
                let locations = |id: &str| {
                    blocks
                        .remote_view(&thread(id))
                        .unwrap()
                        .read(cx)
                        .locations()
                };
                assert_eq!(locations(SINGLE), [None]);
                assert_eq!(locations(RANGE), [Some("Lines 20–25".into())]);
                assert_eq!(locations(DELETED_LINE), [Some("Old line 193".into())]);
                assert_eq!(locations(FILE), [Some("File comment".into())]);
            });

            // The toggles show the others, in every tab of the pull request.
            dispatch(ToggleResolvedThreads, cx);
            let row = move_to_row_with_text(&rhs, "STRINGS 48", cx);
            assert_eq!(
                thread_row(&review, RESOLVED, cx),
                (false, display_row(&rhs, row, cx) + 1)
            );
            dispatch(ToggleOutdatedThreads, cx);
            assert!(
                remote_keys(&review, cx)
                    .contains(&RemoteKey::Moved(RepoPath::new("src/engine.rs").unwrap()))
            );
            blocks.read_with(cx, |blocks, cx| {
                let moved = blocks
                    .remote_view(&RemoteKey::Moved(RepoPath::new("src/engine.rs").unwrap()))
                    .unwrap()
                    .read(cx);
                assert_eq!(
                    moved.locations(),
                    [Some("Originally line 223 at 5c5cdf4".into())]
                );
            });
            dispatch(ToggleBotReviews, cx);
            let row = move_to_row_with_text(&rhs, "filter 38", cx);
            assert_eq!(
                thread_row(&review, BOT, cx),
                (false, display_row(&rhs, row, cx) + 1)
            );
            top_block.read_with(cx, |top_block, _| {
                assert!(top_block.markdown_keys().contains(&BOT_SUMMARY.into()));
            });
            assert_eq!(remote_keys(&review, cx).len(), 7);
            for action in [
                Box::new(ToggleResolvedThreads) as Box<dyn gpui::Action>,
                Box::new(ToggleOutdatedThreads),
                Box::new(ToggleBotReviews),
            ] {
                cx.update(|window, cx| window.dispatch_action(action, cx));
                cx.run_until_parked();
            }
            assert_eq!(remote_keys(&review, cx).len(), 4);

            // `]t` visits them in the diff's order (a file thread is above the cursor's row) and
            // wraps around.
            move_to_row_with_text(&rhs, "GUIDE 30", cx);
            let targeted = |id: &str, cx: &mut VisualTestContext| {
                blocks.read_with(cx, |blocks, cx| {
                    blocks
                        .remote_view(&thread(id))
                        .unwrap()
                        .read(cx)
                        .is_targeted()
                })
            };
            cx.simulate_keystrokes("] t");
            assert_eq!(cursor_line(&rhs, cx), "ENGINE 134");
            assert!(targeted(SINGLE, cx));
            cx.simulate_keystrokes("] t");
            assert_eq!(cursor_line(&rhs, cx), "engine 193");
            assert!(targeted(DELETED_LINE, cx));
            assert!(!targeted(SINGLE, cx));
            cx.simulate_keystrokes("] t");
            assert_eq!(cursor_line(&rhs, cx), "filter 25");
            cx.simulate_keystrokes("] t");
            assert_eq!(
                path_at_cursor(&review, cx).as_deref(),
                Some("docs/guide.md")
            );
            assert!(targeted(FILE, cx));
            cx.simulate_keystrokes("[ t");
            assert_eq!(cursor_line(&rhs, cx), "filter 25");

            // Threads are read-only: `i` opens nothing, `dd` refuses, `; Y` copies the first
            // comment.
            cx.simulate_keystrokes("[ t [ t");
            assert_eq!(cursor_line(&rhs, cx), "ENGINE 134");
            notification_count(&workspace, cx);
            cx.simulate_keystrokes("i");
            assert!(!has_open_comment_editor(&review, cx));
            cx.simulate_keystrokes("d d");
            assert_eq!(notification_count(&workspace, cx), 1);
            assert!(!has_open_comment_editor(&review, cx));
            cx.simulate_keystrokes("; shift-y");
            assert!(
                cx.read_from_clipboard()
                    .and_then(|item| item.text())
                    .unwrap()
                    .starts_with("Splitting on `|`")
            );

            // The panel lists them, in the review's order.
            let rows = review.read_with(cx, |review, cx| review.panel_comments(cx));
            assert_eq!(
                rows.iter()
                    .map(|row| (row.key.clone(), row.location.to_string(), row.remote))
                    .collect::<Vec<_>>(),
                [
                    (
                        PanelCommentKey::Thread(FILE.into()),
                        "docs/guide.md".into(),
                        true
                    ),
                    (
                        PanelCommentKey::Thread(SINGLE.into()),
                        "src/engine.rs:134".into(),
                        true
                    ),
                    (
                        PanelCommentKey::Thread(DELETED_LINE.into()),
                        "src/engine.rs:~193".into(),
                        true
                    ),
                    (
                        PanelCommentKey::Thread(RANGE.into()),
                        "src/filters.rs:20-25".into(),
                        true
                    ),
                ]
            );
            assert_eq!(rows[1].item, ItemKey::Remote(thread(SINGLE)));
            assert!(rows[1].first_line.starts_with("drew-council: Splitting"));

            // Copy actions.
            dispatch(CopyPullRequestUrl, cx);
            assert_eq!(
                cx.read_from_clipboard()
                    .and_then(|item| item.text())
                    .as_deref(),
                Some("https://github.com/drew-council/code-review-sandbox/pull/1")
            );
            dispatch(CopyBranchName, cx);
            assert_eq!(
                cx.read_from_clipboard()
                    .and_then(|item| item.text())
                    .as_deref(),
                Some("feature/template-filters")
            );

            // Reloading asks GitHub again.
            let loads = requests_of(&requests, "ReviewThreads");
            dispatch(ReloadPullRequest, cx);
            assert_eq!(requests_of(&requests, "ReviewThreads"), loads + 1);
            assert_eq!(requests_of(&requests, "PullRequestDetails"), 2);
            assert_eq!(remote_keys(&review, cx).len(), 4);
        }

        #[gpui::test]
        async fn test_comments_only_on_lines_github_accepts(cx: &mut TestAppContext) {
            let (_fs, workspace, review, _requests, cx) =
                open_pull_request_review(DiffViewStyle::Unified, Default::default(), cx).await;
            let rhs = rhs_editor(&review, cx);
            let comment_count = |cx: &VisualTestContext| {
                review.read_with(cx, |review, cx| {
                    review.session().read(cx).comments().iter().count()
                })
            };
            notification_count(&workspace, cx);

            // Line 100 is outside GitHub's hunks: a toast says so and nothing opens.
            move_to_row_with_text(&rhs, "ENGINE 100", cx);
            cx.simulate_keystrokes("c");
            assert!(!has_open_comment_editor(&review, cx));
            assert_eq!(comment_count(cx), 0);
            assert_eq!(notification_count(&workspace, cx), 1);

            // Line 59 is inside one, and so is the deleted old line 193.
            for row_text in ["ENGINE 59", "engine 193"] {
                move_to_row_with_text(&rhs, row_text, cx);
                cx.simulate_keystrokes("c");
                assert!(has_open_comment_editor(&review, cx), "{row_text}");
                cx.simulate_input("ok");
                cx.simulate_keystrokes("ctrl-enter");
            }
            assert_eq!(comment_count(cx), 2);
            assert_eq!(notification_count(&workspace, cx), 0);

            // File and review comments.
            cx.simulate_keystrokes("shift-c");
            assert!(has_open_comment_editor(&review, cx));
            cx.simulate_input("file");
            cx.simulate_keystrokes("ctrl-enter");
            cx.simulate_keystrokes("; c");
            assert!(has_open_comment_editor(&review, cx));
            cx.simulate_input("review");
            cx.simulate_keystrokes("ctrl-enter");
            assert_eq!(comment_count(cx), 4);

            // On a row with a thread and a comment, `i` edits the comment.
            move_to_row_with_text(&rhs, "engine 193", cx);
            cx.simulate_keystrokes("i");
            assert!(has_open_comment_editor(&review, cx));
            cx.simulate_keystrokes("escape escape");
            assert!(!has_open_comment_editor(&review, cx));

            // The export has only the review's own comments.
            let markdown = review
                .read_with(cx, |review, cx| review.review_markdown(cx))
                .unwrap();
            assert!(markdown.contains("src/engine.rs:59"));
            assert!(!markdown.contains("Splitting on"));
        }

        /// A selection is checked as a whole, as GitHub checks a range: one across two of its
        /// hunks takes the comment (the ends are in hunks), as does one of unchanged lines
        /// inside a hunk.
        #[gpui::test]
        async fn test_range_comments_follow_the_comment_gate(cx: &mut TestAppContext) {
            let (_fs, workspace, review, _requests, cx) =
                open_pull_request_review(DiffViewStyle::Unified, Default::default(), cx).await;
            let rhs = rhs_editor(&review, cx);
            let refused_plus = |row: u32, cx: &mut VisualTestContext| {
                draw(cx);
                let row = display_row(&rhs, row, cx);
                cx.debug_bounds(format!("review-gutter-comment-refused-rhs-{row}").leak())
                    .is_some()
            };
            let comment_lines = |cx: &mut VisualTestContext| {
                review.read_with(cx, |review, cx| {
                    review
                        .session()
                        .read(cx)
                        .comments()
                        .iter()
                        .filter_map(|comment| match &comment.target {
                            CommentTarget::Lines { lines, .. } => Some(lines.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                })
            };
            notification_count(&workspace, cx);

            // R59 is in GitHub's hunk `+48,19`, R134 in `+130,20`.
            move_to_row_with_text(&rhs, "ENGINE 59", cx);
            cx.simulate_keystrokes("shift-v");
            for _ in 0..30 {
                if cursor_line(&rhs, cx) == "ENGINE 134" {
                    break;
                }
                cx.simulate_keystrokes("j");
            }
            assert_eq!(cursor_line(&rhs, cx), "ENGINE 134");
            assert!(!refused_plus(cursor_point(&rhs, cx).row, cx));
            cx.simulate_keystrokes("c");
            cx.run_until_parked();
            assert!(has_open_comment_editor(&review, cx));
            cx.simulate_input("across");
            cx.simulate_keystrokes("ctrl-enter");
            assert_eq!(comment_lines(cx), [59..=134]);

            // Unchanged lines inside one hunk.
            move_to_row_with_text(&rhs, "engine 57", cx);
            cx.simulate_keystrokes("shift-v j");
            assert!(!refused_plus(cursor_point(&rhs, cx).row, cx));
            cx.simulate_keystrokes("c");
            cx.run_until_parked();
            assert!(has_open_comment_editor(&review, cx));
            cx.simulate_input("context");
            cx.simulate_keystrokes("ctrl-enter");
            assert_eq!(comment_lines(cx).len(), 2);
            assert_eq!(notification_count(&workspace, cx), 0);
        }

        /// The gutter's "+" follows GitHub's comment gate: on a row it refuses, it's disabled,
        /// says why, and makes no comment; on a row it takes, it opens a comment editor.
        #[gpui::test]
        async fn test_gutter_button_follows_the_comment_gate(cx: &mut TestAppContext) {
            let (_fs, workspace, review, _requests, cx) =
                open_pull_request_review(DiffViewStyle::Unified, Default::default(), cx).await;
            let rhs = rhs_editor(&review, cx);
            let blocks = review.read_with(cx, |review, _| review.comments().clone());
            let plus_button = |selector: String, cx: &mut VisualTestContext| {
                draw(cx);
                cx.debug_bounds(selector.leak())
            };
            notification_count(&workspace, cx);

            let row = move_to_row_with_text(&rhs, "ENGINE 100", cx);
            let refused_row = display_row(&rhs, row, cx);
            assert!(plus_button(format!("review-gutter-comment-rhs-{refused_row}"), cx).is_none());
            let refused = plus_button(
                format!("review-gutter-comment-refused-rhs-{refused_row}"),
                cx,
            )
            .expect("the cursor row has a disabled \"+\"");
            assert_eq!(
                cx.update(|_, cx| {
                    blocks.update(cx, |blocks, cx| {
                        blocks.gutter_refusal(&rhs, refused_row, cx)
                    })
                })
                .as_deref(),
                Some("GitHub only accepts comments within its diff hunks")
            );
            cx.simulate_click(refused.center(), gpui::Modifiers::none());
            cx.run_until_parked();
            assert!(!has_open_comment_editor(&review, cx));
            assert_eq!(notification_count(&workspace, cx), 0);
            assert_eq!(
                review.read_with(cx, |review, cx| review
                    .session()
                    .read(cx)
                    .comments()
                    .iter()
                    .count()),
                0
            );

            for row_text in ["ENGINE 59", "engine 193"] {
                let row = move_to_row_with_text(&rhs, row_text, cx);
                let row = display_row(&rhs, row, cx);
                let plus = plus_button(format!("review-gutter-comment-rhs-{row}"), cx)
                    .unwrap_or_else(|| panic!("{row_text} takes comments"));
                cx.simulate_click(plus.center(), gpui::Modifiers::none());
                cx.run_until_parked();
                assert!(has_open_comment_editor(&review, cx), "{row_text}");
                cx.simulate_input("ok");
                cx.simulate_keystrokes("ctrl-enter");
            }
            assert_eq!(
                review.read_with(cx, |review, cx| review
                    .session()
                    .read(cx)
                    .comments()
                    .iter()
                    .count()),
                2
            );
            assert_eq!(notification_count(&workspace, cx), 0);
        }

        #[gpui::test]
        async fn test_threads_in_split_view(cx: &mut TestAppContext) {
            let (_fs, _workspace, review, _requests, cx) =
                open_pull_request_review(DiffViewStyle::Split, Default::default(), cx).await;
            let rhs = rhs_editor(&review, cx);

            // The deleted line's thread is on the left side, the others on the right.
            let lhs = lhs_editor(&review, cx);
            let row = move_to_row_with_text(&lhs, "engine 193", cx);
            let left_row = display_row(&lhs, row, cx);
            let (on_lhs, block_row) = thread_row(&review, DELETED_LINE, cx);
            assert!(on_lhs);
            assert!(block_row > left_row, "{block_row} {left_row}");
            let row = move_to_row_with_text(&rhs, "ENGINE 134", cx);
            assert_eq!(
                thread_row(&review, SINGLE, cx),
                (false, display_row(&rhs, row, cx) + 1)
            );

            // Unsplitting moves it below the removed row, splitting into the new left side.
            for split in [false, true, false, true] {
                cx.update(|window, cx| {
                    rhs.focus_handle(cx)
                        .dispatch_action(&editor::ToggleSplitDiff, window, cx)
                });
                cx.run_until_parked();
                assert_eq!(
                    splittable(&review, cx).read_with(cx, |editor, _| editor.is_split()),
                    split
                );
                assert_eq!(remote_keys(&review, cx).len(), 4, "split: {split}");
                assert_eq!(thread_row(&review, DELETED_LINE, cx).0, split);
            }
            assert_ne!(lhs_editor(&review, cx), lhs);

            // `]t` lands on the left side for it.
            move_to_row_with_text(&rhs, "ENGINE 134", cx);
            cx.simulate_keystrokes("] t");
            assert_eq!(cursor_line(&rhs, cx), "ENGINE 134");
            cx.simulate_keystrokes("] t");
            let lhs = lhs_editor(&review, cx);
            assert!(
                splittable(&review, cx).read_with(cx, |editor, _| editor.focused_editor() == &lhs)
            );
            assert_eq!(cursor_line(&lhs, cx), "engine 193");
        }

        /// Restricted Mode: no git runs to check out a pull request (in place or in a new
        /// worktree) in a repository Zed doesn't trust. Reading it is fine. (The fake
        /// repository's fetch would panic.)
        #[gpui::test]
        async fn test_no_checkout_in_restricted_mode(cx: &mut TestAppContext) {
            let (fs, workspace, _review, requests, cx) =
                open_pull_request_review(DiffViewStyle::Unified, Default::default(), cx).await;
            let repository = workspace.read_with(cx, |workspace, cx| {
                workspace.project().read(cx).active_repository(cx).unwrap()
            });
            assert!(!repository.read_with(cx, |repository, _| repository.is_trusted()));
            fs.with_git_state(dot_git(), true, |state| {
                state.refs.insert("HEAD".into(), C1.into());
            })
            .unwrap();
            cx.run_until_parked();
            let details_loaded = requests_of(&requests, "PullRequestDetails");

            for in_new_worktree in [false, true] {
                let task = workspace.update_in(cx, |workspace, window, cx| {
                    open_pull_request(
                        workspace,
                        repository.clone(),
                        crate::target::parse_pull_request_reference("1").unwrap(),
                        OpenPullRequestOptions {
                            in_new_worktree,
                            confirm_checkout: false,
                        },
                        window,
                        cx,
                    )
                });
                let error = task.await.unwrap_err();
                assert!(
                    error.downcast_ref::<checkout::RestrictedMode>().is_some(),
                    "{error:#}"
                );
            }
            assert_eq!(
                requests_of(&requests, "PullRequestDetails"),
                details_loaded + 2
            );
            assert_eq!(
                fs.with_git_state(dot_git(), false, |state| state.refs.get("HEAD").cloned())
                    .unwrap()
                    .as_deref(),
                Some(C1)
            );
        }

        /// A link from outside Zed (`zed://review?…&target=pr:1`) asks before it moves the
        /// checkout; `zed --review pr:1` doesn't, and neither does a link once the pull request is
        /// checked out.
        #[gpui::test]
        async fn test_link_checkout_asks_first(cx: &mut TestAppContext) {
            use crate::cli::tests::{Cli, clicked_link, command_line, exit, stderr};
            use project::git_store::RepositoryState;

            let (fs, workspace, review, requests, cx) =
                open_pull_request_review(DiffViewStyle::Unified, Default::default(), cx).await;
            drop(review);
            let repository = workspace.read_with(cx, |workspace, cx| {
                workspace.project().read(cx).active_repository(cx).unwrap()
            });
            let trusted = repository.update(cx, |repository, _| {
                repository.send_job("trust", None, |state, _| async move {
                    if let RepositoryState::Local(state) = state {
                        state.backend.set_trusted(true);
                    }
                })
            });
            cx.run_until_parked();
            trusted.await.unwrap();
            assert!(repository.read_with(cx, |repository, _| repository.is_trusted()));
            let set_head = |sha: &str| {
                fs.with_git_state(dot_git(), true, |state| {
                    state.refs.insert("HEAD".into(), sha.into());
                })
                .unwrap();
            };
            set_head(C1);
            fs.insert_file(
                path!("/project/.git/HEAD"),
                b"ref: refs/heads/main\n".to_vec(),
            )
            .await;
            cx.run_until_parked();
            let mut app_state = cx.update(|_, cx| workspace::AppState::test(cx));
            Arc::get_mut(&mut app_state).unwrap().fs = fs.clone();
            notification_count(&workspace, cx);

            let details_loaded = requests_of(&requests, "PullRequestDetails");
            let mut cli = Cli::run(clicked_link(path!("/project"), "pr:1"), &app_state, cx);
            assert_eq!(
                requests_of(&requests, "PullRequestDetails"),
                details_loaded + 1
            );
            let (message, detail) = cx.pending_prompt().unwrap();
            assert_eq!(message, "Check out pull request #1?");
            assert!(
                detail.starts_with(
                    "Reviewing drew-council/code-review-sandbox#1 checks out \
                     feature/template-filters in"
                ),
                "{detail}"
            );
            cx.simulate_prompt_answer("Cancel");
            cx.run_until_parked();
            assert_eq!(cli.received(), [stderr("Review cancelled"), exit(1)]);
            assert_eq!(notification_count(&workspace, cx), 0);
            assert_eq!(
                fs.with_git_state(dot_git(), false, |state| state.refs.get("HEAD").cloned())
                    .unwrap()
                    .as_deref(),
                Some(C1)
            );

            // Checked out already: nothing to ask, whoever sent it.
            set_head(PR1_HEAD_OID);
            cx.run_until_parked();
            for request in [
                command_line(path!("/project"), "pr:1", false),
                clicked_link(path!("/project"), "pr:1"),
            ] {
                let mut cli = Cli::run(request, &app_state, cx);
                assert!(!cx.has_pending_prompt());
                assert_eq!(cli.received(), [exit(0)]);
            }
            assert_eq!(
                workspace.read_with(cx, |workspace, cx| {
                    workspace.items_of_type::<ReviewView>(cx).count()
                }),
                1
            );
        }

        #[gpui::test]
        async fn test_threads_move_with_head_and_auth_failures(cx: &mut TestAppContext) {
            let fail_threads = Arc::new(AtomicBool::new(true));
            let (fs, _workspace, review, requests, cx) =
                open_pull_request_review(DiffViewStyle::Unified, fail_threads.clone(), cx).await;
            let pull_request =
                review.read_with(cx, |review, _| review.pull_request().unwrap().clone());

            // GitHub rejected the token: the forge was resolved again and the load retried once,
            // then the error shows. The rest loaded.
            pull_request.read_with(cx, |pull_request, _| {
                assert_eq!(
                    pull_request.load_errors(),
                    [
                        "GitHub rejected the GitHub CLI's token for github.com. Run \
                         \"gh auth login --hostname github.com\"."
                    ]
                );
                assert!(pull_request.commentable().is_some());
                assert!(pull_request.threads().is_empty());
            });
            let resolutions = requests_of(&requests, "RepositoryContext");
            assert_eq!(requests_of(&requests, "ReviewThreads"), 2);
            assert!(remote_keys(&review, cx).is_empty());

            // Another checkout reviewed the pull request since, and was closed (a `pr-N`
            // worktree's window): the forge is resolved from one still open.
            {
                let project = Project::test(fs.clone(), [Path::new(path!("/project"))], cx).await;
                cx.run_until_parked();
                let repository =
                    project.read_with(cx, |project, cx| project.active_repository(cx).unwrap());
                pull_request.update(cx, |pull_request, _| {
                    pull_request.add_checkout(&repository, &project)
                });
            }
            cx.run_until_parked();

            // A new token: reloading resolves the forge again.
            fail_threads.store(false, Ordering::SeqCst);
            dispatch(ReloadPullRequest, cx);
            assert_eq!(requests_of(&requests, "RepositoryContext"), resolutions + 1);
            pull_request.read_with(cx, |pull_request, _| {
                assert!(pull_request.load_errors().is_empty());
            });
            assert_eq!(remote_keys(&review, cx).len(), 4);

            // Another commit checked out: the line threads go to their files' items of moved
            // threads, the file thread stays; the pull request is loaded again.
            let threads_loaded = requests_of(&requests, "ReviewThreads");
            fs.with_git_state(dot_git(), true, |state| {
                state.refs.insert("HEAD".into(), C1.into());
            })
            .unwrap();
            cx.run_until_parked();
            let mut keys = remote_keys(&review, cx);
            keys.sort();
            let mut expected = vec![
                thread(FILE),
                RemoteKey::Moved(RepoPath::new("src/engine.rs").unwrap()),
                RemoteKey::Moved(RepoPath::new("src/filters.rs").unwrap()),
            ];
            expected.sort();
            assert_eq!(keys, expected);
            assert!(requests_of(&requests, "ReviewThreads") > threads_loaded);
        }

        /// GitHub numbers new lines in the pull request's head commit, the review shows the
        /// working tree: threads and comments are mapped through the checkout's changes, and
        /// lines changed locally take neither.
        #[gpui::test]
        async fn test_threads_and_comments_follow_local_changes(cx: &mut TestAppContext) {
            let fixture = Fixture {
                local_edits: true,
                ..Fixture::default()
            };
            let (_fs, workspace, review, _requests, cx) = open_review(fixture, cx).await;
            let rhs = rhs_editor(&review, cx);

            // HEAD's line 134 is two rows down in the working tree.
            let row = move_to_row_with_text(&rhs, "ENGINE 134", cx);
            assert_eq!(row_text(&rhs, row, cx), "ENGINE 134");
            assert_eq!(
                thread_row(&review, SINGLE, cx),
                (false, display_row(&rhs, row, cx) + 1)
            );
            // A line of the range changed locally: the thread is with the file's moved ones.
            let filters_moved = RemoteKey::Moved(RepoPath::new("src/filters.rs").unwrap());
            let keys = remote_keys(&review, cx);
            assert!(keys.contains(&filters_moved), "{keys:?}");
            assert!(!keys.contains(&thread(RANGE)));
            let blocks = review.read_with(cx, |review, _| review.comments().clone());
            blocks.read_with(cx, |blocks, cx| {
                assert_eq!(
                    blocks
                        .remote_view(&filters_moved)
                        .unwrap()
                        .read(cx)
                        .locations(),
                    [Some("Lines 20–25 at 3f22f1b · changed locally".into())]
                );
            });

            // HEAD's line 59 is inside GitHub's hunks; the inserted lines and the edited one
            // aren't in the pull request.
            notification_count(&workspace, cx);
            move_to_row_with_text(&rhs, "ENGINE 59", cx);
            cx.simulate_keystrokes("c");
            assert!(has_open_comment_editor(&review, cx));
            cx.simulate_input("ok");
            cx.simulate_keystrokes("ctrl-enter");
            let targets = review.read_with(cx, |review, cx| {
                review
                    .session()
                    .read(cx)
                    .comments()
                    .iter()
                    .map(|comment| comment.target.clone())
                    .collect::<Vec<_>>()
            });
            assert_eq!(
                targets,
                [CommentTarget::Lines {
                    path: RepoPath::new("src/engine.rs").unwrap(),
                    side: Side::New,
                    lines: 61..=61,
                }]
            );
            for row_text in ["local 1", "filter 22 edited"] {
                move_to_row_with_text(&rhs, row_text, cx);
                cx.simulate_keystrokes("c");
                assert!(!has_open_comment_editor(&review, cx), "{row_text}");
                assert_eq!(notification_count(&workspace, cx), 1, "{row_text}");
            }

            // Why: the tab's checkout decides.
            let (pull_request, local_changes) = review.read_with(cx, |review, _| {
                (
                    review.pull_request().unwrap().clone(),
                    review.local_changes().unwrap().clone(),
                )
            });
            let head: Oid = PR1_HEAD_OID.parse().unwrap();
            let other: Oid = C1.parse().unwrap();
            let engine = |lines: std::ops::RangeInclusive<u32>| CommentTarget::Lines {
                path: RepoPath::new("src/engine.rs").unwrap(),
                side: Side::New,
                lines,
            };
            let file = CommentTarget::File {
                path: RepoPath::new("docs/guide.md").unwrap(),
            };
            cx.update(|_, cx| {
                let check = |target: &CommentTarget, local_head: Oid| {
                    pull_request.read(cx).check_comment(
                        target,
                        Some(local_head),
                        local_changes.read(cx),
                        cx,
                    )
                };
                assert_eq!(check(&engine(61..=61), head), Ok(()));
                assert_eq!(
                    check(&engine(1..=3), head),
                    Err(
                        "These lines have local changes, which the pull request doesn't have"
                            .into()
                    )
                );
                // Line 100 of the working tree is HEAD's 98, outside GitHub's hunks.
                assert_eq!(
                    check(&engine(100..=100), head),
                    Err("GitHub only accepts comments within its diff hunks".into())
                );
                assert_eq!(
                    check(&engine(61..=61), other),
                    Err(
                        "Check out the pull request's head 3f22f1b to comment on its lines: \
                         GitHub numbers them in that commit"
                            .into()
                    )
                );
                assert_eq!(check(&file, other), Ok(()));
            });
        }

        /// Threads on one line are visited in the order their blocks show them (GitHub's), not
        /// by their ids.
        #[gpui::test]
        async fn test_threads_on_one_line_in_display_order(cx: &mut TestAppContext) {
            let second = serde_json::json!({
                "id": "PRRT_aaa",
                "isResolved": false,
                "isOutdated": false,
                "subjectType": "LINE",
                "path": "src/engine.rs",
                "line": 134,
                "startLine": null,
                "originalLine": 134,
                "originalStartLine": null,
                "diffSide": "RIGHT",
                "startDiffSide": null,
                "comments": {
                    "totalCount": 1,
                    "nodes": [{
                        "id": "PRRC_aaa",
                        "databaseId": 1,
                        "body": "Another reviewer",
                        "createdAt": "2026-10-17T04:00:00Z",
                        "url": "https://github.com/drew-council/code-review-sandbox/pull/1#discussion_r1",
                        "state": "SUBMITTED",
                        "commit": { "oid": PR1_HEAD_OID },
                        "author": { "__typename": "User", "login": "someone" },
                    }],
                },
            });
            let fixture = Fixture {
                extra_threads: vec![second],
                ..Fixture::default()
            };
            let (_fs, _workspace, review, _requests, cx) = open_review(fixture, cx).await;
            let rhs = rhs_editor(&review, cx);
            let (_, first_row) = thread_row(&review, SINGLE, cx);
            let (_, second_row) = thread_row(&review, "PRRT_aaa", cx);
            assert!(first_row < second_row, "{first_row} {second_row}");

            let blocks = review.read_with(cx, |review, _| review.comments().clone());
            let targeted = |id: &str, cx: &mut VisualTestContext| {
                blocks.read_with(cx, |blocks, cx| {
                    blocks
                        .remote_view(&thread(id))
                        .unwrap()
                        .read(cx)
                        .is_targeted()
                })
            };
            move_to_row_with_text(&rhs, "ENGINE 100", cx);
            cx.simulate_keystrokes("] t");
            assert!(targeted(SINGLE, cx));
            cx.simulate_keystrokes("] t");
            assert!(targeted("PRRT_aaa", cx));
            assert_eq!(cursor_line(&rhs, cx), "ENGINE 134");
            cx.simulate_keystrokes("] t");
            assert_eq!(cursor_line(&rhs, cx), "engine 193");
            cx.simulate_keystrokes("[ t");
            assert!(targeted("PRRT_aaa", cx));
            cx.simulate_keystrokes("[ t");
            assert!(targeted(SINGLE, cx));
        }

        /// The review's top stays its top when a thread of the first file shows up above that
        /// file's first row: the first file's header doesn't go under the sticky header.
        #[gpui::test]
        async fn test_top_of_the_review_stays_when_threads_load(cx: &mut TestAppContext) {
            let second_file_thread = serde_json::json!({
                "id": "PRRT_guide",
                "isResolved": false,
                "isOutdated": false,
                "subjectType": "FILE",
                "path": "docs/guide.md",
                "line": 1,
                "startLine": null,
                "originalLine": 1,
                "originalStartLine": null,
                "diffSide": "RIGHT",
                "startDiffSide": null,
                "comments": {
                    "totalCount": 1,
                    "nodes": [{
                        "id": "PRRC_guide",
                        "databaseId": 3,
                        "body": "Another file comment",
                        "createdAt": "2026-10-17T04:00:00Z",
                        "url": "https://github.com/drew-council/code-review-sandbox/pull/1#discussion_r3",
                        "state": "SUBMITTED",
                        "commit": { "oid": PR1_HEAD_OID },
                        "author": { "__typename": "User", "login": "someone" },
                    }],
                },
            });
            let add_thread = Arc::new(AtomicBool::new(false));
            let respond = {
                let add_thread = add_thread.clone();
                move |request: &SentRequest| {
                    if request.operation() != Some("ReviewThreads")
                        || !add_thread.load(Ordering::SeqCst)
                    {
                        return None;
                    }
                    let mut threads: serde_json::Value = serde_json::from_str(PR1_THREADS).unwrap();
                    threads["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"]
                        .as_array_mut()
                        .unwrap()
                        .push(second_file_thread.clone());
                    Some(Reply::json(threads.to_string()))
                }
            };
            let fixture = Fixture {
                respond: Some(Arc::new(respond)),
                ..Fixture::default()
            };
            let (_fs, _workspace, review, _requests, cx) = open_review(fixture, cx).await;
            let rhs = rhs_editor(&review, cx);
            assert_eq!(
                path_at_cursor(&review, cx).as_deref(),
                Some("docs/guide.md")
            );
            // The cursor away from the file's first row, and the view scrolled to the top (as an
            // autoscroll does, which anchors it on the first row).
            cx.simulate_keystrokes("j j");
            rhs.update_in(cx, |editor, window, cx| {
                editor.set_scroll_position(gpui::point(0., 0.), window, cx);
            });
            cx.run_until_parked();
            let scroll_top = |cx: &mut VisualTestContext| {
                rhs.update(cx, |editor, cx| editor.scroll_position(cx).y)
            };
            assert_eq!(scroll_top(cx), 0.);

            add_thread.store(true, Ordering::SeqCst);
            let pull_request = review.read_with(cx, |review, _| review.pull_request().cloned());
            pull_request
                .unwrap()
                .update(cx, |pull_request, cx| pull_request.reload(cx));
            cx.run_until_parked();
            assert!(remote_keys(&review, cx).contains(&thread("PRRT_guide")));
            assert_eq!(scroll_top(cx), 0.);
        }

        /// GitHub's references in the description, review summaries and threads link to their
        /// issues on GitHub, and a click opens them.
        #[gpui::test]
        async fn test_references_link_to_github(cx: &mut TestAppContext) {
            let mut details: serde_json::Value = serde_json::from_str(PR1_DETAILS).unwrap();
            details["data"]["repository"]["pullRequest"]["body"] =
                json!("GH-7 fixes it, after other/repo#8.\n\nNot `#9`.");
            let mut summaries: serde_json::Value = serde_json::from_str(PR1_SUMMARIES).unwrap();
            let summary = summaries["data"]["repository"]["pullRequest"]["reviews"]["nodes"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|summary| summary["id"] == USER_SUMMARY)
                .unwrap();
            summary["body"] = json!("Initial pass. Like #3.");
            let thread_with_reference = json!({
                "id": "PRRT_reference",
                "isResolved": false,
                "isOutdated": false,
                "subjectType": "FILE",
                "path": "src/util/strings.rs",
                "line": null,
                "startLine": null,
                "originalLine": null,
                "originalStartLine": null,
                "diffSide": "RIGHT",
                "startDiffSide": null,
                "comments": {
                    "totalCount": 1,
                    "nodes": [{
                        "id": "PRRC_reference",
                        "databaseId": 3,
                        "body": "Same as #5",
                        "createdAt": "2026-10-17T04:00:00Z",
                        "url": "https://github.com/drew-council/code-review-sandbox/pull/1#discussion_r3",
                        "state": "SUBMITTED",
                        "commit": { "oid": PR1_HEAD_OID },
                        "author": { "__typename": "User", "login": "someone" },
                    }],
                },
            });
            let respond = move |request: &SentRequest| match request.operation() {
                Some("PullRequestDetails") => Some(Reply::json(details.to_string())),
                Some("ReviewSummaries") => Some(Reply::json(summaries.to_string())),
                _ => None,
            };
            let fixture = Fixture {
                respond: Some(Arc::new(respond)),
                extra_threads: vec![thread_with_reference],
                ..Fixture::default()
            };
            let (_fs, _workspace, review, _requests, cx) = open_review(fixture, cx).await;
            let issue = |number: u32| {
                format!("https://github.com/drew-council/code-review-sandbox/issues/{number}")
            };

            let top_block = review.read_with(cx, |review, _| review.top_block().clone());
            top_block.read_with(cx, |top_block, cx| {
                assert_eq!(
                    top_block.markdown_source("description", cx).unwrap(),
                    format!(
                        "[GH-7]({}) fixes it, after \
                         [other/repo#8](https://github.com/other/repo/issues/8).\n\nNot `#9`.",
                        issue(7)
                    )
                );
                assert_eq!(
                    top_block.markdown_source(USER_SUMMARY, cx).unwrap(),
                    format!("Initial pass. Like [#3]({}).", issue(3))
                );
            });
            let blocks = review.read_with(cx, |review, _| review.comments().clone());
            let thread_bodies = blocks.read_with(cx, |blocks, cx| {
                blocks
                    .remote_view(&thread("PRRT_reference"))
                    .unwrap()
                    .read(cx)
                    .body_sources(cx)
            });
            assert_eq!(thread_bodies, [format!("Same as [#5]({})", issue(5))]);

            // The description starts with its first link: a click there opens the issue.
            draw(cx);
            let description = cx
                .debug_bounds("review-pull-request-description-text")
                .expect("the description shows");
            cx.simulate_click(
                description.origin + gpui::point(gpui::px(4.), gpui::px(6.)),
                gpui::Modifiers::none(),
            );
            cx.run_until_parked();
            assert_eq!(cx.opened_url(), Some(issue(7)));
        }

        /// A file thread that shows up above the cursor on the file's first row (a reload, as
        /// after a submit) is shown with the file's header at the top, rather than where the
        /// view's top happens to be, which may leave it under the sticky header.
        #[gpui::test]
        async fn test_reload_keeps_the_file_header_at_the_top(cx: &mut TestAppContext) {
            let file_thread = serde_json::json!({
                "id": "PRRT_file",
                "isResolved": false,
                "isOutdated": false,
                "subjectType": "FILE",
                "path": "src/util/strings.rs",
                "line": 1,
                "startLine": null,
                "originalLine": 1,
                "originalStartLine": null,
                "diffSide": "RIGHT",
                "startDiffSide": null,
                "comments": {
                    "totalCount": 1,
                    "nodes": [{
                        "id": "PRRC_file",
                        "databaseId": 2,
                        "body": "A file comment\nover\nseveral\nlines",
                        "createdAt": "2026-10-17T04:00:00Z",
                        "url": "https://github.com/drew-council/code-review-sandbox/pull/1#discussion_r2",
                        "state": "SUBMITTED",
                        "commit": { "oid": PR1_HEAD_OID },
                        "author": { "__typename": "User", "login": "someone" },
                    }],
                },
            });
            let add_thread = Arc::new(AtomicBool::new(false));
            let respond = {
                let add_thread = add_thread.clone();
                move |request: &SentRequest| {
                    if request.operation() != Some("ReviewThreads")
                        || !add_thread.load(Ordering::SeqCst)
                    {
                        return None;
                    }
                    let mut threads: serde_json::Value = serde_json::from_str(PR1_THREADS).unwrap();
                    threads["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"]
                        .as_array_mut()
                        .unwrap()
                        .push(file_thread.clone());
                    Some(Reply::json(threads.to_string()))
                }
            };
            let fixture = Fixture {
                respond: Some(Arc::new(respond)),
                ..Fixture::default()
            };
            let (_fs, _workspace, review, _requests, cx) = open_review(fixture, cx).await;
            let rhs = rhs_editor(&review, cx);
            let scroll_and_header_rows = |cx: &mut VisualTestContext| {
                rhs.update(cx, |editor, cx| {
                    let snapshot = editor.display_snapshot(cx);
                    let cursor = editor.selections.newest::<Point>(&snapshot).head();
                    let cursor_row = snapshot.point_to_display_point(cursor, Bias::Left).row();
                    let header_row = snapshot
                        .blocks_in_range(editor::display_map::DisplayRow(0)..cursor_row)
                        .filter(|(_, block)| block.is_buffer_header())
                        .map(|(row, _)| row)
                        .last()
                        .unwrap();
                    (editor.scroll_position(cx).y, f64::from(header_row.0))
                })
            };

            cx.simulate_keystrokes("] f");
            assert_eq!(
                path_at_cursor(&review, cx).as_deref(),
                Some("src/util/strings.rs")
            );
            let (scroll_top, header_row) = scroll_and_header_rows(cx);
            assert_eq!(scroll_top, header_row);
            // The view's top inside the file, as a centered scroll may leave it.
            rhs.update_in(cx, |editor, window, cx| {
                let position = editor.scroll_position(cx);
                editor.set_scroll_position(gpui::point(position.x, position.y + 1.5), window, cx);
            });
            cx.run_until_parked();

            add_thread.store(true, Ordering::SeqCst);
            let pull_request = review.read_with(cx, |review, _| review.pull_request().cloned());
            pull_request
                .unwrap()
                .update(cx, |pull_request, cx| pull_request.reload(cx));
            cx.run_until_parked();
            assert!(remote_keys(&review, cx).contains(&thread("PRRT_file")));
            let (scroll_top, header_row) = scroll_and_header_rows(cx);
            assert_eq!(scroll_top, header_row);
        }

        /// The first blocks of a side: (display row, id, height, whether it's a spacer).
        fn first_blocks(
            editor: &Entity<Editor>,
            cx: &mut VisualTestContext,
        ) -> Vec<(u32, editor::display_map::BlockId, u32, bool)> {
            editor.update(cx, |editor, cx| {
                let snapshot = editor.display_snapshot(cx);
                snapshot
                    .blocks_in_range(editor::display_map::DisplayRow(0)..snapshot.max_point().row())
                    .take(2)
                    .map(|(row, block)| {
                        (
                            row.0,
                            block.id(),
                            block.height(),
                            block.style() == editor::display_map::BlockStyle::Spacer,
                        )
                    })
                    .collect()
            })
        }

        fn draw(cx: &mut VisualTestContext) {
            cx.update(|window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            });
        }

        /// The overview is one block whatever happens to the diff: resizes, scrolling, reloads,
        /// wrap and split toggles, and reviewed files folding at the start of the diff (a folded
        /// first file used to get another copy of the overview each time it was rebuilt).
        #[gpui::test]
        async fn test_overview_stays_one_block(cx: &mut TestAppContext) {
            for split in [false, true] {
                let mut test_cx = cx.clone();
                let (_fs, _workspace, review, _requests, cx) = open_review(
                    Fixture {
                        split,
                        ..Fixture::default()
                    },
                    &mut test_cx,
                )
                .await;
                let overview = review
                    .read_with(cx, |review, _| review.top_block_id())
                    .expect("the overview has a block");
                let count = |cx: &mut VisualTestContext| {
                    let rhs = rhs_editor(&review, cx);
                    draw(cx);
                    rhs.update(cx, |editor, cx| {
                        let snapshot = editor.display_snapshot(cx);
                        snapshot
                            .blocks_in_range(
                                editor::display_map::DisplayRow(0)
                                    ..editor::display_map::DisplayRow(
                                        snapshot.max_point().row().0 + 1,
                                    ),
                            )
                            .filter(|(_, block)| {
                                block.id() == editor::display_map::BlockId::Custom(overview)
                            })
                            .count()
                    })
                };
                assert_eq!(count(cx), 1, "split {split}: opened");
                for (width, height) in [(700., 500.), (1600., 1000.), (900., 700.), (1600., 1000.)]
                {
                    cx.simulate_resize(gpui::size(gpui::px(width), gpui::px(height)));
                    cx.run_until_parked();
                    assert_eq!(count(cx), 1, "split {split}: {width}x{height}");
                }
                cx.simulate_keystrokes("shift-g g g ctrl-d ctrl-d ctrl-u ctrl-u");
                assert_eq!(count(cx), 1, "split {split}: scrolled");
                dispatch(crate::ReloadPullRequest, cx);
                assert_eq!(count(cx), 1, "split {split}: reloaded");
                dispatch(editor::actions::ToggleSoftWrap, cx);
                dispatch(editor::actions::ToggleSoftWrap, cx);
                assert_eq!(count(cx), 1, "split {split}: wrap toggled");
                dispatch(editor::ToggleSplitDiff, cx);
                dispatch(editor::ToggleSplitDiff, cx);
                assert_eq!(count(cx), 1, "split {split}: split toggled");

                // Reviewed files fold to their header, the first ones too.
                let rhs = rhs_editor(&review, cx);
                for reviewed in 1..=3 {
                    cx.focus(&rhs);
                    cx.simulate_keystrokes("g g r");
                    cx.run_until_parked();
                    assert_eq!(count(cx), 1, "split {split}: {reviewed} files reviewed");
                }
            }
        }

        /// Over a long line (300 columns, in the uncommitted fixture's `long.txt`), `ctrl-u` and
        /// `ctrl-d` move whole lines, as in any Zed buffer that doesn't wrap: the cursor stays
        /// at the start of lines and never lands along the long line.
        #[gpui::test]
        async fn test_half_page_scrolls_move_whole_lines(cx: &mut TestAppContext) {
            use editor::SelectionEffects;
            crate::test_support::init_vim_test(cx, DiffViewStyle::Unified);
            let (_fs, _workspace, review, cx) = crate::test_support::open_review_fixture(cx).await;
            cx.simulate_resize(gpui::size(gpui::px(800.), gpui::px(400.)));
            cx.run_until_parked();
            let rhs = rhs_editor(&review, cx);
            let position = |cx: &mut VisualTestContext| {
                draw(cx);
                rhs.update(cx, |editor, cx| {
                    let snapshot = editor.display_snapshot(cx);
                    let head = editor.selections.newest::<Point>(&snapshot).head();
                    (editor.scroll_position(cx).y, head.row, head.column)
                })
            };
            let last_row = rhs.update(cx, |editor, cx| {
                editor.buffer().read(cx).snapshot(cx).max_point().row
            });
            rhs.update_in(cx, |editor, window, cx| {
                editor.change_selections(
                    SelectionEffects::scroll(editor::scroll::Autoscroll::center()),
                    window,
                    cx,
                    |selections| {
                        selections.select_ranges([Point::new(last_row, 0)..Point::new(last_row, 0)])
                    },
                )
            });
            for keys in ["ctrl-u", "ctrl-d"] {
                let mut trace = vec![position(cx)];
                for _ in 0..12 {
                    cx.simulate_keystrokes(keys);
                    trace.push(position(cx));
                }
                assert!(
                    trace.iter().all(|(_, _, column)| *column == 0),
                    "{keys}: {trace:?}"
                );
                let rows = trace.iter().map(|(_, row, _)| *row);
                let (first, last) = (trace[0].1, trace[trace.len() - 1].1);
                assert_eq!(
                    last,
                    if keys == "ctrl-u" { 0 } else { last_row },
                    "{keys}: {trace:?}"
                );
                assert!(rows.count() > 1 && first != last, "{keys}: {trace:?}");
            }
        }

        /// An idle review doesn't redraw: drawing frames with nothing happening notifies none of
        /// its views or editors (a view notified on every frame redraws the whole review every
        /// frame, which keys then wait behind).
        #[gpui::test]
        async fn test_idle_review_stays_idle(cx: &mut TestAppContext) {
            use crate::github::test_support::{stack_node, stacked_pull_request};
            // A description like a real one: paragraphs, long lines, a list, links, code.
            let paragraph = "This adds SBC end dates, as already extracted by the extractor, as a \
                data source for Connection.EstimatedMedicalPlanYearEnd, and backfills the \
                existing rows; see https://github.com/drew-council/code-review-sandbox/issues/7 \
                and #5 for the background. ";
            let body = format!(
                "{}\n\n- one\n- two with `code`\n- three\n\n```rust\nfn main() {{}}\n```\n\n{}",
                paragraph.repeat(3),
                paragraph.repeat(2)
            );
            let mut details: serde_json::Value = serde_json::from_str(PR1_DETAILS).unwrap();
            details["data"]["repository"]["pullRequest"]["body"] = json!(body);
            let stack = stack_node(
                7,
                (1..=7)
                    .map(|number| stacked_pull_request(number, "OPEN", number == 7, false))
                    .collect(),
            );
            let respond = move |request: &SentRequest| match request.operation() {
                Some("PullRequestDetails") => Some(Reply::json(details.to_string())),
                Some("PullRequestStackOf") => Some(Reply::json(
                    json!({ "data": { "repository": { "pullRequest": { "stack": stack } } } })
                        .to_string(),
                )),
                _ => None,
            };
            let respond: Arc<dyn Fn(&SentRequest) -> Option<Reply> + Send + Sync> =
                Arc::new(respond);
            for (split, width) in [
                (false, 1600.),
                (false, 1100.),
                (true, 1700.),
                (true, 1300.),
                (true, 900.),
            ] {
                let mut test_cx = cx.clone();
                let (_fs, workspace, review, _requests, cx) = open_review(
                    Fixture {
                        split,
                        respond: Some(respond.clone()),
                        ..Fixture::default()
                    },
                    &mut test_cx,
                )
                .await;
                cx.simulate_resize(gpui::size(gpui::px(width), gpui::px(1000.)));
                cx.run_until_parked();
                crate::test_support::add_review_panel(&workspace, cx);
                workspace.update_in(cx, |workspace, window, cx| {
                    workspace.open_panel::<crate::ReviewPanel>(window, cx)
                });
                let rhs = rhs_editor(&review, cx);
                cx.focus(&rhs);
                // Reviewed files fold, as in a review under way: the first ones here.
                for _ in 0..3 {
                    cx.focus(&rhs);
                    cx.simulate_keystrokes("g g r");
                    cx.run_until_parked();
                }
                cx.focus(&rhs);
                fn watch<T: 'static>(
                    name: &'static str,
                    entity: &Entity<T>,
                    notified: &Rc<RefCell<Vec<&'static str>>>,
                    cx: &mut VisualTestContext,
                ) -> gpui::Subscription {
                    let notified = notified.clone();
                    cx.update(|_, cx| {
                        cx.observe(entity, move |_, _| notified.borrow_mut().push(name))
                    })
                }
                let notified = Rc::new(RefCell::new(Vec::<&'static str>::new()));
                let splittable = splittable(&review, cx);
                let mut subscriptions = vec![
                    watch("review", &review, &notified, cx),
                    watch("splittable", &splittable, &notified, cx),
                    watch("rhs", &rhs, &notified, cx),
                ];
                // Too narrow for two sides, a split diff shows one.
                if splittable.read_with(cx, |editor, _| editor.is_split()) {
                    let lhs = lhs_editor(&review, cx);
                    subscriptions.push(watch("lhs", &lhs, &notified, cx));
                }
                let frame = |cx: &mut VisualTestContext| {
                    cx.update(|window, cx| window.draw(cx).clear(cx));
                    cx.run_until_parked();
                };
                for _ in 0..3 {
                    frame(cx);
                }
                notified.borrow_mut().clear();
                for _ in 0..10 {
                    frame(cx);
                }
                assert_eq!(
                    *notified.borrow(),
                    Vec::<&str>::new(),
                    "split {split} at {width}"
                );
                drop(subscriptions);
            }
        }

        /// `ctrl-u` and `ctrl-d` from all over a review with tall blocks (the overview, file
        /// headers, comments) move the view and the cursor one way only, until the cursor reaches
        /// the diff's first or last row. Zed's scroll top used to be the scroll anchor's row even
        /// when the view's top was in the blocks above it, and vim moved a cursor that landed in a
        /// block to the row above: `ctrl-u` dragged the cursor down, `ctrl-d` left it behind.
        async fn assert_half_page_scrolls_go_one_way(
            split: bool,
            reviewed_files: usize,
            cx: &mut TestAppContext,
        ) {
            use editor::SelectionEffects;
            let (_fs, _workspace, review, _requests, cx) = open_review(
                Fixture {
                    split,
                    ..Fixture::default()
                },
                cx,
            )
            .await;
            let rhs = rhs_editor(&review, cx);
            for text in ["ENGINE 59", "filter 25"] {
                move_to_row_with_text(&rhs, text, cx);
                cx.simulate_keystrokes("c");
                cx.simulate_input("one\ntwo\nthree\nfour\nfive\nsix\nseven\neight");
                cx.simulate_keystrokes("ctrl-enter");
            }
            // Reviewed files fold to their header, the first ones of the diff here.
            for _ in 0..reviewed_files {
                cx.focus(&rhs);
                cx.simulate_keystrokes("g g r");
                cx.run_until_parked();
            }
            cx.focus(&rhs);
            let position = |cx: &mut VisualTestContext| {
                draw(cx);
                rhs.update(cx, |editor, cx| {
                    let snapshot = editor.display_snapshot(cx);
                    let head = editor.selections.newest::<Point>(&snapshot).head();
                    (editor.scroll_position(cx).y, head.row, head.column)
                })
            };
            for (width, height) in [(800., 400.), (1600., 1000.)] {
                cx.simulate_resize(gpui::size(gpui::px(width), gpui::px(height)));
                cx.run_until_parked();
                let last_row = rhs.update(cx, |editor, cx| {
                    editor.buffer().read(cx).snapshot(cx).max_point().row
                });
                for start in (0..=last_row).step_by(9) {
                    for (keys, up) in [("ctrl-u", true), ("ctrl-d", false)] {
                        rhs.update_in(cx, |editor, window, cx| {
                            editor.change_selections(
                                SelectionEffects::scroll(editor::scroll::Autoscroll::center()),
                                window,
                                cx,
                                |selections| {
                                    selections
                                        .select_ranges([Point::new(start, 0)..Point::new(start, 0)])
                                },
                            )
                        });
                        let mut previous = position(cx);
                        let mut trace = vec![previous];
                        loop {
                            cx.simulate_keystrokes(keys);
                            let current = position(cx);
                            trace.push(current);
                            let one_way = if up {
                                current.0 <= previous.0 && current.1 <= previous.1
                            } else {
                                current.0 >= previous.0 && current.1 >= previous.1
                            };
                            assert!(
                                one_way,
                                "{keys} at {width}x{height} from row {start}: {trace:?}"
                            );
                            // A half page moves rows (or nothing at an end), not along a line.
                            assert!(
                                current.0 != previous.0
                                    || current.1 != previous.1
                                    || current.2 == previous.2,
                                "{keys} moved within a line at {width}x{height} from row \
                                 {start}: {trace:?}"
                            );
                            if current == previous {
                                break;
                            }
                            assert!(trace.len() < 80, "{keys} doesn't stop: {trace:?}");
                            previous = current;
                        }
                        let end = if up { 0 } else { last_row };
                        assert_eq!(
                            previous.1, end,
                            "{keys} at {width}x{height} from row {start}: {trace:?}"
                        );
                    }
                }
            }
        }

        #[gpui::test]
        async fn test_half_page_scrolls_go_one_way(cx: &mut TestAppContext) {
            assert_half_page_scrolls_go_one_way(false, 0, cx).await
        }

        #[gpui::test]
        async fn test_half_page_scrolls_go_one_way_in_split_view(cx: &mut TestAppContext) {
            assert_half_page_scrolls_go_one_way(true, 0, cx).await
        }

        #[gpui::test]
        async fn test_half_page_scrolls_go_one_way_over_reviewed_files(cx: &mut TestAppContext) {
            assert_half_page_scrolls_go_one_way(false, 2, cx).await
        }

        #[gpui::test]
        async fn test_half_page_scrolls_go_one_way_over_reviewed_files_in_split_view(
            cx: &mut TestAppContext,
        ) {
            assert_half_page_scrolls_go_one_way(true, 2, cx).await
        }

        /// Vim's half-page scrolls run to either end of a review with the overview above its
        /// first file, expanded or collapsed, moving the view and the cursor one way only.
        #[gpui::test]
        async fn test_half_page_scrolls_reach_both_ends(cx: &mut TestAppContext) {
            let (_fs, _workspace, review, _requests, cx) =
                open_review(Fixture::default(), cx).await;
            let rhs = rhs_editor(&review, cx);
            let position = |cx: &mut VisualTestContext| {
                draw(cx);
                let scroll_top = rhs.update(cx, |editor, cx| editor.scroll_position(cx).y);
                (scroll_top, cursor_point(&rhs, cx).row)
            };
            let last_row = rhs.update(cx, |editor, cx| {
                editor.buffer().read(cx).snapshot(cx).max_point().row
            });

            for collapsed in [false, true] {
                if collapsed {
                    dispatch(crate::ToggleHeader, cx);
                    cx.run_until_parked();
                }
                cx.simulate_keystrokes("shift-g");
                let mut previous = position(cx);
                assert!(previous.0 > 0., "{previous:?}");
                for _ in 0..20 {
                    cx.simulate_keystrokes("ctrl-u");
                    let current = position(cx);
                    assert!(
                        current.0 <= previous.0 && current.1 <= previous.1,
                        "ctrl-u went from {previous:?} to {current:?} (collapsed: {collapsed})"
                    );
                    previous = current;
                }
                assert_eq!(previous.0, 0., "collapsed: {collapsed}");

                for _ in 0..20 {
                    cx.simulate_keystrokes("ctrl-d");
                    let current = position(cx);
                    assert!(
                        current.0 >= previous.0 && current.1 >= previous.1,
                        "ctrl-d went from {previous:?} to {current:?} (collapsed: {collapsed})"
                    );
                    previous = current;
                }
                assert_eq!(previous.1, last_row, "collapsed: {collapsed}");
            }

            cx.simulate_keystrokes("g g");
            for row in 1..=5 {
                cx.simulate_keystrokes("j");
                assert_eq!(cursor_point(&rhs, cx).row, row);
            }
        }

        /// The overview (the description, the reviews, the thread toggles, the review comments)
        /// is a block above the first file: it scrolls away with the diff like the rest of the
        /// document, nothing in it scrolls on its own, and it collapses to its header line.
        #[gpui::test]
        async fn test_overview_scrolls_with_the_diff(cx: &mut TestAppContext) {
            use editor::display_map::BlockId;

            let (_fs, _workspace, review, _requests, cx) =
                open_review(Fixture::default(), cx).await;
            let rhs = rhs_editor(&review, cx);
            let overview = review
                .read_with(cx, |review, _| review.top_block_id())
                .expect("the overview has a block");
            let blocks = first_blocks(&rhs, cx);
            assert_eq!(blocks[0].0, 0);
            assert_eq!(blocks[0].1, BlockId::Custom(overview));
            let overview_rows = blocks[0].2;
            assert!(
                matches!(blocks[1].1, BlockId::ExcerptBoundary(_)),
                "the first file's header follows: {blocks:?}"
            );
            assert_eq!(blocks[1].0, overview_rows);

            let scroll_top = |cx: &mut VisualTestContext| {
                rhs.update(cx, |editor, cx| editor.scroll_position(cx).y)
            };
            let scroll_to = |y: f64, cx: &mut VisualTestContext| {
                rhs.update_in(cx, |editor, window, cx| {
                    editor.set_scroll_position(gpui::point(0., y), window, cx)
                });
                cx.run_until_parked();
                draw(cx);
            };
            let has_sticky_header = |cx: &mut VisualTestContext| {
                rhs.update_in(cx, |editor, window, cx| {
                    let snapshot = editor.snapshot(window, cx);
                    snapshot
                        .sticky_header_excerpt(snapshot.scroll_position().y)
                        .is_some()
                })
            };

            // Opened at the top, the description in full (it scrolled after 8 lines).
            assert_eq!(scroll_top(cx), 0.);
            assert_eq!(
                path_at_cursor(&review, cx).as_deref(),
                Some("docs/guide.md")
            );
            draw(cx);
            let line_height = rhs.update_in(cx, |editor, window, cx| {
                editor
                    .style(cx)
                    .text
                    .line_height_in_pixels(window.rem_size())
            });
            let description = cx
                .debug_bounds("review-pull-request-description-text")
                .expect("the description shows");
            assert!(
                description.size.height > line_height * 8.,
                "{description:?}, {line_height:?}"
            );
            assert!(!has_sticky_header(cx));

            // Scrolled past it, it's gone with the rows above, and the first file's header is
            // the sticky one; scrolled back, it shows again.
            scroll_to(f64::from(overview_rows) + 1., cx);
            assert!(cx.debug_bounds("review-overview").is_none());
            assert!(has_sticky_header(cx));
            scroll_to(f64::from(overview_rows) - 2., cx);
            assert!(cx.debug_bounds("review-overview").is_some());
            assert!(!has_sticky_header(cx));

            // Its disclosure collapses it to the header line, which `; t`'s action expands.
            scroll_to(0., cx);
            let disclosure = cx
                .debug_bounds("review-pull-request-disclosure")
                .expect("the disclosure shows");
            cx.simulate_click(disclosure.center(), gpui::Modifiers::none());
            draw(cx);
            cx.run_until_parked();
            let collapsed_rows = first_blocks(&rhs, cx)[0].2;
            assert!(collapsed_rows < 4, "{collapsed_rows} rows");
            assert!(
                cx.debug_bounds("review-pull-request-description-text")
                    .is_none()
            );
            cx.dispatch_action(crate::ToggleHeader);
            draw(cx);
            cx.run_until_parked();
            assert_eq!(first_blocks(&rhs, cx)[0].2, overview_rows);

            // Too tall for the view, the overview shows its bottom for `; c`: the new review
            // comment's editor, measured once it's rendered.
            cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(500.)));
            cx.run_until_parked();
            cx.simulate_keystrokes("shift-g ; c");
            draw(cx);
            cx.run_until_parked();
            draw(cx);
            cx.run_until_parked();
            let overview_rows = first_blocks(&rhs, cx)[0].2;
            let visible_lines = rhs.update(cx, |editor, _| editor.visible_line_count().unwrap());
            assert!(f64::from(overview_rows) > visible_lines);
            assert_eq!(
                scroll_top(cx),
                f64::from(overview_rows) - visible_lines.floor()
            );
            assert!(has_open_comment_editor(&review, cx));
        }

        /// In split view the overview is drawn across both sides, over the left side's spacer
        /// for it (rows stay aligned), also once the left side is recreated (a split toggle, a
        /// pane too narrow and wide again); a review comment being written in it keeps its text
        /// and the focus meanwhile.
        #[gpui::test]
        async fn test_overview_in_split_view(cx: &mut TestAppContext) {
            use editor::{ToggleSplitDiff, display_map::BlockId};

            let fixture = Fixture {
                split: true,
                ..Fixture::default()
            };
            let (_fs, _workspace, review, _requests, cx) = open_review(fixture, cx).await;
            cx.simulate_resize(gpui::size(gpui::px(1600.), gpui::px(1000.)));
            cx.run_until_parked();
            let overview = review
                .read_with(cx, |review, _| review.top_block_id())
                .expect("the overview has a block");
            let check_sides = |cx: &mut VisualTestContext| {
                assert!(splittable(&review, cx).read_with(cx, |editor, _| editor.is_split()));
                let rhs_blocks = first_blocks(&rhs_editor(&review, cx), cx);
                let lhs_blocks = first_blocks(&lhs_editor(&review, cx), cx);
                assert_eq!(rhs_blocks[0].0, 0);
                assert_eq!(rhs_blocks[0].1, BlockId::Custom(overview));
                assert_eq!(
                    (lhs_blocks[0].0, lhs_blocks[0].2, lhs_blocks[0].3),
                    (0, rhs_blocks[0].2, true),
                    "the left side starts with the overview's spacer: {lhs_blocks:?}"
                );
                assert_eq!(
                    lhs_blocks[1].0, rhs_blocks[1].0,
                    "the headers face each other"
                );
                draw(cx);
                let viewport = cx.update(|window, _| window.viewport_size());
                let shown = cx
                    .debug_bounds("review-overview")
                    .expect("the overview shows");
                assert!(
                    shown.size.width > viewport.width * 0.75,
                    "across both sides: {shown:?} in {viewport:?}"
                );
            };
            check_sides(cx);

            // Collapsing and expanding it move the diff in the frame that draws the change,
            // without waiting for the next input event to draw another one.
            let header_top = |cx: &mut VisualTestContext| {
                cx.debug_bounds("review-file-comment-docs/guide.md")
                    .expect("the first file's header shows")
                    .top()
            };
            let expanded_header_top = header_top(cx);
            let disclosure = cx
                .debug_bounds("review-pull-request-disclosure")
                .expect("the disclosure shows");
            cx.simulate_click(disclosure.center(), gpui::Modifiers::none());
            let collapsed_header_top = header_top(cx);
            assert!(collapsed_header_top < expanded_header_top);
            draw(cx);
            assert_eq!(header_top(cx), collapsed_header_top);
            cx.dispatch_action(crate::ToggleHeader);
            assert_eq!(header_top(cx), expanded_header_top);
            draw(cx);
            assert_eq!(header_top(cx), expanded_header_top);
            check_sides(cx);

            cx.simulate_keystrokes("; c");
            cx.simulate_input("overall");
            let unsaved = |cx: &mut VisualTestContext| {
                review.read_with(cx, |review, cx| {
                    review
                        .comments()
                        .read(cx)
                        .unsaved_bodies(cx)
                        .into_values()
                        .collect::<Vec<_>>()
                })
            };
            assert_eq!(unsaved(cx), ["overall"]);

            let toggle_split = |cx: &mut VisualTestContext| {
                splittable(&review, cx).update_in(cx, |editor, window, cx| {
                    editor.toggle_split(&ToggleSplitDiff, window, cx)
                });
                cx.run_until_parked();
                draw(cx);
                cx.run_until_parked();
            };
            toggle_split(cx);
            assert!(!splittable(&review, cx).read_with(cx, |editor, _| editor.is_split()));
            assert_eq!(
                first_blocks(&rhs_editor(&review, cx), cx)[0].1,
                BlockId::Custom(overview)
            );
            cx.simulate_input(" one");
            toggle_split(cx);
            check_sides(cx);
            cx.simulate_input(" two");
            assert_eq!(unsaved(cx), ["overall one two"]);

            // Too narrow for two sides, then wide again.
            cx.simulate_resize(gpui::size(gpui::px(500.), gpui::px(1000.)));
            cx.run_until_parked();
            draw(cx);
            cx.run_until_parked();
            assert!(!splittable(&review, cx).read_with(cx, |editor, _| editor.is_split()));
            cx.simulate_input(" three");
            cx.simulate_resize(gpui::size(gpui::px(1600.), gpui::px(1000.)));
            cx.run_until_parked();
            draw(cx);
            cx.run_until_parked();
            check_sides(cx);
            cx.simulate_input(" four");
            assert_eq!(unsaved(cx), ["overall one two three four"]);
        }
    }
}
