//! `zed --review TARGET [REPO_DIR]` and `zed://review?repo=PATH&target=TARGET` links (DESIGN §2.3).
//! The CLI turns `--review` into a link, so both arrive here: the link opens the review, and
//! `--wait` waits for its outcome.

use crate::{
    ReviewView,
    commits::{check_branch_base, resolve_commit_spec},
    forge,
    pull_request::{OpenPullRequestOptions, ReviewCancelled, open_pull_request, repository_at},
    session::ReviewOutcome,
    target::{BranchBase, ReviewTarget, TargetSpec, parse_target_spec},
};
use ::cli::{CliResponse, CliResponseSink};
use anyhow::{Context as _, Result, anyhow, bail};
use fs::Fs;
use futures::{FutureExt as _, channel::oneshot};
use gpui::{App, AsyncApp, Entity, Task, WeakEntity, Window, WindowHandle};
use http_client::Url;
use project::git_store::Repository;
use std::{
    cell::Cell,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::Duration,
};
use util::ResultExt as _;
use workspace::{AppState, MultiWorkspace, OpenOptions, Workspace, WorkspaceMatching};

/// Whether `url` is a `zed://review` link (`zed://review`, `zed://review?…`, `zed://review/…`).
pub fn is_review_url(url: &str) -> bool {
    url.strip_prefix("zed://review")
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('?') || rest.starts_with('/'))
}

/// A parsed `zed://review` link.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ReviewLink {
    /// Any directory inside the repository's working tree.
    pub(crate) repo: PathBuf,
    pub(crate) target: String,
}

impl ReviewLink {
    pub(crate) fn parse(url: &str) -> Result<Self> {
        let invalid = |what: &str| {
            anyhow!("Invalid review link: {what}. Use zed://review?repo=PATH&target=TARGET")
        };
        let parsed = Url::parse(url).map_err(|error| invalid(&error.to_string()))?;
        if parsed.scheme() != "zed"
            || parsed.host_str() != Some("review")
            || !matches!(parsed.path(), "" | "/")
        {
            return Err(invalid("not a zed://review link"));
        }
        let mut repo = None;
        let mut target = None;
        for (key, value) in parsed.query_pairs() {
            match key.as_ref() {
                "repo" if repo.is_none() => repo = Some(value.into_owned()),
                "target" if target.is_none() => target = Some(value.into_owned()),
                _ => {}
            }
        }
        let repo = repo
            .filter(|repo| !repo.is_empty())
            .ok_or_else(|| invalid("no repo"))?;
        let target = target
            .filter(|target| !target.is_empty())
            .ok_or_else(|| invalid("no target"))?;
        let repo = PathBuf::from(repo);
        if !repo.is_absolute() {
            bail!(
                "Invalid review link: the repository path {} isn't absolute",
                repo.display()
            );
        }
        Ok(Self { repo, target })
    }
}

/// A `zed --review` request, or a link the CLI was given.
pub struct CliReviewRequest {
    pub url: String,
    /// The paths the CLI sent besides the link.
    pub paths: Vec<String>,
    pub wait: bool,
    /// How to open the repository when no window has it open (the CLI's `-n`, `-e`, …).
    pub open_options: OpenOptions,
}

/// Answers the CLI: always exactly one `Exit`, whoever comes first (the review, or Zed quitting).
struct CliReply {
    sink: Box<dyn CliResponseSink>,
    exited: Cell<bool>,
}

impl CliReply {
    fn ping(&self) -> Result<()> {
        self.sink.send(CliResponse::Ping)
    }

    fn exit(&self, status: i32, stdout: Option<String>, stderr: Option<String>) {
        if self.exited.replace(true) {
            return;
        }
        if let Some(message) = stdout {
            self.sink.send(CliResponse::Stdout { message }).log_err();
        }
        if let Some(message) = stderr {
            self.sink.send(CliResponse::Stderr { message }).log_err();
        }
        self.sink.send(CliResponse::Exit { status }).log_err();
    }
}

/// Opens the review a `zed://review` link from the CLI names and, with `--wait`, reports its
/// outcome: the posted review's URL or a finished local review's Markdown (exit 0), every tab of
/// the review closed or Zed quit (exit 2), or an error (exit 1).
pub async fn handle_cli_review(
    request: CliReviewRequest,
    responses: Box<dyn CliResponseSink>,
    app_state: Arc<AppState>,
    cx: &mut AsyncApp,
) {
    let reply = Rc::new(CliReply {
        sink: responses,
        exited: Cell::new(false),
    });
    cx.update(|cx| cx.activate(true));
    // `zed --review` sends the repository's path along with the link. A link from a browser
    // (xdg-open runs the CLI with the link alone) can't, so only the CLI's own request is
    // trusted to move the checkout without asking.
    let is_trusted = |link: &ReviewLink| match request.paths.as_slice() {
        [path] => Path::new(path) == link.repo,
        _ => false,
    };
    let mut opened_in = None;
    let (opened, trusted) = match ReviewLink::parse(&request.url) {
        Ok(link) => {
            let trusted = is_trusted(&link);
            let opened = open_link(
                &link,
                trusted,
                &request.open_options,
                &app_state,
                &mut opened_in,
                cx,
            )
            .await;
            (opened, trusted)
        }
        Err(error) => (Err(error), false),
    };
    let review = match opened {
        Ok(review) => review,
        Err(error) => {
            if !trusted {
                show_link_error(opened_in, &error, cx);
            }
            reply.exit(1, None, Some(forge::error_message(&error).to_string()));
            return;
        }
    };
    if !request.wait {
        reply.exit(0, None, None);
        return;
    }
    // Only the receiver is kept: a handle on the review or its session would keep the session
    // alive after its tabs closed, and the wait would never end.
    let outcome = review
        .read_with(cx, |review, _| review.session().clone())
        .update(cx, |session, _| session.wait_for_outcome());
    drop(review);
    wait_for_outcome(outcome, reply, cx).await;
}

