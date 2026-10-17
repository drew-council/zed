//! Opening a pull request for review: its details from the forge, checking it out (in place or
//! in a new worktree), and the state the review tabs of one pull request share: its review
//! threads, review summaries and the lines GitHub accepts comments on.

use crate::{
    checkout::{self, CheckoutStep, canonical_source, head_remote, plan_checkout, run_checkout},
    comments::{CommentTarget, Side},
    forge::{
        self, CommentableLines, DiffSide, Forge, ForgeRepoRef, GitHubError, PullRequestDetails,
        RemoteReviewSummary, RemoteThread, RepositoryContext, ThreadAnchor,
    },
    github::{self, GitHubRemote},
    session::ReviewKey,
    target::{PullRequestReference, PullRequestTarget, ReviewTarget, validate_branch_name},
    view::ReviewView,
};
use anyhow::{Context as _, Result, bail};
use collections::HashMap;
use git::{Oid, repository::RepoPath};
use gpui::{
    App, AppContext as _, AsyncWindowContext, BackgroundExecutor, Context, Entity, EventEmitter,
    Global, PromptLevel, SharedString, Task, WeakEntity, Window,
};
use project::{Project, git_store::Repository, project_settings::ProjectSettings};
use settings::Settings as _;
use std::{path::PathBuf, sync::Arc, time::Duration};
use util::{ResultExt as _, path_list::PathList};
use workspace::{OpenMode, Workspace};
use zed_actions::{CreateWorktree, NewWorktreeBranchTarget};

