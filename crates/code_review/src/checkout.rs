//! Checking out a pull request the way Zed checks out branches: a pull request whose head branch
//! is on one of the checkout's remotes becomes a local branch tracking it (Zed's `change_branch`),
//! any other one is fetched from `refs/pull/N/head` into a local `pr/N` branch. Nothing is
//! stashed: with uncommitted changes git carries them over or refuses, and the error is shown.

use crate::{
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
    repository::{FetchOptions, Remote},
};
use git_ui_core::askpass_modal::AskPassModal;
use gpui::{
    AsyncWindowContext, BackgroundExecutor, Context, Entity, SharedString, WeakEntity, Window,
};
use project::git_store::{LocalRepositoryState, Repository, RepositoryState};
use std::{path::PathBuf, sync::Arc};
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
    /// `git fetch <source> <refspecs>`, without forcing: a pull request that was force-pushed
    /// can't update an existing `pr/N`.
    FetchRefspec {
        /// A remote's name or a repository URL.
        source: String,
        refspecs: Vec<String>,
    },
    /// `git merge --ff-only <to>`, skipped when HEAD already is the pull request's head: Zed's
    /// branch switch leaves an existing branch where it was.
    FastForward { to: String },
}

/// The local branch a pull request without a remote for its head branch is fetched into.
pub(crate) fn pull_request_branch(number: u64) -> String {
    format!("pr/{number}")
}

/// What checking out the pull request takes, given the checkout's GitHub remotes (preferred
/// first), its current branch and HEAD. Nothing when HEAD already is the pull request's head.
pub(crate) fn plan_checkout(
    details: &PullRequestDetails,
    context: &RepositoryContext,
    remotes: &[GitHubRemote],
    current_branch: Option<&str>,
    head: Option<Oid>,
) -> Result<Vec<CheckoutStep>> {
    validate_branch_name(&details.head_ref).context("The pull request's head branch")?;
    validate_branch_name(&details.base_ref).context("The pull request's base branch")?;
    if head == Some(details.head_oid) {
        return Ok(Vec::new());
    }
    let fast_forward = CheckoutStep::FastForward {
        to: details.head_oid.to_string(),
    };
    let base_source = canonical_source(context, remotes);
    let base_refspec = format!("refs/heads/{}", details.base_ref);

    if let Some(head_remote) = head_remote(details, remotes) {
        let mut steps = vec![
            CheckoutStep::FetchRemote {
                remote: head_remote.name.clone(),
            },
            CheckoutStep::ChangeBranch {
                name: format!("{}/{}", head_remote.name, details.head_ref),
            },
        ];
        // A pull request from a fork into the canonical repository: the base commit isn't on
        // the fork's remote.
        if !head_remote.repo.is_same_repo(&context.canonical) {
            steps.push(CheckoutStep::FetchRefspec {
                source: base_source,
                refspecs: vec![base_refspec],
            });
        }
        steps.push(fast_forward);
        return Ok(steps);
    }

    let pull_ref = format!("refs/pull/{}/head", details.number);
    let branch = pull_request_branch(details.number);
    // git refuses to fetch into the checked-out branch, so that one moves by a fast-forward to
    // what the fetch brought.
    if current_branch == Some(branch.as_str()) {
        Ok(vec![
            CheckoutStep::FetchRefspec {
                source: base_source,
                refspecs: vec![pull_ref, base_refspec],
            },
            fast_forward,
        ])
    } else {
        Ok(vec![
            CheckoutStep::FetchRefspec {
                source: base_source,
                refspecs: vec![format!("{pull_ref}:refs/heads/{branch}"), base_refspec],
            },
            CheckoutStep::ChangeBranch { name: branch },
            fast_forward,
        ])
    }
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

/// Runs the steps of [`plan_checkout`] in `repository`. Fetches ask for credentials through
/// `workspace`'s modal.
pub(crate) async fn run_checkout(
    repository: &Entity<Repository>,
    steps: Vec<CheckoutStep>,
    head_oid: Oid,
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
                run_git_job(repository, args, Some(askpass), "git fetch", cx)
                    .await
                    .map_err(|error| fetch_error(error, &refspecs))?;
            }
            CheckoutStep::FastForward { to } => {
                if head_sha(repository, cx).await? == Some(head_oid) {
                    continue;
                }
                let args = vec![
                    "merge".to_string(),
                    "--ff-only".to_string(),
                    "--end-of-options".to_string(),
                    to,
                ];
                let branch = repository.read_with(cx, |repository, _| {
                    repository
                        .branch
                        .as_ref()
                        .map(|branch| branch.name().to_string())
                });
                run_git_job(repository, args, None, "git merge --ff-only", cx)
                    .await
                    .map_err(|error| fast_forward_error(error, branch.as_deref()))?;
            }
        }
    }
    Ok(())
}