/// Opens the review a `zed://review` link names, from outside the CLI (a link Zed was started
/// with, or one the platform handed it). Errors show as a toast: nobody else would see them.
pub async fn open_review_link(
    url: String,
    open_options: OpenOptions,
    app_state: Arc<AppState>,
    cx: &mut AsyncApp,
) {
    let mut opened_in = None;
    let opened = match ReviewLink::parse(&url) {
        Ok(link) => open_link(&link, false, &open_options, &app_state, &mut opened_in, cx).await,
        Err(error) => Err(error),
    };
    if let Err(error) = opened {
        show_link_error(opened_in, &error, cx);
    }
}

async fn wait_for_outcome(
    outcome: oneshot::Receiver<ReviewOutcome>,
    reply: Rc<CliReply>,
    cx: &mut AsyncApp,
) {
    // Tasks don't run again once Zed quits, so the quit hook answers itself.
    let _quit_subscription = cx.update(|cx| {
        cx.on_app_quit({
            let reply = reply.clone();
            move |_| {
                reply.exit(
                    2,
                    None,
                    Some("Zed quit before the review was submitted".into()),
                );
                async {}
            }
        })
    });
    let mut outcome = outcome.fuse();
    let background = cx.background_executor().clone();
    let outcome = loop {
        let mut timer = background.timer(Duration::from_secs(1)).fuse();
        futures::select_biased! {
            outcome = outcome => break outcome,
            _ = timer => {
                // The CLI is gone (killed, or its terminal closed): stop waiting, so a later
                // finish copies the review instead.
                if reply.ping().is_err() {
                    return;
                }
            }
        }
    };
    match outcome {
        Ok(ReviewOutcome::Submitted { url }) => reply.exit(0, Some(url), None),
        // The CLI prints each message with a newline.
        Ok(ReviewOutcome::Finished {
            markdown: Some(markdown),
        }) => {
            let markdown = markdown.strip_suffix('\n').unwrap_or(&markdown).to_string();
            reply.exit(0, Some(markdown), None)
        }
        Ok(ReviewOutcome::Finished { markdown: None }) => reply.exit(0, None, None),
        Err(oneshot::Canceled) => {
            reply.exit(2, None, Some("Review closed without submitting".into()))
        }
    }
}

/// Finds or opens the window with the link's repository and opens the review there.
/// `opened_in` is the workspace it opens in, once known.
async fn open_link(
    link: &ReviewLink,
    trusted: bool,
    open_options: &OpenOptions,
    app_state: &Arc<AppState>,
    opened_in: &mut Option<WeakEntity<Workspace>>,
    cx: &mut AsyncApp,
) -> Result<Entity<ReviewView>> {
    // Checked before anything is opened.
    let spec = parse_target_spec(&link.target)?;
    let root = repository_root(app_state.fs.as_ref(), &link.repo).await?;
    // `-n` and `-r` ask for another window.
    let found = if open_options.workspace_matching == WorkspaceMatching::None {
        None
    } else {
        cx.update(|cx| find_review_workspace(&root, cx))
    };
    let (window, workspace, repository) = match found {
        Some(found) => found,
        None => {
            let opened = cx
                .update(|cx| {
                    workspace::open_paths(
                        std::slice::from_ref(&root),
                        app_state.clone(),
                        open_options.clone(),
                        cx,
                    )
                })
                .await?;
            *opened_in = Some(opened.workspace.downgrade());
            let repository = repository_at(&opened.workspace, &root, cx).await?;
            (opened.window, opened.workspace, repository)
        }
    };
    *opened_in = Some(workspace.downgrade());
    let options = OpenReviewOptions {
        confirm_checkout: !trusted,
    };
    let open = window.update(cx, |multi_workspace, window, cx| {
        window.activate_window();
        if multi_workspace.workspace() != &workspace {
            multi_workspace.activate(workspace.clone(), None, window, cx);
        }
        workspace.update(cx, |workspace, cx| {
            open_review(workspace, repository, spec, options, window, cx)
        })
    })?;
    open.await
}

