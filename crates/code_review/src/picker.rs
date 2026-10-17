use crate::{
    OpenPullRequest, StartReview, TogglePullRequestFilter,
    commits::{RECENT_COMMITS_LIMIT, first_parent_chain},
    forge::{
        self, Forge, ForgeRepo, GitHubError, PullRequestListFilter, PullRequestSummary,
        resolve_forge,
    },
    pull_request::{OpenPullRequestOptions, open_pull_request_and_notify},
    target::{
        BranchBase, CommitRange, PullRequestReference, ReviewTarget, parse_pull_request_reference,
    },
    view::{ReviewView, active_repository, pick_base_for_new_review},
};
use anyhow::Context as _;
use fuzzy::StringMatchCandidate;
use git::repository::CommitData;
use gpui::{
    Action as _, AnyElement, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    Subscription, Task, WeakEntity,
};
use picker::{Picker, PickerDelegate};
use project::{Project, git_store::Repository};
use std::sync::Arc;
use time::{OffsetDateTime, UtcOffset};
use ui::{
    Chip, HighlightedLabel, KeyBinding, ListItem, ListItemSpacing, ListSubHeader, prelude::*,
};
use util::ResultExt as _;
use workspace::{ModalView, Toast, Workspace, notifications::NotificationId};

pub(crate) fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &StartReview, window, cx| {
        StartReviewModal::toggle(workspace, StartReviewFocus::Default, window, cx);
    });
    workspace.register_action(|workspace, action: &OpenPullRequest, window, cx| {
        let Some(reference) = &action.reference else {
            StartReviewModal::toggle(workspace, StartReviewFocus::PullRequests, window, cx);
            return;
        };
        let reference = match parse_pull_request_reference(reference) {
            Ok(reference) => reference,
            Err(error) => {
                workspace.show_toast(
                    Toast::new(
                        NotificationId::unique::<OpenPullRequest>(),
                        error.to_string(),
                    ),
                    cx,
                );
                return;
            }
        };
        let Some(repo) = active_repository(workspace, cx) else {
            return;
        };
        open_pull_request_and_notify(
            workspace,
            repo,
            reference,
            OpenPullRequestOptions::default(),
            window,
            cx,
        );
    });
}

/// Which row the Start Review picker selects when it opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartReviewFocus {
    /// "Uncommitted changes".
    Default,
    /// The newest commit, once the commits have loaded.
    Commits,
    /// The first pull request, once the pull requests have loaded.
    PullRequests,
}

/// The Start Review picker: uncommitted changes, the current branch against its base, and recent
/// commits, where multi-selection picks a range; then the pull requests of the repository's
/// GitHub remote (open ones, or those requesting the user's review), and any pull request by
/// number or URL.
pub struct StartReviewModal {
    picker: Entity<Picker<StartReviewDelegate>>,
    _load_tasks: Vec<Task<()>>,
    _picker_subscription: Subscription,
}

impl StartReviewModal {
    pub(crate) fn toggle(
        workspace: &mut Workspace,
        focus: StartReviewFocus,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(repo) = active_repository(workspace, cx) else {
            return;
        };
        let workspace_handle = workspace.weak_handle();
        let project = workspace.project().clone();
        workspace.toggle_modal(window, cx, |window, cx| {
            Self::new(workspace_handle, project, repo, focus, window, cx)
        });
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        repo: Entity<Repository>,
        focus: StartReviewFocus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let default_branch = repo.update(cx, |repo, _| repo.default_branch(true));
        let recent_commits = first_parent_chain(&repo, None, None, RECENT_COMMITS_LIMIT, cx);
        let delegate = StartReviewDelegate::new(workspace, project, repo, focus, cx.focus_handle());
        let picker = cx.new(|cx| {
            Picker::list(delegate, window, cx)
                .initial_width(rems(34.))
                .show_scrollbar(true)
        });
        picker.update(cx, |picker, cx| {
            picker.delegate.focus_handle = picker.focus_handle(cx);
            picker.delegate.load_pull_requests(window, cx);
        });
        let picker_subscription =
            cx.subscribe(&picker, |_, _, _: &DismissEvent, cx| cx.emit(DismissEvent));

        let load_tasks = vec![
            cx.spawn_in(window, async move |this, cx| {
                let base = match default_branch
                    .await
                    .context("the git job was canceled")
                    .flatten()
                    .and_then(|base| base.map(BranchBase::new).transpose())
                {
                    Ok(Some(base)) => BaseState::Found(base),
                    Ok(None) => BaseState::Missing,
                    Err(error) => {
                        log::error!("finding the default branch: {error:#}");
                        BaseState::Failed(format!("No default branch: {error}").into())
                    }
                };
                this.update_in(cx, |this, window, cx| {
                    this.picker.update(cx, |picker, cx| {
                        picker.delegate.default_base = base;
                        picker.refresh(window, cx);
                    })
                })
                .log_err();
            }),
            cx.spawn_in(window, async move |this, cx| {
                let commits = match recent_commits.await {
                    Ok(chain) => RecentCommits::Loaded(chain.commits),
                    Err(error) => {
                        log::error!("listing recent commits: {error:#}");
                        RecentCommits::Failed(format!("{error:#}").into())
                    }
                };
                this.update_in(cx, |this, window, cx| {
                    this.picker.update(cx, |picker, cx| {
                        picker.delegate.commits = commits;
                        picker.refresh(window, cx);
                    })
                })
                .log_err();
            }),
        ];

        Self {
            picker,
            _load_tasks: load_tasks,
            _picker_subscription: picker_subscription,
        }
    }

    #[cfg(test)]
    pub(crate) fn picker(&self) -> &Entity<Picker<StartReviewDelegate>> {
        &self.picker
    }
}

impl ModalView for StartReviewModal {}

impl EventEmitter<DismissEvent> for StartReviewModal {}

impl Focusable for StartReviewModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for StartReviewModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("StartReview")
            .w(rems(34.))
            .on_action(
                cx.listener(|this, _: &TogglePullRequestFilter, window, cx| {
                    this.picker.update(cx, |picker, cx| {
                        picker.delegate.toggle_pull_request_filter(window, cx);
                        // Shows the loading state right away.
                        picker.refresh(window, cx);
                    })
                }),
            )
            .child(self.picker.clone())
    }
}

enum BaseState {
    Loading,
    Found(BranchBase),
    Missing,
    Failed(SharedString),
}

pub(crate) enum RecentCommits {
    Loading,
    /// Newest first, following first parents from HEAD.
    Loaded(Vec<CommitData>),
    Failed(SharedString),
}

