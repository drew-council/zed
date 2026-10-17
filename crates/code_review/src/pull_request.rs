//! Opening a pull request for review: its details from the forge, checking it out (in place or
//! in a new worktree), and the state the review tabs of one pull request share.

use crate::{
    checkout::{self, plan_checkout, run_checkout},
    forge::{self, Forge, PullRequestDetails},
    github,
    session::ReviewKey,
    target::{PullRequestReference, PullRequestTarget, ReviewTarget},
    view::ReviewView,
};
use anyhow::{Result, bail};
use collections::HashMap;
use gpui::{
    App, AppContext as _, AsyncWindowContext, Context, Entity, Global, PromptLevel, Task,
    WeakEntity, Window,
};
use project::git_store::Repository;
use std::sync::Arc;
use util::ResultExt as _;
use workspace::Workspace;

/// What the review tabs of one pull request share: the forge it is on and its details.
pub struct PullRequestState {
    forge: Arc<dyn Forge>,
    details: PullRequestDetails,
}

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
    /// review it.
    fn set(
        key: ReviewKey,
        forge: Arc<dyn Forge>,
        details: PullRequestDetails,
        cx: &mut App,
    ) -> Entity<Self> {
        if let Some(pull_request) = Self::for_key(&key, cx) {
            pull_request.update(cx, |pull_request, cx| {
                pull_request.forge = forge;
                pull_request.details = details;
                cx.notify();
            });
            return pull_request;
        }
        let pull_request = cx.new(|_| Self { forge, details });
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
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpenPullRequestOptions {
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
        check_out_and_open(
            workspace_handle,
            repository,
            pull_request,
            options.confirm_checkout,
            cx,
        )
        .await
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
    cx.spawn(async move |workspace, cx| {
        if let Err(error) = task.await {
            workspace
                .update(cx, |workspace, cx| {
                    forge::show_error_toast(workspace, &error, cx)
                })
                .log_err();
        }
    })
    .detach();
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
        let _pull_request = PullRequestState::set(key, forge, details, cx);
        ReviewView::deploy_in_repo(workspace, target, repository, window, cx)
    })
}