/// Shows why a link didn't open, unless the user cancelled it.
fn show_link_error(
    opened_in: Option<WeakEntity<Workspace>>,
    error: &anyhow::Error,
    cx: &mut AsyncApp,
) {
    if error.chain().any(|cause| cause.is::<ReviewCancelled>()) {
        return;
    }
    let shown = cx.update(|cx| {
        let workspace = opened_in
            .and_then(|workspace| workspace.upgrade())
            .or_else(|| any_workspace(cx))?;
        workspace.update(cx, |workspace, cx| {
            forge::show_error_toast(workspace, error, cx)
        });
        Some(())
    });
    if shown.is_none() {
        log::error!("opening a review link: {error:#}");
    }
}

/// The displayed workspace of the active window, else of any window.
fn any_workspace(cx: &App) -> Option<Entity<Workspace>> {
    workspace_windows(cx).into_iter().find_map(|window| {
        window
            .read(cx)
            .ok()
            .map(|multi_workspace| multi_workspace.workspace().clone())
    })
}

/// The workspace windows, the active one first.
fn workspace_windows(cx: &App) -> Vec<WindowHandle<MultiWorkspace>> {
    let active = cx
        .active_window()
        .and_then(|window| window.downcast::<MultiWorkspace>());
    active
        .into_iter()
        .chain(
            cx.windows()
                .into_iter()
                .filter_map(|window| window.downcast::<MultiWorkspace>())
                .filter(|window| Some(*window) != active),
        )
        .collect()
}

/// The root of the git working tree `path` is in (the deepest one: repositories can nest).
async fn repository_root(fs: &dyn Fs, path: &Path) -> Result<PathBuf> {
    let Some(metadata) = fs.metadata(path).await? else {
        bail!("{} doesn't exist", path.display());
    };
    if !metadata.is_dir {
        bail!("{} isn't a directory", path.display());
    }
    let directory = fs.canonicalize(path).await?;
    for ancestor in directory.ancestors() {
        let dot_git = ancestor.join(".git");
        let Some(metadata) = fs.metadata(&dot_git).await? else {
            continue;
        };
        // A linked worktree's `.git` is a file. A `.git` directory without a HEAD isn't a
        // repository for git either (an empty `/tmp/.git` happens).
        if !metadata.is_dir || fs.metadata(&dot_git.join("HEAD")).await?.is_some() {
            return Ok(ancestor.to_path_buf());
        }
    }
    bail!("{} isn't in a git repository", path.display())
}