pub(crate) enum PullRequestList {
    Loading,
    /// Most recently updated first.
    Loaded(Vec<PullRequestSummary>),
    Failed(SharedString),
    /// The repository has no GitHub remote: the picker has no pull request section.
    Unsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Section {
    Local,
    PullRequests,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    Header(Section),
    Uncommitted {
        positions: Vec<usize>,
    },
    Branch {
        positions: Vec<usize>,
    },
    /// `index` is into the recent commits.
    Commit {
        index: usize,
        positions: Vec<usize>,
    },
    /// `index` is into the loaded pull requests.
    PullRequest {
        index: usize,
        positions: Vec<usize>,
    },
    /// The pull request the query names, when it isn't in the list.
    OpenReference(PullRequestReference),
    Status(SharedString),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum EntryKey {
    Uncommitted,
    Branch,
    Commit(usize),
    /// By number, so the selection survives reloading the list.
    PullRequest(u64),
    OpenReference,
}

impl Entry {
    fn key(&self, pull_requests: &[PullRequestSummary]) -> Option<EntryKey> {
        match self {
            Entry::Uncommitted { .. } => Some(EntryKey::Uncommitted),
            Entry::Branch { .. } => Some(EntryKey::Branch),
            Entry::Commit { index, .. } => Some(EntryKey::Commit(*index)),
            Entry::PullRequest { index, .. } => pull_requests
                .get(*index)
                .map(|pull_request| EntryKey::PullRequest(pull_request.number)),
            Entry::OpenReference(_) => Some(EntryKey::OpenReference),
            Entry::Header(_) | Entry::Status(_) => None,
        }
    }
}

const UNCOMMITTED_CANDIDATE: usize = 0;
const BRANCH_CANDIDATE: usize = 1;
const FIRST_COMMIT_CANDIDATE: usize = 2;
/// Far above the commits' ids, so sorting matches by id keeps the sections apart.
const FIRST_PULL_REQUEST_CANDIDATE: usize = 1 << 20;

/// A contiguous range of the recent commits, as indices `(newest, oldest)` with
/// `newest <= oldest`. Toggling never leaves a gap (tuicr's range selection): a commit outside
/// the range extends it, an end commit shrinks it, and a commit in the middle cuts the range back
/// to the commits newer than it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CommitSelection(Option<(usize, usize)>);

impl CommitSelection {
    pub(crate) fn toggle(&mut self, index: usize) {
        self.0 = match self.0 {
            None => Some((index, index)),
            Some((newest, oldest)) if index < newest || index > oldest => {
                Some((newest.min(index), oldest.max(index)))
            }
            Some((newest, oldest)) if newest == oldest => None,
            Some((newest, oldest)) if index == newest => Some((newest + 1, oldest)),
            Some((newest, oldest)) if index == oldest => Some((newest, oldest - 1)),
            Some((newest, _)) => Some((newest, index - 1)),
        };
    }

    pub(crate) fn contains(&self, index: usize) -> bool {
        self.0
            .is_some_and(|(newest, oldest)| (newest..=oldest).contains(&index))
    }

    pub(crate) fn len(&self) -> usize {
        self.0.map_or(0, |(newest, oldest)| oldest - newest + 1)
    }

    pub(crate) fn range(&self) -> Option<(usize, usize)> {
        self.0
    }

    pub(crate) fn clear(&mut self) {
        self.0 = None;
    }
}

pub struct StartReviewDelegate {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    repo: Entity<Repository>,
    default_base: BaseState,
    commits: RecentCommits,
    selection: CommitSelection,
    /// Resolved on the first load and reused when the filter changes; dropped after an error, so
    /// that the next load resolves it again (with a new token, say).
    forge: Option<Arc<dyn Forge>>,
    pull_request_filter: PullRequestListFilter,
    pull_requests: PullRequestList,
    load_pull_requests_task: Task<()>,
    entries: Vec<Entry>,
    selected_index: usize,
    query: String,
    /// Set until the commits load when the picker opens on them.
    select_first_commit: bool,
    /// Set until the pull requests load when the picker opens on them.
    select_first_pull_request: bool,
    focus_handle: FocusHandle,
    timezone: UtcOffset,
}

impl StartReviewDelegate {
    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        repo: Entity<Repository>,
        focus: StartReviewFocus,
        focus_handle: FocusHandle,
    ) -> Self {
        Self {
            workspace,
            project,
            repo,
            default_base: BaseState::Loading,
            commits: RecentCommits::Loading,
            selection: CommitSelection::default(),
            forge: None,
            pull_request_filter: PullRequestListFilter::Open,
            pull_requests: PullRequestList::Loading,
            load_pull_requests_task: Task::ready(()),
            entries: Vec::new(),
            selected_index: 0,
            query: String::new(),
            select_first_commit: focus == StartReviewFocus::Commits,
            select_first_pull_request: focus == StartReviewFocus::PullRequests,
            focus_handle,
            timezone: UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC),
        }
    }

    #[cfg(test)]
    pub(crate) fn entries(&self) -> &[Entry] {
        &self.entries
    }

    fn recent_commits(&self) -> &[CommitData] {
        match &self.commits {
            RecentCommits::Loaded(commits) => commits,
            RecentCommits::Loading | RecentCommits::Failed(_) => &[],
        }
    }

    fn loaded_pull_requests(&self) -> &[PullRequestSummary] {
        match &self.pull_requests {
            PullRequestList::Loaded(pull_requests) => pull_requests,
            PullRequestList::Loading
            | PullRequestList::Failed(_)
            | PullRequestList::Unsupported => &[],
        }
    }

    fn entry_key(&self, index: usize) -> Option<EntryKey> {
        self.entries
            .get(index)
            .and_then(|entry| entry.key(self.loaded_pull_requests()))
    }

    /// The repository whose pull requests are listed, once known.
    pub(crate) fn pull_request_repo(&self) -> Option<&ForgeRepo> {
        self.forge.as_ref().map(|forge| &forge.context().canonical)
    }