pub(crate) async fn head_sha(
    repository: &Entity<Repository>,
    cx: &mut AsyncWindowContext,
) -> Result<Option<Oid>> {
    let head = repository.update(cx, |repository, _| repository.head_sha());
    head.await
        .context("the git job was canceled")??
        .map(|sha| sha.parse::<Oid>())
        .transpose()
        .context("reading HEAD")
}

fn fetch_error(error: anyhow::Error, refspecs: &[String]) -> anyhow::Error {
    let message = format!("{error:#}");
    let branch = refspecs
        .iter()
        .find_map(|refspec| refspec.split_once(":refs/heads/").map(|(_, branch)| branch));
    match branch {
        Some(branch) if message.contains("non-fast-forward") => anyhow!(
            "{}\nThe pull request was force-pushed. Delete the local branch {branch} and try \
             again.",
            message.trim()
        ),
        _ => error,
    }
}

fn fast_forward_error(error: anyhow::Error, branch: Option<&str>) -> anyhow::Error {
    let message = format!("{error:#}");
    if message.contains("Not possible to fast-forward") {
        let branch = branch.unwrap_or("HEAD");
        anyhow!(
            "{}\n{branch} has commits the pull request doesn't have, so it can't move to the \
             pull request's head.",
            message.trim()
        )
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

/// Runs git on the repository's job queue, like Zed's own git commands.
async fn run_git_job(
    repository: &Entity<Repository>,
    args: Vec<String>,
    askpass: Option<AskPassDelegate>,
    status: &'static str,
    cx: &mut AsyncWindowContext,
) -> Result<String> {
    let work_dir = repository.read_with(cx, |repository, _| {
        repository.work_directory_abs_path.to_path_buf()
    });
    let job = repository.update(cx, |repository, _| {
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
                let askpass = askpass.map(|askpass| (askpass, cx.background_executor().clone()));
                run_git(&context, &args, askpass).await
            },
        )
    });
    job.await.context("the git job was canceled")?
}

