//! Checking out a pull request the way Zed checks out branches: a pull request whose head branch
//! is on one of the checkout's remotes becomes a local branch tracking it (Zed's `change_branch`),
//! any other one is fetched from `refs/pull/N/head` into a local `pr/N` branch. Nothing is
//! stashed: with uncommitted changes git carries them over or refuses, and the error is shown.

use crate::{
    commits::local_job,
    forge::{PullRequestDetails, PullRequestStatus, RepositoryContext},
    github::GitHubRemote,
    target::validate_branch_name,
};
use anyhow::{Context as _, Result, anyhow, bail};
use askpass::{AskPassDelegate, AskPassResult, AskPassSession};
use collections::HashMap;
use futures::{FutureExt as _, select_biased};
use git::{
    Oid,
    repository::{FetchOptions, GitRepository, Remote},
};
use git_ui_core::askpass_modal::AskPassModal;
use gpui::{
    App, AsyncWindowContext, BackgroundExecutor, Context, Entity, SharedString, Task, WeakEntity,
    Window,
};
use project::git_store::{LocalRepositoryState, Repository, RepositoryState};
use std::{fmt, future::Future, path::PathBuf, process::Output, sync::Arc};
use util::command::{Stdio, new_command};
use workspace::Workspace;

/// One step of checking out a pull request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CheckoutStep {
    /// `git fetch <remote>`, run by Zed.
    FetchRemote { remote: String },
    /// Zed's branch switch: a local branch, or a remote-tracking branch (`remote/branch`), which
    /// gets a local branch tracking it first.
    ChangeBranch { name: String },
    /// `git fetch <source> <refspecs>`, without forcing.
    FetchRefspec {
        /// A remote's name or a repository URL.
        source: String,
        refspecs: Vec<String>,
    },
    /// Stops before switching to an existing local branch that has diverged from the pull
    /// request's head `to`. A branch the head is in (the user's commits on top) is fine.
    EnsureFastForward { branch: String, to: String },
    /// `git merge --ff-only <to>` while `branch` is checked out, which is checked in the same git
    /// job. Nothing when HEAD already is `to`: Zed's branch switch leaves an existing branch
    /// where it was.
    FastForward { branch: String, to: String },
    /// `git checkout --detach <to>`: a review worktree stays at the pull request's head.
    Detach { to: String },
}

/// The local branch a pull request without a remote for its head branch is fetched into.
pub(crate) fn pull_request_branch(number: u64) -> String {
    format!("pr/{number}")
}

/// What the checkout has, read from git right before planning rather than from Zed's snapshot,
/// which can lag behind a branch switch made in a terminal.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LocalState {
    /// `None` when HEAD is detached.
    pub(crate) current_branch: Option<String>,
    pub(crate) head: Option<Oid>,
    /// Whether the pull request's base commit is in the repository.
    pub(crate) has_base: bool,
    /// The local branches, with the remote-tracking branch each one tracks
    /// (`refs/remotes/R/b`).
    pub(crate) branches: HashMap<String, Option<String>>,
}

pub(crate) fn read_local_state(
    repository: &Entity<Repository>,
    base: Oid,
    cx: &mut App,
) -> Task<Result<LocalState>> {
    local_job(
        repository,
        "code review: read the checkout",
        move |backend| async move { local_state(backend.as_ref(), base).await },
        cx,
    )
}

pub(crate) async fn local_state(backend: &dyn GitRepository, base: Oid) -> Result<LocalState> {
    let mut resolved = backend
        .revparse_batch(vec!["HEAD".into(), format!("{base}^{{commit}}")])
        .await?
        .into_iter();
    let mut commit = || {
        resolved
            .next()
            .flatten()
            .and_then(|sha| sha.parse::<Oid>().ok())
    };
    let head = commit();
    let has_base = commit().is_some();
    let scan = backend.branches().await?;
    if let Some(error) = scan.error {
        log::warn!("listing branches: {error}");
    }
    let mut current_branch = None;
    let mut branches = HashMap::default();
    for branch in scan.branches {
        let Some(name) = branch.ref_name.strip_prefix("refs/heads/") else {
            continue;
        };
        if branch.is_head {
            current_branch = Some(name.to_string());
        }
        branches.insert(
            name.to_string(),
            branch
                .upstream
                .map(|upstream| upstream.ref_name.to_string()),
        );
    }
    Ok(LocalState {
        current_branch,
        head,
        has_base,
        branches,
    })
}