    /// (Re)loads the pull requests for the current filter.
    fn load_pull_requests(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if matches!(self.pull_requests, PullRequestList::Unsupported) {
            return;
        }
        let forge = match &self.forge {
            Some(forge) => Task::ready(Ok(forge.clone())),
            None => resolve_forge(&self.repo, None, &self.project, cx),
        };
        let filter = self.pull_request_filter;
        self.pull_requests = PullRequestList::Loading;
        self.load_pull_requests_task = cx.spawn_in(window, async move |picker, cx| {
            let result = async {
                let forge = forge.await?;
                let pull_requests = cx
                    .background_executor()
                    .spawn(forge.list_pull_requests(filter))
                    .await;
                anyhow::Ok((forge, pull_requests?))
            }
            .await;
            picker
                .update_in(cx, |picker, window, cx| {
                    let delegate = &mut picker.delegate;
                    // The filter changed meanwhile: that load's result counts.
                    if delegate.pull_request_filter != filter {
                        return;
                    }
                    match result {
                        Ok((forge, pull_requests)) => {
                            delegate.forge = Some(forge);
                            delegate.pull_requests = PullRequestList::Loaded(pull_requests);
                        }
                        Err(error) => {
                            delegate.forge = None;
                            delegate.pull_requests = match forge::github_error(&error) {
                                Some(GitHubError::Unsupported) => PullRequestList::Unsupported,
                                _ => {
                                    log::error!("listing pull requests: {error:#}");
                                    PullRequestList::Failed(forge::error_message(&error))
                                }
                            };
                        }
                    }
                    picker.refresh(window, cx);
                })
                .log_err();
        });
    }

    /// Switches between the open pull requests and those requesting the user's review.
    pub(crate) fn toggle_pull_request_filter(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        if matches!(self.pull_requests, PullRequestList::Unsupported) {
            return;
        }
        self.pull_request_filter = match self.pull_request_filter {
            PullRequestListFilter::Open => PullRequestListFilter::ReviewRequested,
            PullRequestListFilter::ReviewRequested => PullRequestListFilter::Open,
        };
        self.load_pull_requests(window, cx);
    }

    fn pull_request_section_title(&self) -> SharedString {
        let title = match self.pull_request_filter {
            PullRequestListFilter::Open => "Pull Requests",
            PullRequestListFilter::ReviewRequested => "Review Requests",
        };
        match self.pull_request_repo() {
            Some(repo) => format!("{title} · {repo}").into(),
            None => title.into(),
        }
    }

    fn pull_request_candidate(pull_request: &PullRequestSummary) -> String {
        format!(
            "#{} {} {} {}",
            pull_request.number,
            pull_request.title,
            pull_request.author.login,
            pull_request.head_ref
        )
    }

    /// The listed pull request a reference names, if any.
    fn listed_pull_request(&self, reference: &PullRequestReference) -> Option<u64> {
        if let Some(repo) = &reference.repo
            && !self
                .pull_request_repo()
                .is_some_and(|listed| repo.matches(listed))
        {
            return None;
        }
        self.loaded_pull_requests()
            .iter()
            .find(|pull_request| pull_request.number == reference.number)
            .map(|pull_request| pull_request.number)
    }

    pub(crate) fn branch_label(&self) -> String {
        match &self.default_base {
            BaseState::Loading => "Branch vs …".to_string(),
            BaseState::Found(base) => format!("Branch vs {base}"),
            BaseState::Missing | BaseState::Failed(_) => "Branch vs (choose base)".to_string(),
        }
    }

    fn commit_candidate(commit: &CommitData) -> String {
        format!(
            "{} {} {}",
            commit.sha.display_short(),
            commit.subject,
            commit.author_name
        )
    }

    fn candidates(&self) -> Vec<StringMatchCandidate> {
        let mut candidates = vec![
            StringMatchCandidate::new(UNCOMMITTED_CANDIDATE, "Uncommitted changes"),
            StringMatchCandidate::new(BRANCH_CANDIDATE, &self.branch_label()),
        ];
        candidates.extend(
            self.recent_commits()
                .iter()
                .enumerate()
                .map(|(index, commit)| {
                    StringMatchCandidate::new(
                        FIRST_COMMIT_CANDIDATE + index,
                        &Self::commit_candidate(commit),
                    )
                }),
        );
        candidates.extend(self.loaded_pull_requests().iter().enumerate().map(
            |(index, pull_request)| {
                StringMatchCandidate::new(
                    FIRST_PULL_REQUEST_CANDIDATE + index,
                    &Self::pull_request_candidate(pull_request),
                )
            },
        ));
        candidates
    }

    /// Builds the rows from matched candidates (in candidate order) and keeps the selection on
    /// the same row when only the data changed.
    fn set_matches(&mut self, query: String, matches: Vec<(usize, Vec<usize>)>) {
        let query_changed = query != self.query;
        let previous_key = (!query_changed)
            .then(|| self.entry_key(self.selected_index))
            .flatten();
        self.query = query;

        let mut local_entries = Vec::new();
        let mut pull_request_entries = Vec::new();
        for (candidate, positions) in matches {
            match candidate {
                UNCOMMITTED_CANDIDATE => local_entries.push(Entry::Uncommitted { positions }),
                BRANCH_CANDIDATE => local_entries.push(Entry::Branch { positions }),
                candidate if candidate >= FIRST_PULL_REQUEST_CANDIDATE => pull_request_entries
                    .push(Entry::PullRequest {
                        index: candidate - FIRST_PULL_REQUEST_CANDIDATE,
                        positions,
                    }),
                candidate => local_entries.push(Entry::Commit {
                    index: candidate - FIRST_COMMIT_CANDIDATE,
                    positions,
                }),
            }
        }
        let status = match &self.commits {
            RecentCommits::Loading => Some("Loading commits…".into()),
            RecentCommits::Failed(error) => Some(error.clone()),
            RecentCommits::Loaded(commits) if commits.is_empty() => Some("No commits".into()),
            RecentCommits::Loaded(_) => None,
        };
        let mut entries = Vec::new();
        if !local_entries.is_empty() || status.is_some() {
            entries.push(Entry::Header(Section::Local));
            for entry in local_entries {
                let is_branch = matches!(entry, Entry::Branch { .. });
                entries.push(entry);
                if is_branch && let BaseState::Failed(error) = &self.default_base {
                    entries.push(Entry::Status(error.clone()));
                }
            }
            entries.extend(status.map(Entry::Status));
        }

        // A query that names a pull request selects it: Enter opens it.
        let reference = parse_pull_request_reference(&self.query).ok();
        let listed_reference = reference
            .as_ref()
            .and_then(|reference| self.listed_pull_request(reference));
        let pull_request_status = match &self.pull_requests {
            PullRequestList::Unsupported => None,
            PullRequestList::Loading => Some("Loading pull requests…".into()),
            PullRequestList::Failed(error) => Some(error.clone()),
            PullRequestList::Loaded(pull_requests) if pull_requests.is_empty() => {
                Some(match self.pull_request_filter {
                    PullRequestListFilter::Open => "No open pull requests".into(),
                    PullRequestListFilter::ReviewRequested => {
                        "No pull requests request your review".into()
                    }
                })
            }
            PullRequestList::Loaded(_) => None,
        };
        // A URL doesn't fuzzy-match its pull request's row.
        if let Some(number) = listed_reference
            && let Some(index) = self
                .loaded_pull_requests()
                .iter()
                .position(|pull_request| pull_request.number == number)
            && !pull_request_entries.iter().any(
                |entry| matches!(entry, Entry::PullRequest { index: shown, .. } if *shown == index),
            )
        {
            pull_request_entries.insert(
                0,
                Entry::PullRequest {
                    index,
                    positions: Vec::new(),
                },
            );
        }
        if !matches!(self.pull_requests, PullRequestList::Unsupported) {
            let open_reference = reference
                .clone()
                .filter(|_| listed_reference.is_none())
                .map(Entry::OpenReference);
            if open_reference.is_some()
                || !pull_request_entries.is_empty()
                || pull_request_status.is_some()
            {
                entries.push(Entry::Header(Section::PullRequests));
                entries.extend(open_reference);
                entries.extend(pull_request_entries);
                entries.extend(pull_request_status.map(Entry::Status));
            }
        }
        self.entries = entries;

        // The reference's row, or its pull request's once the list has it.
        let reference_key = reference.as_ref().map(|_| match listed_reference {
            Some(number) => EntryKey::PullRequest(number),
            None => EntryKey::OpenReference,
        });
        let wanted_key =
            if self.select_first_commit && matches!(self.commits, RecentCommits::Loaded(_)) {
                self.select_first_commit = false;
                Some(EntryKey::Commit(0))
            } else if self.select_first_pull_request
                && matches!(self.pull_requests, PullRequestList::Loaded(_))
            {
                self.select_first_pull_request = false;
                self.loaded_pull_requests()
                    .first()
                    .map(|pull_request| EntryKey::PullRequest(pull_request.number))
            } else if reference_key.is_some()
                && (query_changed || previous_key == Some(EntryKey::OpenReference))
            {
                reference_key
            } else {
                previous_key
            };
        let pull_requests = self.loaded_pull_requests();
        self.selected_index = wanted_key
            .and_then(|key| {
                self.entries
                    .iter()
                    .position(|entry| entry.key(pull_requests) == Some(key.clone()))
            })
            .or_else(|| {
                self.entries
                    .iter()
                    .position(|entry| entry.key(pull_requests).is_some())
            })
            .unwrap_or(0);
    }