/// What the review tabs of one pull request share: the forge it is on, its details, and what it
/// has on GitHub besides the code (threads, summaries, GitHub's own diff).
pub struct PullRequestState {
    forge: Arc<dyn Forge>,
    details: PullRequestDetails,
    /// The checkout the forge was resolved from, to resolve it again after GitHub rejected its
    /// token.
    repository: WeakEntity<Repository>,
    project: WeakEntity<Project>,
    threads: Vec<Arc<RemoteThread>>,
    summaries: Vec<RemoteReviewSummary>,
    /// GitHub's diff hunks: comments are only accepted inside them.
    commentable: Option<CommentableLines>,
    filters: ThreadFilters,
    is_loading: bool,
    load_errors: Vec<SharedString>,
    /// GitHub rejected the token: the next load resolves the forge, and so the token, again.
    forge_is_stale: bool,
    _load_task: Task<()>,
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
                pull_request.repository = repository.downgrade();
                pull_request.project = project.downgrade();
                pull_request.forge_is_stale = false;
                pull_request.load(false, cx);
            });
            return pull_request;
        }
        let pull_request = cx.new(|cx| {
            let mut pull_request = Self {
                forge,
                details,
                repository: repository.downgrade(),
                project: project.downgrade(),
                threads: Vec::new(),
                summaries: Vec::new(),
                commentable: None,
                filters: ThreadFilters::default(),
                is_loading: false,
                load_errors: Vec::new(),
                forge_is_stale: false,
                _load_task: Task::ready(()),
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

    pub fn forge(&self) -> &Arc<dyn Forge> {
        &self.forge
    }

    pub fn threads(&self) -> &[Arc<RemoteThread>] {
        &self.threads
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

    /// Loads the details, threads, summaries and GitHub's diff again (on request, after a
    /// submit, and when HEAD moves).
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.load(true, cx);
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

    fn resolve_forge(&self, cx: &mut App) -> Option<Task<Result<Arc<dyn Forge>>>> {
        let repository = self.repository.upgrade()?;
        let project = self.project.upgrade()?;
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
            Ok(threads) => self.threads = threads.into_iter().map(Arc::new).collect(),
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
        self.forge = forge;
        self.forge_is_stale = auth_failed;
        self.load_errors = errors;
        self.is_loading = false;
        cx.emit(PullRequestEvent::Loaded);
        cx.notify();
    }
}

struct Loaded {
    details: Option<Result<PullRequestDetails>>,
    threads: Result<Vec<RemoteThread>>,
    summaries: Result<Vec<RemoteReviewSummary>>,
    files: Result<CommentableLines>,
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
    let details = match details {
        Some(details) => Some(details.await),
        None => None,
    };
    Loaded {
        details,
        threads: threads.await,
        summaries: summaries.await,
        files: files.await,
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
        // A range from an old line to a new one sits below its new line, like its last row.
        ThreadAnchor::Range {
            start_side,
            start,
            side,
            line,
        } => ThreadPlacement::Inline(CommentTarget::Lines {
            path,
            side: comment_side(*side),
            lines: if start_side == side && start <= line {
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
    let project = workspace.project().clone();
    let resolve = forge::resolve_forge(&repository, reference.repo.clone(), &project, cx);
    let workspace_handle = cx.weak_entity();
    cx.spawn_in(window, async move |_, cx| {
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
        Err(error) if error.downcast_ref::<ShownByWorktreeService>().is_some() => {
            log::error!("opening a pull request: {error:#}");
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

/// Zed's worktree service shows its errors itself.
#[derive(Debug)]
struct ShownByWorktreeService;

impl std::fmt::Display for ShownByWorktreeService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Couldn't create the worktree")
    }
}

struct LoadedPullRequest {
    forge: Arc<dyn Forge>,
    details: PullRequestDetails,
}

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
    let current_branch = repository.read_with(cx, |repository, _| {
        repository
            .branch
            .as_ref()
            .map(|branch| branch.name().to_string())
    });
    let head = checkout::head_sha(&repository, cx).await?;
    let steps = plan_checkout(
        &details,
        forge.context(),
        &remotes,
        current_branch.as_deref(),
        head,
    )?;
    if confirm_checkout && !steps.is_empty() {
        let work_dir = repository.read_with(cx, |repository, _| {
            repository.work_directory_abs_path.clone()
        });
        let answer = cx.update(|window, cx| {
            window.prompt(
                PromptLevel::Warning,
                &format!("Check out pull request #{}?", details.number),
                Some(&format!(
                    "Reviewing {}#{} checks out {} in {}.",
                    forge.context().canonical,
                    details.number,
                    details.head_ref,
                    work_dir.display()
                )),
                &["Check Out", "Cancel"],
                cx,
            )
        })?;
        if answer.await? != 0 {
            bail!("Review cancelled");
        }
    }
    run_checkout(&repository, steps, details.head_oid, &workspace, cx).await?;

    let target = ReviewTarget::PullRequest(target);
    workspace.update_in(cx, |workspace, window, cx| {
        let key = target.review_key(&repository.read(cx));
        // Held until the tab holds it.
        let project = workspace.project().clone();
        let _pull_request = PullRequestState::set(key, forge, details, &repository, &project, cx);
        ReviewView::deploy_in_repo(workspace, target, repository, window, cx)
    })
}

/// How to make a worktree at the pull request's head: the fetches to run first, then what Zed's
/// worktree creation starts the (detached) worktree from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorktreePlan {
    pub(crate) steps: Vec<CheckoutStep>,
    pub(crate) branch_target: NewWorktreeBranchTarget,
}

/// A pull request whose head branch is on a remote starts from that remote branch (Zed's worktree
/// creation fetches the remote); any other one from its head commit, fetched from `refs/pull`
/// without creating a branch (a branch can't be fetched into while another worktree has it).
pub(crate) fn plan_worktree(
    details: &PullRequestDetails,
    context: &RepositoryContext,
    remotes: &[GitHubRemote],
) -> Result<WorktreePlan> {
    validate_branch_name(&details.head_ref).context("The pull request's head branch")?;
    validate_branch_name(&details.base_ref).context("The pull request's base branch")?;
    let base_refspec = format!("refs/heads/{}", details.base_ref);
    Ok(match head_remote(details, remotes) {
        Some(head_remote) => WorktreePlan {
            steps: if head_remote.repo.is_same_repo(&context.canonical) {
                Vec::new()
            } else {
                vec![CheckoutStep::FetchRefspec {
                    source: canonical_source(context, remotes),
                    refspecs: vec![base_refspec],
                }]
            },
            branch_target: NewWorktreeBranchTarget::RemoteBranch {
                remote_name: head_remote.name.clone(),
                branch_name: details.head_ref.to_string(),
            },
        },
        None => WorktreePlan {
            steps: vec![CheckoutStep::FetchRefspec {
                source: canonical_source(context, remotes),
                refspecs: vec![format!("refs/pull/{}/head", details.number), base_refspec],
            }],
            // `git worktree add --detach` takes any revision.
            branch_target: NewWorktreeBranchTarget::ExistingBranch {
                name: details.head_oid.to_string(),
            },
        },
    })
}

/// Reviews the pull request in a linked worktree `pr-N` of the repository, opened as another
/// workspace of the window; the original checkout stays as it is. A worktree made for it before
/// is reused.
async fn open_in_new_worktree(
    workspace: WeakEntity<Workspace>,
    repository: Entity<Repository>,
    pull_request: LoadedPullRequest,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<ReviewView>> {
    let worktree_name = format!("pr-{}", pull_request.details.number);
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

    let new_workspace = if worktrees.iter().any(|worktree| worktree.path == path) {
        open_existing_worktree(&workspace, path, cx).await?
    } else {
        let remotes = cx.update(|_, cx| github::remotes(&repository, cx))?.await?;
        let plan = plan_worktree(
            &pull_request.details,
            pull_request.forge.context(),
            &remotes,
        )?;
        run_checkout(
            &repository,
            plan.steps,
            pull_request.details.head_oid,
            &workspace,
            cx,
        )
        .await?;
        let action = CreateWorktree {
            worktree_name: Some(worktree_name),
            branch_target: plan.branch_target,
        };
        let created = workspace
            .update_in(cx, |workspace, window, cx| {
                git_ui_core::worktree_service::create_worktree_workspace(
                    workspace, &action, window, None, cx,
                )
            })?
            .await
            .context(ShownByWorktreeService)?;
        // It opens in the background.
        let multi_workspace =
            workspace.read_with(cx, |workspace, _| workspace.multi_workspace().cloned())?;
        if let Some(multi_workspace) = multi_workspace {
            let new_workspace = created.workspace.clone();
            multi_workspace.update_in(cx, |multi_workspace, window, cx| {
                multi_workspace.activate(new_workspace, Some(workspace.clone()), window, cx)
            })?;
        }
        created.workspace
    };

    let new_repository = repository_of(&new_workspace, cx).await?;
    check_out_and_open(
        new_workspace.downgrade(),
        new_repository,
        pull_request,
        false,
        cx,
    )
    .await
}

async fn open_existing_worktree(
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

/// The repository of a workspace that was just opened on a worktree, once Zed found it.
async fn repository_of(
    workspace: &Entity<Workspace>,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<Repository>> {
    let scan = workspace.read_with(cx, |workspace, cx| {
        workspace.project().read(cx).wait_for_initial_scan(cx)
    });
    scan.await;
    // Repositories are discovered from the scan's results shortly after it ends.
    for _ in 0..50 {
        let repository = workspace.read_with(cx, |workspace, cx| {
            workspace.project().read(cx).active_repository(cx)
        });
        if let Some(repository) = repository {
            return Ok(repository);
        }
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
    }
    bail!("No git repository found in the new worktree")
}

#[cfg(test)]
mod tests {
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

        assert_eq!(
            plan_worktree(&details(Some(canonical.clone())), &context, &remotes).unwrap(),
            WorktreePlan {
                steps: Vec::new(),
                branch_target: NewWorktreeBranchTarget::RemoteBranch {
                    remote_name: "origin".into(),
                    branch_name: "fix/truncate".into(),
                },
            }
        );
        let fork_remotes = [remote("origin", repo("me")), remote("upstream", canonical)];
        assert_eq!(
            plan_worktree(&details(Some(repo("me"))), &context, &fork_remotes).unwrap(),
            WorktreePlan {
                steps: vec![CheckoutStep::FetchRefspec {
                    source: "upstream".into(),
                    refspecs: vec!["refs/heads/main".into()],
                }],
                branch_target: NewWorktreeBranchTarget::RemoteBranch {
                    remote_name: "origin".into(),
                    branch_name: "fix/truncate".into(),
                },
            }
        );
        assert_eq!(
            plan_worktree(&details(Some(repo("someone"))), &context, &remotes).unwrap(),
            WorktreePlan {
                steps: vec![CheckoutStep::FetchRefspec {
                    source: "origin".into(),
                    refspecs: vec!["refs/pull/2/head".into(), "refs/heads/main".into()],
                }],
                branch_target: NewWorktreeBranchTarget::ExistingBranch {
                    name: "632ee13692b487a7ea71911f473781416f35ff01".into(),
                },
            }
        );
        let mut hostile = details(None);
        hostile.head_ref = "-x".into();
        assert!(plan_worktree(&hostile, &context, &remotes).is_err());
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
    }

    mod review {
        use super::*;
        use crate::{
            CopyBranchName, CopyPullRequestUrl, OpenPullRequest, ReloadPullRequest,
            ToggleBotReviews, ToggleOutdatedThreads, ToggleResolvedThreads,
            comments::{ItemKey, RemoteKey},
            github::{
                set_fake_token_source,
                test_support::{
                    PR1_BASE_OID, PR1_DETAILS, PR1_FILES, PR1_HEAD_OID, PR1_SUMMARIES, PR1_THREADS,
                    Reply, SentRequests, fake_github, repository_context_reply,
                },
            },
            test_support::{
                C1, active_review, commit_data, cursor_line, dispatch, dot_git, init_vim_test,
                insert_commit_ref, lhs_editor, move_to_row_with_text, open_workspace,
                path_at_cursor, rhs_editor, row_text, splittable,
            },
            top_block::TopBlockContent,
            view::PanelCommentKey,
        };
        use editor::{Bias, Editor};
        use gpui::{Focusable as _, TestAppContext, VisualContext as _, VisualTestContext};
        use language::Point;
        use project::FakeFs;
        use serde_json::json;
        use settings::DiffViewStyle;
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

        /// A checkout of PR A at its head, with files shaped so that its threads' lines exist:
        /// `src/engine.rs` changes lines 59, 100 and 134 and deletes old line 193,
        /// `src/filters.rs` is added (71 lines), `src/util/strings.rs` changes line 48 and
        /// `docs/guide.md` line 30. The fake GitHub has PR A's threads, summaries and files.
        /// `fail_threads` makes the threads answer 401.
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
            init_vim_test(cx, diff_view_style);
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

            let fs = FakeFs::new(cx.executor());
            fs.insert_tree(
                path!("/project"),
                json!({
                    ".git": {},
                    "docs": { "guide.md": text(&guide_new) },
                    "src": {
                        "engine.rs": text(&engine_new),
                        "filters.rs": filters.clone(),
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
            let (http, requests) = fake_github(move |request| match request.operation() {
                Some("RepositoryContext") => repository_context_reply(request),
                Some("PullRequestDetails") => Reply::json(PR1_DETAILS),
                Some("ReviewThreads") if fail_threads.load(Ordering::SeqCst) => {
                    Reply::status(401, r#"{"message":"Bad credentials"}"#)
                }
                Some("ReviewThreads") => Reply::json(PR1_THREADS),
                Some("ReviewSummaries") => Reply::json(PR1_SUMMARIES),
                None if request.uri.contains("/pulls/1/files") => Reply::json(PR1_FILES),
                _ => Reply::status(500, "{}"),
            });
            cx.update(|cx| {
                cx.set_http_client(http);
                set_fake_token_source(cx, |_| Ok("token".into()));
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

        fn requests_of(requests: &SentRequests, operation: &str) -> usize {
            requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request.operation() == Some(operation))
                .count()
        }

        fn notification_count(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> usize {
            workspace.update(cx, |workspace, cx| {
                let ids = workspace.notification_ids();
                for id in &ids {
                    workspace.dismiss_notification(id, cx);
                }
                ids.len()
            })
        }

        fn has_open_comment_editor(review: &Entity<ReviewView>, cx: &VisualTestContext) -> bool {
            review.read_with(cx, |review, cx| review.comments().read(cx).is_editing(cx))
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

            // Threads are read-only: `i` opens nothing, `dd` refuses, `Y` copies the first
            // comment.
            cx.simulate_keystrokes("[ t [ t");
            assert_eq!(cursor_line(&rhs, cx), "ENGINE 134");
            notification_count(&workspace, cx);
            cx.simulate_keystrokes("i");
            assert!(!has_open_comment_editor(&review, cx));
            cx.simulate_keystrokes("d d");
            assert_eq!(notification_count(&workspace, cx), 1);
            assert!(!has_open_comment_editor(&review, cx));
            cx.simulate_keystrokes("shift-y");
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
            // The line cursor moved over with the focus.
            let highlighted = crate::line_cursor::cursor_rows(&lhs, cx);
            assert_eq!(highlighted.len(), 1, "{highlighted:?}");
            assert_eq!(row_text(&lhs, highlighted[0].0, cx), "engine 193");
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
                        "GitHub authentication failed for github.com. Set GITHUB_TOKEN or run \
                      `gh auth login --hostname github.com`."
                    ]
                );
                assert!(pull_request.commentable().is_some());
                assert!(pull_request.threads().is_empty());
            });
            let resolutions = requests_of(&requests, "RepositoryContext");
            assert_eq!(requests_of(&requests, "ReviewThreads"), 2);
            assert!(remote_keys(&review, cx).is_empty());

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
    }
}