/// Runs git with Zed's safety settings (`GitBinary::build_command`) and, for commands that may
/// need credentials, Zed's askpass (`run_git_command`). Returns stdout.
pub(crate) async fn run_git(
    context: &GitContext,
    args: &[String],
    askpass: Option<(AskPassDelegate, BackgroundExecutor)>,
) -> Result<String> {
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

    fn fast_forward() -> CheckoutStep {
        CheckoutStep::FastForward { to: HEAD.into() }
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
                    current_branch: Option<&str>,
                    head: Option<&str>| {
            plan_checkout(
                details,
                context,
                remotes,
                current_branch,
                head.map(|head| head.parse().unwrap()),
            )
            .unwrap()
        };
        let origin_canonical = context(canonical.clone(), canonical.clone(), false);
        let canonical_remotes = [remote("origin", canonical.clone())];

        // Already at the pull request's head: nothing to do, not even a fetch.
        assert_eq!(
            plan(
                &same_repo,
                &origin_canonical,
                &canonical_remotes,
                Some("feature/filters"),
                Some(HEAD)
            ),
            []
        );

        // The head branch is on origin: a branch tracking it.
        assert_eq!(
            plan(
                &same_repo,
                &origin_canonical,
                &canonical_remotes,
                Some("main"),
                Some(BASE)
            ),
            [
                fetch_remote("origin"),
                change_branch("origin/feature/filters"),
                fast_forward(),
            ]
        );

        // origin is the user's fork, `upstream` the canonical repository.
        let fork_checkout = context(fork.clone(), canonical.clone(), true);
        let fork_remotes = [
            remote("origin", fork.clone()),
            remote("upstream", canonical.clone()),
        ];
        assert_eq!(
            plan(
                &same_repo,
                &fork_checkout,
                &fork_remotes,
                Some("main"),
                None
            ),
            [
                fetch_remote("upstream"),
                change_branch("upstream/feature/filters"),
                fast_forward(),
            ]
        );
        // The user's own pull request from their fork: its branch is on origin, the base on
        // upstream.
        assert_eq!(
            plan(
                &details(Some(fork.clone())),
                &fork_checkout,
                &fork_remotes,
                Some("main"),
                None
            ),
            [
                fetch_remote("origin"),
                change_branch("origin/feature/filters"),
                fetch_refspec("upstream", &["refs/heads/main"]),
                fast_forward(),
            ]
        );

        // Someone else's fork: refs/pull from the canonical remote, or its URL.
        let third_party_pull_request = details(Some(third_party));
        assert_eq!(
            plan(
                &third_party_pull_request,
                &fork_checkout,
                &fork_remotes,
                Some("main"),
                None
            ),
            [
                fetch_refspec(
                    "upstream",
                    &["refs/pull/7/head:refs/heads/pr/7", "refs/heads/main"]
                ),
                change_branch("pr/7"),
                fast_forward(),
            ]
        );
        let fork_only = [remote("origin", fork.clone())];
        assert_eq!(
            plan(
                &third_party_pull_request,
                &fork_checkout,
                &fork_only,
                Some("main"),
                None
            ),
            [
                fetch_refspec(
                    "git@github.com:o/r.git",
                    &["refs/pull/7/head:refs/heads/pr/7", "refs/heads/main"]
                ),
                change_branch("pr/7"),
                fast_forward(),
            ]
        );
        let https_fork_checkout = context(fork.clone(), canonical, false);
        assert_eq!(
            plan(
                &third_party_pull_request,
                &https_fork_checkout,
                &fork_only,
                Some("main"),
                None
            ),
            [
                fetch_refspec(
                    "https://github.com/o/r",
                    &["refs/pull/7/head:refs/heads/pr/7", "refs/heads/main"]
                ),
                change_branch("pr/7"),
                fast_forward(),
            ]
        );

        // Already on pr/7: git can't fetch into it, so it fast-forwards to what was fetched.
        assert_eq!(
            plan(
                &third_party_pull_request,
                &fork_checkout,
                &fork_remotes,
                Some("pr/7"),
                Some(BASE)
            ),
            [
                fetch_refspec("upstream", &["refs/pull/7/head", "refs/heads/main"]),
                fast_forward(),
            ]
        );

        // A closed pull request's branch may be gone; refs/pull stays.
        let mut closed = same_repo.clone();
        closed.state = PullRequestStatus::Merged;
        assert_eq!(
            plan(
                &closed,
                &origin_canonical,
                &canonical_remotes,
                Some("main"),
                None
            ),
            [
                fetch_refspec(
                    "origin",
                    &["refs/pull/7/head:refs/heads/pr/7", "refs/heads/main"]
                ),
                change_branch("pr/7"),
                fast_forward(),
            ]
        );

        // The user's fork's `main` into `main`: switching to `origin/main` would make the local
        // `main` track the fork.
        let mut from_fork_main = details(Some(fork));
        from_fork_main.head_ref = "main".into();
        assert_eq!(
            plan(
                &from_fork_main,
                &fork_checkout,
                &fork_remotes,
                Some("main"),
                None
            ),
            [
                fetch_refspec(
                    "upstream",
                    &["refs/pull/7/head:refs/heads/pr/7", "refs/heads/main"]
                ),
                change_branch("pr/7"),
                fast_forward(),
            ]
        );

        // The head repository was deleted.
        assert_eq!(
            plan(
                &details(None),
                &origin_canonical,
                &canonical_remotes,
                Some("main"),
                None
            ),
            [
                fetch_refspec(
                    "origin",
                    &["refs/pull/7/head:refs/heads/pr/7", "refs/heads/main"]
                ),
                change_branch("pr/7"),
                fast_forward(),
            ]
        );

        // Branch names from the forge never reach git as options.
        let mut hostile = same_repo.clone();
        hostile.head_ref = "--upload-pack=touch /tmp/x".into();
        assert!(
            plan_checkout(&hostile, &origin_canonical, &canonical_remotes, None, None).is_err()
        );
        let mut hostile = same_repo;
        hostile.base_ref = "main:refs/heads/x".into();
        assert!(
            plan_checkout(&hostile, &origin_canonical, &canonical_remotes, None, None).is_err()
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

    /// Runs git in `clone` the way `run_checkout` runs its own git commands.
    fn run(clone: &Path, args: &[&str]) -> Result<String> {
        let environment = HashMap::from_iter([
            ("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string()),
            ("GIT_CONFIG_GLOBAL".to_string(), "/dev/null".to_string()),
            (
                "PATH".to_string(),
                std::env::var("PATH").unwrap_or_default(),
            ),
        ]);
        let context = GitContext {
            work_dir: clone.to_path_buf(),
            environment: Arc::new(environment),
            is_trusted: false,
        };
        let args = args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        block_on(run_git(&context, &args, None))
    }

    #[test]
    fn test_fetching_pull_requests_with_real_git() {
        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        let clone = temp.path().join("clone");
        std::fs::create_dir(&work).unwrap();
        git(&work, &["init", "-q"]);
        let base = commit(&work, "a.txt", "one\n");
        git(
            temp.path(),
            &["clone", "-q", "--bare", "work", "remote.git"],
        );
        git(temp.path(), &["clone", "-q", "remote.git", "clone"]);

        // A pull request from a fork: only GitHub's refs/pull/N/head has it.
        git(&work, &["checkout", "-q", "-b", "contribution"]);
        let first = commit(&work, "a.txt", "one\ntwo\n");
        git(
            &work,
            &[
                "push",
                "-q",
                "../remote.git",
                "contribution:refs/pull/7/head",
            ],
        );

        let fetch = |refspecs: &[&str]| {
            let mut args = vec!["fetch", "--end-of-options", "origin"];
            args.extend(refspecs);
            run(&clone, &args).map_err(|error| {
                fetch_error(
                    error,
                    &refspecs
                        .iter()
                        .map(|refspec| refspec.to_string())
                        .collect::<Vec<_>>(),
                )
            })
        };
        fetch(&["refs/pull/7/head:refs/heads/pr/7", "refs/heads/main"]).unwrap();
        assert_eq!(git(&clone, &["rev-parse", "pr/7"]), first);
        git(&clone, &["checkout", "-q", "pr/7"]);

        // A new push: on pr/7 the fetch goes to FETCH_HEAD, then HEAD fast-forwards.
        let second = commit(&work, "a.txt", "one\ntwo\nthree\n");
        git(
            &work,
            &[
                "push",
                "-q",
                "../remote.git",
                "contribution:refs/pull/7/head",
            ],
        );
        fetch(&["refs/pull/7/head", "refs/heads/main"]).unwrap();
        run(&clone, &["merge", "--ff-only", "--end-of-options", &second]).unwrap();
        assert_eq!(git(&clone, &["rev-parse", "HEAD"]), second);
        assert_eq!(git(&clone, &["rev-parse", "pr/7"]), second);

        // A force-push can't update an existing pr/7.
        git(&clone, &["checkout", "-q", "main"]);
        git(&work, &["reset", "-q", "--hard", &base]);
        let rewritten = commit(&work, "a.txt", "one\nTWO\n");
        git(
            &work,
            &[
                "push",
                "-q",
                "-f",
                "../remote.git",
                "contribution:refs/pull/7/head",
            ],
        );
        let error = fetch(&["refs/pull/7/head:refs/heads/pr/7", "refs/heads/main"]).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("git fetch failed"), "{message}");
        assert!(
            message.contains("Delete the local branch pr/7 and try again."),
            "{message}"
        );
        assert_eq!(git(&clone, &["rev-parse", "pr/7"]), second);

        // A diverged branch doesn't fast-forward; nothing moves.
        git(&clone, &["checkout", "-q", "pr/7"]);
        fetch(&["refs/pull/7/head", "refs/heads/main"]).unwrap();
        let error = run(
            &clone,
            &["merge", "--ff-only", "--end-of-options", &rewritten],
        )
        .map_err(|error| fast_forward_error(error, Some("pr/7")))
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("pr/7 has commits the pull request doesn't have"),
            "{message}"
        );
        assert_eq!(git(&clone, &["rev-parse", "HEAD"]), second);
    }
}