    fn commit_range(&self, newest: usize, oldest: usize) -> Option<CommitRange> {
        let commits = self.recent_commits();
        let newest = commits.get(newest)?;
        let oldest = commits.get(oldest)?;
        Some(CommitRange {
            // A merge commit is reviewed against its first parent, like the commit list follows
            // first parents.
            base: oldest.parents.first().copied(),
            head: newest.sha,
        })
    }

    fn deploy(&self, target: ReviewTarget, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
        let repo = self.repo.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                ReviewView::deploy_in_repo(workspace, target, repo, window, cx);
            })
            .log_err();
    }

    /// `in_new_worktree`: the secondary confirm, "Review in Worktree".
    fn open_pull_request(
        &self,
        reference: PullRequestReference,
        in_new_worktree: bool,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        cx.emit(DismissEvent);
        let repo = self.repo.clone();
        let options = OpenPullRequestOptions {
            in_new_worktree,
            ..OpenPullRequestOptions::default()
        };
        self.workspace
            .update(cx, |workspace, cx| {
                open_pull_request_and_notify(workspace, repo, reference, options, window, cx);
            })
            .log_err();
    }

    fn choose_base(&self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
        let workspace = self.workspace.clone();
        let repo = self.repo.clone();
        let selected_base = match &self.default_base {
            BaseState::Found(base) => Some(base.name().clone()),
            BaseState::Loading | BaseState::Missing | BaseState::Failed(_) => None,
        };
        // The branch picker is another modal: it can only open once this one is gone.
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    pick_base_for_new_review(workspace, repo, selected_base, window, cx)
                })
                .log_err();
        });
    }

    fn render_pull_request(
        &self,
        entry_index: usize,
        index: usize,
        positions: &[usize],
        selected: bool,
    ) -> Option<AnyElement> {
        let pull_request = self.loaded_pull_requests().get(index)?;
        let number = format!("#{}", pull_request.number);
        let title_start = number.len() + 1;
        let title_end = title_start + pull_request.title.len();
        let positions_in = |start: usize, end: usize| {
            positions
                .iter()
                .filter(|position| (start..end).contains(*position))
                .map(|position| position - start)
                .collect::<Vec<_>>()
        };
        let number_positions = positions_in(0, number.len());
        let title_positions = positions_in(title_start, title_end);
        let time = time_format::format_localized_timestamp(
            pull_request.updated_at,
            OffsetDateTime::now_utc(),
            self.timezone,
            time_format::TimestampFormat::Relative,
        );
        Some(
            ListItem::new(("start-review-pull-request", entry_index))
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    v_flex()
                        .w_full()
                        .min_w_0()
                        .child(
                            h_flex()
                                .w_full()
                                .gap_2()
                                .justify_between()
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .min_w_0()
                                        .child(
                                            HighlightedLabel::new(number, number_positions)
                                                .color(Color::Muted)
                                                .flex_none(),
                                        )
                                        .child(
                                            HighlightedLabel::new(
                                                pull_request.title.clone(),
                                                title_positions,
                                            )
                                            .truncate(),
                                        )
                                        .when(pull_request.is_draft, |row| {
                                            row.child(
                                                Chip::new("Draft")
                                                    .label_size(LabelSize::XSmall)
                                                    .label_color(Color::Muted),
                                            )
                                        }),
                                )
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .flex_none()
                                        .child(
                                            Label::new(pull_request.author.login.clone())
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        )
                                        .child(
                                            Label::new(time)
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        ),
                                ),
                        )
                        .child(
                            Label::new(if pull_request.is_cross_repository {
                                format!(
                                    "{} → {} · fork",
                                    pull_request.head_ref, pull_request.base_ref
                                )
                            } else {
                                format!("{} → {}", pull_request.head_ref, pull_request.base_ref)
                            })
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_commit(
        &self,
        entry_index: usize,
        commit_index: usize,
        positions: &[usize],
        selected: bool,
        checkbox: Option<AnyElement>,
    ) -> Option<AnyElement> {
        let commit = self.recent_commits().get(commit_index)?;
        let sha = commit.sha.display_short();
        let subject_start = sha.len() + 1;
        let author_start = subject_start + commit.subject.len() + 1;
        let positions_in = |start: usize, end: usize| {
            positions
                .iter()
                .filter(|position| (start..end).contains(*position))
                .map(|position| position - start)
                .collect::<Vec<_>>()
        };
        let sha_positions = positions_in(0, sha.len());
        let subject_positions = positions_in(subject_start, author_start - 1);
        let author_positions = positions_in(author_start, usize::MAX);
        let timestamp = OffsetDateTime::from_unix_timestamp(commit.commit_timestamp)
            .unwrap_or_else(|_| OffsetDateTime::now_utc());
        let time = time_format::format_localized_timestamp(
            timestamp,
            OffsetDateTime::now_utc(),
            self.timezone,
            time_format::TimestampFormat::Relative,
        );
        Some(
            ListItem::new(("start-review-commit", entry_index))
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot::<AnyElement>(checkbox)
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .justify_between()
                        .child(
                            h_flex()
                                .gap_2()
                                .min_w_0()
                                .child(
                                    HighlightedLabel::new(sha, sha_positions)
                                        .color(Color::Muted)
                                        .flex_none(),
                                )
                                .child(
                                    HighlightedLabel::new(
                                        commit.subject.clone(),
                                        subject_positions,
                                    )
                                    .truncate(),
                                ),
                        )
                        .child(
                            h_flex()
                                .gap_1()
                                .flex_none()
                                .child(
                                    HighlightedLabel::new(
                                        commit.author_name.clone(),
                                        author_positions,
                                    )
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                                )
                                .child(Label::new(time).size(LabelSize::Small).color(Color::Muted)),
                        ),
                )
                .into_any_element(),
        )
    }
}