/// The workspace that has the repository at `root` open (the active window first, a window's
/// displayed workspace first): a local project with a visible worktree at `root` or above it,
/// so the review can open every file of the repository.
fn find_review_workspace(
    root: &Path,
    cx: &App,
) -> Option<(
    WindowHandle<MultiWorkspace>,
    Entity<Workspace>,
    Entity<Repository>,
)> {
    workspace_windows(cx).into_iter().find_map(|window| {
        let multi_workspace = window.read(cx).ok()?;
        let displayed = multi_workspace.workspace();
        std::iter::once(displayed)
            .chain(
                multi_workspace
                    .workspaces()
                    .filter(|workspace| *workspace != displayed),
            )
            .find_map(|workspace| {
                let project = workspace.read(cx).project().read(cx);
                if !project.is_local() {
                    return None;
                }
                let has_root = project
                    .visible_worktrees(cx)
                    .any(|worktree| root.starts_with(worktree.read(cx).abs_path()));
                if !has_root {
                    return None;
                }
                let repository = project
                    .repositories(cx)
                    .values()
                    .find(|repository| *repository.read(cx).work_directory_abs_path == *root)?;
                Some((window, workspace.clone(), repository.clone()))
            })
    })
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct OpenReviewOptions {
    /// Ask before a pull request's checkout moves the repository (a link from outside Zed).
    pub(crate) confirm_checkout: bool,
}

/// Opens (or activates) the review `spec` names in `repository`, once its revisions resolve.
pub(crate) fn open_review(
    workspace: &mut Workspace,
    repository: Entity<Repository>,
    spec: TargetSpec,
    options: OpenReviewOptions,
    window: &mut Window,
    cx: &mut gpui::Context<Workspace>,
) -> Task<Result<Entity<ReviewView>>> {
    let workspace_handle = cx.weak_entity();
    let target: Task<Result<ReviewTarget>> = match spec {
        TargetSpec::Uncommitted => Task::ready(Ok(ReviewTarget::Uncommitted)),
        // A typed base is checked first, so a typo is an error rather than a tab explaining it.
        TargetSpec::Branch { base: Some(base) } => {
            let check = check_branch_base(&repository, base.clone(), cx);
            cx.spawn(async move |_, _| {
                check.await?;
                Ok(ReviewTarget::Branch { base })
            })
        }
        TargetSpec::Branch { base: None } => {
            let default_branch =
                repository.update(cx, |repository, _| repository.default_branch(true));
            cx.spawn(async move |_, _| {
                let base = default_branch
                    .await??
                    .context("No default branch: use branch:BASE")?;
                Ok(ReviewTarget::Branch {
                    base: BranchBase::new(base)?,
                })
            })
        }
        TargetSpec::Commits(spec) => {
            let resolve = resolve_commit_spec(&repository, spec, cx);
            cx.spawn(async move |_, _| Ok(ReviewTarget::Commits(resolve.await?)))
        }
        TargetSpec::PullRequest(reference) => {
            return open_pull_request(
                workspace,
                repository,
                reference,
                OpenPullRequestOptions {
                    in_new_worktree: false,
                    confirm_checkout: options.confirm_checkout,
                },
                window,
                cx,
            );
        }
    };
    window.spawn(cx, async move |cx| {
        let target = target.await?;
        workspace_handle.update_in(cx, |workspace, window, cx| {
            ReviewView::deploy_in_repo(workspace, target, repository, window, cx)
        })
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        comments::{CommentTarget, NewComment},
        init_test,
        submit::finish_local_review,
        test_support::dot_git,
    };
    use futures::channel::mpsc;
    use gpui::{ClipboardItem, TestAppContext, VisualTestContext};
    use project::Project;
    use serde_json::json;
    use util::path;
    use workspace::{SaveIntent, SplitDirection};

    struct TestSink(mpsc::UnboundedSender<CliResponse>);

    impl CliResponseSink for TestSink {
        fn send(&self, response: CliResponse) -> Result<()> {
            self.0
                .unbounded_send(response)
                .map_err(|error| anyhow!("{error}"))
        }
    }

    /// A `zed --review` command line whose request is being handled.
    pub(crate) struct Cli {
        responses: mpsc::UnboundedReceiver<CliResponse>,
        pings: usize,
        _handler: Task<()>,
    }

    impl Cli {
        pub(crate) fn run(
            request: CliReviewRequest,
            app_state: &Arc<AppState>,
            cx: &TestAppContext,
        ) -> Self {
            let (sender, responses) = mpsc::unbounded();
            let app_state = app_state.clone();
            let handler = cx.spawn(|mut cx| async move {
                handle_cli_review(request, Box::new(TestSink(sender)), app_state, &mut cx).await
            });
            cx.run_until_parked();
            Self {
                responses,
                pings: 0,
                _handler: handler,
            }
        }

        /// What the CLI received since the last call, pings counted separately.
        pub(crate) fn received(&mut self) -> Vec<String> {
            let mut received = Vec::new();
            while let Ok(response) = self.responses.try_recv() {
                match response {
                    CliResponse::Ping => self.pings += 1,
                    response => received.push(format!("{response:?}")),
                }
            }
            received
        }

        /// The CLI went away (killed, or its terminal closed).
        fn disconnect(&mut self) {
            self.responses.close();
        }
    }

    fn link(repo: &str, target: &str) -> String {
        let mut url = Url::parse("zed://review").unwrap();
        url.query_pairs_mut()
            .append_pair("repo", repo)
            .append_pair("target", target);
        url.to_string()
    }

    /// What `zed --review TARGET REPO` sends: the link and the repository's path.
    pub(crate) fn command_line(repo: &str, target: &str, wait: bool) -> CliReviewRequest {
        CliReviewRequest {
            url: link(repo, target),
            paths: vec![repo.to_string()],
            wait,
            open_options: OpenOptions::default(),
        }
    }

    /// A link opened from outside the CLI (a browser, through xdg-open).
    pub(crate) fn clicked_link(repo: &str, target: &str) -> CliReviewRequest {
        CliReviewRequest {
            paths: Vec::new(),
            ..command_line(repo, target, false)
        }
    }

    pub(crate) fn exit(status: i32) -> String {
        format!("{:?}", CliResponse::Exit { status })
    }

    fn stdout(message: &str) -> String {
        format!(
            "{:?}",
            CliResponse::Stdout {
                message: message.to_string()
            }
        )
    }

    pub(crate) fn stderr(message: &str) -> String {
        format!(
            "{:?}",
            CliResponse::Stderr {
                message: message.to_string()
            }
        )
    }

    /// A repository at `/project` with an uncommitted change in `src/a.rs`.
    async fn app_with_repository(cx: &mut TestAppContext) -> Arc<AppState> {
        init_test(cx);
        let app_state = cx.update(AppState::test);
        let fs = app_state.fs.as_fake();
        fs.insert_tree(
            path!("/project"),
            json!({ ".git": { "HEAD": "ref: refs/heads/main\n" }, "src": { "a.rs": "one\nTWO\n" } }),
        )
        .await;
        fs.set_head_and_index_for_repo(dot_git(), &[("src/a.rs", "one\ntwo\n".into())]);
        app_state
    }

    async fn open_window(
        app_state: &Arc<AppState>,
        root: &str,
        cx: &mut TestAppContext,
    ) -> WindowHandle<MultiWorkspace> {
        let project = Project::test(app_state.fs.clone(), [Path::new(root)], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        cx.run_until_parked();
        window
    }

    fn workspace_of(
        window: WindowHandle<MultiWorkspace>,
        cx: &mut TestAppContext,
    ) -> Entity<Workspace> {
        window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap()
    }

    fn reviews(workspace: &Entity<Workspace>, cx: &mut TestAppContext) -> Vec<Entity<ReviewView>> {
        workspace.read_with(cx, |workspace, cx| {
            workspace.items_of_type::<ReviewView>(cx).collect()
        })
    }

    fn workspace_windows_in(cx: &mut TestAppContext) -> Vec<WindowHandle<MultiWorkspace>> {
        cx.windows()
            .into_iter()
            .filter_map(|window| window.downcast::<MultiWorkspace>())
            .collect()
    }

    /// Closes the tab (and drops the test's handle on it).
    fn close(
        workspace: &Entity<Workspace>,
        review: Entity<ReviewView>,
        window: WindowHandle<MultiWorkspace>,
        cx: &mut TestAppContext,
    ) {
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let close = workspace.update_in(&mut cx, |workspace, window, cx| {
            let pane = workspace.pane_for(&review).unwrap();
            pane.update(cx, |pane, cx| {
                pane.close_item_by_id(review.entity_id(), SaveIntent::Skip, window, cx)
            })
        });
        cx.run_until_parked();
        drop((close, review));
        // The window's last frame holds on to what it painted (the tab's top block, through its
        // listeners) until the window draws again, as it does after a tab closes.
        window
            .update(&mut cx, |_, window, _| window.refresh())
            .unwrap();
        cx.run_until_parked();
    }

    fn split(
        workspace: &Entity<Workspace>,
        window: WindowHandle<MultiWorkspace>,
        cx: &mut TestAppContext,
    ) -> Entity<ReviewView> {
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let pane = workspace.read_with(&cx, |workspace, _| workspace.active_pane().clone());
        let split = workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.split_and_clone(pane, SplitDirection::Right, window, cx)
        });
        cx.run_until_parked();
        drop(split);
        workspace.read_with(&cx, |workspace, cx| {
            workspace.active_item_as::<ReviewView>(cx).unwrap()
        })
    }

    fn add_review_comment(review: &Entity<ReviewView>, body: &str, cx: &mut TestAppContext) {
        let session = review.read_with(cx, |review, _| review.session().clone());
        session.update(cx, |session, cx| {
            let id = session
                .add_comment(
                    NewComment {
                        target: CommentTarget::Review,
                        line_text: None,
                    },
                    cx,
                )
                .unwrap();
            session.set_comment_body(id, body, cx);
        });
    }

    fn finish(workspace: &Entity<Workspace>, review: &Entity<ReviewView>, cx: &mut TestAppContext) {
        workspace.update(cx, |workspace, cx| {
            finish_local_review(workspace, review, cx)
        });
        cx.run_until_parked();
    }

    fn notification_count(workspace: &Entity<Workspace>, cx: &mut TestAppContext) -> usize {
        workspace.update(cx, |workspace, cx| {
            let ids = workspace.notification_ids();
            for id in &ids {
                workspace.dismiss_notification(id, cx);
            }
            ids.len()
        })
    }

    #[test]
    fn test_parse_review_link() {
        let parse = |url: &str| ReviewLink::parse(url).map_err(|error| error.to_string());
        let parsed = |repo: &str, target: &str| {
            Ok(ReviewLink {
                repo: PathBuf::from(repo),
                target: target.to_string(),
            })
        };
        assert_eq!(
            parse("zed://review?repo=%2Fhome%2Fme%2Frepo&target=uncommitted"),
            parsed("/home/me/repo", "uncommitted")
        );
        assert_eq!(
            parse("zed://review/?target=branch%3Aorigin%2Fmain&repo=/a%20b&utm=x"),
            parsed("/a b", "branch:origin/main")
        );
        assert_eq!(
            parse("zed://review?repo=/r&target=o%2Fr%231"),
            parsed("/r", "o/r#1")
        );
        // Form encoding: `+` is a space, `%2B` a plus.
        assert_eq!(
            parse("zed://review?repo=/a+b%2Bc&target=uncommitted"),
            parsed("/a b+c", "uncommitted")
        );
        assert_eq!(
            parse("zed://review?repo=/r&target=pr:1&repo=/other"),
            parsed("/r", "pr:1")
        );
        let usage = "Use zed://review?repo=PATH&target=TARGET";
        assert_eq!(
            parse("zed://review?target=uncommitted"),
            Err(format!("Invalid review link: no repo. {usage}"))
        );
        assert_eq!(
            parse("zed://review?repo=&target=uncommitted"),
            Err(format!("Invalid review link: no repo. {usage}"))
        );
        assert_eq!(
            parse("zed://review?repo=/r"),
            Err(format!("Invalid review link: no target. {usage}"))
        );
        assert_eq!(
            parse("zed://review?repo=relative%2Fpath&target=uncommitted"),
            Err("Invalid review link: the repository path relative/path isn't absolute".into())
        );
        assert_eq!(
            parse("zed://review/x?repo=/r&target=uncommitted"),
            Err(format!(
                "Invalid review link: not a zed://review link. {usage}"
            ))
        );

        assert!(is_review_url("zed://review"));
        assert!(is_review_url("zed://review?repo=/r&target=x"));
        assert!(is_review_url("zed://review/?repo=/r&target=x"));
        assert!(!is_review_url("zed://reviewer"));
        assert!(!is_review_url("zed://settings"));
        assert!(!is_review_url("https://review"));
    }

    #[gpui::test]
    async fn test_repository_root(cx: &mut TestAppContext) {
        let fs = project::FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "project": {
                    ".git": { "HEAD": "ref: refs/heads/main\n" },
                    "src": { "a.rs": "" },
                    "nested": { ".git": { "HEAD": "ref: refs/heads/main\n" }, "deep": {} },
                    "not_a_repository": { ".git": {}, "x": {} },
                },
                "linked": { ".git": "gitdir: /root/project/.git/worktrees/linked", "x": {} },
                "plain": { "x": {} },
            }),
        )
        .await;
        let root = |path: &str| {
            let fs = fs.clone();
            let path = PathBuf::from(path);
            async move {
                repository_root(fs.as_ref(), &path)
                    .await
                    .map_err(|error| error.to_string())
            }
        };
        assert_eq!(
            root(path!("/root/project/src")).await,
            Ok(PathBuf::from(path!("/root/project")))
        );
        assert_eq!(
            root(path!("/root/project")).await,
            Ok(PathBuf::from(path!("/root/project")))
        );
        assert_eq!(
            root(path!("/root/project/nested/deep")).await,
            Ok(PathBuf::from(path!("/root/project/nested")))
        );
        assert_eq!(
            root(path!("/root/project/not_a_repository/x")).await,
            Ok(PathBuf::from(path!("/root/project")))
        );
        assert_eq!(
            root(path!("/root/linked/x")).await,
            Ok(PathBuf::from(path!("/root/linked")))
        );
        assert_eq!(
            root(path!("/root/plain/x")).await,
            Err(format!(
                "{} isn't in a git repository",
                path!("/root/plain/x")
            ))
        );
        assert_eq!(
            root(path!("/root/missing")).await,
            Err(format!("{} doesn't exist", path!("/root/missing")))
        );
        assert_eq!(
            root(path!("/root/project/src/a.rs")).await,
            Err(format!(
                "{} isn't a directory",
                path!("/root/project/src/a.rs")
            ))
        );
    }

    #[gpui::test]
    async fn test_find_review_workspace(cx: &mut TestAppContext) {
        let app_state = app_with_repository(cx).await;
        let fs = app_state.fs.as_fake();
        let dot_git = || json!({ "HEAD": "ref: refs/heads/main\n" });
        fs.insert_tree(path!("/other"), json!({ ".git": dot_git(), "b.rs": "" }))
            .await;
        fs.insert_tree(
            path!("/outer"),
            json!({ ".git": dot_git(), "inner": { ".git": dot_git(), "c.rs": "" } }),
        )
        .await;
        let find = |root: &str, cx: &mut TestAppContext| {
            cx.update(|cx| {
                find_review_workspace(Path::new(root), cx).map(|(window, workspace, repository)| {
                    (
                        window,
                        workspace,
                        repository.read(cx).work_directory_abs_path.to_path_buf(),
                    )
                })
            })
        };

        // A window on a folder of the repository can't show all of its files.
        open_window(&app_state, path!("/project/src"), cx).await;
        assert!(find(path!("/project"), cx).is_none());

        open_window(&app_state, path!("/other"), cx).await;
        let project_window = open_window(&app_state, path!("/project"), cx).await;
        open_window(&app_state, path!("/other"), cx).await;
        let (window, workspace, repository) = find(path!("/project"), cx).unwrap();
        assert_eq!(window, project_window);
        assert_eq!(workspace, workspace_of(project_window, cx));
        assert_eq!(repository, Path::new(path!("/project")));

        // A repository inside a bigger worktree.
        let outer_window = open_window(&app_state, path!("/outer"), cx).await;
        let (window, _, repository) = find(path!("/outer/inner"), cx).unwrap();
        assert_eq!(window, outer_window);
        assert_eq!(repository, Path::new(path!("/outer/inner")));
        assert!(find(path!("/missing"), cx).is_none());
    }

    #[gpui::test]
    async fn test_wait_until_closed(cx: &mut TestAppContext) {
        let app_state = app_with_repository(cx).await;
        let window = open_window(&app_state, path!("/project"), cx).await;
        let workspace = workspace_of(window, cx);

        let mut cli = Cli::run(
            command_line(path!("/project"), "uncommitted", true),
            &app_state,
            cx,
        );
        assert_eq!(workspace_windows_in(cx).len(), 1);
        let review = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<ReviewView>(cx)
            })
            .unwrap();
        assert_eq!(
            review.read_with(cx, |review, _| review.target().clone()),
            ReviewTarget::Uncommitted
        );
        assert_eq!(cli.received(), Vec::<String>::new());

        cx.executor().advance_clock(Duration::from_secs(2));
        cx.run_until_parked();
        assert_eq!(cli.received(), Vec::<String>::new());
        assert!(cli.pings >= 2);

        close(&workspace, review, window, cx);
        assert_eq!(
            cli.received(),
            [stderr("Review closed without submitting"), exit(2)]
        );
    }

    #[gpui::test]
    async fn test_finish_prints_markdown(cx: &mut TestAppContext) {
        let app_state = app_with_repository(cx).await;
        let window = open_window(&app_state, path!("/project"), cx).await;
        let workspace = workspace_of(window, cx);
        let clipboard = |cx: &mut TestAppContext| {
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .unwrap_or_default()
        };

        let mut cli = Cli::run(
            command_line(path!("/project"), "uncommitted", true),
            &app_state,
            cx,
        );
        let review = reviews(&workspace, cx).pop().unwrap();
        add_review_comment(&review, "Looks good", cx);
        let markdown = review
            .read_with(cx, |review, cx| review.review_markdown(cx))
            .unwrap();
        assert!(markdown.ends_with("Looks good\n"), "{markdown}");
        cx.write_to_clipboard(ClipboardItem::new_string("untouched".into()));
        finish(&workspace, &review, cx);
        assert_eq!(
            cli.received(),
            [stdout(markdown.strip_suffix('\n').unwrap()), exit(0)]
        );
        assert_eq!(clipboard(cx), "untouched");

        // Without a waiter, the review goes to the clipboard.
        finish(&workspace, &review, cx);
        assert_eq!(clipboard(cx), markdown);

        // No comments: nothing to print.
        let session = review.read_with(cx, |review, _| review.session().clone());
        let ids: Vec<_> = session.read_with(cx, |session, _| {
            session
                .comments()
                .iter()
                .map(|comment| comment.id)
                .collect()
        });
        session.update(cx, |session, cx| {
            for id in ids {
                session.delete_comment(id, cx);
            }
        });
        let mut cli = Cli::run(
            command_line(path!("/project"), "uncommitted", true),
            &app_state,
            cx,
        );
        assert_eq!(reviews(&workspace, cx).len(), 1);
        finish(&workspace, &review, cx);
        assert_eq!(cli.received(), [exit(0)]);
    }

    #[gpui::test]
    async fn test_submitted_prints_url(cx: &mut TestAppContext) {
        let app_state = app_with_repository(cx).await;
        let window = open_window(&app_state, path!("/project"), cx).await;
        let workspace = workspace_of(window, cx);
        let mut cli = Cli::run(
            command_line(path!("/project"), "uncommitted", true),
            &app_state,
            cx,
        );
        let session = reviews(&workspace, cx)
            .pop()
            .unwrap()
            .read_with(cx, |review, _| review.session().clone());
        let url = "https://github.com/o/r/pull/1#pullrequestreview-7";
        session.update(cx, |session, _| {
            session.resolve_outcome(ReviewOutcome::Submitted {
                url: url.to_string(),
            })
        });
        cx.run_until_parked();
        assert_eq!(cli.received(), [stdout(url), exit(0)]);
    }

    #[gpui::test]
    async fn test_opens_repository_without_window(cx: &mut TestAppContext) {
        let app_state = app_with_repository(cx).await;
        let mut cli = Cli::run(
            command_line(path!("/project/src"), "uncommitted", false),
            &app_state,
            cx,
        );
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(cli.received(), [exit(0)]);
        let windows = workspace_windows_in(cx);
        assert_eq!(windows.len(), 1);
        let workspace = workspace_of(windows[0], cx);
        let roots: Vec<PathBuf> = workspace.read_with(cx, |workspace, cx| {
            workspace
                .project()
                .read(cx)
                .visible_worktrees(cx)
                .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
                .collect()
        });
        assert_eq!(roots, [PathBuf::from(path!("/project"))]);
        assert_eq!(reviews(&workspace, cx).len(), 1);
    }

    #[gpui::test]
    async fn test_new_window_flag(cx: &mut TestAppContext) {
        let app_state = app_with_repository(cx).await;
        let window = open_window(&app_state, path!("/project"), cx).await;
        let mut cli = Cli::run(
            CliReviewRequest {
                open_options: OpenOptions {
                    workspace_matching: WorkspaceMatching::None,
                    add_dirs_to_sidebar: false,
                    ..OpenOptions::default()
                },
                ..command_line(path!("/project"), "uncommitted", false)
            },
            &app_state,
            cx,
        );
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(cli.received(), [exit(0)]);
        let windows = workspace_windows_in(cx);
        assert_eq!(windows.len(), 2);
        assert_eq!(reviews(&workspace_of(window, cx), cx).len(), 0);
        let new_window = windows.into_iter().find(|other| *other != window).unwrap();
        assert_eq!(reviews(&workspace_of(new_window, cx), cx).len(), 1);
    }

    #[gpui::test]
    async fn test_errors(cx: &mut TestAppContext) {
        let app_state = app_with_repository(cx).await;
        app_state
            .fs
            .as_fake()
            .insert_tree(path!("/plain"), json!({ "x": "" }))
            .await;
        let window = open_window(&app_state, path!("/project"), cx).await;
        let workspace = workspace_of(window, cx);
        notification_count(&workspace, cx);

        let project = path!("/project");
        let cases = [
            (
                command_line(project, "bogus", false),
                "Unknown review target \"bogus\". Use uncommitted, branch, branch:BASE, \
                 commit:REV, commits:A..B, pr:N, OWNER/REPO#N or a pull request URL."
                    .to_string(),
            ),
            (
                command_line(project, "123", true),
                "\"123\" isn't a review target: did you mean pr:123? (A commit is commit:123.)"
                    .to_string(),
            ),
            (
                command_line(path!("/plain"), "uncommitted", true),
                format!("{} isn't in a git repository", path!("/plain")),
            ),
            (
                command_line(path!("/missing"), "uncommitted", true),
                format!("{} doesn't exist", path!("/missing")),
            ),
            (
                command_line(project, "branch:-x", true),
                "Invalid base \"-x\"".to_string(),
            ),
        ];
        for (request, message) in cases {
            let mut cli = Cli::run(request, &app_state, cx);
            assert_eq!(cli.received(), [stderr(&message), exit(1)]);
            assert_eq!(reviews(&workspace, cx).len(), 0);
            // Nobody sees a link's errors in a terminal: they show in Zed too.
            assert_eq!(notification_count(&workspace, cx), 0, "{message}");
        }

        let mut cli = Cli::run(command_line(project, "commit:nope", true), &app_state, cx);
        let received = cli.received();
        assert_eq!(received.len(), 2, "{received:?}");
        assert!(received[0].starts_with("Stderr"), "{received:?}");
        assert_eq!(received[1], exit(1));
        assert_eq!(reviews(&workspace, cx).len(), 0);

        let mut cli = Cli::run(clicked_link(project, "bogus"), &app_state, cx);
        assert_eq!(cli.received().last(), Some(&exit(1)));
        assert_eq!(notification_count(&workspace, cx), 1);
        let mut cli = Cli::run(
            CliReviewRequest {
                url: "zed://review?repo=relative%2Fpath&target=uncommitted".into(),
                ..clicked_link(project, "uncommitted")
            },
            &app_state,
            cx,
        );
        assert_eq!(
            cli.received(),
            [
                stderr("Invalid review link: the repository path relative/path isn't absolute"),
                exit(1)
            ]
        );
        assert_eq!(notification_count(&workspace, cx), 1);
        let mut cli = Cli::run(
            clicked_link(path!("/missing"), "uncommitted"),
            &app_state,
            cx,
        );
        assert_eq!(cli.received().last(), Some(&exit(1)));
        assert_eq!(notification_count(&workspace, cx), 1);
        assert_eq!(reviews(&workspace, cx).len(), 0);
    }

    #[gpui::test]
    async fn test_split_and_two_waiters(cx: &mut TestAppContext) {
        let app_state = app_with_repository(cx).await;
        let window = open_window(&app_state, path!("/project"), cx).await;
        let workspace = workspace_of(window, cx);
        let request = || command_line(path!("/project"), "uncommitted", true);

        let mut first = Cli::run(request(), &app_state, cx);
        let mut second = Cli::run(request(), &app_state, cx);
        let original = reviews(&workspace, cx).pop().unwrap();
        assert_eq!(reviews(&workspace, cx).len(), 1);
        let clone = split(&workspace, window, cx);
        assert_ne!(clone, original);
        close(&workspace, original, window, cx);
        assert_eq!(first.received(), Vec::<String>::new());
        assert_eq!(second.received(), Vec::<String>::new());

        add_review_comment(&clone, "Both", cx);
        let markdown = clone
            .read_with(cx, |review, cx| review.review_markdown(cx))
            .unwrap();
        finish(&workspace, &clone, cx);
        let printed = [stdout(markdown.strip_suffix('\n').unwrap()), exit(0)];
        assert_eq!(first.received(), printed);
        assert_eq!(second.received(), printed);

        let mut third = Cli::run(request(), &app_state, cx);
        let mut fourth = Cli::run(request(), &app_state, cx);
        let other_clone = split(&workspace, window, cx);
        close(&workspace, clone, window, cx);
        assert_eq!(third.received(), Vec::<String>::new());
        close(&workspace, other_clone, window, cx);
        let closed = [stderr("Review closed without submitting"), exit(2)];
        assert_eq!(third.received(), closed);
        assert_eq!(fourth.received(), closed);
    }

    #[gpui::test]
    async fn test_quit_and_lost_cli(cx: &mut TestAppContext) {
        let app_state = app_with_repository(cx).await;
        let window = open_window(&app_state, path!("/project"), cx).await;
        let workspace = workspace_of(window, cx);

        // The CLI goes away: the review isn't waited for any more.
        let mut lost = Cli::run(
            command_line(path!("/project"), "uncommitted", true),
            &app_state,
            cx,
        );
        let review = reviews(&workspace, cx).pop().unwrap();
        let session = review.read_with(cx, |review, _| review.session().clone());
        lost.disconnect();
        cx.executor().advance_clock(Duration::from_secs(2));
        cx.run_until_parked();
        assert!(!session.update(cx, |session, _| session.has_outcome_waiters()));
        add_review_comment(&review, "Copied", cx);
        finish(&workspace, &review, cx);
        let clipboard = cx.read_from_clipboard().and_then(|item| item.text());
        assert!(clipboard.unwrap().ends_with("Copied\n"));

        let mut waiting = Cli::run(
            command_line(path!("/project"), "uncommitted", true),
            &app_state,
            cx,
        );
        drop((review, session));
        cx.quit();
        cx.run_until_parked();
        assert_eq!(
            waiting.received(),
            [stderr("Zed quit before the review was submitted"), exit(2)]
        );
    }
}