/// What checking out the pull request takes, given the checkout's GitHub remotes (preferred
/// first) and what it has now. Nothing when HEAD already is the pull request's head, unless the
/// base commit is missing.
pub(crate) fn plan_checkout(
    details: &PullRequestDetails,
    context: &RepositoryContext,
    remotes: &[GitHubRemote],
    local: &LocalState,
) -> Result<Vec<CheckoutStep>> {
    validate_branch_name(&details.head_ref).context("The pull request's head branch")?;
    validate_branch_name(&details.base_ref).context("The pull request's base branch")?;
    let base_source = canonical_source(context, remotes);
    let base_refspec = format!("refs/heads/{}", details.base_ref);
    if local.head == Some(details.head_oid) {
        // The base branch may have moved on GitHub since this was checked out, and the review's
        // diff needs the base commit.
        return Ok(if local.has_base {
            Vec::new()
        } else {
            vec![CheckoutStep::FetchRefspec {
                source: base_source,
                refspecs: vec![base_refspec],
            }]
        });
    }
    let to = details.head_oid.to_string();
    let fast_forward = |branch: &str| CheckoutStep::FastForward {
        branch: branch.to_string(),
        to: to.clone(),
    };
    let switch_to_existing = |branch: &str| {
        if local.current_branch.as_deref() == Some(branch) {
            Vec::new()
        } else {
            vec![
                CheckoutStep::EnsureFastForward {
                    branch: branch.to_string(),
                    to: to.clone(),
                },
                CheckoutStep::ChangeBranch {
                    name: branch.to_string(),
                },
            ]
        }
    };

    if let Some(head_remote) = head_remote(details, remotes) {
        let tracking = format!("refs/remotes/{}/{}", head_remote.name, details.head_ref);
        let switch = match local.branches.get(details.head_ref.as_ref()) {
            None => Some(vec![CheckoutStep::ChangeBranch {
                name: format!("{}/{}", head_remote.name, details.head_ref),
            }]),
            // A local branch of that name is only reused while it tracks the pull request's
            // branch: Zed's branch switch would point it at the pull request's branch, and the
            // fast-forward would then move the user's own branch.
            Some(Some(upstream)) if *upstream == tracking => {
                Some(switch_to_existing(&details.head_ref))
            }
            Some(_) => None,
        };
        if let Some(switch) = switch {
            let mut steps = vec![CheckoutStep::FetchRemote {
                remote: head_remote.name.clone(),
            }];
            steps.extend(switch);
            // A pull request from a fork into the canonical repository: the base commit isn't
            // on the fork's remote.
            if !head_remote.repo.is_same_repo(&context.canonical) {
                steps.push(CheckoutStep::FetchRefspec {
                    source: base_source,
                    refspecs: vec![base_refspec],
                });
            }
            steps.push(fast_forward(&details.head_ref));
            return Ok(steps);
        }
    }

    let pull_ref = format!("refs/pull/{}/head", details.number);
    let branch = pull_request_branch(details.number);
    let mut steps = if local.branches.contains_key(&branch) {
        // git refuses to fetch into the checked-out branch, and an earlier review's `pr/N` may
        // have commits of its own: it moves by a fast-forward to what the fetch brought.
        let mut steps = vec![CheckoutStep::FetchRefspec {
            source: base_source,
            refspecs: vec![pull_ref, base_refspec],
        }];
        steps.extend(switch_to_existing(&branch));
        steps
    } else {
        vec![
            CheckoutStep::FetchRefspec {
                source: base_source,
                refspecs: vec![format!("{pull_ref}:refs/heads/{branch}"), base_refspec],
            },
            CheckoutStep::ChangeBranch {
                name: branch.clone(),
            },
        ]
    };
    steps.push(fast_forward(&branch));
    Ok(steps)
}

/// The branch the steps leave checked out, if they check one out.
pub(crate) fn checked_out_branch(steps: &[CheckoutStep]) -> Option<&str> {
    steps.iter().find_map(|step| match step {
        CheckoutStep::FastForward { branch, .. } => Some(branch.as_str()),
        _ => None,
    })
}

/// The remote a pull request's head branch is checked out from: one pointing at its head
/// repository (the canonical one, or the user's fork). Not for a pull request that is no longer
/// open, whose branch is often deleted (GitHub keeps `refs/pull/N/head`), nor for a fork's pull
/// request from a branch named like its base (a fork's `main` into `main`), which would make the
/// local base branch track the fork.
pub(crate) fn head_remote<'a>(
    details: &PullRequestDetails,
    remotes: &'a [GitHubRemote],
) -> Option<&'a GitHubRemote> {
    if details.state != PullRequestStatus::Open
        || (details.is_cross_repository && details.head_ref == details.base_ref)
    {
        return None;
    }
    let head_repository = details.head_repository.as_ref()?;
    remotes
        .iter()
        .find(|remote| remote.repo.is_same_repo(head_repository))
}

/// Where the canonical repository's refs are fetched from: a remote pointing at it, else its URL
/// (SSH when the checkout's remote is an SSH one).
pub(crate) fn canonical_source(context: &RepositoryContext, remotes: &[GitHubRemote]) -> String {
    remotes
        .iter()
        .find(|remote| remote.repo.is_same_repo(&context.canonical))
        .map(|remote| remote.name.clone())
        .unwrap_or_else(|| {
            if context.origin_is_ssh {
                context.canonical_ssh_url.clone()
            } else {
                context.canonical_https_url.clone()
            }
        })
}

/// Zed doesn't run git to check out a pull request in a repository it doesn't trust: the
/// repository's configuration could name commands for git to run (`core.gitProxy`, filters).
#[derive(Debug)]
pub(crate) struct RestrictedMode;

impl fmt::Display for RestrictedMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(
            "This project is in Restricted Mode, so Zed doesn't run git to check out pull \
             requests in it: a repository's configuration can make git run commands. Trust the \
             project, then open the pull request again.",
        )
    }
}

impl std::error::Error for RestrictedMode {}

/// Refuses with [`RestrictedMode`] when Zed doesn't trust the repository.
pub(crate) fn ensure_trusted(repository: &Entity<Repository>, cx: &App) -> Result<()> {
    if repository.read(cx).is_trusted() {
        Ok(())
    } else {
        Err(RestrictedMode.into())
    }
}

