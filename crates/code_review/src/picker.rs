use crate::{
    CodeReviewSettings, EditPullRequestSearch, OpenPullRequest, StartReview,
    commits::{RECENT_COMMITS_LIMIT, first_parent_chain},
    forge::{
        self, Forge, ForgeRepo, GitHubError, PullRequestStack, PullRequestStatus,
        PullRequestSummary, ReviewDecision, resolve_forge,
    },
    pull_request::{OpenPullRequestOptions, open_pull_request_and_notify},
    target::{
        BranchBase, CommitRange, PullRequestReference, ReviewTarget, parse_pull_request_reference,
    },
    view::{ReviewView, active_repository, pick_base_for_new_review},
};
use anyhow::Context as _;
use collections::HashMap;
use fuzzy::StringMatchCandidate;
use git::repository::CommitData;
use gpui::{
    Action as _, AnyElement, DismissEvent, ElementId, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, Subscription, Task, WeakEntity,
};
use picker::{Picker, PickerDelegate};
use project::{Project, git_store::Repository};
use settings::Settings as _;
use std::{sync::Arc, time::Duration};
use time::{OffsetDateTime, UtcOffset};
use ui::{
    Chip, HighlightedLabel, KeyBinding, ListItem, ListItemSpacing, ListSubHeader, Tooltip,
    prelude::*,
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

/// How long editing the pull request search waits for typing to pause before searching.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);

/// The Start Review picker: uncommitted changes and the current branch against its base; then the
/// pull requests of the repository's GitHub remote that a GitHub search finds (the
/// `code_review.pull_request_search` setting, editable in the picker), the stacked ones as one
/// row per stack, and any pull request by number or URL; then recent commits, where
/// multi-selection picks a range.
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
        let settings = CodeReviewSettings::get_global(cx);
        let search = settings.pull_request_search.clone();
        let delegate = StartReviewDelegate::new(
            workspace,
            project,
            repo,
            focus,
            search.clone(),
            settings.hide_approved_pull_requests,
            cx.focus_handle(),
        );
        // Rows carry a stack's chain and several chips, so the picker takes most of the window
        // (Zed's pickers can be dragged bigger, and remember it).
        let viewport = window.viewport_size();
        let width = Rems::from_pixels(viewport.width * 0.7, window);
        let width = Rems(width.0.max(picker::DEFAULT_MODAL_WIDTH.0));
        let height = Rems::from_pixels(viewport.height * 0.8, window);
        let picker = cx.new(|cx| {
            Picker::list(delegate, window, cx)
                .initial_width(width)
                .max_height(height)
                .show_scrollbar(true)
        });
        picker.update(cx, |picker, cx| {
            picker.delegate.focus_handle = picker.focus_handle(cx);
            picker
                .delegate
                .load_pull_requests(search, false, window, cx);
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

impl StartReviewModal {
    fn finish_editing_search(&mut self, apply: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.picker.read(cx).delegate.is_editing_search() {
            return;
        }
        cx.stop_propagation();
        self.picker.update(cx, |picker, cx| {
            StartReviewDelegate::finish_editing_search(picker, apply, window, cx)
        });
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
            .debug_selector(|| "start-review-modal".into())
            .on_action(cx.listener(|this, _: &EditPullRequestSearch, window, cx| {
                this.picker.update(cx, |picker, cx| {
                    if picker.delegate.is_editing_search() {
                        StartReviewDelegate::finish_editing_search(picker, true, window, cx);
                    } else {
                        StartReviewDelegate::start_editing_search(picker, window, cx);
                    }
                })
            }))
            // While the query edits the search, Enter applies it and Escape reverts it instead of
            // opening a row or closing the picker.
            .capture_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                this.finish_editing_search(true, window, cx)
            }))
            .capture_action(cx.listener(|this, _: &menu::SecondaryConfirm, window, cx| {
                this.finish_editing_search(true, window, cx)
            }))
            .capture_action(cx.listener(|this, _: &menu::Cancel, window, cx| {
                this.finish_editing_search(false, window, cx)
            }))
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

#[derive(Clone)]
pub(crate) enum PullRequestList {
    Loading,
    Loaded(SearchResults),
    Failed(SharedString),
    /// The repository has no GitHub remote: the picker has no pull request section.
    Unsupported,
}

/// What a pull request search found, without the pull requests the viewer approved when those
/// are hidden.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SearchResults {
    /// The pull requests in no stack: most recently updated first, unless the search sorts them.
    pub(crate) pull_requests: Vec<PullRequestSummary>,
    /// The stacks of the others, in the order of their first pull request found.
    pub(crate) stacks: Vec<ListedStack>,
}

impl SearchResults {
    fn set_review_decisions(&mut self, decisions: &HashMap<SharedString, ReviewDecision>) {
        let stacked = self
            .stacks
            .iter_mut()
            .flat_map(|listed| &mut listed.stack.pull_requests);
        for pull_request in self.pull_requests.iter_mut().chain(stacked) {
            pull_request.review_decision = decisions.get(&pull_request.node_id).copied();
        }
    }
}

/// A stack the search found pull requests of. The whole stack is listed as one row, which opens
/// its review start.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ListedStack {
    pub(crate) stack: PullRequestStack,
    /// The index of [`PullRequestStack::review_start`].
    pub(crate) start: usize,
}

impl ListedStack {
    fn new(stack: PullRequestStack) -> Self {
        Self {
            start: stack.review_start(),
            stack,
        }
    }

    pub(crate) fn start(&self) -> Option<&PullRequestSummary> {
        self.stack.pull_requests.get(self.start)
    }
}

