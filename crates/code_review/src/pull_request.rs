//! Opening a pull request for review: its details from the forge, checking it out (in place or
//! in a new worktree), and the state the review tabs of one pull request share.

use crate::{
    checkout::{self, CheckoutStep, canonical_source, plan_checkout, run_checkout},
    forge::{self, Forge, PullRequestDetails, RepositoryContext},
    github::{self, GitHubRemote},
    session::ReviewKey,
    target::{PullRequestReference, PullRequestTarget, ReviewTarget, validate_branch_name},
    view::ReviewView,
};
use anyhow::{Context as _, Result, bail};
use collections::HashMap;
use gpui::{
    App, AppContext as _, AsyncWindowContext, Context, Entity, Global, PromptLevel, Task,
    WeakEntity, Window,
};
use project::{git_store::Repository, project_settings::ProjectSettings};
use settings::Settings as _;
use std::{path::PathBuf, sync::Arc, time::Duration};
use util::{ResultExt as _, path_list::PathList};
use workspace::{OpenMode, Workspace};
use zed_actions::{CreateWorktree, NewWorktreeBranchTarget};

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
    let head_remote = details
        .head_repository
        .as_ref()
        .and_then(|head_repository| {
            remotes
                .iter()
                .find(|remote| remote.repo.is_same_repo(head_repository))
        });
    Ok(match head_remote {
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
            .await?;
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
}