impl PickerDelegate for StartReviewDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "start review"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Review changes, the branch, commits, or a pull request (#123, URL)…".into()
    }

    fn match_count(&self) -> usize {
        self.entries.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        index: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = index;
    }

    fn can_select(
        &self,
        index: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> bool {
        self.entry_key(index).is_some()
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let candidates = self.candidates();
        if query.is_empty() {
            let matches = candidates
                .into_iter()
                .map(|candidate| (candidate.id, Vec::new()))
                .collect();
            self.set_matches(query, matches);
            return Task::ready(());
        }
        cx.spawn_in(window, async move |picker, cx| {
            let mut matches = fuzzy::match_strings(
                &candidates,
                &query,
                false,
                true,
                10_000,
                &Default::default(),
                cx.background_executor().clone(),
            )
            .await;
            // Commits keep their history order rather than the score order.
            matches.sort_by_key(|string_match| string_match.candidate_id);
            let matches = matches
                .into_iter()
                .map(|string_match| (string_match.candidate_id, string_match.positions))
                .collect();
            picker
                .update(cx, |picker, _| picker.delegate.set_matches(query, matches))
                .log_err();
        })
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(key) = self.entry_key(self.selected_index) else {
            return;
        };
        match key {
            EntryKey::Uncommitted => self.deploy(ReviewTarget::Uncommitted, window, cx),
            EntryKey::Branch => match (&self.default_base, secondary) {
                (BaseState::Found(base), false) => {
                    let target = ReviewTarget::Branch { base: base.clone() };
                    self.deploy(target, window, cx);
                }
                _ => self.choose_base(window, cx),
            },
            EntryKey::Commit(index) => {
                if let Some(range) = self.commit_range(index, index) {
                    self.deploy(ReviewTarget::Commits(range), window, cx);
                }
            }
            // Listed pull requests are the checkout's.
            EntryKey::PullRequest(number) => {
                let reference = PullRequestReference { repo: None, number };
                self.open_pull_request(reference, secondary, window, cx);
            }
            EntryKey::OpenReference => {
                if let Some(Entry::OpenReference(reference)) =
                    self.entries.get(self.selected_index).cloned()
                {
                    self.open_pull_request(reference, secondary, window, cx);
                }
            }
        }
    }

    fn supports_multi_select(&self) -> bool {
        true
    }

    fn is_item_selected(&self, index: usize) -> bool {
        match self.entries.get(index) {
            Some(Entry::Commit { index, .. }) => self.selection.contains(*index),
            _ => false,
        }
    }

    fn toggle_item_selected(
        &mut self,
        entry_index: usize,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        if let Some(Entry::Commit { index, .. }) = self.entries.get(entry_index) {
            self.selection.toggle(*index);
            cx.notify();
        }
    }

    fn selected_item_count(&self) -> usize {
        self.selection.len()
    }

    fn clear_selection(&mut self, cx: &mut Context<Picker<Self>>) {
        self.selection.clear();
        cx.notify();
    }

    fn confirm_multi(
        &mut self,
        _secondary: bool,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        let Some((newest, oldest)) = self.selection.range() else {
            return;
        };
        self.selection.clear();
        if let Some(range) = self.commit_range(newest, oldest) {
            self.deploy(ReviewTarget::Commits(range), window, cx);
        }
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        entry_index: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let row = |label: AnyElement| {
            ListItem::new(("start-review-entry", entry_index))
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(label)
                .into_any_element()
        };
        match self.entries.get(entry_index)? {
            Entry::Header(Section::Local) => Some(
                div()
                    .pt_1()
                    .child(ListSubHeader::new("Local").inset(true))
                    .into_any_element(),
            ),
            Entry::Header(Section::PullRequests) => Some(
                div()
                    .pt_1()
                    .child(ListSubHeader::new(self.pull_request_section_title()).inset(true))
                    .into_any_element(),
            ),
            Entry::PullRequest { index, positions } => {
                self.render_pull_request(entry_index, *index, positions, selected)
            }
            Entry::OpenReference(reference) => Some(row(Label::new(format!(
                "Open pull request {reference}"
            ))
            .into_any_element())),
            Entry::Uncommitted { positions } => Some(row(HighlightedLabel::new(
                "Uncommitted changes",
                positions.clone(),
            )
            .into_any_element())),
            Entry::Branch { positions } => Some(row(HighlightedLabel::new(
                self.branch_label(),
                positions.clone(),
            )
            .into_any_element())),
            Entry::Commit { index, positions } => {
                self.render_commit(entry_index, *index, positions, selected, None)
            }
            Entry::Status(text) => Some(
                ListItem::new(("start-review-status", entry_index))
                    .inset(true)
                    .spacing(ListItemSpacing::Sparse)
                    .child(Label::new(text.clone()).color(Color::Muted))
                    .into_any_element(),
            ),
        }
    }

    /// Only commits take part in the range selection, so only they get a checkbox.
    fn render_match_with_checkbox(
        &self,
        entry_index: usize,
        selected: bool,
        checkbox: AnyElement,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        match self.entries.get(entry_index)? {
            Entry::Commit { index, positions } => {
                self.render_commit(entry_index, *index, positions, selected, Some(checkbox))
            }
            _ => self.render_match(entry_index, selected, window, cx),
        }
    }

    fn render_footer(
        &self,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        let focus_handle = &self.focus_handle;
        let selected = self.entry_key(self.selected_index);
        let selected_count = self.selection.len();
        let has_pull_requests = !matches!(self.pull_requests, PullRequestList::Unsupported);
        let key_binding = |action: &dyn gpui::Action, cx: &App| {
            KeyBinding::for_action_in(action, focus_handle, cx).size(rems_from_px(12_f32))
        };
        Some(
            h_flex()
                .w_full()
                .p_1p5()
                .gap_1()
                .justify_between()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .map(|footer| {
                    if selected_count == 0 && has_pull_requests {
                        footer.child(
                            Button::new(
                                "start-review-pull-request-filter",
                                match self.pull_request_filter {
                                    PullRequestListFilter::Open => "Review Requests",
                                    PullRequestListFilter::ReviewRequested => "Open Pull Requests",
                                },
                            )
                            .key_binding(key_binding(&TogglePullRequestFilter, cx))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(TogglePullRequestFilter.boxed_clone(), cx)
                            }),
                        )
                    } else {
                        footer.child(
                            Label::new(match selected_count {
                                0 => String::new(),
                                1 => "1 commit selected".to_string(),
                                count => format!("{count} commits selected"),
                            })
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        )
                    }
                })
                .child(
                    h_flex()
                        .gap_1()
                        .when(selected == Some(EntryKey::Branch), |buttons| {
                            buttons.child(
                                Button::new("start-review-choose-base", "Choose Base")
                                    .key_binding(key_binding(&menu::SecondaryConfirm, cx))
                                    .on_click(cx.listener(|picker, _, window, cx| {
                                        picker.delegate.confirm(true, window, cx)
                                    })),
                            )
                        })
                        .when(
                            matches!(
                                selected,
                                Some(EntryKey::PullRequest(_) | EntryKey::OpenReference)
                            ),
                            |buttons| {
                                buttons.child(
                                    Button::new("start-review-in-worktree", "Review in Worktree")
                                        .key_binding(key_binding(&menu::SecondaryConfirm, cx))
                                        .on_click(cx.listener(|picker, _, window, cx| {
                                            picker.delegate.confirm(true, window, cx)
                                        })),
                                )
                            },
                        )
                        .when(matches!(selected, Some(EntryKey::Commit(_))), |buttons| {
                            buttons.child(
                                Button::new("start-review-select-range", "Select Range")
                                    .key_binding(key_binding(&picker::MultiSelectNext, cx))
                                    .on_click(|_, window, cx| {
                                        window.dispatch_action(
                                            picker::MultiSelectNext.boxed_clone(),
                                            cx,
                                        )
                                    }),
                            )
                        })
                        .child(
                            Button::new("start-review-confirm", "Review")
                                .key_binding(key_binding(&menu::Confirm, cx))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(menu::Confirm.boxed_clone(), cx)
                                }),
                        ),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ReviewCommits,
        github::test_support::{PR1_BASE_OID, PR1_HEAD_OID},
        test_support::{
            C1, C2, C3, active_review, commit_data, dispatch, dot_git, insert_commit_ref, oid,
            open_workspace, review_count, tab_title,
        },
    };
    use gpui::{TestAppContext, VisualTestContext};
    use project::FakeFs;
    use serde_json::json;
    use util::path;

    fn start_review_picker(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Entity<Picker<StartReviewDelegate>> {
        workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_modal::<StartReviewModal>(cx)
                .expect("the Start Review picker is open")
                .read(cx)
                .picker()
                .clone()
        })
    }

    fn entry_keys(
        picker: &Entity<Picker<StartReviewDelegate>>,
        cx: &mut VisualTestContext,
    ) -> Vec<Option<EntryKey>> {
        picker.read_with(cx, |picker, _| {
            (0..picker.delegate.entries().len())
                .map(|index| picker.delegate.entry_key(index))
                .collect()
        })
    }

    fn selected_key(
        picker: &Entity<Picker<StartReviewDelegate>>,
        cx: &mut VisualTestContext,
    ) -> Option<EntryKey> {
        picker.read_with(cx, |picker, _| {
            picker.delegate.entry_key(picker.delegate.selected_index())
        })
    }

    #[gpui::test]
    async fn test_start_review_picker(cx: &mut TestAppContext) {
        crate::init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ ".git": {}, "a.txt": "a\n" }))
            .await;
        fs.set_head_and_index_for_repo(dot_git(), &[("a.txt", "a\n".into())]);
        fs.with_git_state(dot_git(), true, |state| {
            state.refs.insert("HEAD".into(), C3.into());
        })
        .unwrap();
        fs.set_commit_data(
            dot_git(),
            [
                (commit_data(C1, None, "Initial commit"), false),
                (commit_data(C2, Some(C1), "Add bravo"), false),
                (commit_data(C3, Some(C2), "Tweak bravo"), false),
            ],
        );
        let (workspace, cx) = open_workspace(fs, cx).await;

        // Without a GitHub remote, there's no pull request section.
        dispatch(StartReview, cx);
        let picker = start_review_picker(&workspace, cx);
        assert_eq!(
            entry_keys(&picker, cx),
            [
                None,
                Some(EntryKey::Uncommitted),
                Some(EntryKey::Branch),
                Some(EntryKey::Commit(0)),
                Some(EntryKey::Commit(1)),
                Some(EntryKey::Commit(2)),
            ]
        );
        assert_eq!(
            picker.read_with(cx, |picker, _| picker.delegate.entries()[0].clone()),
            Entry::Header(Section::Local)
        );
        assert_eq!(selected_key(&picker, cx), Some(EntryKey::Uncommitted));
        assert_eq!(
            picker.read_with(cx, |picker, _| picker.delegate.branch_label()),
            "Branch vs origin/main"
        );

        picker.update_in(cx, |picker, window, cx| {
            picker.set_query("tweak", window, cx)
        });
        cx.run_until_parked();
        assert_eq!(entry_keys(&picker, cx), [None, Some(EntryKey::Commit(0))]);
        assert_eq!(selected_key(&picker, cx), Some(EntryKey::Commit(0)));
        picker.update_in(cx, |picker, window, cx| picker.set_query("", window, cx));
        cx.run_until_parked();

        // Selecting the newest and the oldest commit selects the range between them.
        picker.update_in(cx, |picker, window, cx| {
            picker.delegate.toggle_item_selected(3, window, cx);
            picker.delegate.toggle_item_selected(5, window, cx);
            // Rows other than commits can't be selected.
            picker.delegate.toggle_item_selected(1, window, cx);
        });
        assert_eq!(
            picker.read_with(cx, |picker, _| picker.delegate.selected_item_count()),
            3
        );
        dispatch(menu::Confirm, cx);
        workspace.read_with(cx, |workspace, cx| {
            assert!(workspace.active_modal::<StartReviewModal>(cx).is_none());
        });
        assert_eq!(
            active_review(&workspace, cx).read_with(cx, |review, _| review.target().clone()),
            ReviewTarget::Commits(CommitRange {
                base: None,
                head: oid(C3)
            })
        );

        // Reviewing commits without naming them opens the picker at the newest commit.
        dispatch(ReviewCommits::default(), cx);
        let picker = start_review_picker(&workspace, cx);
        assert_eq!(selected_key(&picker, cx), Some(EntryKey::Commit(0)));
        dispatch(menu::Confirm, cx);
        assert_eq!(
            active_review(&workspace, cx).read_with(cx, |review, _| review.target().clone()),
            ReviewTarget::Commits(CommitRange {
                base: Some(oid(C2)),
                head: oid(C3)
            })
        );
        assert_eq!(review_count(&workspace, cx), 2);

        dispatch(StartReview, cx);
        let picker = start_review_picker(&workspace, cx);
        picker.update_in(cx, |picker, window, cx| {
            picker.set_selected_index(2, None, false, window, cx)
        });
        dispatch(menu::Confirm, cx);
        assert_eq!(
            active_review(&workspace, cx).read_with(cx, |review, _| review.target().clone()),
            ReviewTarget::branch("origin/main").unwrap()
        );
    }

    /// A checkout of the sandbox repository at PR #1's head, and a fake GitHub that knows its
    /// open pull requests and PR #1. `fail_lists` makes listing answer 401; `base_oid`, when set,
    /// replaces PR #1's base commit.
    async fn pull_request_fixture(
        fail_lists: Arc<std::sync::atomic::AtomicBool>,
        base_oid: Arc<std::sync::Mutex<Option<String>>>,
        cx: &mut TestAppContext,
    ) -> (Arc<FakeFs>, Entity<Workspace>, &mut VisualTestContext) {
        use crate::github::{
            set_fake_token_source,
            test_support::{
                OPEN_PULL_REQUESTS, PR1_DETAILS, Reply, fake_github, repository_context_reply,
            },
        };
        use std::sync::atomic::Ordering;

        crate::init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ ".git": {}, "a.txt": "a\n" }))
            .await;
        fs.set_head_and_index_for_repo(dot_git(), &[("a.txt", "a\n".into())]);
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
        let (http, _) = fake_github(move |request| match request.operation() {
            Some("RepositoryContext") => repository_context_reply(request),
            Some("OpenPullRequests" | "SearchPullRequests")
                if fail_lists.load(Ordering::SeqCst) =>
            {
                Reply::status(401, r#"{"message":"Bad credentials"}"#)
            }
            Some("OpenPullRequests") => Reply::json(OPEN_PULL_REQUESTS),
            Some("SearchPullRequests") => Reply::json(r#"{"data":{"search":{"nodes":[{}]}}}"#),
            Some("PullRequestDetails") => match base_oid.lock().unwrap().as_deref() {
                Some(base_oid) => Reply::json(PR1_DETAILS.replace(PR1_BASE_OID, base_oid)),
                None => Reply::json(PR1_DETAILS),
            },
            _ => Reply::status(500, "{}"),
        });
        cx.update(|cx| {
            cx.set_http_client(http);
            set_fake_token_source(cx, |_| Some("token".into()));
        });
        let (workspace, cx) = open_workspace(fs.clone(), cx).await;
        (fs, workspace, cx)
    }

    fn pull_request_keys(numbers: &[u64]) -> Vec<Option<EntryKey>> {
        numbers
            .iter()
            .map(|number| Some(EntryKey::PullRequest(*number)))
            .collect()
    }

    #[gpui::test]
    async fn test_pull_request_section(cx: &mut TestAppContext) {
        let base_oid = Arc::new(std::sync::Mutex::new(None));
        let (fs, workspace, cx) =
            pull_request_fixture(Default::default(), base_oid.clone(), cx).await;

        dispatch(StartReview, cx);
        let picker = start_review_picker(&workspace, cx);
        let keys = entry_keys(&picker, cx);
        let pull_requests_header = keys.len() - 5;
        assert_eq!(
            keys[pull_requests_header..],
            [None]
                .into_iter()
                .chain(pull_request_keys(&[4, 1, 3, 2]))
                .collect::<Vec<_>>()
        );
        picker.read_with(cx, |picker, _| {
            assert_eq!(
                picker.delegate.entries()[pull_requests_header],
                Entry::Header(Section::PullRequests)
            );
            assert_eq!(
                picker.delegate.pull_request_section_title(),
                "Pull Requests · drew-council/code-review-sandbox"
            );
        });
        assert_eq!(selected_key(&picker, cx), Some(EntryKey::Uncommitted));

        // Review requests, and back.
        dispatch(TogglePullRequestFilter, cx);
        let keys = entry_keys(&picker, cx);
        assert_eq!(keys[keys.len() - 2..], [None, None]);
        picker.read_with(cx, |picker, _| {
            assert_eq!(
                picker.delegate.entries().last(),
                Some(&Entry::Status(
                    "No pull requests request your review".into()
                ))
            );
            assert_eq!(
                picker.delegate.pull_request_section_title(),
                "Review Requests · drew-council/code-review-sandbox"
            );
        });
        dispatch(TogglePullRequestFilter, cx);
        assert_eq!(
            entry_keys(&picker, cx)[pull_requests_header + 1..],
            pull_request_keys(&[4, 1, 3, 2])
        );

        // A number selects the pull request; one that isn't listed gets its own row.
        picker.update_in(cx, |picker, window, cx| picker.set_query("#1", window, cx));
        cx.run_until_parked();
        assert_eq!(selected_key(&picker, cx), Some(EntryKey::PullRequest(1)));
        picker.update_in(cx, |picker, window, cx| {
            picker.set_query("drew-council/code-review-sandbox#77", window, cx)
        });
        cx.run_until_parked();
        assert_eq!(selected_key(&picker, cx), Some(EntryKey::OpenReference));
        picker.update_in(cx, |picker, window, cx| {
            picker.set_query(
                "https://github.com/drew-council/code-review-sandbox/pull/1/files",
                window,
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(selected_key(&picker, cx), Some(EntryKey::PullRequest(1)));

        // HEAD is the pull request's head already, so it opens without a checkout.
        dispatch(menu::Confirm, cx);
        let review = active_review(&workspace, cx);
        review.read_with(cx, |review, cx| {
            let ReviewTarget::PullRequest(target) = review.target() else {
                panic!("expected a pull request review");
            };
            assert_eq!(target.number, 1);
            assert_eq!(target.repo.to_string(), "drew-council/code-review-sandbox");
            assert_eq!(target.base.name(), PR1_BASE_OID);
            assert_eq!(
                review.session().read(cx).key().0,
                "pr:github.com/drew-council/code-review-sandbox#1"
            );
            assert_eq!(
                review.pull_request().unwrap().read(cx).details().head_oid,
                oid(PR1_HEAD_OID)
            );
        });
        assert_eq!(
            tab_title(&review, cx),
            "PR #1: Add template filters and retire the legacy renderer"
        );
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert!(workspace.active_modal::<StartReviewModal>(cx).is_none());
        });

        // The direct action, with a reference: the same tab.
        dispatch(
            OpenPullRequest {
                reference: Some("1".into()),
            },
            cx,
        );
        assert_eq!(active_review(&workspace, cx), review);
        assert_eq!(review_count(&workspace, cx), 1);

        // Without one, it opens the picker at the pull requests.
        dispatch(OpenPullRequest::default(), cx);
        let picker = start_review_picker(&workspace, cx);
        assert_eq!(selected_key(&picker, cx), Some(EntryKey::PullRequest(4)));
        dispatch(menu::Cancel, cx);

        // A reference that doesn't parse is an error, and nothing opens.
        dispatch(
            OpenPullRequest {
                reference: Some("main".into()),
            },
            cx,
        );
        assert_eq!(review_count(&workspace, cx), 1);

        // The pull request was rebased: opening it again moves its tab to the new base.
        insert_commit_ref(&fs, C1, C1);
        *base_oid.lock().unwrap() = Some(C1.to_string());
        dispatch(
            OpenPullRequest {
                reference: Some("#1".into()),
            },
            cx,
        );
        assert_eq!(review_count(&workspace, cx), 1);
        review.read_with(cx, |review, cx| {
            let ReviewTarget::PullRequest(target) = review.target() else {
                panic!("expected a pull request review");
            };
            assert_eq!(target.base.name(), C1);
            assert_eq!(
                review.pull_request().unwrap().read(cx).details().base_oid,
                oid(C1)
            );
        });
    }

    #[gpui::test]
    async fn test_pull_request_section_errors(cx: &mut TestAppContext) {
        use std::sync::atomic::{AtomicBool, Ordering};

        let fail_lists = Arc::new(AtomicBool::new(true));
        let (_fs, workspace, cx) =
            pull_request_fixture(fail_lists.clone(), Default::default(), cx).await;
        dispatch(StartReview, cx);
        let picker = start_review_picker(&workspace, cx);
        picker.read_with(cx, |picker, _| {
            assert_eq!(
                picker.delegate.entries().last(),
                Some(&Entry::Status(
                    "GitHub rejected the GitHub CLI's token for github.com. Run \
                     `gh auth login --hostname github.com`."
                        .into()
                ))
            );
        });
        // Toggling tries again (resolving the forge again after the 401).
        fail_lists.store(false, Ordering::SeqCst);
        dispatch(TogglePullRequestFilter, cx);
        dispatch(TogglePullRequestFilter, cx);
        let keys = entry_keys(&picker, cx);
        assert_eq!(keys[keys.len() - 4..], pull_request_keys(&[4, 1, 3, 2]));
    }

    /// A remote whose host merely has "github" in its name: the picker says GitHub isn't
    /// configured for it and nothing, least of all a token, is sent there.
    #[gpui::test]
    async fn test_pull_requests_of_a_host_that_only_looks_like_github(cx: &mut TestAppContext) {
        use crate::github::{
            FakeLookups, set_fake_lookups,
            test_support::{fake_github, repository_context_reply},
        };
        use collections::HashMap;
        use git::GitHostingProviderRegistry;

        crate::init_test(cx);
        cx.update(|cx| {
            GitHostingProviderRegistry::default_global(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ ".git": {}, "a.txt": "a\n" }))
            .await;
        fs.set_head_and_index_for_repo(dot_git(), &[("a.txt", "a\n".into())]);
        fs.set_remote_for_repo(dot_git(), "origin", "https://github.attacker.dev/x/y.git");
        let (http, requests) = fake_github(repository_context_reply);
        cx.update(|cx| {
            cx.set_http_client(http);
            set_fake_lookups(
                cx,
                FakeLookups {
                    environment: HashMap::from_iter([(
                        "GITHUB_TOKEN".to_string(),
                        "secret".to_string(),
                    )]),
                    gh: Box::new(|_| None),
                    ssh_hostnames: HashMap::default(),
                },
            );
        });
        let (workspace, cx) = open_workspace(fs, cx).await;

        dispatch(StartReview, cx);
        let picker = start_review_picker(&workspace, cx);
        picker.read_with(cx, |picker, _| {
            let entries = picker.delegate.entries();
            assert!(entries.contains(&Entry::Header(Section::PullRequests)));
            assert_eq!(
                entries.last(),
                Some(&Entry::Status(
                    "GitHub isn't configured for github.attacker.dev. If it is a GitHub \
                     Enterprise Server, add it to `git_hosting_providers` in your settings \
                     (provider \"github\") or run `gh auth login --hostname \
                     github.attacker.dev`."
                        .into()
                ))
            );
        });
        // Not even when asked again, or for a pull request by number.
        dispatch(TogglePullRequestFilter, cx);
        dispatch(
            OpenPullRequest {
                reference: Some("1".into()),
            },
            cx,
        );
        assert_eq!(requests.lock().unwrap().len(), 0);
    }

    #[test]
    fn test_commit_selection_stays_contiguous() {
        let mut selection = CommitSelection::default();
        selection.toggle(2);
        assert_eq!(selection.range(), Some((2, 2)));
        // Outside the range: extends it, filling the gap.
        selection.toggle(5);
        assert_eq!(selection.range(), Some((2, 5)));
        assert_eq!(selection.len(), 4);
        selection.toggle(0);
        assert_eq!(selection.range(), Some((0, 5)));
        // The ends shrink the range.
        selection.toggle(0);
        assert_eq!(selection.range(), Some((1, 5)));
        selection.toggle(5);
        assert_eq!(selection.range(), Some((1, 4)));
        // The middle cuts the range back to the newer commits.
        selection.toggle(3);
        assert_eq!(selection.range(), Some((1, 2)));
        assert!(selection.contains(1) && selection.contains(2) && !selection.contains(3));
        // The only selected commit clears the selection.
        selection.toggle(1);
        selection.toggle(2);
        assert_eq!(selection.range(), None);
        assert_eq!(selection.len(), 0);

        // Toggle-and-next sweeps a contiguous run.
        for index in 1..=3 {
            selection.toggle(index);
        }
        assert_eq!(selection.range(), Some((1, 3)));
        selection.clear();
        assert_eq!(selection.range(), None);
    }
}