/// Splits what a search `found` into the pull requests in no stack and the stacks of the others
/// (`stacks` has the stacks of everything found). With `hide_approved`, the pull requests the
/// viewer approved are left out first, and their number is returned: a stack is listed while it
/// has a pull request found that isn't left out.
pub(crate) fn group_search_results(
    mut found: Vec<PullRequestSummary>,
    stacks: Vec<PullRequestStack>,
    hide_approved: bool,
) -> (SearchResults, usize) {
    let count = found.len();
    if hide_approved {
        found.retain(|pull_request| pull_request.viewer_review != Some(ReviewDecision::Approved));
    }
    let approved_hidden = count - found.len();
    let mut results = SearchResults::default();
    for pull_request in found {
        match stacks
            .iter()
            .find(|stack| stack.contains(pull_request.number))
        {
            Some(stack) => {
                if !results
                    .stacks
                    .iter()
                    .any(|listed| listed.stack.number == stack.number)
                {
                    results.stacks.push(ListedStack::new(stack.clone()));
                }
            }
            None => results.pull_requests.push(pull_request),
        }
    }
    (results, approved_hidden)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Section {
    Local,
    Stacks,
    PullRequests,
    Commits,
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
    /// `index` is into the loaded stacks.
    Stack {
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
    /// By the stack's number.
    Stack(u64),
    OpenReference,
}

impl Entry {
    fn key(&self, results: &SearchResults) -> Option<EntryKey> {
        match self {
            Entry::Uncommitted { .. } => Some(EntryKey::Uncommitted),
            Entry::Branch { .. } => Some(EntryKey::Branch),
            Entry::Commit { index, .. } => Some(EntryKey::Commit(*index)),
            Entry::PullRequest { index, .. } => results
                .pull_requests
                .get(*index)
                .map(|pull_request| EntryKey::PullRequest(pull_request.number)),
            Entry::Stack { index, .. } => results
                .stacks
                .get(*index)
                .map(|listed| EntryKey::Stack(listed.stack.number)),
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
const FIRST_STACK_CANDIDATE: usize = 1 << 21;

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

/// What editing the search replaced, to put back when the edit is cancelled.
struct SearchEdit {
    query: String,
    search: String,
    pull_requests: PullRequestList,
    approved_hidden: usize,
}

pub struct StartReviewDelegate {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    repo: Entity<Repository>,
    default_base: BaseState,
    commits: RecentCommits,
    selection: CommitSelection,
    /// Resolved on the first load and reused when the search changes; dropped after an error, so
    /// that the next load resolves it again (with a new token, say).
    forge: Option<Arc<dyn Forge>>,
    /// The GitHub search the pull requests are listed by.
    search: String,
    /// The search `pull_requests` holds (or is loading) the results of.
    searched: Option<String>,
    searching: bool,
    hide_approved: bool,
    pull_requests: PullRequestList,
    approved_hidden: usize,
    load_pull_requests_task: Task<()>,
    /// Set while the query edits the search.
    editing_search: Option<SearchEdit>,
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
        search: String,
        hide_approved: bool,
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
            search,
            searched: None,
            searching: false,
            hide_approved,
            pull_requests: PullRequestList::Loading,
            approved_hidden: 0,
            load_pull_requests_task: Task::ready(()),
            editing_search: None,
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

    fn search_results(&self) -> &SearchResults {
        static NONE: SearchResults = SearchResults {
            pull_requests: Vec::new(),
            stacks: Vec::new(),
        };
        match &self.pull_requests {
            PullRequestList::Loaded(results) => results,
            PullRequestList::Loading
            | PullRequestList::Failed(_)
            | PullRequestList::Unsupported => &NONE,
        }
    }

    fn loaded_pull_requests(&self) -> &[PullRequestSummary] {
        &self.search_results().pull_requests
    }

    fn loaded_stacks(&self) -> &[ListedStack] {
        &self.search_results().stacks
    }

    fn entry_key(&self, index: usize) -> Option<EntryKey> {
        self.entries
            .get(index)
            .and_then(|entry| entry.key(self.search_results()))
    }

    /// The repository whose pull requests are listed, once known.
    pub(crate) fn pull_request_repo(&self) -> Option<&ForgeRepo> {
        self.forge.as_ref().map(|forge| &forge.context().canonical)
    }

    pub(crate) fn is_editing_search(&self) -> bool {
        self.editing_search.is_some()
    }

    /// Searches the pull requests. `debounce` (while the search is being typed) waits for typing
    /// to pause and keeps the current results until the new ones arrive.
    fn load_pull_requests(
        &mut self,
        search: String,
        debounce: bool,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.searched = Some(search.clone());
        if matches!(self.pull_requests, PullRequestList::Unsupported) {
            return;
        }
        if !debounce {
            self.pull_requests = PullRequestList::Loading;
            self.approved_hidden = 0;
        }
        self.searching = true;
        let forge = match &self.forge {
            Some(forge) => Task::ready(Ok(forge.clone())),
            None => resolve_forge(&self.repo, None, &self.project, cx),
        };
        let hide_approved = self.hide_approved;
        self.load_pull_requests_task = cx.spawn_in(window, async move |picker, cx| {
            if debounce {
                cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            }
            let result = async {
                let forge = forge.await?;
                let found = cx
                    .background_executor()
                    .spawn(forge.search_pull_requests(search))
                    .await?;
                let node_ids = found
                    .iter()
                    .map(|pull_request| pull_request.node_id.clone())
                    .collect();
                // Without stacks (a GitHub that has none), every pull request is listed alone.
                let stacks = cx
                    .background_executor()
                    .spawn(forge.pull_request_stacks(node_ids))
                    .await
                    .unwrap_or_else(|error| {
                        log::warn!("finding the stacks of pull requests: {error:#}");
                        Vec::new()
                    });
                anyhow::Ok((forge, found, stacks))
            }
            .await;
            // The rows show first; their review decisions follow (GitHub is slow to work them
            // out).
            let decisions_request = result.as_ref().ok().map(|(forge, found, stacks)| {
                let node_ids = found
                    .iter()
                    .chain(stacks.iter().flat_map(|stack| &stack.pull_requests))
                    .map(|pull_request| pull_request.node_id.clone())
                    .collect::<Vec<_>>();
                forge.review_decisions(node_ids)
            });
            picker
                .update_in(cx, |picker, window, cx| {
                    let delegate = &mut picker.delegate;
                    delegate.searching = false;
                    match result {
                        Ok((forge, found, stacks)) => {
                            delegate.forge = Some(forge);
                            let (results, approved_hidden) =
                                group_search_results(found, stacks, hide_approved);
                            delegate.approved_hidden = approved_hidden;
                            delegate.pull_requests = PullRequestList::Loaded(results);
                        }
                        Err(error) => {
                            delegate.forge = None;
                            delegate.approved_hidden = 0;
                            delegate.pull_requests = match forge::github_error(&error) {
                                Some(GitHubError::Unsupported) => PullRequestList::Unsupported,
                                _ => {
                                    log::error!("searching pull requests: {error:#}");
                                    PullRequestList::Failed(forge::error_message(&error))
                                }
                            };
                        }
                    }
                    picker.refresh(window, cx);
                })
                .log_err();
            let Some(decisions_request) = decisions_request else {
                return;
            };
            let decisions = match cx.background_executor().spawn(decisions_request).await {
                Ok(decisions) => decisions,
                Err(error) => {
                    log::warn!("loading the review decisions of pull requests: {error:#}");
                    return;
                }
            };
            picker
                .update(cx, |picker, cx| {
                    if let PullRequestList::Loaded(results) = &mut picker.delegate.pull_requests {
                        results.set_review_decisions(&decisions);
                        cx.notify();
                    }
                })
                .log_err();
        });
    }

    /// Turns the query into the pull request search, showing its results as it is typed.
    fn start_editing_search(
        picker: &mut Picker<Self>,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        if picker.delegate.is_editing_search()
            || matches!(picker.delegate.pull_requests, PullRequestList::Unsupported)
        {
            return;
        }
        let query = picker.query(cx);
        let delegate = &mut picker.delegate;
        delegate.selection.clear();
        delegate.editing_search = Some(SearchEdit {
            query,
            search: delegate.search.clone(),
            pull_requests: delegate.pull_requests.clone(),
            approved_hidden: delegate.approved_hidden,
        });
        let search = delegate.search.clone();
        picker.set_query(&search, window, cx);
        picker.refresh_placeholder(window, cx);
        picker.refresh(window, cx);
    }

    /// Back to the query: `apply` keeps the edited search, otherwise the previous one returns
    /// with its results.
    pub(crate) fn finish_editing_search(
        picker: &mut Picker<Self>,
        apply: bool,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        let Some(edit) = picker.delegate.editing_search.take() else {
            return;
        };
        if apply {
            let search = picker.query(cx);
            let delegate = &mut picker.delegate;
            // Applying the same search again retries it after an error.
            if delegate.searching
                || delegate.searched.as_deref() != Some(search.as_str())
                || matches!(delegate.pull_requests, PullRequestList::Failed(_))
            {
                delegate.load_pull_requests(search.clone(), false, window, cx);
            }
            delegate.search = search;
            delegate.select_first_pull_request = true;
        } else {
            let delegate = &mut picker.delegate;
            match edit.pull_requests {
                PullRequestList::Loading => {
                    delegate.load_pull_requests(edit.search.clone(), false, window, cx)
                }
                pull_requests => {
                    delegate.load_pull_requests_task = Task::ready(());
                    delegate.searching = false;
                    delegate.searched = Some(edit.search.clone());
                    delegate.pull_requests = pull_requests;
                    delegate.approved_hidden = edit.approved_hidden;
                }
            }
            delegate.search = edit.search;
        }
        picker.set_query(&edit.query, window, cx);
        picker.refresh_placeholder(window, cx);
        picker.refresh(window, cx);
    }

    pub(crate) fn pull_request_section_title(&self) -> SharedString {
        match self.pull_request_repo() {
            Some(repo) => format!("Pull Requests · {repo}").into(),
            None => "Pull Requests".into(),
        }
    }

    /// The pull request section's status rows: loading, an error, nothing found, and how many
    /// approved pull requests are hidden.
    fn pull_request_statuses(&self, include_hidden: bool) -> Vec<SharedString> {
        let mut statuses = Vec::new();
        match &self.pull_requests {
            PullRequestList::Unsupported => {}
            PullRequestList::Loading => statuses.push("Loading pull requests…".into()),
            PullRequestList::Failed(error) => statuses.push(error.clone()),
            PullRequestList::Loaded(results)
                if results.pull_requests.is_empty() && results.stacks.is_empty() =>
            {
                statuses.push("No pull requests match the search".into())
            }
            PullRequestList::Loaded(_) => {}
        }
        match self.approved_hidden {
            _ if !include_hidden => {}
            0 => {}
            1 => statuses.push("1 pull request you approved is hidden".into()),
            count => statuses.push(format!("{count} pull requests you approved are hidden").into()),
        }
        statuses
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

    /// The stack's review start first (what its row shows), then the others from the bottom.
    fn stack_candidate(listed: &ListedStack) -> String {
        let mut candidate = listed
            .start()
            .map(Self::pull_request_candidate)
            .unwrap_or_default();
        for (index, pull_request) in listed.stack.pull_requests.iter().enumerate() {
            if index != listed.start {
                candidate.push_str(&format!(" #{} {}", pull_request.number, pull_request.title));
            }
        }
        candidate
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
        candidates.extend(
            self.loaded_stacks()
                .iter()
                .enumerate()
                .map(|(index, listed)| {
                    StringMatchCandidate::new(
                        FIRST_STACK_CANDIDATE + index,
                        &Self::stack_candidate(listed),
                    )
                }),
        );
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
        let mut stack_entries = Vec::new();
        let mut pull_request_entries = Vec::new();
        let mut commit_entries = Vec::new();
        for (candidate, positions) in matches {
            match candidate {
                UNCOMMITTED_CANDIDATE => local_entries.push(Entry::Uncommitted { positions }),
                BRANCH_CANDIDATE => local_entries.push(Entry::Branch { positions }),
                candidate if candidate >= FIRST_STACK_CANDIDATE => {
                    stack_entries.push(Entry::Stack {
                        index: candidate - FIRST_STACK_CANDIDATE,
                        positions,
                    })
                }
                candidate if candidate >= FIRST_PULL_REQUEST_CANDIDATE => pull_request_entries
                    .push(Entry::PullRequest {
                        index: candidate - FIRST_PULL_REQUEST_CANDIDATE,
                        positions,
                    }),
                candidate => commit_entries.push(Entry::Commit {
                    index: candidate - FIRST_COMMIT_CANDIDATE,
                    positions,
                }),
            }
        }
        let commit_status = match &self.commits {
            RecentCommits::Loading => Some("Loading commits…".into()),
            RecentCommits::Failed(error) => Some(error.clone()),
            RecentCommits::Loaded(commits) if commits.is_empty() => Some("No commits".into()),
            RecentCommits::Loaded(_) => None,
        };
        let mut entries = Vec::new();
        if !local_entries.is_empty() {
            entries.push(Entry::Header(Section::Local));
            for entry in local_entries {
                let is_branch = matches!(entry, Entry::Branch { .. });
                entries.push(entry);
                if is_branch && let BaseState::Failed(error) = &self.default_base {
                    entries.push(Entry::Status(error.clone()));
                }
            }
        }

        // A query that names a pull request selects it: Enter opens it.
        let reference = parse_pull_request_reference(&self.query).ok();
        let listed_reference = reference
            .as_ref()
            .and_then(|reference| self.listed_pull_request(reference));
        let pull_request_statuses = self.pull_request_statuses(self.query.is_empty());
        // The named pull request comes first, above rows that only fuzzy-match the query (`1`
        // matches #17's title, say). A URL doesn't fuzzy-match its pull request's row at all.
        if let Some(number) = listed_reference
            && let Some(index) = self
                .loaded_pull_requests()
                .iter()
                .position(|pull_request| pull_request.number == number)
        {
            let entry = match pull_request_entries.iter().position(
                |entry| matches!(entry, Entry::PullRequest { index: shown, .. } if *shown == index),
            ) {
                Some(position) => pull_request_entries.remove(position),
                None => Entry::PullRequest {
                    index,
                    positions: Vec::new(),
                },
            };
            pull_request_entries.insert(0, entry);
        }
        if !stack_entries.is_empty() {
            entries.push(Entry::Header(Section::Stacks));
            entries.extend(stack_entries);
        }
        if !matches!(self.pull_requests, PullRequestList::Unsupported) {
            let open_reference = reference
                .clone()
                .filter(|_| listed_reference.is_none())
                .map(Entry::OpenReference);
            if open_reference.is_some()
                || !pull_request_entries.is_empty()
                || !pull_request_statuses.is_empty()
            {
                entries.push(Entry::Header(Section::PullRequests));
                entries.extend(open_reference);
                entries.extend(pull_request_entries);
                entries.extend(pull_request_statuses.into_iter().map(Entry::Status));
            }
        }
        if !commit_entries.is_empty() || commit_status.is_some() {
            entries.push(Entry::Header(Section::Commits));
            entries.extend(commit_entries);
            entries.extend(commit_status.map(Entry::Status));
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
                self.loaded_stacks()
                    .first()
                    .map(|listed| EntryKey::Stack(listed.stack.number))
                    .or_else(|| {
                        self.loaded_pull_requests()
                            .first()
                            .map(|pull_request| EntryKey::PullRequest(pull_request.number))
                    })
            } else if reference_key.is_some()
                && (query_changed || previous_key == Some(EntryKey::OpenReference))
            {
                reference_key
            } else {
                previous_key
            };
        let results = self.search_results();
        self.selected_index = wanted_key
            .and_then(|key| {
                self.entries
                    .iter()
                    .position(|entry| entry.key(results) == Some(key.clone()))
            })
            .or_else(|| {
                self.entries
                    .iter()
                    .position(|entry| entry.key(results).is_some())
            })
            .unwrap_or(0);
    }

    /// While the search is edited: only the stacks and pull requests, nothing selectable.
    fn set_search_entries(&mut self) {
        let mut entries = Vec::new();
        if !self.loaded_stacks().is_empty() {
            entries.push(Entry::Header(Section::Stacks));
            entries.extend((0..self.loaded_stacks().len()).map(|index| Entry::Stack {
                index,
                positions: Vec::new(),
            }));
        }
        entries.push(Entry::Header(Section::PullRequests));
        entries.extend(
            (0..self.loaded_pull_requests().len()).map(|index| Entry::PullRequest {
                index,
                positions: Vec::new(),
            }),
        );
        entries.extend(
            self.pull_request_statuses(true)
                .into_iter()
                .map(Entry::Status),
        );
        self.entries = entries;
        self.selected_index = 0;
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
        Some(render_pull_request_row(
            ("start-review-pull-request", entry_index).into(),
            pull_request,
            positions,
            selected,
            self.timezone,
        ))
    }

    fn render_stack(
        &self,
        entry_index: usize,
        index: usize,
        positions: &[usize],
        selected: bool,
    ) -> Option<AnyElement> {
        let listed = self.loaded_stacks().get(index)?;
        render_stack_row(
            ("start-review-stack", entry_index).into(),
            listed,
            positions,
            selected,
            self.timezone,
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
        if self.is_editing_search() {
            "GitHub search for pull requests (is:open author:octocat)…".into()
        } else {
            "Review changes, the branch, a pull request (#123, URL), or commits…".into()
        }
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
        !self.is_editing_search() && self.entry_key(index).is_some()
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        if self.is_editing_search() {
            if self.searched.as_deref() != Some(query.as_str()) {
                self.load_pull_requests(query, true, window, cx);
            }
            self.set_search_entries();
            return Task::ready(());
        }
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
        // `StartReviewModal` applies the edited search instead.
        if self.is_editing_search() {
            return;
        }
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
            EntryKey::Stack(stack_number) => {
                let start = self
                    .loaded_stacks()
                    .iter()
                    .find(|listed| listed.stack.number == stack_number)
                    .and_then(ListedStack::start);
                if let Some(start) = start {
                    let reference = PullRequestReference {
                        repo: None,
                        number: start.number,
                    };
                    self.open_pull_request(reference, secondary, window, cx);
                }
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
            Entry::Header(Section::Stacks) => Some(
                div()
                    .pt_1()
                    .child(ListSubHeader::new("Stacks").inset(true))
                    .into_any_element(),
            ),
            Entry::Stack { index, positions } => {
                self.render_stack(entry_index, *index, positions, selected)
            }
            Entry::Header(Section::PullRequests) => Some(
                div()
                    .pt_1()
                    .child(ListSubHeader::new(self.pull_request_section_title()).inset(true))
                    .into_any_element(),
            ),
            Entry::Header(Section::Commits) => Some(
                div()
                    .pt_1()
                    .child(ListSubHeader::new("Recent Commits").inset(true))
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
            // Hints (how to set up a token, a GitHub host) are often longer than a row, and the
            // picker's list measures rows unwrapped: the whole text is in the tooltip.
            Entry::Status(text) => Some(
                ListItem::new(("start-review-status", entry_index))
                    .inset(true)
                    .spacing(ListItemSpacing::Sparse)
                    .tooltip(Tooltip::text(text.clone()))
                    .child(Label::new(text.clone()).color(Color::Muted).truncate())
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
        let key_binding = |action: &dyn gpui::Action, cx: &App| {
            KeyBinding::for_action_in(action, focus_handle, cx).size(rems_from_px(12_f32))
        };
        let footer = h_flex()
            .w_full()
            .p_1p5()
            .gap_1()
            .justify_between()
            .border_t_1()
            .border_color(cx.theme().colors().border_variant);
        if self.is_editing_search() {
            return Some(
                footer
                    .child(
                        Label::new(if self.searching { "Searching…" } else { "" })
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("start-review-cancel-search", "Cancel")
                                    .key_binding(key_binding(&menu::Cancel, cx))
                                    .on_click(|_, window, cx| {
                                        window.dispatch_action(menu::Cancel.boxed_clone(), cx)
                                    }),
                            )
                            .child(
                                Button::new("start-review-apply-search", "Search")
                                    .key_binding(key_binding(&menu::Confirm, cx))
                                    .on_click(|_, window, cx| {
                                        window.dispatch_action(menu::Confirm.boxed_clone(), cx)
                                    }),
                            ),
                    )
                    .into_any_element(),
            );
        }
        let selected = self.entry_key(self.selected_index);
        let selected_count = self.selection.len();
        let has_pull_requests = !matches!(self.pull_requests, PullRequestList::Unsupported);
        let search = SharedString::from(format!("Search: {}", self.search));
        Some(
            footer
                .map(|footer| {
                    if selected_count == 0 && has_pull_requests {
                        footer.child(
                            Button::new("start-review-edit-search", "Edit Search")
                                .key_binding(key_binding(&EditPullRequestSearch, cx))
                                .tooltip(Tooltip::text(search))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(EditPullRequestSearch.boxed_clone(), cx)
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
                                Some(
                                    EntryKey::PullRequest(_)
                                        | EntryKey::Stack(_)
                                        | EntryKey::OpenReference
                                )
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

/// A pull request's row: number, title, author and last update, then its branches and its state,
/// review decision and the viewer's own verdict. `positions` are the matched characters of
/// `#number title`.
pub(crate) fn render_pull_request_row(
    id: ElementId,
    pull_request: &PullRequestSummary,
    positions: &[usize],
    selected: bool,
    timezone: UtcOffset,
) -> AnyElement {
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
        timezone,
        time_format::TimestampFormat::Relative,
    );
    let state = pull_request_state(pull_request);
    let (decision, viewer_review) = review_labels(pull_request);
    ListItem::new(id)
        .inset(true)
        .spacing(ListItemSpacing::Sparse)
        .toggle_state(selected)
        .child(
            v_flex()
                .w_full()
                .min_w_0()
                .gap_0p5()
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
                                ),
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
                                .child(Label::new(time).size(LabelSize::Small).color(Color::Muted)),
                        ),
                )
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .justify_between()
                        .child(
                            div().min_w_0().child(
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
                        .child(
                            h_flex()
                                .gap_1()
                                .flex_none()
                                .child(state_chip(state))
                                .when_some(decision, |chips, decision| {
                                    chips.child(state_chip(decision))
                                })
                                .when_some(viewer_review, |chips, review| {
                                    chips.child(state_chip(review))
                                }),
                        ),
                ),
        )
        .into_any_element()
}

/// A pull request's state as its rows show it: Open, Draft, Merged or Closed, and its color.
pub(crate) fn pull_request_state(pull_request: &PullRequestSummary) -> (&'static str, Color) {
    match pull_request.status {
        PullRequestStatus::Merged => ("Merged", Color::Accent),
        PullRequestStatus::Closed => ("Closed", Color::Error),
        PullRequestStatus::Open if pull_request.is_draft => ("Draft", Color::Muted),
        PullRequestStatus::Open => ("Open", Color::Success),
    }
}

/// The review decision and the viewer's own verdict of a pull request, as chips' labels.
fn review_labels(
    pull_request: &PullRequestSummary,
) -> (Option<(&'static str, Color)>, Option<(&'static str, Color)>) {
    let decision = pull_request.review_decision.map(|decision| match decision {
        ReviewDecision::Approved => ("Approved", Color::Success),
        ReviewDecision::ChangesRequested => ("Changes requested", Color::Error),
        ReviewDecision::ReviewRequired => ("Review required", Color::Warning),
    });
    let viewer_review = pull_request.viewer_review.map(|review| match review {
        ReviewDecision::Approved => ("You approved", Color::Success),
        ReviewDecision::ChangesRequested => ("You requested changes", Color::Error),
        ReviewDecision::ReviewRequired => ("Review required", Color::Muted),
    });
    (decision, viewer_review)
}

fn state_chip((label, color): (&'static str, Color)) -> Chip {
    Chip::new(label)
        .label_size(LabelSize::XSmall)
        .label_color(color)
}

/// A stack's number in its chain of pull requests: colored by its state, struck through when
/// closed, underlined when `current`.
pub(crate) fn stack_member_label(pull_request: &PullRequestSummary, current: bool) -> Label {
    let (_, color) = pull_request_state(pull_request);
    Label::new(format!("#{}", pull_request.number))
        .size(LabelSize::XSmall)
        .color(color)
        .when(pull_request.status == PullRequestStatus::Closed, |label| {
            label.strikethrough()
        })
        .when(current, |label| label.weight(FontWeight::BOLD).underline())
}

/// A stack's row: the pull request its review starts at (number, title, author, last update),
/// then the stack from its base to its top, gh-stack's way (`main ← #1 ← #2`), each pull request
/// colored by its state with the start underlined, and the start's position, state and reviews.
/// `positions` are the matched characters of the start's `#number title`.
pub(crate) fn render_stack_row(
    id: ElementId,
    listed: &ListedStack,
    positions: &[usize],
    selected: bool,
    timezone: UtcOffset,
) -> Option<AnyElement> {
    let start = listed.start()?;
    let stack = &listed.stack;
    let number = format!("#{}", start.number);
    let title_start = number.len() + 1;
    let title_end = title_start + start.title.len();
    let positions_in = |begin: usize, end: usize| {
        positions
            .iter()
            .filter(|position| (begin..end).contains(*position))
            .map(|position| position - begin)
            .collect::<Vec<_>>()
    };
    let time = time_format::format_localized_timestamp(
        start.updated_at,
        OffsetDateTime::now_utc(),
        timezone,
        time_format::TimestampFormat::Relative,
    );
    let (decision, viewer_review) = review_labels(start);
    let arrow = || {
        Label::new("←")
            .size(LabelSize::XSmall)
            .color(Color::Muted)
            .flex_none()
    };
    let chain = h_flex()
        .gap_1()
        .min_w_0()
        .overflow_hidden()
        .child(
            Label::new(stack.base_ref.clone())
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .flex_none(),
        )
        .children(
            stack
                .pull_requests
                .iter()
                .enumerate()
                .flat_map(|(index, pull_request)| {
                    [
                        arrow().into_any_element(),
                        stack_member_label(pull_request, index == listed.start)
                            .flex_none()
                            .into_any_element(),
                    ]
                }),
        );
    Some(
        ListItem::new(id)
            .inset(true)
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(selected)
            .child(
                v_flex()
                    .w_full()
                    .min_w_0()
                    .gap_0p5()
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
                                        Icon::new(IconName::ListTree)
                                            .size(IconSize::Small)
                                            .color(Color::Muted),
                                    )
                                    .child(
                                        HighlightedLabel::new(
                                            number.clone(),
                                            positions_in(0, number.len()),
                                        )
                                        .color(Color::Muted)
                                        .flex_none(),
                                    )
                                    .child(
                                        HighlightedLabel::new(
                                            start.title.clone(),
                                            positions_in(title_start, title_end),
                                        )
                                        .truncate(),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .gap_1()
                                    .flex_none()
                                    .child(
                                        Label::new(start.author.login.clone())
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                    .child(
                                        Label::new(time).size(LabelSize::Small).color(Color::Muted),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .justify_between()
                            .child(chain)
                            .child(
                                h_flex()
                                    .gap_1()
                                    .flex_none()
                                    .child(
                                        Chip::new(format!(
                                            "{} of {}",
                                            listed.start + 1,
                                            stack.pull_requests.len()
                                        ))
                                        .label_size(LabelSize::XSmall),
                                    )
                                    .child(state_chip(pull_request_state(start)))
                                    .when_some(decision, |chips, decision| {
                                        chips.child(state_chip(decision))
                                    })
                                    .when_some(viewer_review, |chips, review| {
                                        chips.child(state_chip(review))
                                    }),
                            ),
                    ),
            )
            .into_any_element(),
    )
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
                None,
                Some(EntryKey::Commit(0)),
                Some(EntryKey::Commit(1)),
                Some(EntryKey::Commit(2)),
            ]
        );
        assert_eq!(
            picker.read_with(cx, |picker, _| picker.delegate.entries()[3].clone()),
            Entry::Header(Section::Commits)
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
            picker.delegate.toggle_item_selected(4, window, cx);
            picker.delegate.toggle_item_selected(6, window, cx);
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
    /// replaces PR #1's base commit. With `stacked`, #1 and #3 are in stack 7: #5 (merged), #3
    /// (approved by the viewer), #1, #6 (a draft), from the bottom; otherwise GitHub has no
    /// stacks.
    async fn pull_request_fixture(
        fail_lists: Arc<std::sync::atomic::AtomicBool>,
        base_oid: Arc<std::sync::Mutex<Option<String>>>,
        stacked: bool,
        cx: &mut TestAppContext,
    ) -> (
        Arc<FakeFs>,
        Entity<Workspace>,
        crate::github::test_support::SentRequests,
        &mut VisualTestContext,
    ) {
        use crate::github::{
            set_fake_token_source,
            test_support::{
                OPEN_PULL_REQUESTS, PR1_DETAILS, Reply, fake_github, repository_context_reply,
                stack_node, stacked_pull_request,
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
        let (http, requests) = fake_github(move |request| match request.operation() {
            Some("RepositoryContext") => repository_context_reply(request),
            Some("SearchPullRequests") if fail_lists.load(Ordering::SeqCst) => {
                Reply::status(401, r#"{"message":"Bad credentials"}"#)
            }
            // The open pull requests, #2 approved by the viewer; nothing for "is:closed".
            Some("SearchPullRequests") => {
                let query = request.variables()["query"].as_str().unwrap_or_default();
                let mut nodes = serde_json::from_str::<serde_json::Value>(OPEN_PULL_REQUESTS)
                    .unwrap()["data"]["repository"]["pullRequests"]["nodes"]
                    .clone();
                nodes[3]["latestOpinionatedReviews"] = json!({ "nodes": [
                    { "state": "APPROVED", "author": { "login": "drew-council" } },
                ] });
                if query.contains("is:closed") {
                    nodes = json!([]);
                }
                for node in nodes.as_array_mut().into_iter().flatten() {
                    node["id"] = json!(format!("PR_{}", node["number"]));
                }
                Reply::json(json!({ "data": { "search": { "nodes": nodes } } }).to_string())
            }
            Some("PullRequestStackIds") if stacked => {
                let nodes = request.variables()["ids"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|id| match id.as_str() {
                        Some("PR_1" | "PR_3") => json!({ "stack": { "id": "PRS_7" } }),
                        _ => json!({ "stack": null }),
                    })
                    .collect::<Vec<_>>();
                Reply::json(json!({ "data": { "nodes": nodes } }).to_string())
            }
            Some("PullRequestStacks") if stacked => Reply::json(
                json!({ "data": { "nodes": [stack_node(7, vec![
                    stacked_pull_request(5, "MERGED", false, false),
                    stacked_pull_request(3, "OPEN", false, true),
                    stacked_pull_request(1, "OPEN", false, false),
                    stacked_pull_request(6, "OPEN", true, false),
                ])] } })
                .to_string(),
            ),
            Some("PullRequestReviewDecisions") => {
                let nodes = request.variables()["ids"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|id| match id.as_str() {
                        Some("PR_4") => json!({ "id": id, "reviewDecision": "CHANGES_REQUESTED" }),
                        Some("PR_1") => json!({ "id": id, "reviewDecision": "APPROVED" }),
                        _ => json!({ "id": id, "reviewDecision": null }),
                    })
                    .collect::<Vec<_>>();
                Reply::json(json!({ "data": { "nodes": nodes } }).to_string())
            }
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
        (fs, workspace, requests, cx)
    }

    /// The searches sent to GitHub, without the repository and sort the client adds.
    fn searches(
        requests: &crate::github::test_support::SentRequests,
        cx: &mut VisualTestContext,
    ) -> Vec<String> {
        cx.run_until_parked();
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.operation() == Some("SearchPullRequests"))
            .filter_map(|request| {
                let query = request.variables()["query"].as_str()?;
                let query = query.strip_prefix("repo:drew-council/code-review-sandbox is:pr ")?;
                Some(query.strip_suffix(" sort:updated-desc")?.to_string())
            })
            .collect()
    }

    fn statuses(
        picker: &Entity<Picker<StartReviewDelegate>>,
        cx: &mut VisualTestContext,
    ) -> Vec<String> {
        picker.read_with(cx, |picker, _| {
            picker
                .delegate
                .entries()
                .iter()
                .filter_map(|entry| match entry {
                    Entry::Status(status) => Some(status.to_string()),
                    _ => None,
                })
                .collect()
        })
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
        let (fs, workspace, requests, cx) =
            pull_request_fixture(Default::default(), base_oid.clone(), false, cx).await;

        // Local first, then the pull requests the setting's search finds (without the one the
        // viewer approved), then the commits.
        dispatch(StartReview, cx);
        let picker = start_review_picker(&workspace, cx);
        assert_eq!(
            entry_keys(&picker, cx),
            [
                None,
                Some(EntryKey::Uncommitted),
                Some(EntryKey::Branch),
                None
            ]
            .into_iter()
            .chain(pull_request_keys(&[4, 1, 3]))
            .chain([None, None, Some(EntryKey::Commit(0))])
            .collect::<Vec<_>>()
        );
        picker.read_with(cx, |picker, _| {
            let entries = picker.delegate.entries();
            assert_eq!(entries[3], Entry::Header(Section::PullRequests));
            assert_eq!(entries[8], Entry::Header(Section::Commits));
            assert_eq!(
                picker.delegate.pull_request_section_title(),
                "Pull Requests · drew-council/code-review-sandbox"
            );
        });
        assert_eq!(
            statuses(&picker, cx),
            ["1 pull request you approved is hidden"]
        );
        assert_eq!(selected_key(&picker, cx), Some(EntryKey::Uncommitted));
        assert_eq!(searches(&requests, cx), ["is:open (review-requested:@me)"]);

        // Editing the search: the query holds it, only the pull requests show (nothing
        // selectable), and its results follow the typing once it pauses.
        let query =
            |cx: &mut VisualTestContext| picker.read_with(cx, |picker, cx| picker.query(cx));
        picker.update_in(cx, |picker, window, cx| picker.set_query("bra", window, cx));
        cx.run_until_parked();
        dispatch(EditPullRequestSearch, cx);
        assert_eq!(query(cx), "review-requested:@me");
        let editing_keys = entry_keys(&picker, cx);
        assert_eq!(
            editing_keys,
            [None]
                .into_iter()
                .chain(pull_request_keys(&[4, 1, 3]))
                .chain([None])
                .collect::<Vec<_>>()
        );
        picker.update_in(cx, |picker, window, cx| {
            picker.set_query("is:closed", window, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            statuses(&picker, cx),
            ["1 pull request you approved is hidden"]
        );
        cx.executor().advance_clock(SEARCH_DEBOUNCE);
        cx.run_until_parked();
        assert_eq!(statuses(&picker, cx), ["No pull requests match the search"]);

        // Escape puts the search, its results and the query back, without closing the picker.
        dispatch(menu::Cancel, cx);
        workspace.read_with(cx, |workspace, cx| {
            assert!(workspace.active_modal::<StartReviewModal>(cx).is_some());
        });
        assert_eq!(query(cx), "bra");
        picker.update_in(cx, |picker, window, cx| picker.set_query("", window, cx));
        cx.run_until_parked();
        assert_eq!(
            entry_keys(&picker, cx)[4..7],
            pull_request_keys(&[4, 1, 3])[..]
        );
        assert_eq!(
            searches(&requests, cx),
            ["is:open (review-requested:@me)", "(is:closed)"]
        );

        // Enter keeps the edited search and opens nothing.
        dispatch(EditPullRequestSearch, cx);
        picker.update_in(cx, |picker, window, cx| {
            picker.set_query("is:closed", window, cx)
        });
        cx.executor().advance_clock(SEARCH_DEBOUNCE);
        cx.run_until_parked();
        dispatch(menu::Confirm, cx);
        assert_eq!(query(cx), "");
        assert_eq!(review_count(&workspace, cx), 0);
        assert_eq!(statuses(&picker, cx), ["No pull requests match the search"]);

        // Applied before the typing pauses, the search runs at once, and its first pull request
        // is selected.
        dispatch(EditPullRequestSearch, cx);
        assert_eq!(query(cx), "is:closed");
        picker.update_in(cx, |picker, window, cx| {
            picker.set_query("is:open", window, cx)
        });
        dispatch(menu::Confirm, cx);
        assert_eq!(
            searches(&requests, cx)[2..],
            ["(is:closed)".to_string(), "(is:open)".to_string()]
        );
        assert_eq!(selected_key(&picker, cx), Some(EntryKey::PullRequest(4)));

        // A number selects the pull request; one that isn't listed gets its own row.
        picker.update_in(cx, |picker, window, cx| picker.set_query("#1", window, cx));
        cx.run_until_parked();
        assert_eq!(selected_key(&picker, cx), Some(EntryKey::PullRequest(1)));
        // It comes first, above pull requests listed before it that match the query too (as if
        // #4's title had a "1").
        picker.update(cx, |picker, _| {
            picker.delegate.set_matches(
                "1".into(),
                vec![
                    (FIRST_PULL_REQUEST_CANDIDATE, Vec::new()),
                    (FIRST_PULL_REQUEST_CANDIDATE + 1, vec![1]),
                ],
            )
        });
        let keys = entry_keys(&picker, cx);
        assert_eq!(keys[keys.len() - 2..], pull_request_keys(&[1, 4]));
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

    /// Stacked pull requests the search finds are one row per stack, in a section of their own,
    /// which reviews the stack's first pull request to review.
    #[gpui::test]
    async fn test_picker_takes_most_of_the_window(cx: &mut TestAppContext) {
        let (_fs, _workspace, _requests, cx) =
            pull_request_fixture(Default::default(), Default::default(), false, cx).await;
        cx.simulate_resize(gpui::size(gpui::px(1600.), gpui::px(1000.)));
        cx.run_until_parked();
        dispatch(StartReview, cx);
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
        let bounds = cx
            .debug_bounds("start-review-modal")
            .expect("the picker is drawn");
        assert!(
            bounds.size.width >= gpui::px(1600. * 0.69),
            "{:?}",
            bounds.size
        );
    }

    #[gpui::test]
    async fn test_stacks_section(cx: &mut TestAppContext) {
        let (_fs, workspace, requests, cx) =
            pull_request_fixture(Default::default(), Default::default(), true, cx).await;
        dispatch(StartReview, cx);
        let picker = start_review_picker(&workspace, cx);
        assert_eq!(
            entry_keys(&picker, cx),
            [
                None,
                Some(EntryKey::Uncommitted),
                Some(EntryKey::Branch),
                None,
                Some(EntryKey::Stack(7)),
                None,
                Some(EntryKey::PullRequest(4)),
                None,
                None,
                Some(EntryKey::Commit(0)),
            ]
        );
        picker.read_with(cx, |picker, _| {
            let entries = picker.delegate.entries();
            assert_eq!(entries[3], Entry::Header(Section::Stacks));
            assert_eq!(entries[5], Entry::Header(Section::PullRequests));
            // #5 is merged and the viewer approved #3: the review starts at #1.
            let listed = &picker.delegate.loaded_stacks()[0];
            assert_eq!(listed.start().map(|start| start.number), Some(1));
            assert_eq!(
                listed
                    .stack
                    .pull_requests
                    .iter()
                    .map(|pull_request| pull_request.number)
                    .collect::<Vec<_>>(),
                [5, 3, 1, 6]
            );
            // The review decisions came after the rows, for every pull request listed.
            let decisions = picker
                .delegate
                .loaded_pull_requests()
                .iter()
                .chain(&listed.stack.pull_requests)
                .map(|pull_request| (pull_request.number, pull_request.review_decision))
                .collect::<Vec<_>>();
            assert_eq!(
                decisions,
                [
                    (4, Some(ReviewDecision::ChangesRequested)),
                    (5, None),
                    (3, None),
                    (1, Some(ReviewDecision::Approved)),
                    (6, None),
                ]
            );
        });
        let operations = requests
            .lock()
            .unwrap()
            .iter()
            .filter_map(|request| request.operation().map(str::to_owned))
            .filter(|operation| operation != "RepositoryContext")
            .collect::<Vec<_>>();
        assert_eq!(
            operations,
            [
                "SearchPullRequests",
                "PullRequestStackIds",
                "PullRequestStacks",
                "PullRequestReviewDecisions",
            ]
        );
        assert_eq!(
            statuses(&picker, cx),
            ["1 pull request you approved is hidden"]
        );

        // Any of its pull requests' titles finds the stack.
        picker.update_in(cx, |picker, window, cx| {
            picker.set_query("layer 6", window, cx)
        });
        cx.run_until_parked();
        assert_eq!(selected_key(&picker, cx), Some(EntryKey::Stack(7)));
        picker.update_in(cx, |picker, window, cx| picker.set_query("", window, cx));
        cx.run_until_parked();
        dispatch(menu::Cancel, cx);

        // Opened at the pull requests, the picker selects the first stack.
        dispatch(OpenPullRequest::default(), cx);
        let picker = start_review_picker(&workspace, cx);
        assert_eq!(selected_key(&picker, cx), Some(EntryKey::Stack(7)));
        dispatch(menu::Confirm, cx);
        let review = active_review(&workspace, cx);
        review.read_with(cx, |review, _| {
            let ReviewTarget::PullRequest(target) = review.target() else {
                panic!("expected a pull request review");
            };
            assert_eq!(target.number, 1);
        });
    }

    #[test]
    fn test_group_search_results() {
        use crate::forge::{
            PullRequestStatus::{Merged, Open},
            tests::{layer, stack_of},
        };
        let approved = Some(ReviewDecision::Approved);
        let stacks = vec![
            stack_of(
                30,
                vec![layer(10, Merged, false, None), layer(11, Open, false, None)],
            ),
            stack_of(40, vec![layer(41, Open, false, approved)]),
        ];
        let found = vec![
            layer(20, Open, false, None),
            layer(41, Open, false, approved),
            layer(11, Open, false, None),
            layer(21, Open, false, approved),
        ];
        let numbers = |results: &SearchResults| {
            (
                results
                    .stacks
                    .iter()
                    .map(|listed| (listed.stack.number, listed.start))
                    .collect::<Vec<_>>(),
                results
                    .pull_requests
                    .iter()
                    .map(|pull_request| pull_request.number)
                    .collect::<Vec<_>>(),
            )
        };

        // Stack 40 has nothing found but what the viewer approved.
        let (results, approved_hidden) = group_search_results(found.clone(), stacks.clone(), true);
        assert_eq!(numbers(&results), (vec![(30, 1)], vec![20]));
        assert_eq!(approved_hidden, 2);

        let (results, approved_hidden) = group_search_results(found.clone(), stacks, false);
        assert_eq!(numbers(&results), (vec![(40, 0), (30, 1)], vec![20, 21]));
        assert_eq!(approved_hidden, 0);

        // Without stacks, every pull request is listed alone.
        let (results, _) = group_search_results(found, Vec::new(), false);
        assert_eq!(numbers(&results), (vec![], vec![20, 41, 11, 21]));
    }

    #[gpui::test]
    async fn test_pull_request_section_errors(cx: &mut TestAppContext) {
        use std::sync::atomic::{AtomicBool, Ordering};

        let fail_lists = Arc::new(AtomicBool::new(true));
        let (_fs, workspace, _requests, cx) =
            pull_request_fixture(fail_lists.clone(), Default::default(), false, cx).await;
        dispatch(StartReview, cx);
        let picker = start_review_picker(&workspace, cx);
        assert_eq!(
            statuses(&picker, cx),
            [
                "GitHub rejected the GitHub CLI's token for github.com. Run \
              \"gh auth login --hostname github.com\"."
            ]
        );
        // Applying the search again tries again (resolving the forge again after the 401).
        fail_lists.store(false, Ordering::SeqCst);
        dispatch(EditPullRequestSearch, cx);
        dispatch(EditPullRequestSearch, cx);
        assert_eq!(
            entry_keys(&picker, cx)[4..7],
            pull_request_keys(&[4, 1, 3])[..]
        );
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
            assert!(
                entries.contains(&Entry::Status(
                    "GitHub isn't configured for github.attacker.dev. If it is a GitHub \
                     Enterprise Server, add it to \"git_hosting_providers\" in your settings \
                     (provider \"github\") or run \"gh auth login --hostname \
                     github.attacker.dev\"."
                        .into()
                ))
            );
        });
        // Not even when asked again, or for a pull request by number.
        dispatch(EditPullRequestSearch, cx);
        dispatch(menu::Confirm, cx);
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