/// Runs the steps of [`plan_checkout`] in `repository`. Fetches ask for credentials through
/// `workspace`'s modal.
pub(crate) async fn run_checkout(
    repository: &Entity<Repository>,
    steps: Vec<CheckoutStep>,
    workspace: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    for step in steps {
        match step {
            CheckoutStep::FetchRemote { remote } => {
                let askpass = askpass_delegate_in(workspace, format!("git fetch {remote}"), cx)?;
                let fetch = repository.update(cx, |repository, cx| {
                    repository.fetch(
                        FetchOptions::Remote(Remote {
                            name: remote.clone().into(),
                        }),
                        askpass,
                        cx,
                    )
                });
                fetch
                    .await
                    .context("the git job was canceled")?
                    .with_context(|| format!("Couldn't fetch {remote}"))?;
            }
            CheckoutStep::ChangeBranch { name } => {
                let change =
                    repository.update(cx, |repository, _| repository.change_branch(name.clone()));
                change
                    .await
                    .context("the git job was canceled")?
                    .with_context(|| format!("Couldn't check out {name}"))?;
            }
            CheckoutStep::FetchRefspec { source, refspecs } => {
                let askpass = askpass_delegate_in(workspace, format!("git fetch {source}"), cx)?;
                let mut args = vec!["fetch".to_string(), "--end-of-options".to_string(), source];
                args.extend(refspecs.iter().cloned());
                run_git_job(
                    repository,
                    "git fetch",
                    cx,
                    move |context, executor| async move {
                        run_git(&context, &args, Some((askpass, executor))).await
                    },
                )
                .await
                .map_err(|error| fetch_error(error, &refspecs))?;
            }
            CheckoutStep::EnsureFastForward { branch, to } => {
                run_git_job(repository, "git merge-base", cx, move |context, _| async move {
                    ensure_fast_forward(&context, &branch, &to).await
                })
                .await?;
            }
            CheckoutStep::FastForward { branch, to } => {
                run_git_job(
                    repository,
                    "git merge --ff-only",
                    cx,
                    move |context, _| async move { fast_forward(&context, &branch, &to).await },
                )
                .await?;
            }
            CheckoutStep::Detach { to } => {
                let args = ["checkout", "--detach", "--end-of-options", &to].map(str::to_string);
                run_git_job(
                    repository,
                    "git checkout --detach",
                    cx,
                    move |context, _| async move { run_git(&context, &args, None).await },
                )
                .await?;
            }
        }
    }
    Ok(())
}

/// Fast-forwards `branch` to `to` if `branch` still is the checked-out branch: one checked out
/// since the plan was made (in a terminal, or by another pull request) must not move to this
/// pull request's head.
pub(crate) async fn fast_forward(context: &GitContext, branch: &str, to: &str) -> Result<()> {
    let output = git_output(context, &["symbolic-ref", "-q", "HEAD"]).await?;
    let checked_out = output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string());
    if checked_out.as_deref() != Some(format!("refs/heads/{branch}").as_str()) {
        let now = match &checked_out {
            Some(checked_out) => format!(
                "{} is checked out now",
                checked_out
                    .strip_prefix("refs/heads/")
                    .unwrap_or(checked_out)
            ),
            None => "HEAD is detached now".to_string(),
        };
        bail!(
            "Didn't fast-forward {branch} to the pull request's head: {now}. Open the pull \
             request again."
        );
    }
    let head = run_git(
        context,
        &["rev-parse", "--verify", "HEAD"].map(str::to_string),
        None,
    )
    .await?;
    if head.trim() == to {
        return Ok(());
    }
    let args = ["merge", "--ff-only", "--end-of-options", to].map(str::to_string);
    run_git(context, &args, None)
        .await
        .map_err(|error| fast_forward_error(error, branch))?;
    Ok(())
}

/// Refuses a local branch that has diverged from `to`: neither contains the other.
pub(crate) async fn ensure_fast_forward(
    context: &GitContext,
    branch: &str,
    to: &str,
) -> Result<()> {
    let branch_ref = format!("refs/heads/{branch}");
    if is_ancestor(context, &branch_ref, to).await? || is_ancestor(context, to, &branch_ref).await?
    {
        return Ok(());
    }
    Err(anyhow!(diverged_hint(branch)))
}

async fn is_ancestor(context: &GitContext, ancestor: &str, descendant: &str) -> Result<bool> {
    let output = git_output(
        context,
        &[
            "merge-base",
            "--is-ancestor",
            "--end-of-options",
            ancestor,
            descendant,
        ],
    )
    .await?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "git merge-base failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

/// Never suggests deleting the branch: it may hold the user's commits.
fn diverged_hint(branch: &str) -> String {
    format!(
        "{branch} has diverged from the pull request: the pull request was force-pushed, or \
         {branch} has commits of its own. Rename the branch (`git branch -m {branch} \
         {branch}-old`), then open the pull request again."
    )
}

fn fetch_error(error: anyhow::Error, refspecs: &[String]) -> anyhow::Error {
    let message = format!("{error:#}");
    let branch = refspecs
        .iter()
        .find_map(|refspec| refspec.split_once(":refs/heads/").map(|(_, branch)| branch));
    match branch {
        Some(branch) if message.contains("non-fast-forward") => {
            anyhow!("{}\n{}", message.trim(), diverged_hint(branch))
        }
        _ => error,
    }
}

fn fast_forward_error(error: anyhow::Error, branch: &str) -> anyhow::Error {
    let message = format!("{error:#}");
    if message.contains("Not possible to fast-forward") {
        anyhow!("{}\n{}", message.trim(), diverged_hint(branch))
    } else {
        error
    }
}

/// How git runs in a repository: Zed's environment for it, and whether it's trusted (an
/// untrusted repository's configuration can't run commands).
#[derive(Clone, Debug)]
pub(crate) struct GitContext {
    pub(crate) work_dir: PathBuf,
    pub(crate) environment: Arc<HashMap<String, String>>,
    pub(crate) is_trusted: bool,
}

/// Runs `job` on the repository's git job queue, like Zed's own git commands: nothing else of
/// Zed's runs git in the repository meanwhile.
async fn run_git_job<R, Fut>(
    repository: &Entity<Repository>,
    status: &'static str,
    cx: &mut AsyncWindowContext,
    job: impl FnOnce(GitContext, BackgroundExecutor) -> Fut + 'static,
) -> Result<R>
where
    R: Send + 'static,
    Fut: Future<Output = Result<R>> + 'static,
{
    let work_dir = repository.read_with(cx, |repository, _| {
        repository.work_directory_abs_path.to_path_buf()
    });
    let receiver = repository.update(cx, |repository, _| {
        repository.send_job(
            "code review: checkout",
            Some(SharedString::from(status)),
            move |state, cx| async move {
                let RepositoryState::Local(LocalRepositoryState {
                    backend,
                    environment,
                    ..
                }) = state
                else {
                    bail!("Code review only supports local repositories");
                };
                let context = GitContext {
                    work_dir,
                    environment,
                    is_trusted: backend.is_trusted(),
                };
                job(context, cx.background_executor().clone()).await
            },
        )
    });
    receiver.await.context("the git job was canceled")?
}

/// git with Zed's safety settings (`GitBinary::build_command`).
fn git_command(
    context: &GitContext,
    args: &[impl AsRef<std::ffi::OsStr>],
) -> util::command::Command {
    let mut command = new_command("git");
    command
        .current_dir(&context.work_dir)
        .envs(context.environment.iter())
        .env("GIT_TERMINAL_PROMPT", "0")
        // Disabled to stop malicious actors from running arbitrary commands via fsmonitor hooks.
        .args(["-c", "core.fsmonitor=false"])
        .arg("--no-pager");
    if !context.is_trusted {
        command.args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.sshCommand=ssh",
            "-c",
            "credential.helper=",
            "-c",
            "protocol.ext.allow=never",
        ]);
    }
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

/// Runs git for its exit status and output, without credentials.
async fn git_output(context: &GitContext, args: &[&str]) -> Result<Output> {
    let subcommand = args.first().copied().unwrap_or("git");
    git_command(context, args)
        .output()
        .await
        .with_context(|| format!("running git {subcommand}"))
}

/// Runs git and, for commands that may need credentials, Zed's askpass (`run_git_command`).
/// Returns stdout.
pub(crate) async fn run_git(
    context: &GitContext,
    args: &[String],
    askpass: Option<(AskPassDelegate, BackgroundExecutor)>,
) -> Result<String> {
    let mut command = git_command(context, args);
    let subcommand = args.first().map(String::as_str).unwrap_or("git");
    let session = match askpass {
        Some((askpass, executor)) if !context.environment.contains_key("GIT_ASKPASS") => {
            let session = AskPassSession::new(executor, askpass).await?;
            command
                .env("GIT_ASKPASS", session.script_path())
                .env("SSH_ASKPASS", session.script_path())
                .env("SSH_ASKPASS_REQUIRE", "force");
            Some(session)
        }
        _ => None,
    };
    let child = command
        .spawn()
        .with_context(|| format!("running git {subcommand}"))?;
    let output = match session {
        Some(mut session) => {
            select_biased! {
                result = session.run(None).fuse() => match result {
                    AskPassResult::CancelledByUser => bail!("git {subcommand} was canceled"),
                    AskPassResult::Timedout => bail!("git {subcommand} timed out"),
                },
                output = child.output().fuse() => output?,
            }
        }
        None => child.output().await?,
    };
    if !output.status.success() {
        bail!(
            "git {subcommand} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A delegate that asks for credentials in a modal of `workspace` (a copy of the worktree
/// service's private one).
pub(crate) fn askpass_delegate(
    workspace: WeakEntity<Workspace>,
    operation: impl Into<SharedString>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> AskPassDelegate {
    let operation = operation.into();
    let window = window.window_handle();
    AskPassDelegate::new_with_cancellation(
        &mut cx.to_async(),
        move |prompt, tx, cancellation, cx| {
            window
                .update(cx, |_, window, cx| {
                    workspace.update(cx, |workspace, cx| {
                        workspace.toggle_modal(window, cx, |window, cx| {
                            AskPassModal::new(
                                operation.clone(),
                                prompt.into(),
                                tx,
                                cancellation,
                                window,
                                cx,
                            )
                        });
                    })
                })
                .ok();
        },
    )
}

fn askpass_delegate_in(
    workspace: &WeakEntity<Workspace>,
    operation: String,
    cx: &mut AsyncWindowContext,
) -> Result<AskPassDelegate> {
    workspace.update_in(cx, |workspace, window, cx| {
        askpass_delegate(workspace.weak_handle(), operation, window, cx)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        forge::{Actor, ForgeRepo},
        github::GitHubHost,
    };
    use futures::executor::block_on;
    use std::path::Path;

    const HEAD: &str = "3f22f1b3bd52ed79a655a055209707e356fa8253";
    const BASE: &str = "deea36cf8fa38276ef75801d05773c4c35c48d45";

    fn repo(owner: &str, name: &str) -> ForgeRepo {
        ForgeRepo {
            host: "github.com".into(),
            owner: owner.to_string().into(),
            name: name.to_string().into(),
        }
    }

    fn remote(name: &str, repo: ForgeRepo) -> GitHubRemote {
        GitHubRemote {
            name: name.into(),
            repo,
            is_ssh: false,
            host_kind: GitHubHost::Cloud,
        }
    }

    fn context(origin: ForgeRepo, canonical: ForgeRepo, origin_is_ssh: bool) -> RepositoryContext {
        RepositoryContext {
            canonical_https_url: format!("https://github.com/{canonical}"),
            canonical_ssh_url: format!("git@github.com:{canonical}.git"),
            origin,
            canonical,
            viewer: "drew-council".into(),
            origin_is_ssh,
        }
    }

    fn details(head_repository: Option<ForgeRepo>) -> PullRequestDetails {
        PullRequestDetails {
            node_id: "PR_1".into(),
            number: 7,
            title: "Add filters".into(),
            url: "https://github.com/o/r/pull/7".into(),
            body: "".into(),
            author: Actor {
                login: "someone".into(),
                is_bot: false,
            },
            state: PullRequestStatus::Open,
            is_draft: false,
            base_ref: "main".into(),
            base_oid: BASE.parse().unwrap(),
            head_ref: "feature/filters".into(),
            head_oid: HEAD.parse().unwrap(),
            is_cross_repository: head_repository
                .as_ref()
                .is_none_or(|head_repository| !head_repository.is_same_repo(&repo("o", "r"))),
            head_repository,
            review_decision: None,
            viewer_is_author: false,
            pending_review: None,
        }
    }

    fn fetch_remote(remote: &str) -> CheckoutStep {
        CheckoutStep::FetchRemote {
            remote: remote.into(),
        }
    }

    fn change_branch(name: &str) -> CheckoutStep {
        CheckoutStep::ChangeBranch { name: name.into() }
    }

    fn fetch_refspec(source: &str, refspecs: &[&str]) -> CheckoutStep {
        CheckoutStep::FetchRefspec {
            source: source.into(),
            refspecs: refspecs.iter().map(|refspec| refspec.to_string()).collect(),
        }
    }

    fn fast_forward_step(branch: &str) -> CheckoutStep {
        CheckoutStep::FastForward {
            branch: branch.into(),
            to: HEAD.into(),
        }
    }

    fn ensure_fast_forward_step(branch: &str) -> CheckoutStep {
        CheckoutStep::EnsureFastForward {
            branch: branch.into(),
            to: HEAD.into(),
        }
    }

    /// A checkout on `current` at `head`, with local branches and their upstreams.
    fn local(
        current: Option<&str>,
        head: Option<&str>,
        branches: &[(&str, Option<&str>)],
    ) -> LocalState {
        let mut branches = branches
            .iter()
            .map(|(name, upstream)| (name.to_string(), upstream.map(str::to_string)))
            .collect::<HashMap<_, _>>();
        if let Some(current) = current {
            branches.entry(current.to_string()).or_insert(None);
        }
        LocalState {
            current_branch: current.map(str::to_string),
            head: head.map(|head| head.parse().unwrap()),
            has_base: true,
            branches,
        }
    }

    #[test]
    fn test_plan_checkout() {
        let canonical = repo("o", "r");
        let fork = repo("me", "r");
        let third_party = repo("someone", "r");
        let same_repo = details(Some(canonical.clone()));
        let plan = |details: &PullRequestDetails,
                    context: &RepositoryContext,
                    remotes: &[GitHubRemote],
                    local: &LocalState| {
            plan_checkout(details, context, remotes, local).unwrap()
        };
        let origin_canonical = context(canonical.clone(), canonical.clone(), false);
        let canonical_remotes = [remote("origin", canonical.clone())];
        let on_main = local(Some("main"), Some(BASE), &[]);

        // Already at the pull request's head: nothing to do, not even a fetch.
        let at_head = local(Some("feature/filters"), Some(HEAD), &[]);
        assert_eq!(
            plan(&same_repo, &origin_canonical, &canonical_remotes, &at_head),
            []
        );
        // Unless the base commit is missing: its branch moved on since.
        let without_base = LocalState {
            has_base: false,
            ..at_head
        };
        assert_eq!(
            plan(
                &same_repo,
                &origin_canonical,
                &canonical_remotes,
                &without_base
            ),
            [fetch_refspec("origin", &["refs/heads/main"])]
        );

        // The head branch is on origin: a branch tracking it, then fast-forwarded on it.
        assert_eq!(
            plan(&same_repo, &origin_canonical, &canonical_remotes, &on_main),
            [
                fetch_remote("origin"),
                change_branch("origin/feature/filters"),
                fast_forward_step("feature/filters"),
            ]
        );
        // A local branch tracking it is reused, unless it has diverged.
        let tracking = local(
            Some("main"),
            Some(BASE),
            &[(
                "feature/filters",
                Some("refs/remotes/origin/feature/filters"),
            )],
        );
        assert_eq!(
            plan(&same_repo, &origin_canonical, &canonical_remotes, &tracking),
            [
                fetch_remote("origin"),
                ensure_fast_forward_step("feature/filters"),
                change_branch("feature/filters"),
                fast_forward_step("feature/filters"),
            ]
        );
        let on_tracking = local(
            Some("feature/filters"),
            Some(BASE),
            &[(
                "feature/filters",
                Some("refs/remotes/origin/feature/filters"),
            )],
        );
        assert_eq!(
            plan(
                &same_repo,
                &origin_canonical,
                &canonical_remotes,
                &on_tracking
            ),
            [fetch_remote("origin"), fast_forward_step("feature/filters")]
        );
        // The user's own branch of that name (tracking something else, or nothing) is left
        // alone: the pull request goes to pr/7.
        for upstream in [Some("refs/remotes/upstream/feature/filters"), None] {
            let own_branch = local(Some("main"), Some(BASE), &[("feature/filters", upstream)]);
            assert_eq!(
                plan(
                    &same_repo,
                    &origin_canonical,
                    &canonical_remotes,
                    &own_branch
                ),
                [
                    fetch_refspec(
                        "origin",
                        &["refs/pull/7/head:refs/heads/pr/7", "refs/heads/main"]
                    ),
                    change_branch("pr/7"),
                    fast_forward_step("pr/7"),
                ],
                "{upstream:?}"
            );
        }

        // origin is the user's fork, `upstream` the canonical repository.
        let fork_checkout = context(fork.clone(), canonical.clone(), true);
        let fork_remotes = [
            remote("origin", fork.clone()),
            remote("upstream", canonical.clone()),
        ];
        let detached = local(None, None, &[]);
        assert_eq!(
            plan(&same_repo, &fork_checkout, &fork_remotes, &detached),
            [
                fetch_remote("upstream"),
                change_branch("upstream/feature/filters"),
                fast_forward_step("feature/filters"),
            ]
        );
        // The user's own pull request from their fork: its branch is on origin, the base on
        // upstream.
        assert_eq!(
            plan(
                &details(Some(fork.clone())),
                &fork_checkout,
                &fork_remotes,
                &detached
            ),
            [
                fetch_remote("origin"),
                change_branch("origin/feature/filters"),
                fetch_refspec("upstream", &["refs/heads/main"]),
                fast_forward_step("feature/filters"),
            ]
        );

        // Someone else's fork: refs/pull from the canonical remote, or its URL.
        let third_party_pull_request = details(Some(third_party));
        assert_eq!(
            plan(
                &third_party_pull_request,
                &fork_checkout,
                &fork_remotes,
                &on_main
            ),
            [
                fetch_refspec(
                    "upstream",
                    &["refs/pull/7/head:refs/heads/pr/7", "refs/heads/main"]
                ),
                change_branch("pr/7"),
                fast_forward_step("pr/7"),
            ]
        );
        let fork_only = [remote("origin", fork.clone())];
        assert_eq!(
            plan(
                &third_party_pull_request,
                &fork_checkout,
                &fork_only,
                &on_main
            ),
            [
                fetch_refspec(
                    "git@github.com:o/r.git",
                    &["refs/pull/7/head:refs/heads/pr/7", "refs/heads/main"]
                ),
                change_branch("pr/7"),
                fast_forward_step("pr/7"),
            ]
        );
        let https_fork_checkout = context(fork.clone(), canonical, false);
        assert_eq!(
            plan(
                &third_party_pull_request,
                &https_fork_checkout,
                &fork_only,
                &on_main
            ),
            [
                fetch_refspec(
                    "https://github.com/o/r",
                    &["refs/pull/7/head:refs/heads/pr/7", "refs/heads/main"]
                ),
                change_branch("pr/7"),
                fast_forward_step("pr/7"),
            ]
        );

        // On pr/7: git can't fetch into it, so it fast-forwards to what was fetched.
        assert_eq!(
            plan(
                &third_party_pull_request,
                &fork_checkout,
                &fork_remotes,
                &local(Some("pr/7"), Some(BASE), &[])
            ),
            [
                fetch_refspec("upstream", &["refs/pull/7/head", "refs/heads/main"]),
                fast_forward_step("pr/7"),
            ]
        );
        // An earlier review's pr/7, maybe with commits of its own: brought forward only when
        // that is a fast-forward, before switching to it.
        assert_eq!(
            plan(
                &third_party_pull_request,
                &fork_checkout,
                &fork_remotes,
                &local(Some("main"), Some(BASE), &[("pr/7", None)])
            ),
            [
                fetch_refspec("upstream", &["refs/pull/7/head", "refs/heads/main"]),
                ensure_fast_forward_step("pr/7"),
                change_branch("pr/7"),
                fast_forward_step("pr/7"),
            ]
        );

        // A closed pull request's branch may be gone; refs/pull stays.
        let mut closed = same_repo.clone();
        closed.state = PullRequestStatus::Merged;
        assert_eq!(
            plan(&closed, &origin_canonical, &canonical_remotes, &on_main),
            [
                fetch_refspec(
                    "origin",
                    &["refs/pull/7/head:refs/heads/pr/7", "refs/heads/main"]
                ),
                change_branch("pr/7"),
                fast_forward_step("pr/7"),
            ]
        );

        // The user's fork's `main` into `main`: switching to `origin/main` would make the local
        // `main` track the fork.
        let mut from_fork_main = details(Some(fork));
        from_fork_main.head_ref = "main".into();
        assert_eq!(
            plan(&from_fork_main, &fork_checkout, &fork_remotes, &on_main),
            [
                fetch_refspec(
                    "upstream",
                    &["refs/pull/7/head:refs/heads/pr/7", "refs/heads/main"]
                ),
                change_branch("pr/7"),
                fast_forward_step("pr/7"),
            ]
        );

        // The head repository was deleted.
        assert_eq!(
            plan(
                &details(None),
                &origin_canonical,
                &canonical_remotes,
                &on_main
            ),
            [
                fetch_refspec(
                    "origin",
                    &["refs/pull/7/head:refs/heads/pr/7", "refs/heads/main"]
                ),
                change_branch("pr/7"),
                fast_forward_step("pr/7"),
            ]
        );

        // Branch names from the forge never reach git as options or as git's own refs
        // (`origin/HEAD` is the default branch).
        for (head_ref, base_ref) in [
            ("--upload-pack=touch /tmp/x", "main"),
            ("feature/filters", "main:refs/heads/x"),
            ("HEAD", "main"),
            ("feature/filters", "HEAD"),
        ] {
            let mut hostile = same_repo.clone();
            hostile.head_ref = head_ref.into();
            hostile.base_ref = base_ref.into();
            assert!(
                plan_checkout(&hostile, &origin_canonical, &canonical_remotes, &on_main).is_err(),
                "{head_ref} {base_ref}"
            );
        }
        assert_eq!(
            checked_out_branch(&plan(
                &same_repo,
                &origin_canonical,
                &canonical_remotes,
                &on_main
            )),
            Some("feature/filters")
        );
    }

    /// Runs git from PATH in `dir` for test setup.
    #[allow(clippy::disallowed_methods)]
    fn git(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(["-c", "user.name=Test", "-c", "user.email=test@invalid"])
            .args([
                "-c",
                "init.defaultBranch=main",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn commit(dir: &Path, file: &str, text: &str) -> String {
        std::fs::write(dir.join(file), text).unwrap();
        git(dir, &["add", file]);
        git(dir, &["commit", "-q", "-m", text]);
        git(dir, &["rev-parse", "HEAD"])
    }

    /// How `run_checkout` runs its own git commands in `clone`, for an untrusted repository.
    fn git_context(clone: &Path) -> GitContext {
        let environment = HashMap::from_iter([
            ("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string()),
            ("GIT_CONFIG_GLOBAL".to_string(), "/dev/null".to_string()),
            (
                "PATH".to_string(),
                std::env::var("PATH").unwrap_or_default(),
            ),
        ]);
        GitContext {
            work_dir: clone.to_path_buf(),
            environment: Arc::new(environment),
            is_trusted: false,
        }
    }

    fn run(clone: &Path, args: &[&str]) -> Result<String> {
        let args = args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        block_on(run_git(&git_context(clone), &args, None))
    }

    /// A bare remote with `main` and a pull request #7 (`refs/pull/7/head`) from a fork, the
    /// working repository it is pushed from, and a clone of it. Returns the base and the head.
    fn pull_request_remote(temp: &Path) -> (String, String) {
        let work = temp.join("work");
        std::fs::create_dir(&work).unwrap();
        git(&work, &["init", "-q"]);
        let base = commit(&work, "a.txt", "one\n");
        git(temp, &["clone", "-q", "--bare", "work", "remote.git"]);
        git(temp, &["clone", "-q", "remote.git", "clone"]);
        git(&work, &["checkout", "-q", "-b", "contribution"]);
        let head = commit(&work, "a.txt", "one\ntwo\n");
        push_pull_request(temp, false);
        (base, head)
    }

    fn push_pull_request(temp: &Path, force: bool) {
        let mut args = vec!["push", "-q"];
        if force {
            args.push("-f");
        }
        args.extend(["../remote.git", "contribution:refs/pull/7/head"]);
        git(&temp.join("work"), &args);
    }

    fn fetch(clone: &Path, refspecs: &[&str]) -> Result<String> {
        let mut args = vec!["fetch", "--end-of-options", "origin"];
        args.extend(refspecs);
        run(clone, &args).map_err(|error| {
            fetch_error(
                error,
                &refspecs
                    .iter()
                    .map(|refspec| refspec.to_string())
                    .collect::<Vec<_>>(),
            )
        })
    }

    #[test]
    fn test_fetching_pull_requests_with_real_git() {
        let temp = tempfile::tempdir().unwrap();
        let (base, first) = pull_request_remote(temp.path());
        let work = temp.path().join("work");
        let clone = temp.path().join("clone");
        let context = git_context(&clone);

        fetch(
            &clone,
            &["refs/pull/7/head:refs/heads/pr/7", "refs/heads/main"],
        )
        .unwrap();
        assert_eq!(git(&clone, &["rev-parse", "pr/7"]), first);
        git(&clone, &["checkout", "-q", "pr/7"]);

        // A new push: on pr/7 the fetch goes to FETCH_HEAD, then HEAD fast-forwards.
        let second = commit(&work, "a.txt", "one\ntwo\nthree\n");
        push_pull_request(temp.path(), false);
        fetch(&clone, &["refs/pull/7/head", "refs/heads/main"]).unwrap();
        block_on(fast_forward(&context, "pr/7", &second)).unwrap();
        assert_eq!(git(&clone, &["rev-parse", "HEAD"]), second);
        assert_eq!(git(&clone, &["rev-parse", "pr/7"]), second);
        // At the head already: nothing to do.
        block_on(fast_forward(&context, "pr/7", &second)).unwrap();

        // A force-push can't update an existing pr/7, and the hint never says to delete it.
        git(&clone, &["checkout", "-q", "main"]);
        git(&work, &["reset", "-q", "--hard", &base]);
        let rewritten = commit(&work, "a.txt", "one\nTWO\n");
        push_pull_request(temp.path(), true);
        let error = fetch(
            &clone,
            &["refs/pull/7/head:refs/heads/pr/7", "refs/heads/main"],
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("git fetch failed"), "{message}");
        assert!(
            message.contains("pr/7 has diverged from the pull request")
                && !message.contains("Delete"),
            "{message}"
        );
        assert_eq!(git(&clone, &["rev-parse", "pr/7"]), second);

        // Before switching to it, a diverged pr/7 stops the checkout; on it, the fast-forward
        // refuses. Nothing moves either way.
        fetch(&clone, &["refs/pull/7/head", "refs/heads/main"]).unwrap();
        let error = block_on(ensure_fast_forward(&context, "pr/7", &rewritten)).unwrap_err();
        assert!(
            format!("{error:#}").contains("git branch -m pr/7 pr/7-old"),
            "{error:#}"
        );
        git(&clone, &["checkout", "-q", "pr/7"]);
        let error = block_on(fast_forward(&context, "pr/7", &rewritten)).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("pr/7 has diverged from the pull request"),
            "{message}"
        );
        assert_eq!(git(&clone, &["rev-parse", "HEAD"]), second);

        // A branch behind the head, or ahead of it (commits of its own on top), is fine.
        git(&clone, &["checkout", "-q", "-b", "behind", &base]);
        block_on(ensure_fast_forward(&context, "behind", &rewritten)).unwrap();
        let ahead = commit(&clone, "b.txt", "mine\n");
        git(&clone, &["branch", "-q", "ahead"]);
        git(&clone, &["checkout", "-q", "main"]);
        block_on(ensure_fast_forward(&context, "ahead", &base)).unwrap();
        assert_eq!(git(&clone, &["rev-parse", "ahead"]), ahead);
    }

    /// The fast-forward checks, in its own git job, that the branch it was planned for is still
    /// checked out: a switch meanwhile must not move another branch to the pull request.
    #[test]
    fn test_fast_forward_only_moves_the_planned_branch() {
        let temp = tempfile::tempdir().unwrap();
        let (base, head) = pull_request_remote(temp.path());
        let clone = temp.path().join("clone");
        let context = git_context(&clone);
        fetch(&clone, &["refs/pull/7/head:refs/heads/pr/7"]).unwrap();
        git(&clone, &["branch", "-q", "-f", "pr/7", &base]);

        // Planned on pr/7, but main is checked out now (a terminal, another pull request).
        let error = block_on(fast_forward(&context, "pr/7", &head)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Didn't fast-forward pr/7 to the pull request's head: main is checked out now. Open \
             the pull request again."
        );
        assert_eq!(git(&clone, &["rev-parse", "main"]), base);

        git(&clone, &["checkout", "-q", "--detach", "main"]);
        let error = block_on(fast_forward(&context, "pr/7", &head)).unwrap_err();
        assert!(
            error.to_string().contains("HEAD is detached now"),
            "{error:#}"
        );
        assert_eq!(git(&clone, &["rev-parse", "HEAD"]), base);

        git(&clone, &["checkout", "-q", "pr/7"]);
        block_on(fast_forward(&context, "pr/7", &head)).unwrap();
        assert_eq!(git(&clone, &["rev-parse", "pr/7"]), head);
        assert_eq!(git(&clone, &["rev-parse", "main"]), base);
    }

    /// The path of the `git` on `PATH`.
    fn git_binary() -> PathBuf {
        std::env::var_os("PATH")
            .iter()
            .flat_map(|path| std::env::split_paths(path).collect::<Vec<_>>())
            .map(|directory| directory.join("git"))
            .find(|path| path.is_file())
            .expect("git on PATH")
    }

    /// A same-named local branch that tracks something else isn't touched: the pull request is
    /// checked out as pr/7. One that tracks the pull request's branch is reused.
    #[gpui::test]
    async fn test_same_named_branch_with_real_git(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let temp = tempfile::tempdir().unwrap();
        let (base, head) = pull_request_remote(temp.path());
        let work = temp.path().join("work");
        let clone = temp.path().join("clone");
        // The pull request's branch `patch-1` is on origin; the user's own `patch-1` tracks
        // their other remote.
        git(
            &work,
            &[
                "push",
                "-q",
                "../remote.git",
                "contribution:refs/heads/patch-1",
            ],
        );
        git(&clone, &["remote", "add", "mine", "../remote.git"]);
        git(&clone, &["fetch", "-q", "mine"]);
        git(&clone, &["branch", "-q", "--track", "patch-1", "mine/main"]);

        let backend = git::repository::RealGitRepository::new(
            &clone.join(".git"),
            None,
            Some(git_binary()),
            cx.executor(),
        )
        .unwrap();
        let base_oid: Oid = base.parse().unwrap();
        let state = local_state(&backend, base_oid).await.unwrap();
        assert_eq!(state.current_branch.as_deref(), Some("main"));
        assert_eq!(state.head, Some(base_oid));
        assert!(state.has_base);
        assert_eq!(
            state.branches.get("patch-1"),
            Some(&Some("refs/remotes/mine/main".to_string()))
        );

        let canonical = repo("o", "r");
        let mut pull_request = details(Some(canonical.clone()));
        pull_request.head_ref = "patch-1".into();
        pull_request.head_oid = head.parse().unwrap();
        pull_request.base_oid = base_oid;
        let checkout_context = context(canonical.clone(), canonical.clone(), false);
        let remotes = [
            remote("origin", canonical.clone()),
            remote("mine", repo("me", "r")),
        ];
        let steps = plan_checkout(&pull_request, &checkout_context, &remotes, &state).unwrap();
        assert_eq!(
            steps,
            [
                fetch_refspec(
                    "origin",
                    &["refs/pull/7/head:refs/heads/pr/7", "refs/heads/main"]
                ),
                change_branch("pr/7"),
                CheckoutStep::FastForward {
                    branch: "pr/7".into(),
                    to: head.clone(),
                },
            ]
        );
        let context = git_context(&clone);
        for step in steps {
            match step {
                CheckoutStep::FetchRefspec { source, refspecs } => {
                    let mut args =
                        vec!["fetch".to_string(), "--end-of-options".to_string(), source];
                    args.extend(refspecs);
                    run_git(&context, &args, None).await.unwrap();
                }
                CheckoutStep::ChangeBranch { name } => backend.change_branch(name).await.unwrap(),
                CheckoutStep::FastForward { branch, to } => {
                    fast_forward(&context, &branch, &to).await.unwrap()
                }
                step => panic!("unexpected {step:?}"),
            }
        }
        assert_eq!(git(&clone, &["rev-parse", "HEAD"]), head);
        assert_eq!(git(&clone, &["symbolic-ref", "--short", "HEAD"]), "pr/7");
        assert_eq!(
            git(&clone, &["rev-parse", "--abbrev-ref", "patch-1@{upstream}"]),
            "mine/main"
        );
        assert_eq!(git(&clone, &["rev-parse", "patch-1"]), base);

        // Tracking the pull request's branch: reused.
        git(&clone, &["checkout", "-q", "main"]);
        git(&clone, &["fetch", "-q", "origin"]);
        git(
            &clone,
            &[
                "branch",
                "-q",
                "--set-upstream-to",
                "origin/patch-1",
                "patch-1",
            ],
        );
        let state = local_state(&backend, base_oid).await.unwrap();
        let steps = plan_checkout(&pull_request, &checkout_context, &remotes, &state).unwrap();
        assert_eq!(
            steps,
            [
                fetch_remote("origin"),
                CheckoutStep::EnsureFastForward {
                    branch: "patch-1".into(),
                    to: head.clone(),
                },
                change_branch("patch-1"),
                CheckoutStep::FastForward {
                    branch: "patch-1".into(),
                    to: head,
                },
            ]
        );
    }
}
