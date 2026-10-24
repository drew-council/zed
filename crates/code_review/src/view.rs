use crate::{
    AddComment, AddFileComment, AddReviewComment, ChangeBase, CopyCommentAtCursor,
    CopyReviewAsMarkdown, CopySelectedLines, DeleteComment, EditComment, FocusDiff, NextComment,
    NextFile, NextHunk, NextPullRequestInStack, NextUnreviewedFile, OpenFileAtCursor,
    OpenPullRequestInBrowser, PreviousComment, PreviousFile, PreviousHunk,
    PreviousPullRequestInStack, PreviousUnreviewedFile, ReloadPullRequest, ReviewBranch,
    ReviewCommits, ReviewPanel, ReviewUncommitted, SubmitReview, ToggleBotReviews,
    ToggleFileReviewed, ToggleHeader, ToggleHunkReviewed, ToggleOutdatedThreads,
    ToggleResolvedThreads, ToggleSingleFileMode,
    comments::{
        CommentBlocks, CommentBlocksEvent, CommentId, CommentTarget, CommentType, ItemKey,
        RemoteItem, RemoteKey, RemoteThreadContent, Side, keep_scroll_at_start, open_web_link,
        stay_at_start, thread_location,
    },
    commits::{
        CommitDiffList, TOP_BLOCK_COMMIT_LIMIT, check_branch_base, first_parent_chain,
        resolve_commit_spec,
    },
    context_menu,
    diff::{ReviewDiff, ReviewDiffEvent, review_path_key},
    export::{self, ExportPreamble},
    forge::StackDirection,
    gitattributes::{FileAttributes, GitAttributes},
    picker::{StartReviewFocus, StartReviewModal},
    pull_request::{
        LineMapping, LocalChanges, LocalChangesEvent, PullRequestEvent, PullRequestState,
        ThreadFilters, ThreadPlacement, open_stack_neighbor, place_thread,
    },
    reviewed::paint_reviewed_hunks,
    session::{ReviewKey, ReviewSession, ReviewSessionEvent},
    submit::{SubmitReviewModal, finish_local_review},
    target::{BranchBase, CommitRange, ReviewTarget, parse_commit_spec},
    top_block::{CommitsState, ReviewTopBlock, TopBlockContent},
};
use collections::{HashMap, HashSet};
use editor::ToPoint as _;
use editor::{
    Bias, Editor, EditorEvent, PathKey, SelectionEffects, SplittableEditor,
    actions::{OpenExcerpts, ReloadFile, ToggleSoftWrap},
    display_map::CustomBlockId,
    scroll::Autoscroll,
};
use git::{Oid, repository::RepoPath, status::FileStatus};
use git_ui::branch_picker::{self, SelectBranchCallback};
use gpui::{
    AnyEntity, ClipboardItem, Entity, EntityId, EventEmitter, FocusHandle, Focusable, Subscription,
    Task, WeakEntity,
};
use language::{Capability, LanguageRegistry, Point, language_settings::SoftWrap};
use multi_buffer::MultiBufferRow;
use project::{
    Project, ProjectPath,
    git_store::{GitStoreEvent, Repository, RepositoryEvent},
};
use regex::{Regex, RegexBuilder};
use std::{
    any::{Any, TypeId},
    collections::BTreeMap,
    sync::Arc,
};
use ui::prelude::*;
use util::{ResultExt as _, truncate_and_trailoff};
use workspace::{
    ItemNavHistory, Toast, Workspace,
    item::{Item, ItemEvent, SaveOptions},
    notifications::{NotificationId, NotifyResultExt as _, NotifyTaskExt as _},
    searchable::SearchableItemHandle,
};

pub(crate) fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &ReviewUncommitted, window, cx| {
        ReviewView::deploy(workspace, ReviewTarget::Uncommitted, window, cx);
    });
    workspace.register_action(|workspace, action: &ReviewBranch, window, cx| {
        review_branch(workspace, action.base.clone(), window, cx);
    });
    workspace.register_action(|workspace, action: &ReviewCommits, window, cx| {
        match &action.commits {
            Some(commits) => review_commits(workspace, commits, window, cx),
            None => StartReviewModal::toggle(workspace, StartReviewFocus::Commits, window, cx),
        }
    });
    workspace.register_action(|workspace, _: &FocusDiff, window, cx| {
        focus_diff(workspace, window, cx);
    });
    workspace.register_action(|workspace, _: &CopyReviewAsMarkdown, window, cx| {
        discard_vim_count(window, cx);
        if let Some(review) = current_review(workspace, cx) {
            copy_review_as_markdown(workspace, &review, cx);
        }
    });
    workspace.register_action(|workspace, _: &ToggleResolvedThreads, _, cx| {
        update_thread_filters(workspace, cx, |filters| {
            filters.show_resolved = !filters.show_resolved
        });
    });
    workspace.register_action(|workspace, _: &ToggleOutdatedThreads, _, cx| {
        update_thread_filters(workspace, cx, |filters| {
            filters.show_outdated = !filters.show_outdated
        });
    });
    workspace.register_action(|workspace, _: &ToggleBotReviews, _, cx| {
        update_thread_filters(workspace, cx, |filters| {
            filters.show_bots = !filters.show_bots
        });
    });
    workspace.register_action(|workspace, _: &ReloadPullRequest, _, cx| {
        if let Some(pull_request) = current_pull_request(workspace, cx) {
            pull_request.update(cx, |pull_request, cx| pull_request.reload(cx));
        }
    });
    workspace.register_action(|workspace, _: &SubmitReview, window, cx| {
        discard_vim_count(window, cx);
        // Only the review in front of the user: submitting posts it, finishing a local one
        // overwrites the clipboard or ends `zed --review --wait`.
        let Some(review) = workspace.active_item_as::<ReviewView>(cx) else {
            workspace.show_toast(
                Toast::new(
                    NotificationId::unique::<SubmitReview>(),
                    "Switch to the review tab to submit its review",
                )
                .autohide(),
                cx,
            );
            return;
        };
        submit_review(workspace, review, window, cx);
    });
    workspace.register_action(|workspace, _: &NextPullRequestInStack, window, cx| {
        discard_vim_count(window, cx);
        if let Some(review) = current_review(workspace, cx) {
            open_stack_neighbor(workspace, review, StackDirection::Up, window, cx);
        }
    });
    workspace.register_action(|workspace, _: &PreviousPullRequestInStack, window, cx| {
        discard_vim_count(window, cx);
        if let Some(review) = current_review(workspace, cx) {
            open_stack_neighbor(workspace, review, StackDirection::Down, window, cx);
        }
    });
    workspace.register_action(|workspace, _: &OpenPullRequestInBrowser, window, cx| {
        if let Some(pull_request) = current_pull_request(workspace, cx) {
            let url = pull_request.read(cx).details().url.clone();
            open_web_link(&url, window, cx);
        }
    });
}

/// `; y`: the review's comments as Markdown, on the clipboard.
pub(crate) fn copy_review_as_markdown(
    workspace: &mut Workspace,
    review: &Entity<ReviewView>,
    cx: &mut Context<Workspace>,
) {
    let message = match review.read(cx).review_markdown(cx) {
        Some(markdown) => {
            cx.write_to_clipboard(ClipboardItem::new_string(markdown));
            "Review copied to clipboard"
        }
        None => "No comments to copy",
    };
    workspace.show_toast(
        Toast::new(NotificationId::unique::<CopyReviewAsMarkdown>(), message).autohide(),
        cx,
    );
}

/// `; s`: opens the submit modal of a pull request review, or finishes a local review.
pub(crate) fn submit_review(
    workspace: &mut Workspace,
    review: Entity<ReviewView>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    match review.read(cx).pull_request().cloned() {
        Some(pull_request) => SubmitReviewModal::open(workspace, review, pull_request, window, cx),
        None => finish_local_review(workspace, &review, cx),
    }
}

/// The pull request of the current review, or a toast saying it isn't a pull request review.
pub(crate) fn current_pull_request(
    workspace: &mut Workspace,
    cx: &mut Context<Workspace>,
) -> Option<Entity<PullRequestState>> {
    let pull_request =
        current_review(workspace, cx).and_then(|review| review.read(cx).pull_request().cloned());
    if pull_request.is_none() {
        workspace.show_toast(
            Toast::new(
                NotificationId::unique::<PullRequestState>(),
                "Open a pull request review first",
            )
            .autohide(),
            cx,
        );
    }
    pull_request
}

/// Changes which threads the current pull request review shows, in all its tabs.
fn update_thread_filters(
    workspace: &mut Workspace,
    cx: &mut Context<Workspace>,
    update: impl FnOnce(&mut ThreadFilters),
) {
    if let Some(pull_request) = current_pull_request(workspace, cx) {
        pull_request.update(cx, |pull_request, cx| {
            let mut filters = pull_request.filters();
            update(&mut filters);
            pull_request.set_filters(filters, cx);
        });
    }
}

/// The repository reviews act on, or a toast saying there is none.
pub(crate) fn active_repository(
    workspace: &mut Workspace,
    cx: &mut Context<Workspace>,
) -> Option<Entity<Repository>> {
    let repo = workspace.project().read(cx).active_repository(cx);
    if repo.is_none() {
        workspace.show_toast(
            Toast::new(
                NotificationId::unique::<ReviewView>(),
                "Code review needs a git repository",
            )
            .autohide(),
            cx,
        );
    }
    repo
}

fn review_branch(
    workspace: &mut Workspace,
    base: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(repo) = active_repository(workspace, cx) else {
        return;
    };
    let workspace_handle = cx.weak_entity();
    let task = match base {
        // A base typed into a keymap is checked before the review opens, so a typo is an error
        // rather than a tab explaining it.
        Some(base) => {
            let check = BranchBase::new(base)
                .map(|base| (base.clone(), check_branch_base(&repo, base, cx)));
            window.spawn(cx, {
                let workspace_handle = workspace_handle.clone();
                async move |cx| {
                    let (base, check) = check?;
                    check.await?;
                    workspace_handle.update_in(cx, |workspace, window, cx| {
                        ReviewView::deploy_in_repo(
                            workspace,
                            ReviewTarget::Branch { base },
                            repo,
                            window,
                            cx,
                        );
                    })
                }
            })
        }
        None => {
            let default_branch = repo.update(cx, |repo, _| repo.default_branch(true));
            window.spawn(cx, {
                let workspace_handle = workspace_handle.clone();
                async move |cx| {
                    let base = default_branch.await??.map(BranchBase::new).transpose()?;
                    workspace_handle.update_in(cx, |workspace, window, cx| match base {
                        Some(base) => {
                            ReviewView::deploy_in_repo(
                                workspace,
                                ReviewTarget::Branch { base },
                                repo,
                                window,
                                cx,
                            );
                        }
                        None => pick_base_for_new_review(workspace, repo, None, window, cx),
                    })
                }
            })
        }
    };
    task.detach_and_notify_err(workspace_handle, window, cx);
}

fn review_commits(
    workspace: &mut Workspace,
    commits: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(repo) = active_repository(workspace, cx) else {
        return;
    };
    let workspace_handle = cx.weak_entity();
    let resolve = parse_commit_spec(commits).map(|spec| resolve_commit_spec(&repo, spec, cx));
    let task = window.spawn(cx, {
        let workspace_handle = workspace_handle.clone();
        async move |cx| {
            let range = resolve?.await?;
            workspace_handle.update_in(cx, |workspace, window, cx| {
                ReviewView::deploy_in_repo(
                    workspace,
                    ReviewTarget::Commits(range),
                    repo,
                    window,
                    cx,
                );
            })
        }
    });
    task.detach_and_notify_err(workspace_handle, window, cx);
}

/// Opens Zed's branch picker; the chosen branch becomes the base of a new branch review.
pub(crate) fn pick_base_for_new_review(
    workspace: &mut Workspace,
    repo: Entity<Repository>,
    selected_base: Option<SharedString>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let workspace_handle = workspace.weak_handle();
    let on_select: SelectBranchCallback = Arc::new({
        let workspace_handle = workspace_handle.clone();
        let repo = repo.clone();
        move |branch, window, cx| {
            let target = ReviewTarget::branch(branch.name().to_owned());
            workspace_handle
                .update(cx, |workspace, cx| {
                    if let Some(target) = target.notify_err(workspace, cx) {
                        ReviewView::deploy_in_repo(workspace, target, repo.clone(), window, cx);
                    }
                })
                .log_err();
        }
    });
    workspace.toggle_modal(window, cx, |window, cx| {
        branch_picker::select_modal(
            workspace_handle,
            Some(repo),
            selected_base,
            on_select,
            window,
            cx,
        )
    });
}

/// The active review tab, else the review the panel shows, else any open review.
pub(crate) fn current_review(workspace: &Workspace, cx: &App) -> Option<Entity<ReviewView>> {
    workspace
        .active_item_as::<ReviewView>(cx)
        .or_else(|| {
            workspace
                .panel::<ReviewPanel>(cx)
                .and_then(|panel| panel.read(cx).active_review())
        })
        .or_else(|| workspace.items_of_type::<ReviewView>(cx).next())
}

/// Focuses the active review tab, else the review the panel shows, else any open review.
pub(crate) fn focus_diff(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(review) = current_review(workspace, cx) {
        workspace.activate_item(&review, true, true, window, cx);
    }
}

fn find_review(
    workspace: &Workspace,
    target: &ReviewTarget,
    repo: &Entity<Repository>,
    cx: &App,
) -> Option<Entity<ReviewView>> {
    workspace
        .items_of_type::<ReviewView>(cx)
        .find(|view| view.read(cx).reviews(target, repo))
}

/// The export's line about a commit review: its commits (tuicr's), or for a long range (or one
/// whose commits aren't all loaded) how many there are and its ends.
fn commits_banner(shas: Option<&[String]>, range: &CommitRange) -> String {
    const MAX_LISTED: usize = 10;
    let ends = || match range.base {
        Some(base) => format!("{}..{}", base.display_short(), range.head.display_short()),
        None => format!("up to {}", range.head.display_short()),
    };
    match shas {
        Some([commit]) => format!("Reviewing commit: {commit}"),
        Some(commits) if commits.len() <= MAX_LISTED => {
            format!("Reviewing commits: {}", commits.join(", "))
        }
        Some(commits) => format!("Reviewing {} commits: {}", commits.len(), ends()),
        None => match range.base {
            Some(_) => format!("Reviewing commits: {}", ends()),
            None => format!("Reviewing commits {}", ends()),
        },
    }
}
fn with_repository_name(name: Option<SharedString>, text: SharedString) -> SharedString {
    match name {
        Some(name) => format!("{name} \u{b7} {text}").into(),
        None => text,
    }
}

fn branch_name(repo: &Entity<Repository>, cx: &App) -> SharedString {
    repo.read(cx)
        .branch
        .as_ref()
        .map(|branch| SharedString::from(branch.name().to_owned()))
        .unwrap_or_else(|| "HEAD".into())
}

/// Vim keeps a pending count after actions that aren't its own, where it would multiply the next
/// motion. The review's own actions don't take counts.
fn discard_vim_count(window: &mut Window, cx: &mut App) {
    if let Some(action) = cx.build_action("vim::ClearOperators", None).log_err() {
        window.dispatch_action(action, cx);
    }
}

/// The review is read-only, but the editor handles staging (which also saves the dirty buffers
/// under the cursor) and reloading (which drops the unsaved edits of every buffer in the review)
/// without checking that. Vim's `d u` and `:e!` dispatch these too.
fn block_write_action(window: &mut Window, cx: &mut App) {
    cx.stop_propagation();
    discard_vim_count(window, cx);
}

/// The repository's HEAD commit, as Zed last saw it.
pub(crate) fn head_oid(repo: &Entity<Repository>, cx: &App) -> Option<Oid> {
    repo.read(cx)
        .head_commit
        .as_ref()
        .and_then(|commit| commit.sha.parse().ok())
}

/// The pull request's threads the filters let through, where a review with `local_head` checked
/// out, whose files differ from it as `local` says, shows them: below their lines, or in their
/// file's item of outdated and moved threads (after the inline ones, by path). New lines are
/// numbered in HEAD on GitHub: a thread on lines changed in the working tree is moved, and one
/// whose file hasn't been compared with HEAD yet waits for that.
fn remote_items(
    pull_request: &Entity<PullRequestState>,
    local_head: Option<Oid>,
    local: &Entity<LocalChanges>,
    cx: &App,
) -> Vec<RemoteItem> {
    let pull_request = pull_request.read(cx);
    let local = local.read(cx);
    let repo = &pull_request.forge().context().canonical;
    let filters = pull_request.filters();
    let commentable = pull_request.commentable();
    let mut items = Vec::new();
    let mut moved = BTreeMap::<RepoPath, Vec<RemoteThreadContent>>::new();
    for thread in pull_request.threads() {
        let placement = match place_thread(thread, local_head, commentable, filters) {
            Some(ThreadPlacement::Inline(CommentTarget::Lines {
                path,
                side: Side::New,
                lines,
            })) => match local.head_to_working(&path, lines, cx) {
                LineMapping::Lines(lines) => Some(ThreadPlacement::Inline(CommentTarget::Lines {
                    path,
                    side: Side::New,
                    lines,
                })),
                LineMapping::Changed => {
                    let location = thread_location(thread, true)
                        .map(|location| format!("{location} · changed locally").into());
                    moved.entry(path).or_default().push(RemoteThreadContent {
                        thread: thread.clone(),
                        location,
                        repo: repo.clone(),
                    });
                    continue;
                }
                LineMapping::Unknown | LineMapping::Failed => continue,
            },
            placement => placement,
        };
        match placement {
            None => {}
            Some(ThreadPlacement::Inline(target)) => items.push(RemoteItem {
                key: RemoteKey::Thread(thread.id.clone()),
                target,
                title: None,
                threads: vec![RemoteThreadContent {
                    thread: thread.clone(),
                    location: thread_location(thread, false),
                    repo: repo.clone(),
                }],
            }),
            Some(ThreadPlacement::Moved { path }) => {
                moved.entry(path).or_default().push(RemoteThreadContent {
                    thread: thread.clone(),
                    location: thread_location(thread, true),
                    repo: repo.clone(),
                })
            }
        }
    }
    items.extend(moved.into_iter().map(|(path, threads)| RemoteItem {
        key: RemoteKey::Moved(path.clone()),
        target: CommentTarget::File { path },
        title: Some(format!("Outdated / moved threads ({})", threads.len()).into()),
        threads,
    }));
    items
}

/// A file as the review panel lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PanelFile {
    pub path_key: PathKey,
    pub repo_path: RepoPath,
    pub status: FileStatus,
    pub reviewed: bool,
    pub attributes: FileAttributes,
}

/// A comment, or a thread of the pull request, as the review panel lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PanelComment {
    pub key: PanelCommentKey,
    /// What Enter lands on: a file's outdated and moved threads share one block.
    pub item: ItemKey,
    pub is_review: bool,
    pub comment_type: Option<CommentType>,
    pub location: SharedString,
    pub first_line: SharedString,
    pub outdated: bool,
    pub submitted: bool,
    /// A thread on GitHub (read-only).
    pub remote: bool,
    pub resolved: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum PanelCommentKey {
    Comment(CommentId),
    Thread(SharedString),
}

/// Which files a review tab shows. Per tab and not persisted (like tuicr's filters).
#[derive(Clone, Default)]
pub(crate) struct ReviewFilters {
    /// Only paths matching this (case-insensitive) regex are shown.
    pub include: Option<Regex>,
    /// Paths matching this (case-insensitive) regex are hidden.
    pub exclude: Option<Regex>,
    pub hide_reviewed: bool,
    /// Generated and vendored files (per `.gitattributes`) are hidden unless shown.
    pub show_generated: bool,
    pub show_vendored: bool,
}

impl ReviewFilters {
    fn passes_patterns(&self, repo_path: &RepoPath) -> bool {
        let path = repo_path.as_unix_str();
        self.include
            .as_ref()
            .is_none_or(|include| include.is_match(path))
            && self
                .exclude
                .as_ref()
                .is_none_or(|exclude| !exclude.is_match(path))
    }

    fn hides_attributes(&self, attributes: FileAttributes) -> bool {
        (attributes.generated && !self.show_generated)
            || (attributes.vendored && !self.show_vendored)
    }
}

/// Builds a filter pattern; an empty pattern clears the filter.
pub(crate) fn filter_regex(pattern: &str) -> anyhow::Result<Option<Regex>> {
    if pattern.is_empty() {
        return Ok(None);
    }
    match RegexBuilder::new(pattern).case_insensitive(true).build() {
        Ok(regex) => Ok(Some(regex)),
        Err(error) => Err(anyhow::anyhow!(
            "Invalid regex: {}",
            regex_error_summary(&error)
        )),
    }
}

/// A syntax error spans several lines (the pattern with a caret under the problem, then what is
/// wrong), which a one-line prompt can't show.
fn regex_error_summary(error: &regex::Error) -> String {
    match error {
        regex::Error::Syntax(message) => message
            .lines()
            .rev()
            .find_map(|line| line.strip_prefix("error: "))
            .unwrap_or(message)
            .trim()
            .to_string(),
        error => error.to_string(),
    }
}

/// What the review panel says about the review's files.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReviewSummary {
    /// Files that pass the filters, reviewed or not.
    pub total: usize,
    pub reviewed: usize,
    /// Files hidden by the include and exclude patterns.
    pub filtered_out: usize,
    /// Generated files hidden by default (and not by the patterns).
    pub generated_hidden: usize,
    /// Vendored files hidden by default (and not by the patterns).
    pub vendored_hidden: usize,
    /// Reviewed files hidden by "hide reviewed".
    pub reviewed_hidden: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Next,
    Previous,
}

/// The review tab: a read-only diff of a [`ReviewTarget`] in one repository.
pub struct ReviewView {
    target: ReviewTarget,
    repo: Entity<Repository>,
    diff: Entity<ReviewDiff>,
    top_block: Entity<ReviewTopBlock>,
    /// The overview's block at the top of the diff's right side, while the overview has
    /// something to show.
    top_block_id: Option<CustomBlockId>,
    /// Shared by every tab reviewing the same thing; replaced when the review key changes.
    session: Entity<ReviewSession>,
    /// The sessions of the keys this tab reviewed before (another base, another branch), kept
    /// while the tab is open so that switching back finds their comments as they were (their
    /// anchors are parked in [`CommentBlocks`]).
    previous_sessions: HashMap<ReviewKey, Entity<ReviewSession>>,
    comments: Entity<CommentBlocks>,
    /// The pull request a pull request review is of, shared with its other tabs.
    pull_request: Option<Entity<PullRequestState>>,
    /// How this tab's working tree differs from HEAD (pull request reviews), which GitHub's line
    /// numbers count in.
    local_changes: Option<Entity<LocalChanges>>,
    /// The commit checked out when the pull request's threads were last placed.
    checked_out: Option<Oid>,
    filters: ReviewFilters,
    /// The generated and vendored files among the review's files.
    attributes: HashMap<RepoPath, FileAttributes>,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    /// The left side is recreated on every split; its reviewed hunks are repainted then.
    lhs_editor_id: Option<EntityId>,
    _load_commits_task: Task<()>,
    _attributes_task: Task<()>,
    _session_subscription: Subscription,
    _subscriptions: Vec<Subscription>,
}

impl ReviewView {
    /// Activates the open review of `target` in the active repository, or opens a new one.
    pub fn deploy(
        workspace: &mut Workspace,
        target: ReviewTarget,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Option<Entity<Self>> {
        let repo = active_repository(workspace, cx)?;
        Some(Self::deploy_in_repo(workspace, target, repo, window, cx))
    }

    /// Activates the open review of `target` in `repo`, or opens a new one. Reviews of other
    /// targets stay open in their own tabs. Either way the review panel shows in the left dock,
    /// while the diff takes the focus.
    pub fn deploy_in_repo(
        workspace: &mut Workspace,
        target: ReviewTarget,
        repo: Entity<Repository>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        // Only here, where a review is opened: switching between review tabs (or anything that
        // refreshes one) leaves the dock as the user left it.
        workspace.open_panel::<ReviewPanel>(window, cx);
        if let Some(existing) = find_review(workspace, &target, &repo, cx) {
            // A pull request's base may have moved since its tab opened.
            existing.update(cx, |view, cx| view.update_pull_request_base(&target, cx));
            workspace.activate_item(&existing, true, true, window, cx);
            return existing;
        }

        let project = workspace.project().clone();
        let workspace_handle = cx.entity();
        let view =
            cx.new(|cx| Self::new(target, repo, project, None, workspace_handle, window, cx));
        workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
        view
    }

    fn new(
        target: ReviewTarget,
        repo: Entity<Repository>,
        project: Entity<Project>,
        commits: Option<Entity<CommitDiffList>>,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let view = cx.weak_entity();
        let diff = cx.new(|cx| {
            ReviewDiff::new(
                &target,
                repo.clone(),
                project.clone(),
                commits,
                workspace.clone(),
                view,
                window,
                cx,
            )
        });
        let key = target.review_key(&repo.read(cx));
        let pull_request = match &target {
            ReviewTarget::PullRequest(_) => PullRequestState::for_key(&key, cx),
            ReviewTarget::Uncommitted | ReviewTarget::Branch { .. } | ReviewTarget::Commits(_) => {
                None
            }
        };
        let content = match &target {
            ReviewTarget::Uncommitted => TopBlockContent::None,
            ReviewTarget::Branch { base } => TopBlockContent::Branch {
                branch: branch_name(&repo, cx),
                base: base.name().clone(),
            },
            ReviewTarget::Commits(range) => TopBlockContent::Commits {
                range: *range,
                commits: CommitsState::Loading,
            },
            ReviewTarget::PullRequest(target) => TopBlockContent::PullRequest {
                repo: target.repo.clone(),
                number: target.number,
                state: pull_request.clone(),
            },
        };
        let session = ReviewSession::for_key(key, cx);
        session.update(cx, |session, _| session.show());
        let comments = cx.new(|cx| {
            CommentBlocks::new(
                session.clone(),
                diff.clone(),
                workspace.downgrade(),
                project.read(cx).languages().clone(),
                window,
                cx,
            )
        });
        let view = cx.weak_entity();
        let language_registry = project.read(cx).languages().clone();
        let top_block = cx.new(|cx| {
            ReviewTopBlock::new(
                content,
                view,
                diff.clone(),
                comments.clone(),
                language_registry,
                cx,
            )
        });
        let load_commits_task = match &target {
            ReviewTarget::Commits(range) => {
                let chain = first_parent_chain(
                    &repo,
                    Some(range.head),
                    range.base,
                    TOP_BLOCK_COMMIT_LIMIT,
                    cx,
                );
                let language_registry = project.read(cx).languages().clone();
                cx.spawn(async move |this, cx| {
                    let chain = chain.await;
                    if let Err(error) = &chain {
                        log::error!("loading the commits of a review: {error:#}");
                    }
                    this.update(cx, |this, cx| {
                        this.top_block.update(cx, |top_block, cx| {
                            top_block.set_commits(chain, language_registry, cx)
                        });
                        cx.emit(EditorEvent::TitleChanged);
                        cx.notify();
                    })
                    .log_err();
                })
            }
            ReviewTarget::Uncommitted
            | ReviewTarget::Branch { .. }
            | ReviewTarget::PullRequest(_) => Task::ready(()),
        };

        let git_store = project.read(cx).git_store().clone();
        let mut subscriptions = vec![
            cx.observe_in(&top_block, window, |this, _, window, cx| {
                this.sync_top_block_host(window, cx)
            }),
            // `zed --review --wait` ends once no tab shows the review.
            cx.on_release(|this, cx| this.session.update(cx, |session, _| session.hide(None))),
            cx.subscribe(&diff, |_, _, event: &EditorEvent, cx| {
                cx.emit(event.clone())
            }),
            cx.subscribe_in(&diff, window, Self::handle_diff_event),
            // The review panel shows the diff's loading state.
            cx.observe(&diff, |_, _, cx| cx.notify()),
            // The tab is writable while a comment is edited, for `:w`.
            cx.subscribe(&comments, |_, _, _: &CommentBlocksEvent, cx| {
                cx.emit(EditorEvent::TitleChanged);
                cx.notify();
            }),
            // Titles name the repository once the project has several (the panel header too).
            cx.subscribe(&git_store, |_, _, event: &GitStoreEvent, cx| {
                if matches!(
                    event,
                    GitStoreEvent::RepositoryAdded | GitStoreEvent::RepositoryRemoved(_)
                ) {
                    cx.emit(EditorEvent::TitleChanged);
                    cx.notify();
                }
            }),
            // A branch review is titled and keyed after the checked-out branch. The merge base of
            // a branch or pull request review depends on HEAD.
            cx.subscribe_in(
                &repo,
                window,
                |this, repo, event: &RepositoryEvent, window, cx| {
                    if !matches!(event, RepositoryEvent::HeadChanged) {
                        return;
                    }
                    match &this.target {
                        ReviewTarget::Branch { base } => {
                            let branch = branch_name(repo, cx);
                            let base = base.name().clone();
                            this.top_block
                                .update(cx, |top_block, cx| top_block.set_branch(branch, base, cx));
                            this.diff.update(cx, |diff, cx| diff.check_base(cx));
                            this.update_session(window, cx);
                        }
                        // Threads are placed by the checked-out commit, and the pull request may
                        // have moved on too. (The branch's upstream changing doesn't count.)
                        ReviewTarget::PullRequest(_) => {
                            let head = head_oid(repo, cx);
                            if head == this.checked_out {
                                return;
                            }
                            this.checked_out = head;
                            this.diff.update(cx, |diff, cx| diff.check_base(cx));
                            this.update_remote_items(window, cx);
                            if let Some(pull_request) = &this.pull_request {
                                pull_request.update(cx, |pull_request, cx| pull_request.reload(cx));
                            }
                        }
                        ReviewTarget::Uncommitted | ReviewTarget::Commits(_) => return,
                    }
                    cx.emit(EditorEvent::TitleChanged);
                    cx.notify();
                },
            ),
        ];
        let checked_out = head_oid(&repo, cx);
        let local_changes = pull_request.as_ref().map(|pull_request| {
            pull_request.update(cx, |pull_request, _| {
                pull_request.add_checkout(&repo, &project)
            });
            let local_changes = cx.new(|_| LocalChanges::new(&project));
            subscriptions.push(cx.observe(pull_request, |_, _, cx| {
                cx.emit(EditorEvent::TitleChanged);
                cx.notify();
            }));
            subscriptions.push(cx.subscribe_in(
                pull_request,
                window,
                |this, _, _: &PullRequestEvent, window, cx| this.update_remote_items(window, cx),
            ));
            subscriptions.push(cx.subscribe_in(
                &local_changes,
                window,
                |this, _, _: &LocalChangesEvent, window, cx| this.update_remote_items(window, cx),
            ));
            let gate = {
                let pull_request = pull_request.downgrade();
                let repo = repo.downgrade();
                let local_changes = local_changes.downgrade();
                move |target: &CommentTarget, cx: &App| {
                    let (Some(pull_request), Some(local_changes)) =
                        (pull_request.upgrade(), local_changes.upgrade())
                    else {
                        return Ok(());
                    };
                    let local_head = repo.upgrade().and_then(|repo| head_oid(&repo, cx));
                    pull_request.read(cx).check_comment(
                        target,
                        local_head,
                        local_changes.read(cx),
                        cx,
                    )
                }
            };
            let items = remote_items(pull_request, checked_out, &local_changes, cx);
            comments.update(cx, |comments, cx| {
                comments.set_gate(Some(Arc::new(gate)));
                comments.set_remote_items(items, window, cx)
            });
            local_changes
        });
        let splittable = diff.read(cx).editor().clone();
        subscriptions.extend([
            cx.subscribe_in(
                &splittable,
                window,
                |this, _, event: &EditorEvent, window, cx| match event {
                    EditorEvent::BufferRangesUpdated { .. }
                    | EditorEvent::BuffersRemoved { .. }
                    | EditorEvent::BufferFoldToggled { .. } => this.repaint_reviewed_hunks(cx),
                    EditorEvent::Focused => this.stay_in_normal_mode_on_folded_header(window, cx),
                    EditorEvent::ScrollPositionChanged { .. } => {
                        let rhs = this.diff.read(cx).editor().read(cx).rhs_editor().clone();
                        rhs.update(cx, |editor, cx| keep_scroll_at_start(editor, window, cx));
                    }
                    _ => {}
                },
            ),
            cx.observe(&splittable, |this, splittable, cx| {
                let lhs_editor = splittable.read(cx).lhs_editor().cloned();
                let lhs_editor_id = lhs_editor.as_ref().map(|lhs| lhs.entity_id());
                if lhs_editor_id != this.lhs_editor_id {
                    this.lhs_editor_id = lhs_editor_id;
                    if let Some(lhs_editor) = &lhs_editor {
                        configure_side(lhs_editor, cx);
                        let rhs_editor = splittable.read(cx).rhs_editor().clone();
                        face_cursor(&rhs_editor, lhs_editor, cx);
                        context_menu::install(lhs_editor, cx.weak_entity(), cx);
                    }
                    this.repaint_reviewed_hunks(cx);
                }
            }),
        ]);
        for side in [
            Some(splittable.read(cx).rhs_editor().clone()),
            splittable.read(cx).lhs_editor().cloned(),
        ]
        .into_iter()
        .flatten()
        {
            configure_side(&side, cx);
            context_menu::install(&side, cx.weak_entity(), cx);
        }
        let lhs_editor_id = splittable.read(cx).lhs_editor().map(|lhs| lhs.entity_id());
        let session_subscription = cx.subscribe_in(&session, window, Self::handle_session_event);
        let mut this = Self {
            target,
            repo,
            diff,
            top_block,
            top_block_id: None,
            session,
            previous_sessions: HashMap::default(),
            comments,
            pull_request,
            local_changes,
            checked_out,
            filters: ReviewFilters::default(),
            attributes: HashMap::default(),
            project,
            workspace: workspace.downgrade(),
            lhs_editor_id,
            _load_commits_task: load_commits_task,
            _attributes_task: Task::ready(()),
            _session_subscription: session_subscription,
            _subscriptions: subscriptions,
        };
        this.sync_top_block_host(window, cx);
        this
    }

    /// Hosts the overview in a block above the diff's first file while it has something to show
    /// (not in an uncommitted review without review-level comments). The block is on the right
    /// side, whose editor stays when the diff is split (drawn across both sides then).
    fn sync_top_block_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let is_empty = self.top_block.read(cx).is_empty(cx);
        let rhs = self.diff.read(cx).editor().read(cx).rhs_editor().clone();
        let block = match (is_empty, self.top_block_id) {
            (false, None) => {
                let properties = ReviewTopBlock::block_properties(&self.top_block);
                Some(rhs.update(cx, |editor, cx| {
                    let ids = editor.insert_blocks([properties], None, cx);
                    let id = ids.first().copied();
                    editor.set_full_width_block(id, cx);
                    id
                }))
            }
            (true, Some(id)) => {
                rhs.update(cx, |editor, cx| {
                    editor.set_full_width_block(None, cx);
                    editor.remove_blocks(HashSet::from_iter([id]), None, cx);
                });
                Some(None)
            }
            (false, Some(_)) | (true, None) => None,
        };
        if let Some(block) = block {
            self.top_block_id = block;
            self.comments.update(cx, |comments, cx| {
                comments.set_overview_block(block, window, cx)
            });
            cx.notify();
        }
    }

    pub(crate) fn session(&self) -> &Entity<ReviewSession> {
        &self.session
    }

    /// Shows the pull request's threads again (they, the filters or the checked-out commit
    /// changed).
    fn update_remote_items(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(pull_request), Some(local_changes)) = (&self.pull_request, &self.local_changes)
        else {
            return;
        };
        let items = remote_items(pull_request, head_oid(&self.repo, cx), local_changes, cx);
        self.comments.update(cx, |comments, cx| {
            comments.set_remote_items(items, window, cx)
        });
        cx.notify();
    }

    /// Switches to the session of the review's current key (the base or branch changed).
    fn update_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.target.review_key(&self.repo.read(cx));
        if self.session.read(cx).key() == &key {
            return;
        }
        let session = self
            .previous_sessions
            .remove(&key)
            .unwrap_or_else(|| ReviewSession::for_key(key, cx));
        let previous = std::mem::replace(&mut self.session, session);
        // `zed --review --wait` waits for the review its tab shows, whatever its key is now,
        // unless another tab (a split clone) still shows the previous one.
        previous.update(cx, |previous, cx| {
            self.session.update(cx, |session, _| {
                previous.hide(Some(session));
                session.show();
            })
        });
        let previous_key = previous.read(cx).key().clone();
        self.previous_sessions.insert(previous_key, previous);
        self._session_subscription =
            cx.subscribe_in(&self.session, window, Self::handle_session_event);
        let session = self.session.clone();
        self.comments
            .update(cx, |comments, cx| comments.set_session(session, window, cx));
        if self.session.read(cx).is_loaded() {
            self.apply_session(window, cx);
        }
        self.repaint_reviewed_hunks(cx);
        cx.notify();
    }

    /// Brings the folds and marks of every file in line with a newly loaded session.
    fn apply_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.recompute_hidden(window, cx);
        let files = self.diff.read(cx).files(cx);
        for file in &files {
            if let Some(digest) = self.diff.read(cx).digest(&file.path_key).cloned() {
                self.session.update(cx, |session, cx| {
                    session.observe_digest(&file.repo_path, &digest, cx)
                });
            }
            self.sync_fold(&file.path_key, cx);
        }
    }

    fn handle_session_event(
        &mut self,
        _: &Entity<ReviewSession>,
        event: &ReviewSessionEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ReviewSessionEvent::Loaded => {
                self.apply_session(window, cx);
                self.repaint_reviewed_hunks(cx);
            }
            ReviewSessionEvent::FilesChanged(changes) => {
                for (repo_path, _) in changes {
                    self.sync_fold(&review_path_key(repo_path), cx);
                }
                if self.filters.hide_reviewed {
                    self.recompute_hidden(window, cx);
                }
            }
            ReviewSessionEvent::HunksChanged => self.repaint_reviewed_hunks(cx),
            ReviewSessionEvent::Comment(_) => {}
            ReviewSessionEvent::Failed(message) => self.show_toast(
                Toast::new(
                    NotificationId::unique::<ReviewSession>(),
                    message.to_string(),
                ),
                cx,
            ),
        }
        cx.notify();
    }

    fn show_toast(&self, toast: Toast, cx: &mut App) {
        self.workspace
            .update(cx, |workspace, cx| workspace.show_toast(toast, cx))
            .log_err();
    }

    fn handle_diff_event(
        &mut self,
        _: &Entity<ReviewDiff>,
        event: &ReviewDiffEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            // A file may need hiding before its excerpts appear, and the label for an entirely
            // hidden review depends on which files have hunks.
            ReviewDiffEvent::FileListChanged => {
                self.recompute_hidden(window, cx);
                self.reload_attributes(window, cx);
                self.sync_local_changes(cx);
            }
            ReviewDiffEvent::FilesChanged => {
                self.recompute_hidden(window, cx);
                self.sync_local_changes(cx);
            }
            ReviewDiffEvent::FileDiffChanged {
                path_key,
                newly_shown,
            } => {
                if *newly_shown {
                    self.sync_local_changes(cx);
                }
                let diff = self.diff.read(cx);
                // A file without changes isn't listed, so its mark keeps the last diff it had:
                // `git stash pop` brings it back reviewed.
                if let Some(repo_path) = diff.repo_path_for(path_key)
                    && let Some(digest) = diff.digest(path_key).cloned()
                    && !digest.hunks.is_empty()
                {
                    self.session.update(cx, |session, cx| {
                        session.observe_digest(&repo_path, &digest, cx)
                    });
                }
                // Reviewed files fold when they appear, but stay open once the user opens them.
                if *newly_shown {
                    self.sync_fold(path_key, cx);
                }
                self.repaint_reviewed_hunks(cx);
            }
            ReviewDiffEvent::SingleFileChanged { previous, current } => {
                match current {
                    Some(current) => self.sync_fold(current, cx),
                    // The other files fold as they reappear.
                    None => {
                        if let Some(previous) = previous {
                            self.sync_fold(previous, cx);
                        }
                    }
                }
            }
        }
        cx.notify();
    }

    /// Compares the review's files with HEAD (pull request reviews).
    fn sync_local_changes(&mut self, cx: &mut Context<Self>) {
        let Some(local_changes) = &self.local_changes else {
            return;
        };
        let diff = self.diff.read(cx);
        let files = diff
            .files(cx)
            .into_iter()
            .filter_map(|file| Some((file.repo_path, diff.display_buffer(&file.path_key)?.clone())))
            .collect();
        local_changes.update(cx, |local_changes, cx| local_changes.sync(files, cx));
    }

    pub(crate) fn local_changes(&self) -> Option<&Entity<LocalChanges>> {
        self.local_changes.as_ref()
    }

    pub(crate) fn filters(&self) -> &ReviewFilters {
        &self.filters
    }

    pub(crate) fn set_filters(
        &mut self,
        filters: ReviewFilters,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.filters = filters;
        self.recompute_hidden(window, cx);
        cx.notify();
    }

    fn attributes(&self, repo_path: &RepoPath) -> FileAttributes {
        self.attributes.get(repo_path).copied().unwrap_or_default()
    }

    /// Whether the include/exclude patterns or the generated/vendored defaults hide the file.
    fn passes_filters_except_reviewed(&self, repo_path: &RepoPath) -> bool {
        self.filters.passes_patterns(repo_path)
            && !self.filters.hides_attributes(self.attributes(repo_path))
    }

    fn is_hidden_by_filters(&self, repo_path: &RepoPath, cx: &App) -> bool {
        !self.passes_filters_except_reviewed(repo_path)
            || (self.filters.hide_reviewed && self.session.read(cx).is_file_reviewed(repo_path))
    }

    /// Reads the review's `.gitattributes` (from the working tree, for every target) and hides
    /// generated and vendored files. Until they are read, such files show.
    fn reload_attributes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = self.diff.read(cx).listed_paths().to_vec();
        let repo = self.repo.read(cx);
        let work_directory = repo.work_directory_abs_path.clone();
        let common_directory = repo.common_dir_abs_path.clone();
        let fs = self.project.read(cx).fs().clone();
        self._attributes_task = cx.spawn_in(window, async move |this, cx| {
            let git_attributes =
                GitAttributes::load(fs.as_ref(), &work_directory, &common_directory, &paths).await;
            let attributes = paths
                .into_iter()
                .filter_map(|path| {
                    let attributes = git_attributes.attributes(&path);
                    (attributes != FileAttributes::default()).then_some((path, attributes))
                })
                .collect();
            this.update_in(cx, |this, window, cx| {
                this.attributes = attributes;
                this.recompute_hidden(window, cx);
                cx.notify();
            })
            .log_err();
        });
    }

    /// Hides the files the filters exclude, in the diff and in the panel.
    fn recompute_hidden(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let diff = self.diff.read(cx);
        let listed = diff.listed_paths();
        let hidden = listed
            .iter()
            .filter(|repo_path| self.is_hidden_by_filters(repo_path, cx))
            .map(review_path_key)
            .collect::<HashSet<_>>();
        let files = diff.files(cx);
        let all_hidden_label = (!files.is_empty()
            && files.iter().all(|file| hidden.contains(&file.path_key)))
        .then(|| {
            // Everything is hidden, so a file that passes the other filters is a hidden
            // reviewed file.
            let reviewed_hidden = files
                .iter()
                .any(|file| self.passes_filters_except_reviewed(&file.repo_path));
            if self.filters.hide_reviewed && reviewed_hidden {
                SharedString::from("All files reviewed")
            } else {
                // The panel counts generated and vendored files on their own, so they aren't
                // among the changed files, unless they are all there is.
                let count = match files
                    .iter()
                    .filter(|file| {
                        !self
                            .filters
                            .hides_attributes(self.attributes(&file.repo_path))
                    })
                    .count()
                {
                    0 => files.len(),
                    count => count,
                };
                format!(
                    "All {} changed file{} are hidden",
                    count,
                    if count == 1 { "" } else { "s" }
                )
                .into()
            }
        });
        self.diff.update(cx, |diff, cx| {
            diff.set_hidden(hidden, all_hidden_label, window, cx)
        });
    }

    pub(crate) fn summary(&self, cx: &App) -> ReviewSummary {
        let session = self.session.read(cx);
        let mut summary = ReviewSummary::default();
        for file in self.diff.read(cx).files(cx) {
            // Generated and vendored files are hidden by default, so they count as such rather
            // than as filtered out by the patterns.
            let attributes = self.attributes(&file.repo_path);
            if self.filters.hides_attributes(attributes) {
                if attributes.generated && !self.filters.show_generated {
                    summary.generated_hidden += 1;
                } else {
                    summary.vendored_hidden += 1;
                }
                continue;
            }
            if !self.filters.passes_patterns(&file.repo_path) {
                summary.filtered_out += 1;
                continue;
            }
            summary.total += 1;
            if session.is_file_reviewed(&file.repo_path) {
                summary.reviewed += 1;
                if self.filters.hide_reviewed {
                    summary.reviewed_hidden += 1;
                }
            }
        }
        summary
    }

    /// Folds a shown file to its header when it is reviewed and unfolds it otherwise. Single-file
    /// mode always shows its file unfolded.
    fn sync_fold(&self, path_key: &PathKey, cx: &mut App) {
        let diff = self.diff.read(cx);
        if !diff.is_shown(path_key) {
            return;
        }
        let Some(buffer_id) = diff.buffer_id_for(path_key, cx) else {
            return;
        };
        let reviewed = diff
            .repo_path_for(path_key)
            .is_some_and(|repo_path| self.session.read(cx).is_file_reviewed(&repo_path));
        let fold = reviewed && diff.single_file() != Some(path_key);
        // Folding the right side folds the left side too.
        let editor = diff.editor().read(cx).rhs_editor().clone();
        editor.update(cx, |editor, cx| {
            if fold {
                editor.fold_buffer(buffer_id, cx);
            } else {
                editor.unfold_buffer(buffer_id, cx);
            }
        });
    }

    /// Toggles a file's reviewed mark without moving the cursor (header checkbox, panel `r`).
    pub(crate) fn toggle_file(&mut self, repo_path: &RepoPath, cx: &mut Context<Self>) {
        let path_key = review_path_key(repo_path);
        let reviewed = !self.session.read(cx).is_file_reviewed(repo_path);
        let Some(content_hash) = self.content_hash_to_mark(repo_path, reviewed, cx) else {
            return;
        };
        self.session.update(cx, |session, cx| match content_hash {
            Some(content_hash) => session.mark_file(repo_path, content_hash, cx),
            None => session.unmark_file(repo_path, cx),
        });
        self.sync_fold(&path_key, cx);
    }

    /// `Some(Some(hash))` to mark a file with its diff's hash, `Some(None)` to unmark it, `None`
    /// when it can't be marked: its diff is only known while it has no unsaved edits.
    fn content_hash_to_mark(
        &self,
        repo_path: &RepoPath,
        reviewed: bool,
        cx: &mut App,
    ) -> Option<Option<u64>> {
        if !reviewed {
            return Some(None);
        }
        let digest = self.diff.read(cx).digest(&review_path_key(repo_path));
        if let Some(digest) = digest {
            return Some(Some(digest.content_hash));
        }
        self.show_toast(
            Toast::new(
                NotificationId::unique::<ToggleFileReviewed>(),
                format!(
                    "{} has unsaved changes: save it to mark it reviewed",
                    repo_path.as_unix_str()
                ),
            )
            .autohide(),
            cx,
        );
        None
    }

    /// `r`: marks the file at the cursor reviewed and moves to the next file, or unmarks it and
    /// moves to its first row.
    fn toggle_file_reviewed(
        &mut self,
        _: &ToggleFileReviewed,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        discard_vim_count(window, cx);
        if !self.session.read(cx).is_loaded() {
            return;
        }
        let diff = self.diff.read(cx);
        let Some(path_key) = diff.path_at_cursor(cx) else {
            return;
        };
        let Some(repo_path) = diff.repo_path_for(&path_key) else {
            return;
        };
        let next = diff.file_order().into_iter().find(|key| key > &path_key);
        let focused = diff.editor().read(cx).focused_editor().clone();
        let reviewed = !self.session.read(cx).is_file_reviewed(&repo_path);
        let Some(content_hash) = self.content_hash_to_mark(&repo_path, reviewed, cx) else {
            return;
        };
        let at_start = focused.update(cx, |editor, cx| editor.scroll_position(cx).y == 0.);
        // Move first, so folding the file doesn't drag the cursor onto its header. When reviewed
        // files are hidden, hiding it moves the cursor on (wrapping around).
        let move_to = next.filter(|_| reviewed && !self.filters.hide_reviewed);
        let moved = move_to.is_some();
        if let Some(next) = move_to {
            self.diff
                .update(cx, |diff, cx| diff.move_to_path(next, window, cx));
        }
        self.session.update(cx, |session, cx| match content_hash {
            Some(content_hash) => session.mark_file(&repo_path, content_hash, cx),
            None => session.unmark_file(&repo_path, cx),
        });
        self.sync_fold(&path_key, cx);
        if moved && at_start {
            focused.update(cx, |editor, cx| stay_at_start(editor, cx));
        }
        if !reviewed {
            self.diff
                .update(cx, |diff, cx| diff.select_path_start(&path_key, window, cx));
        }
    }

    /// `R`: toggles the mark of the hunk at the cursor.
    fn toggle_hunk_reviewed(
        &mut self,
        _: &ToggleHunkReviewed,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        discard_vim_count(window, cx);
        if !self.session.read(cx).is_loaded() {
            return;
        }
        let editor = self
            .diff
            .read(cx)
            .editor()
            .read(cx)
            .focused_editor()
            .clone();
        // The anchor, not the resolved selection: on a folded header the latter spans the file.
        let cursor = editor.read(cx).selections.newest_anchor().head();
        match self.hunk_key_at(&editor, cursor, cx) {
            Some((repo_path, hunk_key)) => self.toggle_hunk(&repo_path, hunk_key, cx),
            None => self.show_toast(
                Toast::new(
                    NotificationId::unique::<ToggleHunkReviewed>(),
                    "Move the cursor onto a changed line to mark its hunk reviewed",
                )
                .autohide(),
                cx,
            ),
        }
    }

    pub(crate) fn toggle_hunk(
        &mut self,
        repo_path: &RepoPath,
        hunk_key: SharedString,
        cx: &mut Context<Self>,
    ) {
        self.session.update(cx, |session, cx| {
            session.toggle_hunk(repo_path, hunk_key, cx)
        });
    }

    /// The file and key of the hunk whose rows contain `position` in `editor` (either side).
    pub(crate) fn hunk_key_at(
        &self,
        editor: &Entity<Editor>,
        position: editor::Anchor,
        cx: &App,
    ) -> Option<(RepoPath, SharedString)> {
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        let row = position.to_point(&snapshot).row;
        let line_end = Point::new(row, snapshot.line_len(MultiBufferRow(row)));
        let editor = editor.read(cx);
        // Hunks of folded files can't be seen, so they aren't marked from their header.
        let hunk = snapshot
            .diff_hunks_in_range(Point::new(row, 0)..line_end)
            .find(|hunk| hunk.row_range.start.0 <= row && row < hunk.row_range.end.0)
            .filter(|hunk| !editor.is_buffer_folded(hunk.buffer_id, cx))?;
        let path_key = snapshot.path_for_buffer(hunk.buffer_id)?;
        let diff = self.diff.read(cx);
        let repo_path = diff.repo_path_for(path_key)?;
        let base_range = hunk.diff_base_byte_range.start.0..hunk.diff_base_byte_range.end.0;
        let hunk_key = diff.digest(path_key)?.hunk_key(&base_range)?.clone();
        Some((repo_path, hunk_key))
    }

    /// Repaints the reviewed hunks on both sides.
    fn repaint_reviewed_hunks(&mut self, cx: &mut Context<Self>) {
        let splittable = self.diff.read(cx).editor().read(cx);
        let editors = [
            Some(splittable.rhs_editor().clone()),
            splittable.lhs_editor().cloned(),
        ];
        for editor in editors.into_iter().flatten() {
            let rows = self.reviewed_hunk_rows(&editor, cx);
            editor.update(cx, |editor, cx| paint_reviewed_hunks(editor, rows, cx));
        }
    }

    fn reviewed_hunk_rows(&self, editor: &Entity<Editor>, cx: &App) -> Vec<std::ops::Range<u32>> {
        let session = self.session.read(cx);
        if !session.has_reviewed_hunks() {
            return Vec::new();
        }
        let digests = self.diff.read(cx).digests();
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        snapshot
            .diff_hunks_in_range(Point::zero()..snapshot.max_point())
            .filter(|hunk| {
                let Some((repo_path, digest)) = snapshot
                    .path_for_buffer(hunk.buffer_id)
                    .and_then(|path_key| digests.get(path_key))
                else {
                    return false;
                };
                let base_range = hunk.diff_base_byte_range.start.0..hunk.diff_base_byte_range.end.0;
                digest
                    .hunk_key(&base_range)
                    .is_some_and(|hunk_key| session.is_hunk_reviewed(repo_path, hunk_key))
            })
            .map(|hunk| hunk.row_range.start.0..hunk.row_range.end.0)
            .collect()
    }

    fn next_hunk(&mut self, _: &NextHunk, window: &mut Window, cx: &mut Context<Self>) {
        self.go_to_hunk(Direction::Next, window, cx);
    }

    fn previous_hunk(&mut self, _: &PreviousHunk, window: &mut Window, cx: &mut Context<Self>) {
        self.go_to_hunk(Direction::Previous, window, cx);
    }

    /// Zed's `GoToHunk` (wrapping, centered), except that the hunks of folded files are skipped:
    /// the cursor would be stuck on their header.
    fn go_to_hunk(&mut self, direction: Direction, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self
            .diff
            .read(cx)
            .editor()
            .read(cx)
            .focused_editor()
            .clone();
        editor.update(cx, |editor, cx| {
            let display_snapshot = editor.display_snapshot(cx);
            let head = editor.selections.newest::<Point>(&display_snapshot).head();
            let buffer = display_snapshot.buffer_snapshot();
            let folded = editor.folded_buffers(cx);
            let starts = buffer
                .diff_hunks_in_range(Point::zero()..buffer.max_point())
                .filter(|hunk| !folded.contains(&hunk.buffer_id))
                .map(|hunk| hunk.row_range.start.0)
                .collect::<Vec<_>>();
            let row = match direction {
                Direction::Next => starts
                    .iter()
                    .find(|row| **row > head.row)
                    .or_else(|| starts.first()),
                Direction::Previous => starts
                    .iter()
                    .rev()
                    .find(|row| **row < head.row)
                    .or_else(|| starts.last()),
            }
            .copied();
            if let Some(row) = row {
                let point = Point::new(row, 0);
                editor.change_selections(
                    SelectionEffects::scroll(Autoscroll::center()),
                    window,
                    cx,
                    |selections| selections.select_ranges([point..point]),
                );
            }
        });
        discard_vim_count(window, cx);
    }

    pub fn target(&self) -> &ReviewTarget {
        &self.target
    }

    /// The reviewed repository's current branch (`None` when HEAD is detached).
    pub(crate) fn branch_name(&self, cx: &App) -> Option<SharedString> {
        self.repo
            .read(cx)
            .branch
            .as_ref()
            .map(|branch| SharedString::from(branch.name().to_owned()))
    }

    pub(crate) fn workspace(&self) -> &WeakEntity<Workspace> {
        &self.workspace
    }

    pub(crate) fn repository(&self) -> &Entity<Repository> {
        &self.repo
    }

    pub(crate) fn project(&self) -> &Entity<Project> {
        &self.project
    }

    /// The base commit a pull request review compares against.
    pub(crate) fn pull_request_base(&self) -> Option<Oid> {
        match &self.target {
            ReviewTarget::PullRequest(target) => target.base.name().parse().ok(),
            ReviewTarget::Uncommitted | ReviewTarget::Branch { .. } | ReviewTarget::Commits(_) => {
                None
            }
        }
    }

    pub(crate) fn language_registry(&self, cx: &App) -> Arc<LanguageRegistry> {
        self.project.read(cx).languages().clone()
    }

    /// The reviewed repository's HEAD commit, as Zed last saw it.
    pub(crate) fn head_sha(&self, cx: &App) -> Option<SharedString> {
        self.repo
            .read(cx)
            .head_commit
            .as_ref()
            .map(|commit| commit.sha.clone())
    }

    /// The project path of a file in the review (`None` for deleted files).
    pub(crate) fn project_path(&self, repo_path: &RepoPath, cx: &App) -> Option<ProjectPath> {
        self.repo.read(cx).repo_path_to_project_path(repo_path, cx)
    }

    pub fn diff(&self) -> &Entity<ReviewDiff> {
        &self.diff
    }

    #[cfg(test)]
    pub(crate) fn top_block(&self) -> &Entity<ReviewTopBlock> {
        &self.top_block
    }

    #[cfg(test)]
    pub(crate) fn top_block_id(&self) -> Option<CustomBlockId> {
        self.top_block_id
    }

    pub fn title(&self, cx: &App) -> SharedString {
        match &self.target {
            ReviewTarget::Uncommitted => "Uncommitted changes".into(),
            ReviewTarget::Branch { base } => {
                format!("{} vs {base}", branch_name(&self.repo, cx)).into()
            }
            ReviewTarget::Commits(range) => self
                .top_block
                .read(cx)
                .commits_title()
                .unwrap_or_else(|| range.head.display_short().into()),
            ReviewTarget::PullRequest(target) => match &self.pull_request {
                Some(pull_request) => format!(
                    "PR #{}: {}",
                    target.number,
                    pull_request.read(cx).details().title
                )
                .into(),
                None => format!("PR #{}", target.number).into(),
            },
        }
    }

    /// The repository's name, when the project has several: their reviews' tabs would look the
    /// same otherwise.
    fn repository_name(&self, cx: &App) -> Option<SharedString> {
        let repositories = self
            .project
            .read(cx)
            .git_store()
            .read(cx)
            .repositories()
            .len();
        (repositories > 1).then(|| self.repo.read(cx).display_name())
    }

    /// [`Self::title`], after the repository's name when the project has several.
    pub(crate) fn title_with_repository(&self, cx: &App) -> SharedString {
        with_repository_name(self.repository_name(cx), self.title(cx))
    }

    /// The pull request a pull request review is of.
    pub(crate) fn pull_request(&self) -> Option<&Entity<PullRequestState>> {
        self.pull_request.as_ref()
    }

    /// The branch name "Copy Branch Name" copies: a pull request's head branch, else the
    /// repository's current branch (`None` when HEAD is detached).
    pub(crate) fn branch_name_to_copy(&self, cx: &App) -> Option<SharedString> {
        match &self.pull_request {
            Some(pull_request) => Some(pull_request.read(cx).details().head_ref.clone()),
            None => self.branch_name(cx),
        }
    }

    /// Moves a pull request review to the pull request's base commit in `target`, when it's
    /// another one (the pull request was rebased, or its base branch changed).
    fn update_pull_request_base(&mut self, target: &ReviewTarget, cx: &mut Context<Self>) {
        let (ReviewTarget::PullRequest(current), ReviewTarget::PullRequest(new)) =
            (&mut self.target, target)
        else {
            return;
        };
        if current.base == new.base {
            return;
        }
        current.base = new.base.clone();
        let base = new.base.clone();
        let empty_label = target.empty_label();
        self.diff
            .update(cx, |diff, cx| diff.set_branch_base(base, empty_label, cx));
        cx.notify();
    }

    /// The files the review panel lists (those the filters don't hide), in review order.
    pub(crate) fn panel_files(&self, cx: &App) -> Vec<PanelFile> {
        let session = self.session.read(cx);
        let diff = self.diff.read(cx);
        diff.files(cx)
            .into_iter()
            .filter(|file| !diff.is_hidden(&file.path_key))
            .map(|file| PanelFile {
                reviewed: session.is_file_reviewed(&file.repo_path),
                attributes: self.attributes(&file.repo_path),
                path_key: file.path_key,
                repo_path: file.repo_path,
                status: file.status,
            })
            .collect()
    }

    /// Moves the diff to the first row of the file (showing only that file in single-file mode).
    pub(crate) fn reveal_file(
        &mut self,
        path_key: PathKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff
            .update(cx, |diff, cx| diff.move_to_path(path_key, window, cx));
    }

    /// Opens Zed's branch picker to change the base of a branch review. A branch that another
    /// tab already reviews against activates that tab instead.
    pub(crate) fn open_base_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ReviewTarget::Branch { base } = &self.target else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let workspace_handle = self.workspace.clone();
        let view = cx.weak_entity();
        let repo = self.repo.clone();
        let on_select: SelectBranchCallback = Arc::new({
            let workspace_handle = workspace_handle.clone();
            move |branch, window, cx| {
                let base = BranchBase::new(branch.name().to_owned());
                let Some(view) = view.upgrade() else {
                    return;
                };
                workspace_handle
                    .update(cx, |workspace, cx| {
                        if let Some(base) = base.notify_err(workspace, cx) {
                            Self::select_base(view, base, workspace, window, cx)
                        }
                    })
                    .log_err();
            }
        });
        let selected_base = Some(base.name().clone());
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                branch_picker::select_modal(
                    workspace_handle,
                    Some(repo),
                    selected_base,
                    on_select,
                    window,
                    cx,
                )
            })
        });
    }

    /// The base picker's choice for `view`: another tab that already reviews the branch against
    /// `base` is activated, otherwise `view` changes its base.
    pub(crate) fn select_base(
        view: Entity<Self>,
        base: BranchBase,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let repo = view.read(cx).repo.clone();
        let target = ReviewTarget::Branch { base: base.clone() };
        match find_review(workspace, &target, &repo, cx) {
            Some(existing) if existing != view => {
                workspace.activate_item(&existing, true, true, window, cx);
            }
            _ => view.update(cx, |view, cx| view.change_base(base, window, cx)),
        }
    }

    /// Reviews the branch against `base` instead, in this tab.
    pub(crate) fn change_base(
        &mut self,
        base: BranchBase,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = ReviewTarget::Branch { base: base.clone() };
        if self.target == target {
            return;
        }
        self.target = target;
        let base_name = base.name().clone();
        let empty_label = self.target.empty_label();
        self.diff
            .update(cx, |diff, cx| diff.set_branch_base(base, empty_label, cx));
        self.update_session(window, cx);
        let branch = branch_name(&self.repo, cx);
        self.top_block.update(cx, |top_block, cx| {
            top_block.set_branch(branch, base_name, cx)
        });
        cx.emit(EditorEvent::TitleChanged);
        cx.notify();
    }

    fn change_base_action(&mut self, _: &ChangeBase, window: &mut Window, cx: &mut Context<Self>) {
        self.open_base_picker(window, cx);
    }

    fn toggle_header(&mut self, _: &ToggleHeader, _: &mut Window, cx: &mut Context<Self>) {
        self.top_block
            .update(cx, |top_block, cx| top_block.toggle_expanded(cx));
    }

    fn next_file(&mut self, _: &NextFile, window: &mut Window, cx: &mut Context<Self>) {
        // No wrapping, like tuicr: the last file is a stop.
        self.move_to_file(window, cx, |order, current| match current {
            Some(current) => order.iter().find(|key| *key > current).cloned(),
            None => order.first().cloned(),
        });
    }

    fn previous_file(&mut self, _: &PreviousFile, window: &mut Window, cx: &mut Context<Self>) {
        self.move_to_file(window, cx, |order, current| {
            let current = current?;
            order.iter().rev().find(|key| *key < current).cloned()
        });
    }

    fn next_unreviewed_file(
        &mut self,
        _: &NextUnreviewedFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_to_unreviewed_file(window, cx, |order, current| {
            let after = current.and_then(|current| order.iter().find(|key| *key > current));
            after
                .or_else(|| order.iter().find(|key| Some(*key) != current))
                .cloned()
        });
    }

    fn previous_unreviewed_file(
        &mut self,
        _: &PreviousUnreviewedFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_to_unreviewed_file(window, cx, |order, current| {
            let before = current.and_then(|current| order.iter().rev().find(|key| *key < current));
            before
                .or_else(|| order.iter().rev().find(|key| Some(*key) != current))
                .cloned()
        });
    }

    /// Unlike plain file navigation, this wraps around: it finds the remaining work from anywhere.
    fn move_to_unreviewed_file(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        target: impl FnOnce(&[PathKey], Option<&PathKey>) -> Option<PathKey>,
    ) {
        let reviewed = self.reviewed_files(cx);
        self.move_to_file(window, cx, |order, current| {
            let unreviewed = order
                .iter()
                .filter(|key| !reviewed.contains(*key))
                .cloned()
                .collect::<Vec<_>>();
            target(&unreviewed, current)
        });
    }

    fn reviewed_files(&self, cx: &App) -> HashSet<PathKey> {
        let session = self.session.read(cx);
        self.diff
            .read(cx)
            .files(cx)
            .into_iter()
            .filter(|file| session.is_file_reviewed(&file.repo_path))
            .map(|file| file.path_key)
            .collect()
    }

    fn move_to_file(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        target: impl FnOnce(&[PathKey], Option<&PathKey>) -> Option<PathKey>,
    ) {
        self.diff.update(cx, |diff, cx| {
            let order = diff.file_order();
            let current = diff.path_at_cursor(cx);
            if let Some(target) = target(&order, current.as_ref()) {
                diff.move_to_path(target, window, cx);
            }
        });
        discard_vim_count(window, cx);
    }

    fn toggle_single_file_mode(
        &mut self,
        _: &ToggleSingleFileMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff.update(cx, |diff, cx| {
            let single_file = if diff.single_file().is_some() {
                None
            } else {
                diff.path_at_cursor(cx)
                    .or_else(|| diff.file_order().first().cloned())
            };
            diff.set_single_file(single_file, window, cx);
        });
        discard_vim_count(window, cx);
    }

    fn open_file_at_cursor(
        &mut self,
        _: &OpenFileAtCursor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.diff.read(cx).is_snapshot(cx) {
            self.open_working_tree_file_at_cursor(window, cx);
        } else {
            let editor = self
                .diff
                .read(cx)
                .editor()
                .read(cx)
                .focused_editor()
                .clone();
            editor.update(cx, |editor, cx| {
                editor.open_excerpts(&OpenExcerpts, window, cx)
            });
        }
        discard_vim_count(window, cx);
    }

    /// Zed's own key for opening the file at the cursor (vim's `g space`) opens a snapshot's
    /// file from the working tree too, not as the historic buffer.
    fn open_excerpts(&mut self, _: &OpenExcerpts, window: &mut Window, cx: &mut Context<Self>) {
        if !self.diff.read(cx).is_snapshot(cx) {
            return;
        }
        let editor = self.diff.read(cx).editor().read(cx);
        let diff_is_focused = [Some(editor.rhs_editor()), editor.lhs_editor()]
            .into_iter()
            .flatten()
            .any(|editor| editor.focus_handle(cx).is_focused(window));
        if diff_is_focused {
            self.open_working_tree_file_at_cursor(window, cx);
            cx.stop_propagation();
        }
    }

    /// A snapshot's buffers are historic, so the file opens as it is in the working tree, at the
    /// same row as far as it still exists.
    fn open_working_tree_file_at_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((path_key, row)) = self
            .diff
            .update(cx, |diff, cx| diff.path_and_row_at_cursor(cx))
        else {
            return;
        };
        let Some(project_path) = self
            .diff
            .read(cx)
            .repo_path_for(&path_key)
            .and_then(|repo_path| self.repo.read(cx).repo_path_to_project_path(&repo_path, cx))
        else {
            return;
        };
        let workspace = self.workspace.clone();
        // Opening activates another item, which calls back into this one.
        window
            .spawn(cx, {
                let workspace = workspace.clone();
                async move |cx| {
                    let item = workspace
                        .update_in(cx, |workspace, window, cx| {
                            workspace.open_path(project_path, None, true, window, cx)
                        })?
                        .await?;
                    if let Some(editor) = item.downcast::<Editor>() {
                        editor.update_in(cx, |editor, window, cx| {
                            let max_row = editor.buffer().read(cx).snapshot(cx).max_point().row;
                            let point = Point::new(row.min(max_row), 0);
                            editor.change_selections(
                                SelectionEffects::scroll(Autoscroll::center()),
                                window,
                                cx,
                                |selections| selections.select_ranges([point..point]),
                            );
                        })?;
                    }
                    anyhow::Ok(())
                }
            })
            .detach_and_notify_err(workspace, window, cx);
    }

    /// Both sides switch between wrapping and not wrapping together. Zed's own toggle would fall
    /// back to the language settings first, which may not change anything. A comment editor
    /// toggles its own wrapping.
    fn toggle_soft_wrap(
        &mut self,
        _: &ToggleSoftWrap,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = self.diff.read(cx).editor().clone();
        let splittable = editor.read(cx);
        let diff_is_focused = [Some(splittable.rhs_editor()), splittable.lhs_editor()]
            .into_iter()
            .flatten()
            .any(|editor| editor.focus_handle(cx).is_focused(window));
        if !diff_is_focused {
            return;
        }
        let wraps = !matches!(
            editor.read(cx).rhs_editor().read(cx).soft_wrap_mode(cx),
            editor::SoftWrap::None
        );
        let mode = if wraps {
            SoftWrap::None
        } else {
            SoftWrap::EditorWidth
        };
        editor.update(cx, |editor, cx| {
            editor.update_editors(cx, |editor, cx| editor.set_soft_wrap_mode(mode, cx))
        });
        cx.stop_propagation();
    }

    /// Which file single-file mode shows: its place among the review's files, their number and
    /// its path (the review toolbar shows them).
    pub(crate) fn single_file_position(&self, cx: &App) -> Option<(usize, usize, SharedString)> {
        let diff = self.diff.read(cx);
        let path_key = diff.single_file()?;
        let order = diff.file_order();
        let position = order
            .iter()
            .position(|key| key == path_key)
            .map_or(0, |index| index + 1);
        let path = diff
            .repo_path_for(path_key)
            .map(|repo_path| SharedString::from(repo_path.as_unix_str().to_string()))
            .unwrap_or_default();
        Some((position, order.len(), path))
    }

    /// The review's own comments with text, not yet submitted: what submitting or finishing
    /// it sends.
    pub(crate) fn draft_comment_count(&self, cx: &App) -> usize {
        self.session
            .read(cx)
            .comments()
            .iter()
            .filter(|comment| !comment.body.is_empty() && !comment.is_submitted())
            .count()
    }

    /// The pull request's threads this tab shows (the thread filters hide the others).
    pub(crate) fn shown_thread_count(&self, cx: &App) -> usize {
        self.comments
            .read(cx)
            .remote_items()
            .map(|(_, _, view)| view.read(cx).threads().count())
            .sum()
    }

    fn add_comment(&mut self, _: &AddComment, window: &mut Window, cx: &mut Context<Self>) {
        discard_vim_count(window, cx);
        self.comments
            .update(cx, |comments, cx| comments.add_line_comment(window, cx));
    }

    fn add_file_comment(
        &mut self,
        _: &AddFileComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        discard_vim_count(window, cx);
        self.comments
            .update(cx, |comments, cx| comments.add_file_comment(window, cx));
    }

    fn add_review_comment(
        &mut self,
        _: &AddReviewComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        discard_vim_count(window, cx);
        self.comments
            .update(cx, |comments, cx| comments.add_review_comment(window, cx));
    }

    /// Without a comment below the cursor line, the key does what it would otherwise (`Enter`
    /// moves down).
    fn edit_comment(&mut self, _: &EditComment, window: &mut Window, cx: &mut Context<Self>) {
        let edited = self.comments.update(cx, |comments, cx| {
            comments.edit_comment_at_cursor(window, cx)
        });
        if edited {
            discard_vim_count(window, cx);
        } else {
            cx.propagate();
        }
    }

    fn delete_comment(&mut self, _: &DeleteComment, window: &mut Window, cx: &mut Context<Self>) {
        discard_vim_count(window, cx);
        self.comments.update(cx, |comments, cx| {
            comments.delete_comment_at_cursor(window, cx)
        });
    }

    fn next_comment(&mut self, _: &NextComment, window: &mut Window, cx: &mut Context<Self>) {
        discard_vim_count(window, cx);
        self.comments
            .update(cx, |comments, cx| comments.go_to_next(window, cx));
    }

    fn previous_comment(
        &mut self,
        _: &PreviousComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        discard_vim_count(window, cx);
        self.comments
            .update(cx, |comments, cx| comments.go_to_previous(window, cx));
    }

    pub(crate) fn comments(&self) -> &Entity<CommentBlocks> {
        &self.comments
    }

    /// The review's comments as Markdown for a coding agent (what `; y` copies), or `None` when
    /// there are none to send. Comment editors open in this tab count with their current text.
    pub fn review_markdown(&self, cx: &App) -> Option<String> {
        let unsaved = self.comments.read(cx).unsaved_bodies(cx);
        let comments = self
            .session
            .read(cx)
            .comments()
            .iter()
            .map(|comment| {
                let mut comment = comment.clone();
                if let Some(body) = unsaved.get(&comment.id) {
                    comment.body = body.clone();
                }
                comment
            })
            .collect::<Vec<_>>();
        export::review_markdown(&self.export_preamble(cx), &comments)
    }

    /// What tuicr's export says about the review's scope, for each kind of review.
    fn export_preamble(&self, cx: &App) -> ExportPreamble {
        match &self.target {
            ReviewTarget::Uncommitted => ExportPreamble {
                banner: vec!["Reviewing staged + unstaged changes".into()],
                scope: "staged + unstaged changes".into(),
            },
            // tuicr has no merge-base review; this says what is compared.
            ReviewTarget::Branch { base } => {
                let branch = branch_name(&self.repo, cx);
                ExportPreamble {
                    banner: vec![format!(
                        "Reviewing branch {branch} against {base} (its commits since the merge \
                         base + staged + unstaged changes)"
                    )],
                    scope: format!("branch {branch} vs {base}"),
                }
            }
            ReviewTarget::Commits(range) => ExportPreamble {
                banner: vec![commits_banner(
                    self.top_block.read(cx).commit_shas().as_deref(),
                    range,
                )],
                scope: "selected commit range".into(),
            },
            ReviewTarget::PullRequest(target) => {
                let name = format!("{}#{}", target.repo, target.number);
                let mut banner = Vec::new();
                match &self.pull_request {
                    Some(pull_request) => {
                        let details = pull_request.read(cx).details();
                        banner.push(format!("Reviewing pull request {name}: {}", details.title));
                        banner.push(format!("URL: {}", details.url));
                        banner.push(format!("Head: {}", details.head_oid.display_short()));
                    }
                    None => banner.push(format!("Reviewing pull request {name}")),
                }
                ExportPreamble {
                    banner,
                    scope: format!("pull request {name}"),
                }
            }
        }
    }

    fn copy_comment_at_cursor(
        &mut self,
        _: &CopyCommentAtCursor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        discard_vim_count(window, cx);
        let body = self
            .comments
            .update(cx, |comments, cx| comments.body_at_cursor(cx));
        let message = match body {
            Some(body) => {
                cx.write_to_clipboard(ClipboardItem::new_string(body));
                "Comment copied to clipboard"
            }
            None => "No comment at the cursor",
        };
        self.show_toast(
            Toast::new(NotificationId::unique::<CopyCommentAtCursor>(), message).autohide(),
            cx,
        );
    }

    fn copy_selected_lines(
        &mut self,
        _: &CopySelectedLines,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        discard_vim_count(window, cx);
        let text = self
            .comments
            .update(cx, |comments, cx| comments.selected_text(window, cx));
        let message = match text {
            Some((text, count)) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                if count == 1 {
                    "Copied 1 line".to_string()
                } else {
                    format!("Copied {count} lines")
                }
            }
            None => "No lines to copy".to_string(),
        };
        self.show_toast(
            Toast::new(NotificationId::unique::<CopySelectedLines>(), message).autohide(),
            cx,
        );
    }

    /// The comments and threads the review panel lists: review comments first, then by file in
    /// review order, file comments and threads before line ones, those by their last line (a
    /// thread before a comment). Comments still being written for the first time (without text)
    /// aren't listed, nor submitted ones once the threads shown include theirs (their review
    /// comment is a review summary in the top block).
    pub(crate) fn panel_comments(&self, cx: &App) -> Vec<PanelComment> {
        let line_of = |target: &CommentTarget| match target {
            CommentTarget::Lines { lines, .. } => Some(*lines.end()),
            CommentTarget::Review | CommentTarget::File { .. } => None,
        };
        let hide_submitted = self
            .pull_request
            .as_ref()
            .is_some_and(|pull_request| pull_request.read(cx).threads_include_submitted());
        let mut rows = self
            .session
            .read(cx)
            .comments()
            .iter()
            .filter(|comment| !comment.body.is_empty())
            .filter(|comment| !(hide_submitted && comment.is_submitted()))
            .map(|comment| {
                let sort_key = (
                    comment.target.path().map(review_path_key),
                    line_of(&comment.target),
                    true,
                    comment.id.0,
                );
                let row = PanelComment {
                    key: PanelCommentKey::Comment(comment.id),
                    item: ItemKey::Comment(comment.id),
                    is_review: comment.target == CommentTarget::Review,
                    comment_type: comment.comment_type,
                    location: comment
                        .location()
                        .unwrap_or_else(|| "Review".to_string())
                        .into(),
                    first_line: comment
                        .body
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .to_string()
                        .into(),
                    outdated: comment.is_outdated(),
                    submitted: comment.is_submitted(),
                    remote: false,
                    resolved: false,
                };
                (sort_key, row)
            })
            .collect::<Vec<_>>();
        let comments = self.comments.read(cx);
        let mut thread_index = 0u64;
        for (key, target, view) in comments.remote_items() {
            let is_moved = matches!(key, RemoteKey::Moved(_));
            for thread in view.read(cx).threads() {
                let location = match target {
                    CommentTarget::Lines { path, side, lines } if !is_moved => {
                        let marker = match side {
                            Side::Old => "~",
                            Side::New => "",
                        };
                        let path = path.as_unix_str();
                        if lines.start() == lines.end() {
                            format!("{path}:{marker}{}", lines.end())
                        } else {
                            format!("{path}:{marker}{}-{marker}{}", lines.start(), lines.end())
                        }
                    }
                    _ => target
                        .path()
                        .map(|path| path.as_unix_str().to_string())
                        .unwrap_or_default(),
                };
                let first_comment = thread.comments.first();
                let first_line = first_comment
                    .map(|comment| {
                        format!(
                            "{}: {}",
                            comment.author.login,
                            comment.body.lines().next().unwrap_or_default()
                        )
                    })
                    .unwrap_or_default();
                let sort_key = (
                    target.path().map(review_path_key),
                    if is_moved { None } else { line_of(target) },
                    false,
                    thread_index,
                );
                thread_index += 1;
                rows.push((
                    sort_key,
                    PanelComment {
                        key: PanelCommentKey::Thread(thread.id.clone()),
                        item: ItemKey::Remote(key.clone()),
                        is_review: false,
                        comment_type: None,
                        location: location.into(),
                        first_line: first_line.into(),
                        outdated: thread.is_outdated || is_moved,
                        submitted: false,
                        remote: true,
                        resolved: thread.is_resolved,
                    },
                ));
            }
        }
        rows.sort_by(|(key_a, _), (key_b, _)| key_a.cmp(key_b));
        rows.into_iter().map(|(_, row)| row).collect()
    }

    /// Compares anchors: a cursor on a folded file's header resolves to the whole folded file.
    fn has_visual_selection(&self, cx: &App) -> bool {
        let editor = self
            .diff
            .read(cx)
            .editor()
            .read(cx)
            .focused_editor()
            .read(cx);
        let selection = editor.selections.newest_anchor();
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        editor.selections.line_mode() || selection.start.cmp(&selection.end, &snapshot).is_ne()
    }

    /// Vim enters visual mode when its editor gains focus with a non-empty selection, and a
    /// cursor on a folded (reviewed) file's header resolves to the whole folded file. That
    /// cursor is a single position, so vim goes back to normal mode.
    fn stay_in_normal_mode_on_folded_header(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_visual_selection(cx) {
            return;
        }
        let editor = self
            .diff
            .read(cx)
            .editor()
            .read(cx)
            .focused_editor()
            .clone();
        let on_folded_header = editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            !editor.selections.newest::<Point>(&snapshot).is_empty()
        });
        if on_folded_header
            && let Some(action) = cx.build_action("vim::SwitchToNormalMode", None).log_err()
        {
            window.dispatch_action(action, cx);
        }
    }

    fn reviews(&self, target: &ReviewTarget, repo: &Entity<Repository>) -> bool {
        self.target.is_same_review(target) && self.repo.entity_id() == repo.entity_id()
    }
}

impl EventEmitter<EditorEvent> for ReviewView {}

/// Sets up a side of the review's diff (the left side on every split). The review's comment
/// button takes the place of the gutter's hover button for breakpoints and bookmarks, and file
/// headers read as paths: directory, then name.
fn configure_side(editor: &Entity<Editor>, cx: &mut App) {
    editor.update(cx, |editor, cx| {
        editor.set_show_breakpoints(false, cx);
        editor.set_show_bookmarks(false, cx);
        editor.set_file_header_path_first(true, cx);
    });
}

/// Puts a new left side's cursor on the row facing the right side's cursor, as the split
/// editor does whenever the right side's cursor moves. A new left side otherwise keeps its own
/// position until then (the end of the diff), and `ctrl-w h` would jump there. Changing the
/// selections directly has no selection effects, so nothing mirrors back onto the right side.
fn face_cursor(rhs_editor: &Entity<Editor>, lhs_editor: &Entity<Editor>, cx: &mut App) {
    let display_point = rhs_editor.update(cx, |editor, cx| {
        let snapshot = editor.display_snapshot(cx);
        let head = editor
            .selections
            .newest_anchor()
            .head()
            .to_point(snapshot.buffer_snapshot());
        snapshot.point_to_display_point(head, Bias::Right)
    });
    lhs_editor.update(cx, |editor, cx| {
        let snapshot = editor.display_snapshot(cx);
        let display_point = snapshot.clip_point(display_point, Bias::Right);
        let point = snapshot.display_point_to_point(display_point, Bias::Right);
        editor.selections.change_with(&snapshot, |selections| {
            selections.select_ranges([point..point])
        });
        cx.notify();
    });
}

impl Focusable for ReviewView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.diff.read(cx).focus_handle(cx)
    }
}

impl Render for ReviewView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_branch_review = matches!(self.target, ReviewTarget::Branch { .. });
        let has_top_block = self.top_block.read(cx).has_content();
        // Without rows (loading, no changes, every file hidden) the diff shows a placeholder
        // instead of its editor, which hosts the overview otherwise.
        let overview_above_placeholder =
            self.top_block_id.is_some() && self.diff.read(cx).multibuffer().read(cx).is_empty();
        v_flex()
            .key_context("CodeReview")
            .capture_action(|_: &git::ToggleStaged, window, cx| block_write_action(window, cx))
            .capture_action(|_: &git::StageAndNext, window, cx| block_write_action(window, cx))
            .capture_action(|_: &git::UnstageAndNext, window, cx| block_write_action(window, cx))
            .capture_action(|_: &ReloadFile, window, cx| block_write_action(window, cx))
            .capture_action(cx.listener(Self::toggle_soft_wrap))
            .capture_action(cx.listener(Self::open_excerpts))
            .on_action(cx.listener(Self::next_file))
            .on_action(cx.listener(Self::previous_file))
            .on_action(cx.listener(Self::next_unreviewed_file))
            .on_action(cx.listener(Self::previous_unreviewed_file))
            .on_action(cx.listener(Self::toggle_single_file_mode))
            .on_action(cx.listener(Self::open_file_at_cursor))
            .on_action(cx.listener(Self::toggle_file_reviewed))
            .on_action(cx.listener(Self::toggle_hunk_reviewed))
            .on_action(cx.listener(Self::next_hunk))
            .on_action(cx.listener(Self::previous_hunk))
            .on_action(cx.listener(Self::add_comment))
            .on_action(cx.listener(Self::add_file_comment))
            .on_action(cx.listener(Self::add_review_comment))
            .on_action(cx.listener(Self::edit_comment))
            .on_action(cx.listener(Self::delete_comment))
            .on_action(cx.listener(Self::next_comment))
            .on_action(cx.listener(Self::previous_comment))
            .on_action(cx.listener(Self::copy_comment_at_cursor))
            .on_action(cx.listener(Self::copy_selected_lines))
            .when(is_branch_review, |view| {
                view.on_action(cx.listener(Self::change_base_action))
            })
            .when(has_top_block, |view| {
                view.on_action(cx.listener(Self::toggle_header))
            })
            .size_full()
            .map(|view| {
                if overview_above_placeholder {
                    // One scroll for both, as in the diff.
                    view.child(
                        v_flex()
                            .id("review-overview-without-diff")
                            .debug_selector(|| "review-overview-without-diff".into())
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .child(self.top_block.clone())
                            .child(div().flex_1().min_h(rems(12.)).child(self.diff.clone())),
                    )
                } else {
                    view.child(div().flex_1().min_h_0().child(self.diff.clone()))
                }
            })
    }
}

impl Item for ReviewView {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ListTodo).color(Color::Muted))
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        let text: SharedString = match &self.target {
            // "PR #N: …" already says what the tab is.
            ReviewTarget::PullRequest(_) => truncate_and_trailoff(&self.title(cx), 64).into(),
            ReviewTarget::Uncommitted | ReviewTarget::Branch { .. } | ReviewTarget::Commits(_) => {
                format!("Review: {}", self.title(cx)).into()
            }
        };
        with_repository_name(self.repository_name(cx), text)
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        let mut title = match &self.target {
            ReviewTarget::PullRequest(target) => {
                format!("Review: {} ({})", self.title(cx), target.repo)
            }
            ReviewTarget::Commits(_) => match self.top_block.read(cx).commits_tooltip() {
                Some(description) => format!("Review: {description}"),
                None => format!("Review: {}", self.title(cx)),
            },
            ReviewTarget::Uncommitted | ReviewTarget::Branch { .. } => {
                format!("Review: {}", self.title(cx))
            }
        };
        if self.repository_name(cx).is_some() {
            title.push_str(&format!(
                "\n{}",
                self.repo.read(cx).work_directory_abs_path.display()
            ));
        }
        let title = SharedString::from(title);
        Some(if self.diff.read(cx).is_snapshot(cx) {
            format!("{title}\nRead-only snapshot (no language server)").into()
        } else {
            title
        })
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.diff
            .update(cx, |diff, cx| diff.deactivated(window, cx));
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.diff
            .update(cx, |diff, cx| diff.navigate(data, window, cx))
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.diff.read(cx).for_each_project_item(cx, f)
    }

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        self.diff.read(cx).active_project_path(cx)
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff
            .update(cx, |diff, cx| diff.set_nav_history(nav_history, cx));
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<workspace::WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>>
    where
        Self: Sized,
    {
        let Some(workspace) = self.workspace.upgrade() else {
            return Task::ready(None);
        };
        let repo = self.repo.clone();
        let target = self.target.clone();
        let project = self.project.clone();
        // Deciding live or snapshot again could disagree with this tab once HEAD has moved.
        let commits = self.diff.read(cx).commit_list().cloned();
        Task::ready(Some(cx.new(|cx| {
            Self::new(target, repo, project, commits, workspace, window, cx)
        })))
    }

    /// The diff is read-only, but while a comment is edited the tab takes vim's `:w`, which
    /// saves the active item, and only an editable one.
    fn capability(&self, cx: &App) -> Capability {
        if self.comments.read(cx).is_editing(cx) {
            Capability::ReadWrite
        } else {
            Capability::ReadOnly
        }
    }

    fn can_save(&self, cx: &App) -> bool {
        self.comments.read(cx).is_editing(cx)
    }

    // Never dirty: autosave would save comments behind the user's back, and `:q!` couldn't
    // discard them.
    fn save(
        &mut self,
        _: SaveOptions,
        _: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        self.comments
            .update(cx, |comments, cx| comments.save_focused(window, cx));
        Task::ready(Ok(()))
    }

    fn as_searchable(&self, _: &Entity<Self>, cx: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.diff.read(cx).editor().clone()))
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(
                self.diff
                    .read(cx)
                    .editor()
                    .read(cx)
                    .rhs_editor()
                    .clone()
                    .into(),
            )
        } else if type_id == TypeId::of::<SplittableEditor>() {
            Some(self.diff.read(cx).editor().clone().into())
        } else if type_id == TypeId::of::<ReviewDiff>() {
            Some(self.diff.clone().into())
        } else {
            None
        }
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff.update(cx, |diff, cx| {
            diff.added_to_workspace(workspace, window, cx)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor::test::editor_test_context::assert_state_with_diff;
    use gpui::{Action as _, TestAppContext};
    use project::FakeFs;
    use serde_json::json;
    use settings::{DiffViewStyle, SettingsStore};
    use std::path::Path;
    use unindent::Unindent as _;
    use util::path;
    use workspace::MultiWorkspace;

    #[test]
    fn test_commits_banner() {
        use crate::test_support::{C1, C2, oid};
        let range = CommitRange {
            base: Some(oid(C1)),
            head: oid(C2),
        };
        let shas = |count: usize| (1..=count).map(|n| format!("c{n}")).collect::<Vec<_>>();
        assert_eq!(
            commits_banner(Some(shas(1).as_slice()), &range),
            "Reviewing commit: c1"
        );
        assert_eq!(
            commits_banner(Some(shas(10).as_slice()), &range),
            "Reviewing commits: c1, c2, c3, c4, c5, c6, c7, c8, c9, c10"
        );
        let ends = format!("{}..{}", oid(C1).display_short(), oid(C2).display_short());
        assert_eq!(
            commits_banner(Some(shas(100).as_slice()), &range),
            format!("Reviewing 100 commits: {ends}")
        );
        assert_eq!(
            commits_banner(None, &range),
            format!("Reviewing commits: {ends}")
        );
        let from_root = CommitRange {
            base: None,
            head: oid(C2),
        };
        assert_eq!(
            commits_banner(None, &from_root),
            format!("Reviewing commits up to {}", oid(C2).display_short())
        );
    }

    #[gpui::test]
    async fn test_review_uncommitted_shows_head_to_working_tree(cx: &mut TestAppContext) {
        crate::init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "added.txt": "new file\n",
                "modified.txt": "one\nTWO\nthree\n",
                "staged.txt": "alpha\nbeta\n",
                "src": {
                    "nested.rs": "fn a() {}\nfn b() {}\n",
                },
            }),
        )
        .await;
        let dot_git = Path::new(path!("/project/.git"));
        fs.set_head_for_repo(
            dot_git,
            &[
                ("deleted.txt", "gone\n".into()),
                ("modified.txt", "one\ntwo\nthree\n".into()),
                ("staged.txt", "alpha\n".into()),
                ("src/nested.rs", "fn a() {}\n".into()),
            ],
            "deadbeef",
        );
        fs.set_index_for_repo(
            dot_git,
            &[
                ("deleted.txt", "gone\n".into()),
                ("modified.txt", "one\ntwo\nthree\n".into()),
                ("staged.txt", "alpha\nbeta\n".into()),
                ("src/nested.rs", "fn a() {}\n".into()),
            ],
        );

        let project = Project::test(fs.clone(), [Path::new(path!("/project"))], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        cx.run_until_parked();

        cx.focus(&workspace);
        cx.update(|window, cx| window.dispatch_action(ReviewUncommitted.boxed_clone(), cx));
        cx.run_until_parked();

        let review = workspace.update(cx, |workspace, cx| {
            let active_item = workspace.active_item(cx).unwrap();
            assert_eq!(
                active_item.tab_content_text(0, cx),
                "Review: Uncommitted changes"
            );
            workspace.active_item_as::<ReviewView>(cx).unwrap()
        });
        let diff = review.read_with(cx, |review, _| review.diff().clone());
        let editor = diff.read_with(cx, |diff, cx| diff.editor().read(cx).rhs_editor().clone());

        // Staged and unstaged changes, untracked files and deletions, in tree order.
        assert_eq!(
            diff.read_with(cx, |diff, cx| diff.excerpt_file_paths(cx)),
            [
                "src/nested.rs",
                "added.txt",
                "deleted.txt",
                "modified.txt",
                "staged.txt"
            ]
        );
        assert_state_with_diff(
            &editor,
            cx,
            &"
                  ˇfn a() {}
                + fn b() {}

                + new file

                - gone

                  one
                - two
                + TWO
                  three

                  alpha
                + beta
            "
            .unindent(),
        );

        // The review is read-only, but the project buffers behind it stay writable.
        editor.update_in(cx, |editor, window, cx| {
            assert!(editor.read_only(cx));
            assert!(editor.buffer().read(cx).read_only());
            editor.insert("typed", window, cx);
        });
        let modified_buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/project/modified.txt"), cx)
            })
            .await
            .unwrap();
        modified_buffer.read_with(cx, |buffer, _| {
            assert_eq!(buffer.capability(), Capability::ReadWrite);
            assert_eq!(buffer.text(), "one\nTWO\nthree\n");
            assert!(!buffer.is_dirty());
        });

        // Reviewing the same target again reuses the open tab.
        cx.update(|window, cx| window.dispatch_action(ReviewUncommitted.boxed_clone(), cx));
        cx.run_until_parked();
        workspace.update(cx, |workspace, cx| {
            assert_eq!(workspace.items_of_type::<ReviewView>(cx).count(), 1);
            assert_eq!(workspace.active_item_as::<ReviewView>(cx).unwrap(), review);
        });

        // The diff follows changes to the working tree.
        fs.insert_file(path!("/project/later.txt"), "later\n".into())
            .await;
        fs.insert_file(path!("/project/modified.txt"), "one\ntwo\nthree\n".into())
            .await;
        cx.run_until_parked();
        assert_eq!(
            diff.read_with(cx, |diff, cx| diff.excerpt_file_paths(cx)),
            [
                "src/nested.rs",
                "added.txt",
                "deleted.txt",
                "later.txt",
                "staged.txt"
            ]
        );
    }

    #[gpui::test]
    async fn test_review_follows_split_diff_view_style(cx: &mut TestAppContext) {
        crate::init_test(cx);
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.editor.diff_view_style = Some(DiffViewStyle::Split);
                });
            });
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "modified.txt": "one\nTWO\nthree\n",
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            Path::new(path!("/project/.git")),
            &[("modified.txt", "one\ntwo\nthree\n".into())],
        );

        let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        cx.run_until_parked();

        cx.focus(&workspace);
        cx.update(|window, cx| window.dispatch_action(ReviewUncommitted.boxed_clone(), cx));
        cx.run_until_parked();

        let review = workspace.update(cx, |workspace, cx| {
            workspace.active_item_as::<ReviewView>(cx).unwrap()
        });
        review.read_with(cx, |review, cx| {
            let editor = review.diff().read(cx).editor().read(cx);
            assert!(editor.is_split());
            assert!(editor.rhs_editor().read(cx).read_only(cx));
            assert!(editor.lhs_editor().unwrap().read(cx).read_only(cx));
        });
    }

    #[gpui::test]
    async fn test_review_uncommitted_without_repository_opens_nothing(cx: &mut TestAppContext) {
        crate::init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ "file.txt": "text\n" }))
            .await;
        let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        cx.run_until_parked();

        cx.focus(&workspace);
        cx.update(|window, cx| window.dispatch_action(ReviewUncommitted.boxed_clone(), cx));
        cx.run_until_parked();

        workspace.update(cx, |workspace, cx| {
            assert_eq!(workspace.items_of_type::<ReviewView>(cx).count(), 0);
            assert!(workspace.active_item(cx).is_none());
        });
    }

    /// With several repositories in the project, review tabs say which one they review.
    #[gpui::test]
    async fn test_titles_name_the_repository_among_several(cx: &mut TestAppContext) {
        use workspace::item::Item as _;

        crate::init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "one": { ".git": {}, "a.txt": "A\n" },
                "two": { ".git": {}, "b.txt": "B\n" },
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            Path::new(path!("/root/one/.git")),
            &[("a.txt", "a\n".into())],
        );
        fs.set_head_and_index_for_repo(
            Path::new(path!("/root/two/.git")),
            &[("b.txt", "b\n".into())],
        );
        let project = Project::test(
            fs,
            [Path::new(path!("/root/one")), Path::new(path!("/root/two"))],
            cx,
        )
        .await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        cx.run_until_parked();
        cx.focus(&workspace);
        cx.update(|window, cx| window.dispatch_action(ReviewUncommitted.boxed_clone(), cx));
        cx.run_until_parked();

        let review = workspace.update(cx, |workspace, cx| {
            workspace.active_item_as::<ReviewView>(cx).unwrap()
        });
        review.read_with(cx, |review, cx| {
            let name = review.repo.read(cx).display_name();
            assert!(name == "one" || name == "two", "{name}");
            assert_eq!(
                review.tab_content_text(0, cx),
                format!("{name} \u{b7} Review: Uncommitted changes")
            );
            assert_eq!(
                review.title_with_repository(cx),
                format!("{name} \u{b7} Uncommitted changes")
            );
            assert!(
                review
                    .tab_tooltip_text(cx)
                    .unwrap()
                    .ends_with(&format!("{name}"))
            );
        });
    }

    /// A repository added to the project renames the open reviews at once, which redraws the
    /// panel's header too (R1: it kept the plain title until something else redrew it).
    #[gpui::test]
    async fn test_titles_name_the_repository_once_another_is_added(cx: &mut TestAppContext) {
        use std::{cell::Cell, rc::Rc};

        crate::init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "one": { ".git": {}, "a.txt": "A\n" },
                "two": { ".git": {}, "b.txt": "B\n" },
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            Path::new(path!("/root/one/.git")),
            &[("a.txt", "a\n".into())],
        );
        let project = Project::test(fs, [Path::new(path!("/root/one"))], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        cx.run_until_parked();
        cx.focus(&workspace);
        cx.update(|window, cx| window.dispatch_action(ReviewUncommitted.boxed_clone(), cx));
        cx.run_until_parked();
        let review = workspace.update(cx, |workspace, cx| {
            workspace.active_item_as::<ReviewView>(cx).unwrap()
        });
        assert_eq!(
            review.read_with(cx, |review, cx| review.title_with_repository(cx)),
            "Uncommitted changes"
        );

        let renamed = Rc::new(Cell::new(0));
        let _subscription = cx.update(|_, cx| {
            let renamed = renamed.clone();
            cx.subscribe(&review, move |_, event: &EditorEvent, _| {
                if matches!(event, EditorEvent::TitleChanged) {
                    renamed.set(renamed.get() + 1);
                }
            })
        });
        project
            .update(cx, |project, cx| {
                project.find_or_create_worktree(path!("/root/two"), true, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();
        assert!(renamed.get() > 0);
        assert_eq!(
            review.read_with(cx, |review, cx| review.title_with_repository(cx)),
            "one \u{b7} Uncommitted changes"
        );
    }

    mod targets {
        use super::*;
        use crate::{
            ReviewBranch, ReviewCommits, ToggleHeader,
            diff::DiffSource,
            target::CommitRange,
            test_support::{
                C1, C2, C3, active_review, commit_data, dispatch, dot_git, excerpt_paths,
                insert_commit_ref, oid, open_workspace, repository, review_count, tab_title,
            },
        };
        use gpui::VisualTestContext;
        use project::git_store::diff_buffer_list::DiffBase;

        /// Error toasts share one id, so each check dismisses them.
        fn take_notifications(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> usize {
            workspace.update(cx, |workspace, cx| {
                let ids = workspace.notification_ids();
                for id in &ids {
                    workspace.dismiss_notification(id, cx);
                }
                ids.len()
            })
        }

        fn diff_base(review: &Entity<ReviewView>, cx: &mut VisualTestContext) -> DiffBase {
            review.read_with(cx, |review, cx| match review.diff().read(cx).source() {
                DiffSource::BufferList(list) => list.read(cx).diff_base().clone(),
                DiffSource::Commits(_) => panic!("not a branch review"),
            })
        }

        /// A `feature` branch whose merge base with `origin/main` differs from the working tree
        /// in committed, uncommitted and untracked ways.
        async fn branch_fixture(
            cx: &mut TestAppContext,
        ) -> (Arc<FakeFs>, Entity<Workspace>, &mut VisualTestContext) {
            let fs = FakeFs::new(cx.executor());
            fs.insert_tree(
                path!("/project"),
                json!({
                    ".git": {},
                    "both.txt": "a\nB\nc\nD\n",
                    "committed.txt": "one\nTWO\nthree\n",
                    "untracked.txt": "new\n",
                }),
            )
            .await;
            fs.set_merge_base_content_for_repo(
                dot_git(),
                &[
                    ("both.txt", "a\nb\nc\nd\n".into()),
                    ("committed.txt", "one\ntwo\nthree\n".into()),
                    ("deleted.txt", "gone\n".into()),
                ],
            );
            fs.set_head_and_index_for_repo(
                dot_git(),
                &[
                    ("both.txt", "a\nB\nc\nd\n".into()),
                    ("committed.txt", "one\nTWO\nthree\n".into()),
                ],
            );
            fs.set_branch_name(dot_git(), Some("feature"));
            insert_commit_ref(&fs, "origin/main", C1);
            insert_commit_ref(&fs, "main", C2);
            let (workspace, cx) = open_workspace(fs.clone(), cx).await;
            (fs, workspace, cx)
        }

        #[gpui::test]
        async fn test_review_branch_shows_merge_base_to_working_tree(cx: &mut TestAppContext) {
            crate::init_test(cx);
            let (fs, workspace, cx) = branch_fixture(cx).await;

            dispatch(ReviewBranch::default(), cx);
            let review = active_review(&workspace, cx);
            assert_eq!(tab_title(&review, cx), "Review: feature vs origin/main");
            assert_eq!(
                review.read_with(cx, |review, _| review.target().clone()),
                ReviewTarget::branch("origin/main").unwrap()
            );
            // Committed, uncommitted and untracked changes since the merge base, and deletions.
            assert_eq!(
                excerpt_paths(&review, cx),
                ["both.txt", "committed.txt", "deleted.txt", "untracked.txt"]
            );
            let editor = review.read_with(cx, |review, cx| {
                review
                    .diff()
                    .read(cx)
                    .editor()
                    .read(cx)
                    .rhs_editor()
                    .clone()
            });
            assert_state_with_diff(
                &editor,
                cx,
                &"
                      ˇa
                    - b
                    + B
                      c
                    - d
                    + D

                      one
                    - two
                    + TWO
                      three

                    - gone

                    + new
                "
                .unindent(),
            );
            review.read_with(cx, |review, cx| {
                assert!(matches!(
                    review.top_block().read(cx).content(),
                    TopBlockContent::Branch { branch, base }
                        if branch == "feature" && base == "origin/main"
                ));
            });

            // The same review, with the default base or the same base given explicitly.
            dispatch(ReviewBranch::default(), cx);
            dispatch(
                ReviewBranch {
                    base: Some("origin/main".into()),
                },
                cx,
            );
            assert_eq!(review_count(&workspace, cx), 1);
            assert_eq!(active_review(&workspace, cx), review);

            // A base that isn't one commit is an error, not an empty review. The fake repository
            // answers like `git cat-file --batch-check` would.
            fs.with_git_state(dot_git(), true, |state| {
                state.refs.insert("abc1".into(), "abc1 ambiguous".into());
                state
                    .refs
                    .insert("abc1^{commit}".into(), "abc1^{commit} ambiguous".into());
                state.refs.insert("HEAD:both.txt".into(), C3.into());
            })
            .unwrap();
            assert_eq!(take_notifications(&workspace, cx), 0);
            for base in ["nope", "abc1", "HEAD:both.txt", "--output=/tmp/x", "a b"] {
                dispatch(
                    ReviewBranch {
                        base: Some(base.into()),
                    },
                    cx,
                );
                assert_eq!(review_count(&workspace, cx), 1, "{base:?}");
                assert_eq!(take_notifications(&workspace, cx), 1, "{base:?}");
            }

            dispatch(
                ReviewBranch {
                    base: Some("main".into()),
                },
                cx,
            );
            assert_eq!(review_count(&workspace, cx), 2);
            let main_review = active_review(&workspace, cx);
            assert_eq!(tab_title(&main_review, cx), "Review: feature vs main");
        }

        #[gpui::test]
        async fn test_comments_survive_review_key_switches(cx: &mut TestAppContext) {
            use crate::test_support::{move_to_row_with_text, rhs_editor};

            crate::test_support::init_vim_test(cx, DiffViewStyle::Unified);
            let (fs, workspace, cx) = branch_fixture(cx).await;
            dispatch(ReviewBranch::default(), cx);
            let review = active_review(&workspace, cx);
            cx.focus(&review);
            cx.run_until_parked();
            let rhs = rhs_editor(&review, cx);
            let bodies = |cx: &mut VisualTestContext| {
                review.read_with(cx, |review, cx| {
                    review
                        .session()
                        .read(cx)
                        .comments()
                        .iter()
                        .map(|comment| comment.body.clone())
                        .collect::<Vec<_>>()
                })
            };
            let hosted = |cx: &mut VisualTestContext| {
                let blocks = review.read_with(cx, |review, _| review.comments().clone());
                cx.update(|_, cx| blocks.update(cx, |blocks, cx| blocks.hosted_rows(cx).len()))
            };

            move_to_row_with_text(&rhs, "TWO", cx);
            cx.simulate_keystrokes("c");
            cx.simulate_input("saved");
            cx.simulate_keystrokes("ctrl-enter");
            // This one is still being written when the key changes.
            move_to_row_with_text(&rhs, "B", cx);
            cx.simulate_keystrokes("c");
            cx.simulate_input("open");

            let change_base = |base: &str, cx: &mut VisualTestContext| {
                let base = BranchBase::new(base.to_string()).unwrap();
                review.update_in(cx, |review, window, cx| {
                    review.change_base(base, window, cx)
                });
                cx.run_until_parked();
            };
            change_base("main", cx);
            assert!(bodies(cx).is_empty());
            assert_eq!(hosted(cx), 0);
            change_base("origin/main", cx);
            assert_eq!(bodies(cx), ["saved", "open"]);
            assert_eq!(hosted(cx), 2);

            // A branch switch and back.
            fs.set_branch_name(dot_git(), Some("other"));
            cx.run_until_parked();
            assert_eq!(tab_title(&review, cx), "Review: other vs origin/main");
            assert!(bodies(cx).is_empty());
            fs.set_branch_name(dot_git(), Some("feature"));
            cx.run_until_parked();
            assert_eq!(bodies(cx), ["saved", "open"]);
            assert_eq!(hosted(cx), 2);
        }

        #[gpui::test]
        async fn test_detached_head_reviews_are_kept_per_worktree(cx: &mut TestAppContext) {
            crate::init_test(cx);
            let (fs, workspace, cx) = branch_fixture(cx).await;
            fs.set_branch_name(dot_git(), None::<&str>);
            cx.run_until_parked();
            dispatch(ReviewBranch::default(), cx);
            let review = active_review(&workspace, cx);
            assert_eq!(tab_title(&review, cx), "Review: HEAD vs origin/main");
            assert_eq!(
                review.read_with(cx, |review, cx| review.session().read(cx).key().clone()),
                crate::ReviewKey(format!("branch:origin/main:HEAD:{}", path!("/project")).into())
            );
        }

        #[gpui::test]
        async fn test_branch_review_says_why_it_cant_compare(cx: &mut TestAppContext) {
            crate::init_test(cx);
            let fs = FakeFs::new(cx.executor());
            fs.insert_tree(path!("/project"), json!({ ".git": {}, "a.txt": "a\n" }))
                .await;
            fs.set_head_and_index_for_repo(dot_git(), &[("a.txt", "a\n".into())]);
            fs.set_branch_name(dot_git(), Some("feature"));
            let (workspace, cx) = open_workspace(fs.clone(), cx).await;
            let empty_label = |review: &Entity<ReviewView>, cx: &mut VisualTestContext| {
                review.read_with(cx, |review, cx| {
                    let diff = review.diff().read(cx);
                    assert!(!diff.is_loading(cx));
                    diff.empty_label(cx).to_string()
                })
            };

            // The default branch isn't checked before the review opens, and the fake repository
            // doesn't know it: the review says so instead of "No changes against origin/main".
            dispatch(ReviewBranch::default(), cx);
            let review = active_review(&workspace, cx);
            assert_eq!(empty_label(&review, cx), "Unknown revision origin/main");

            insert_commit_ref(&fs, "main", C2);
            cx.focus(&review);
            workspace.update_in(cx, |workspace, window, cx| {
                let base = BranchBase::new("main").unwrap();
                ReviewView::select_base(review.clone(), base, workspace, window, cx)
            });
            cx.run_until_parked();
            assert_eq!(empty_label(&review, cx), "No changes against main");
        }

        #[gpui::test]
        async fn test_change_base_in_place_and_reuse(cx: &mut TestAppContext) {
            crate::init_test(cx);
            let (_fs, workspace, cx) = branch_fixture(cx).await;
            dispatch(ReviewBranch::default(), cx);
            let review = active_review(&workspace, cx);

            // Change Base opens Zed's branch picker.
            cx.focus(&review);
            dispatch(crate::ChangeBase, cx);
            workspace.read_with(cx, |workspace, cx| {
                assert!(
                    workspace
                        .active_modal::<git_ui::branch_picker::BranchList>(cx)
                        .is_some()
                );
            });
            dispatch(menu::Cancel, cx);

            workspace.update_in(cx, |workspace, window, cx| {
                let base = BranchBase::new("main").unwrap();
                ReviewView::select_base(review.clone(), base, workspace, window, cx)
            });
            cx.run_until_parked();
            assert_eq!(
                review.read_with(cx, |review, _| review.target().clone()),
                ReviewTarget::branch("main").unwrap()
            );
            assert_eq!(tab_title(&review, cx), "Review: feature vs main");
            assert_eq!(
                diff_base(&review, cx),
                DiffBase::Merge {
                    base_ref: "main".into()
                }
            );
            assert_eq!(
                excerpt_paths(&review, cx),
                ["both.txt", "committed.txt", "deleted.txt", "untracked.txt"]
            );

            // Choosing a base another tab already reviews against activates that tab.
            let repo = repository(&workspace, cx);
            let other = workspace.update_in(cx, |workspace, window, cx| {
                ReviewView::deploy_in_repo(
                    workspace,
                    ReviewTarget::branch("origin/main").unwrap(),
                    repo,
                    window,
                    cx,
                )
            });
            cx.run_until_parked();
            assert_ne!(other, review);
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.activate_item(&review, true, true, window, cx);
                let base = BranchBase::new("origin/main").unwrap();
                ReviewView::select_base(review.clone(), base, workspace, window, cx)
            });
            cx.run_until_parked();
            assert_eq!(active_review(&workspace, cx), other);
            assert_eq!(
                review.read_with(cx, |review, _| review.target().clone()),
                ReviewTarget::branch("main").unwrap()
            );
            assert_eq!(review_count(&workspace, cx), 2);
        }

        /// `zed --review branch --wait` waits for the review its tab shows, also after the tab
        /// moved to another review key.
        #[gpui::test]
        async fn test_waiters_follow_key_switch(cx: &mut TestAppContext) {
            crate::init_test(cx);
            let (_fs, workspace, cx) = branch_fixture(cx).await;
            dispatch(ReviewBranch::default(), cx);
            let review = active_review(&workspace, cx);
            let session = review.read_with(cx, |review, _| review.session().clone());
            let outcome = session.update(cx, |session, _| session.wait_for_outcome());

            workspace.update_in(cx, |workspace, window, cx| {
                let base = BranchBase::new("main").unwrap();
                ReviewView::select_base(review.clone(), base, workspace, window, cx)
            });
            cx.run_until_parked();
            let new_session = review.read_with(cx, |review, _| review.session().clone());
            assert_ne!(new_session, session);
            assert!(!session.update(cx, |session, _| session.has_outcome_waiters()));
            assert!(new_session.update(cx, |session, _| session.has_outcome_waiters()));

            workspace.update(cx, |workspace, cx| {
                crate::submit::finish_local_review(workspace, &review, cx)
            });
            assert_eq!(
                futures::FutureExt::now_or_never(outcome),
                Some(Ok(crate::ReviewOutcome::Finished { markdown: None }))
            );
        }

        /// `--wait` waits for the review a tab shows. A split clone changing its base leaves the
        /// waiter with the original tab, which still shows the review; once no tab shows it (the
        /// clone keeps it only to switch back), the wait ends as closed.
        #[gpui::test]
        async fn test_waiters_stay_with_the_tab_showing_their_review(cx: &mut TestAppContext) {
            crate::init_test(cx);
            let (_fs, workspace, cx) = branch_fixture(cx).await;
            dispatch(ReviewBranch::default(), cx);
            let review = active_review(&workspace, cx);
            let session = review.read_with(cx, |review, _| review.session().clone());
            let mut outcome = session.update(cx, |session, _| session.wait_for_outcome());

            let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
            let split = workspace.update_in(cx, |workspace, window, cx| {
                workspace.split_and_clone(pane, workspace::SplitDirection::Right, window, cx)
            });
            cx.run_until_parked();
            drop(split);
            let clone = active_review(&workspace, cx);
            assert_ne!(clone, review);
            assert_eq!(session.read_with(cx, |session, _| session.shown_by()), 2);

            workspace.update_in(cx, |workspace, window, cx| {
                let base = BranchBase::new("main").unwrap();
                ReviewView::select_base(clone.clone(), base, workspace, window, cx)
            });
            cx.run_until_parked();
            let clone_session = clone.read_with(cx, |review, _| review.session().clone());
            assert_ne!(clone_session, session);
            assert_eq!(session.read_with(cx, |session, _| session.shown_by()), 1);
            assert!(session.update(cx, |session, _| session.has_outcome_waiters()));
            assert!(!clone_session.update(cx, |session, _| session.has_outcome_waiters()));
            assert_eq!(outcome.try_recv(), Ok(None));

            // The original tab closes; the clone still holds the session to switch back to.
            let pane = workspace.read_with(cx, |workspace, _| workspace.pane_for(&review).unwrap());
            let close = pane.update_in(cx, |pane, window, cx| {
                pane.close_item_by_id(review.entity_id(), workspace::SaveIntent::Skip, window, cx)
            });
            cx.run_until_parked();
            drop((close, review));
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
            assert_eq!(session.read_with(cx, |session, _| session.shown_by()), 0);
            assert_eq!(outcome.try_recv(), Err(futures::channel::oneshot::Canceled));

            // Switching back shows it again, for a new waiter.
            let outcome = session.update(cx, |session, _| session.wait_for_outcome());
            workspace.update_in(cx, |workspace, window, cx| {
                let base = BranchBase::new("origin/main").unwrap();
                ReviewView::select_base(clone.clone(), base, workspace, window, cx)
            });
            cx.run_until_parked();
            assert_eq!(
                clone.read_with(cx, |review, _| review.session().clone()),
                session
            );
            workspace.update(cx, |workspace, cx| {
                crate::submit::finish_local_review(workspace, &clone, cx)
            });
            assert_eq!(
                futures::FutureExt::now_or_never(outcome),
                Some(Ok(crate::ReviewOutcome::Finished { markdown: None }))
            );
        }

        #[gpui::test]
        async fn test_commit_review_reuse_titles_and_top_block(cx: &mut TestAppContext) {
            crate::init_test(cx);
            let fs = FakeFs::new(cx.executor());
            fs.insert_tree(path!("/project"), json!({ ".git": {}, "a.txt": "three\n" }))
                .await;
            fs.set_merge_base_content_for_repo(dot_git(), &[("a.txt", "one\n".into())]);
            fs.set_head_and_index_for_repo(dot_git(), &[("a.txt", "three\n".into())]);
            fs.with_git_state(dot_git(), true, |state| {
                state.refs.insert("HEAD".into(), C3.into());
            })
            .unwrap();
            fs.set_commit_data(
                dot_git(),
                [
                    (commit_data(C1, None, "Start"), false),
                    (
                        commit_data(C2, Some(C1), "Write two\n\nThe second line."),
                        false,
                    ),
                    (
                        commit_data(C3, Some(C2), "Write three, which is a long subject"),
                        false,
                    ),
                ],
            );
            for sha in [C1, C2, C3] {
                insert_commit_ref(&fs, sha, sha);
            }
            let (workspace, cx) = open_workspace(fs.clone(), cx).await;

            let range = format!("{C1}..{C3}");
            dispatch(
                ReviewCommits {
                    commits: Some(range.clone()),
                },
                cx,
            );
            let review = active_review(&workspace, cx);
            assert_eq!(
                review.read_with(cx, |review, _| review.target().clone()),
                ReviewTarget::Commits(CommitRange {
                    base: Some(oid(C1)),
                    head: oid(C3)
                })
            );
            assert_eq!(tab_title(&review, cx), "Review: 2 commits 2222222–3333333");
            review.read_with(cx, |review, cx| {
                let top_block = review.top_block().read(cx);
                let TopBlockContent::Commits {
                    commits: CommitsState::Loaded { commits, truncated },
                    ..
                } = top_block.content()
                else {
                    panic!("commits didn't load");
                };
                assert!(!truncated);
                let subjects = commits
                    .iter()
                    .map(|commit| commit.data.subject.to_string())
                    .collect::<Vec<_>>();
                assert_eq!(
                    subjects,
                    ["Write three, which is a long subject", "Write two"]
                );
                assert!(commits[0].body.is_none());
                assert_eq!(
                    commits[1].body.as_ref().unwrap().read(cx).source(),
                    "The second line."
                );
                assert!(!top_block.is_expanded());
            });
            cx.focus(&review);
            dispatch(ToggleHeader, cx);
            assert!(review.read_with(cx, |review, cx| review.top_block().read(cx).is_expanded()));

            dispatch(
                ReviewCommits {
                    commits: Some(range),
                },
                cx,
            );
            assert_eq!(review_count(&workspace, cx), 1);

            dispatch(
                ReviewCommits {
                    commits: Some(C3.into()),
                },
                cx,
            );
            assert_eq!(review_count(&workspace, cx), 2);
            let single = active_review(&workspace, cx);
            assert_eq!(
                single.read_with(cx, |review, _| review.target().clone()),
                ReviewTarget::Commits(CommitRange {
                    base: Some(oid(C2)),
                    head: oid(C3)
                })
            );
            assert_eq!(
                tab_title(&single, cx),
                "Review: 3333333 Write three, which i…"
            );

            // Typos, revisions that aren't commits and ranges that aren't consecutive commits are
            // errors.
            const MISSING: &str = "4444444444444444444444444444444444444444";
            const TREE: &str = "5555555555555555555555555555555555555555";
            fs.with_git_state(dot_git(), true, |state| {
                state.refs.insert(TREE.into(), TREE.into());
            })
            .unwrap();
            assert_eq!(take_notifications(&workspace, cx), 0);
            for commits in [
                "nope..HEAD".to_string(),
                "a b".to_string(),
                MISSING.to_string(),
                TREE.to_string(),
                format!("{TREE}..{C3}"),
                format!("{C3}..{C1}"),
                format!("{C2}..{C2}"),
            ] {
                dispatch(
                    ReviewCommits {
                        commits: Some(commits.clone()),
                    },
                    cx,
                );
                assert_eq!(take_notifications(&workspace, cx), 1, "{commits:?}");
            }
            assert_eq!(review_count(&workspace, cx), 2);
        }

        /// A branch review's overview (its base line) is the diff's first block: it stays
        /// above the first file when `r` folds that file, and while the filters hide every file
        /// it's drawn above the diff's placeholder, in one scroll, until they show again.
        #[gpui::test]
        async fn test_overview_above_folded_and_hidden_files(cx: &mut TestAppContext) {
            use crate::test_support::{init_vim_test, is_folded, rhs_editor};
            use editor::display_map::{Block, DisplayRow};

            init_vim_test(cx, DiffViewStyle::Unified);
            let (_fs, workspace, cx) = branch_fixture(cx).await;
            dispatch(ReviewBranch::default(), cx);
            let review = active_review(&workspace, cx);
            cx.focus(&review);
            cx.run_until_parked();
            let overview = review
                .read_with(cx, |review, _| review.top_block_id())
                .expect("the overview has a block");
            let rhs = rhs_editor(&review, cx);
            let first_blocks = |cx: &mut VisualTestContext| {
                rhs.update(cx, |editor, cx| {
                    let snapshot = editor.display_snapshot(cx);
                    snapshot
                        .blocks_in_range(DisplayRow(0)..snapshot.max_point().row())
                        .take(2)
                        .map(|(_, block)| match block {
                            Block::Custom(block) if block.id == overview => "overview",
                            Block::BufferHeader { .. } => "header",
                            Block::FoldedBuffer { .. } => "folded",
                            _ => "other",
                        })
                        .collect::<Vec<_>>()
                })
            };
            let draw = |cx: &mut VisualTestContext| {
                cx.run_until_parked();
                cx.update(|window, cx| {
                    window.refresh();
                    window.draw(cx).clear(cx);
                });
            };
            assert_eq!(first_blocks(cx), ["overview", "header"]);

            cx.simulate_keystrokes("g g r");
            assert!(is_folded(&review, "both.txt", cx));
            assert_eq!(first_blocks(cx), ["overview", "folded"]);
            assert_eq!(
                rhs.update(cx, |editor, cx| editor.scroll_position(cx).y),
                0.
            );

            let set_exclude = |pattern: &str, cx: &mut VisualTestContext| {
                review.update_in(cx, |review, window, cx| {
                    let filters = ReviewFilters {
                        exclude: filter_regex(pattern).unwrap(),
                        ..ReviewFilters::default()
                    };
                    review.set_filters(filters, window, cx);
                });
                draw(cx);
            };
            set_exclude(".", cx);
            assert!(review.read_with(cx, |review, cx| {
                review.diff().read(cx).multibuffer().read(cx).is_empty()
            }));
            assert!(cx.debug_bounds("review-overview").is_some());
            assert!(cx.debug_bounds("review-overview-without-diff").is_some());

            set_exclude("", cx);
            assert_eq!(first_blocks(cx)[0], "overview");
            assert!(cx.debug_bounds("review-overview").is_some());
            assert!(cx.debug_bounds("review-overview-without-diff").is_none());
        }
    }

    mod read_only {
        use super::*;
        use crate::test_support::{
            cursor_line, dispatch, dot_git, init_vim_test, lhs_editor, move_to_row_with_text,
            open_review_fixture, rhs_editor, splittable,
        };
        use fs::Fs as _;
        use gpui::{Action, VisualTestContext};
        use language::Buffer;

        fn index(fs: &FakeFs) -> HashMap<String, String> {
            fs.with_git_state(dot_git(), false, |state| {
                state
                    .index_contents
                    .iter()
                    .map(|(path, content)| {
                        let content = String::from_utf8_lossy(content).into_owned();
                        (path.as_unix_str().to_string(), content)
                    })
                    .collect()
            })
            .unwrap()
        }

        async fn open_buffer(
            workspace: &Entity<Workspace>,
            path: &str,
            cx: &mut VisualTestContext,
        ) -> Entity<Buffer> {
            let project = workspace.read_with(cx, |workspace, _| workspace.project().clone());
            project
                .update(cx, |project, cx| project.open_local_buffer(path, cx))
                .await
                .unwrap()
        }

        #[gpui::test]
        async fn test_staging_actions_do_nothing(cx: &mut TestAppContext) {
            init_vim_test(cx, DiffViewStyle::Unified);
            let (fs, workspace, review, cx) = open_review_fixture(cx).await;
            let editor = rhs_editor(&review, cx);
            let index_before = index(&fs);
            // Staging saves the dirty buffers under the cursor first.
            let buffer = open_buffer(&workspace, path!("/project/modified.txt"), cx).await;
            buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "unsaved ")], None, cx));
            cx.run_until_parked();
            cx.focus(&review);

            let actions: [Box<dyn Action>; 3] = [
                Box::new(git::ToggleStaged),
                Box::new(git::StageAndNext),
                Box::new(git::UnstageAndNext),
            ];
            for action in actions {
                move_to_row_with_text(&editor, "four", cx);
                cx.update(|window, cx| window.dispatch_action(action.boxed_clone(), cx));
                cx.run_until_parked();
                assert_eq!(index(&fs), index_before, "{}", action.name());
                assert!(buffer.read_with(cx, |buffer, _| buffer.is_dirty()));
            }

            // Vim's `d u`, `d shift-o` and `d shift-u`.
            for keystrokes in ["d u", "d shift-o", "d shift-u"] {
                move_to_row_with_text(&editor, "LINE 2", cx);
                cx.simulate_keystrokes(keystrokes);
                assert_eq!(index(&fs), index_before, "{keystrokes}");
                // The operator doesn't stay pending.
                cx.simulate_keystrokes("j");
                assert_eq!(cursor_line(&editor, cx), "line 3", "{keystrokes}");
            }
            assert_eq!(
                fs.load(path!("/project/modified.txt").as_ref())
                    .await
                    .unwrap(),
                "ONE\ntwo\nthree\nfour\n"
            );
        }

        #[gpui::test]
        async fn test_staging_on_the_old_side_does_nothing(cx: &mut TestAppContext) {
            init_vim_test(cx, DiffViewStyle::Split);
            let (fs, _workspace, review, cx) = open_review_fixture(cx).await;
            let lhs = lhs_editor(&review, cx);
            let index_before = index(&fs);
            cx.simulate_keystrokes("ctrl-w h");
            assert!(
                splittable(&review, cx).read_with(cx, |editor, _| editor.focused_editor() == &lhs)
            );

            move_to_row_with_text(&lhs, "one", cx);
            dispatch(git::StageAndNext, cx);
            dispatch(git::ToggleStaged, cx);
            cx.simulate_keystrokes("d u");
            assert_eq!(index(&fs), index_before);
        }

        #[gpui::test]
        async fn test_reload_keeps_unsaved_edits(cx: &mut TestAppContext) {
            init_vim_test(cx, DiffViewStyle::Unified);
            let (_fs, workspace, review, cx) = open_review_fixture(cx).await;
            let buffer = open_buffer(&workspace, path!("/project/modified.txt"), cx).await;
            buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "unsaved ")], None, cx));
            cx.run_until_parked();
            cx.focus(&review);

            // Vim's `:e` and `:e!` dispatch the same action.
            dispatch(ReloadFile, cx);
            buffer.read_with(cx, |buffer, _| {
                assert!(buffer.is_dirty());
                assert!(buffer.text().starts_with("unsaved ONE"));
            });
        }
    }

    mod navigation {
        use super::*;
        use crate::test_support::{
            cursor_line, cursor_point, init_vim_test, lhs_editor, move_to_row_with_text,
            open_review_fixture, path_at_cursor, rhs_editor, splittable,
        };
        use fs::Fs as _;
        use gpui::VisualTestContext;

        fn excerpt_paths(review: &Entity<ReviewView>, cx: &mut VisualTestContext) -> Vec<String> {
            review.read_with(cx, |review, cx| {
                review.diff().read(cx).excerpt_file_paths(cx)
            })
        }

        #[gpui::test]
        async fn test_file_navigation(cx: &mut TestAppContext) {
            init_vim_test(cx, DiffViewStyle::Unified);
            let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
            let editor = rhs_editor(&review, cx);
            assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("a.rs"));

            for (keystrokes, path, line) in [
                ("] f", "added.txt", "new 1"),
                ("] f", "deleted.txt", "gone 1"),
                ("] f", "long.txt", "short"),
                ("] f", "modified.txt", "one"),
                // The last file is a stop.
                ("] f", "modified.txt", "one"),
                ("[ f", "long.txt", "short"),
                ("[ f", "deleted.txt", "gone 1"),
                ("[ f", "added.txt", "new 1"),
                ("[ f", "a.rs", "line 1"),
                ("[ f", "a.rs", "line 1"),
            ] {
                cx.simulate_keystrokes(keystrokes);
                assert_eq!(
                    path_at_cursor(&review, cx).as_deref(),
                    Some(path),
                    "after {keystrokes:?}"
                );
                assert_eq!(cursor_line(&editor, cx), line, "after {keystrokes:?}");
            }

            // `[ f` goes to the previous file, not to the start of the current one.
            move_to_row_with_text(&editor, "line 36", cx);
            cx.simulate_keystrokes("[ f");
            assert_eq!(cursor_line(&editor, cx), "line 36");
            cx.simulate_keystrokes("] f");
            assert_eq!(cursor_line(&editor, cx), "new 1");

            // Counts are discarded rather than applied to the next motion.
            cx.simulate_keystrokes("3 ] f");
            assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("deleted.txt"));
            let row = cursor_point(&editor, cx).row;
            cx.simulate_keystrokes("j");
            assert_eq!(cursor_point(&editor, cx).row, row + 1);
            assert_eq!(cursor_line(&editor, cx), "gone 2");
        }

        /// `]f` scrolls the file's header to the top, so the blocks between it and the file's
        /// first row (a file comment here) aren't left under the sticky header.
        #[gpui::test]
        async fn test_file_navigation_shows_the_file_header(cx: &mut TestAppContext) {
            use editor::{Bias, display_map::DisplayRow};
            use language::Point;

            init_vim_test(cx, DiffViewStyle::Unified);
            let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
            let editor = rhs_editor(&review, cx);
            move_to_row_with_text(&editor, "short", cx);
            cx.simulate_keystrokes("shift-c");
            cx.simulate_input("a file comment\nover\nseveral\nlines");
            cx.simulate_keystrokes("ctrl-enter");
            cx.simulate_keystrokes("g g");

            let rows = |cx: &mut VisualTestContext| {
                editor.update(cx, |editor, cx| {
                    let snapshot = editor.display_snapshot(cx);
                    let cursor = editor.selections.newest::<Point>(&snapshot).head();
                    let cursor_row = snapshot.point_to_display_point(cursor, Bias::Left).row();
                    let header_row = snapshot
                        .blocks_in_range(DisplayRow(0)..cursor_row)
                        .filter(|(_, block)| block.is_buffer_header())
                        .map(|(row, _)| row)
                        .last()
                        .unwrap();
                    (editor.scroll_position(cx).y, header_row.0, cursor_row.0)
                })
            };
            cx.simulate_keystrokes("] f ] f ] f");
            assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("long.txt"));
            let (scroll_top, header_row, cursor_row) = rows(cx);
            assert_eq!(scroll_top, f64::from(header_row));
            let comment_rows = review.update(cx, |review, cx| {
                review
                    .comments()
                    .update(cx, |comments, cx| comments.hosted_rows(cx))
            });
            assert_eq!(comment_rows.len(), 1);
            let comment_row = comment_rows[0].1;
            assert!(
                header_row < comment_row && comment_row < cursor_row,
                "header {header_row}, comment {comment_row}, cursor {cursor_row}"
            );

            // Landing on the file comment (`]t`, the panel) puts the header at the top too.
            cx.simulate_keystrokes("g g ] t");
            assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("long.txt"));
            let (scroll_top, header_row, _) = rows(cx);
            assert_eq!(scroll_top, f64::from(header_row));

            // And so does `; f` on the file's first row.
            cx.simulate_keystrokes("g g");
            move_to_row_with_text(&editor, "short", cx);
            cx.simulate_keystrokes("; f");
            assert_eq!(excerpt_paths(&review, cx), ["long.txt"]);
            let (scroll_top, header_row, _) = rows(cx);
            assert_eq!(scroll_top, f64::from(header_row));
        }

        /// `r` at the very top folds the first file and moves to the next one with the view
        /// staying at the top, so the folded header stays in view (R1: following the cursor put
        /// the next file's header at the top, above it).
        #[gpui::test]
        async fn test_reviewing_the_first_file_keeps_the_top(cx: &mut TestAppContext) {
            use crate::test_support::is_folded;

            init_vim_test(cx, DiffViewStyle::Unified);
            let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
            let editor = rhs_editor(&review, cx);
            cx.simulate_keystrokes("g g r");
            assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("added.txt"));
            assert!(is_folded(&review, "a.rs", cx));
            assert_eq!(
                editor.update(cx, |editor, cx| editor.scroll_position(cx).y),
                0.
            );
        }

        #[gpui::test]
        async fn test_file_navigation_on_the_old_side(cx: &mut TestAppContext) {
            init_vim_test(cx, DiffViewStyle::Split);
            let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
            let lhs = lhs_editor(&review, cx);
            cx.simulate_keystrokes("ctrl-w h");
            assert!(
                splittable(&review, cx).read_with(cx, |editor, _| editor.focused_editor() == &lhs)
            );

            cx.simulate_keystrokes("] f");
            assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("added.txt"));
            cx.simulate_keystrokes("] f");
            assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("deleted.txt"));
            assert_eq!(cursor_line(&lhs, cx), "gone 1");
            cx.simulate_keystrokes("[ f [ f");
            assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("a.rs"));
            assert_eq!(cursor_line(&lhs, cx), "line 1");
        }

        #[gpui::test]
        async fn test_unreviewed_file_navigation_wraps(cx: &mut TestAppContext) {
            init_vim_test(cx, DiffViewStyle::Unified);
            let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;

            cx.simulate_keystrokes("[ u");
            assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("modified.txt"));
            cx.simulate_keystrokes("] u");
            assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("a.rs"));
            cx.simulate_keystrokes("] u");
            assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("added.txt"));
        }

        #[gpui::test]
        async fn test_single_file_mode(cx: &mut TestAppContext) {
            init_vim_test(cx, DiffViewStyle::Unified);
            let (fs, _workspace, review, cx) = open_review_fixture(cx).await;
            let editor = rhs_editor(&review, cx);
            let all_files = [
                "a.rs",
                "added.txt",
                "deleted.txt",
                "long.txt",
                "modified.txt",
            ];

            cx.simulate_keystrokes("] f j");
            cx.simulate_keystrokes("; f");
            assert_eq!(excerpt_paths(&review, cx), ["added.txt"]);
            assert_eq!(cursor_line(&editor, cx), "new 2");

            cx.simulate_keystrokes("] f");
            assert_eq!(excerpt_paths(&review, cx), ["deleted.txt"]);
            assert_eq!(cursor_line(&editor, cx), "gone 1");
            cx.update(|window, cx| assert!(editor.focus_handle(cx).is_focused(window)));
            cx.simulate_keystrokes("[ f [ f");
            assert_eq!(excerpt_paths(&review, cx), ["a.rs"]);
            assert_eq!(cursor_line(&editor, cx), "line 1");
            cx.simulate_keystrokes("] f");

            cx.simulate_keystrokes("; f");
            assert_eq!(excerpt_paths(&review, cx), all_files);
            assert_eq!(cursor_line(&editor, cx), "new 1");

            // Files that change while in single-file mode stay hidden.
            cx.simulate_keystrokes("; f");
            fs.insert_file(path!("/project/later.txt"), "later\n".into())
                .await;
            cx.run_until_parked();
            assert_eq!(excerpt_paths(&review, cx), ["added.txt"]);
            cx.simulate_keystrokes("] f ] f");
            assert_eq!(excerpt_paths(&review, cx), ["later.txt"]);
            cx.simulate_keystrokes("[ f [ f");
            assert_eq!(excerpt_paths(&review, cx), ["added.txt"]);

            // When the shown file has no changes left, every file is shown again.
            fs.remove_file(path!("/project/added.txt").as_ref(), Default::default())
                .await
                .unwrap();
            cx.run_until_parked();
            assert_eq!(
                excerpt_paths(&review, cx),
                [
                    "a.rs",
                    "deleted.txt",
                    "later.txt",
                    "long.txt",
                    "modified.txt"
                ]
            );
            cx.update(|window, cx| assert!(editor.focus_handle(cx).is_focused(window)));
        }

        /// File headers show the directory before the file's name, on both sides, the left side
        /// recreated by a split too.
        #[gpui::test]
        async fn test_file_headers_read_as_paths(cx: &mut TestAppContext) {
            init_vim_test(cx, DiffViewStyle::Split);
            let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
            let path_first = |editor: &Entity<Editor>, cx: &mut VisualTestContext| {
                editor.read_with(cx, |editor, _| editor.file_header_path_first())
            };
            assert!(path_first(&rhs_editor(&review, cx), cx));
            assert!(path_first(&lhs_editor(&review, cx), cx));
            let rhs = rhs_editor(&review, cx);
            for _ in 0..2 {
                cx.update(|window, cx| {
                    rhs.focus_handle(cx)
                        .dispatch_action(&editor::ToggleSplitDiff, window, cx)
                });
                cx.run_until_parked();
            }
            assert!(path_first(&lhs_editor(&review, cx), cx));
        }

        #[gpui::test]
        async fn test_open_file_at_cursor(cx: &mut TestAppContext) {
            init_vim_test(cx, DiffViewStyle::Unified);
            let (_fs, workspace, review, cx) = open_review_fixture(cx).await;
            let editor = rhs_editor(&review, cx);

            move_to_row_with_text(&editor, "three", cx);
            cx.simulate_keystrokes("g space");
            cx.run_until_parked();
            workspace.update(cx, |workspace, cx| {
                assert!(workspace.active_item_as::<ReviewView>(cx).is_none());
                assert_eq!(workspace.items_of_type::<ReviewView>(cx).count(), 1);
                let opened = workspace.active_item_as::<Editor>(cx).unwrap();
                let opened = opened.read(cx);
                let buffer = opened.buffer().read(cx).as_singleton().unwrap();
                assert_eq!(
                    buffer.read(cx).file().unwrap().path().as_unix_str(),
                    "modified.txt"
                );
            });
            let opened = workspace.update(cx, |workspace, cx| {
                workspace.active_item_as::<Editor>(cx).unwrap()
            });
            assert_eq!(cursor_point(&opened, cx).row, 2);
        }

        /// Without review comments an uncommitted review has no overview: the diff starts with
        /// its first file. `; c` brings the overview with the comment's editor, scrolled into
        /// view from the end of the review, in insert mode; a new comment left empty takes it
        /// away again. Editing a review comment (the review panel's row) scrolls it into view
        /// too.
        #[gpui::test]
        async fn test_review_comments_bring_the_overview(cx: &mut TestAppContext) {
            use editor::display_map::DisplayRow;

            init_vim_test(cx, DiffViewStyle::Unified);
            let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
            // Short enough to scroll.
            cx.simulate_resize(gpui::size(px(800.), px(400.)));
            cx.run_until_parked();
            let rhs = rhs_editor(&review, cx);
            let overview = |cx: &mut VisualTestContext| {
                review.read_with(cx, |review, _| review.top_block_id())
            };
            let starts_with_a_header = |cx: &mut VisualTestContext| {
                rhs.update(cx, |editor, cx| {
                    let snapshot = editor.display_snapshot(cx);
                    snapshot
                        .blocks_in_range(DisplayRow(0)..DisplayRow(1))
                        .next()
                        .is_some_and(|(row, block)| row.0 == 0 && block.is_buffer_header())
                })
            };
            let scroll_top = |cx: &mut VisualTestContext| {
                rhs.update(cx, |editor, cx| editor.scroll_position(cx).y)
            };
            let review_comments = |cx: &mut VisualTestContext| {
                review.read_with(cx, |review, cx| {
                    review
                        .session()
                        .read(cx)
                        .comments()
                        .iter()
                        .filter(|comment| comment.target == CommentTarget::Review)
                        .map(|comment| comment.body.clone())
                        .collect::<Vec<_>>()
                })
            };
            assert_eq!(overview(cx), None);
            assert!(starts_with_a_header(cx));

            cx.simulate_keystrokes("; c");
            assert!(overview(cx).is_some());
            cx.simulate_keystrokes("ctrl-enter");
            assert_eq!(overview(cx), None);
            assert!(starts_with_a_header(cx));

            cx.simulate_keystrokes("shift-g");
            assert!(scroll_top(cx) > 0.);
            cx.simulate_keystrokes("; c");
            assert!(overview(cx).is_some());
            assert_eq!(scroll_top(cx), 0.);
            cx.simulate_input("overall");
            cx.simulate_keystrokes("ctrl-enter");
            assert_eq!(review_comments(cx), ["overall"]);
            assert!(overview(cx).is_some());
            // The diff's cursor is still at the end, out of view: the next motion goes back.
            cx.simulate_keystrokes("k");
            assert!(scroll_top(cx) > 0.);
            let id = review.read_with(cx, |review, cx| {
                review
                    .session()
                    .read(cx)
                    .comments()
                    .iter()
                    .next()
                    .unwrap()
                    .id
            });
            let comments = review.read_with(cx, |review, _| review.comments().clone());
            comments.update_in(cx, |comments, window, cx| comments.edit(id, window, cx));
            cx.run_until_parked();
            assert_eq!(scroll_top(cx), 0.);
            assert!(comments.read_with(cx, |comments, cx| comments.is_editing(cx)));
        }
    }

    mod soft_wrap {
        use super::*;
        use crate::test_support::{
            dispatch, init_vim_test, lhs_editor, move_to_row_with_text, open_review_fixture,
            redraw, rhs_editor, row_center, splittable,
        };
        use gpui::{Modifiers, ScrollDelta, ScrollWheelEvent, TouchPhase, VisualTestContext};

        fn init(cx: &mut TestAppContext, diff_view_style: DiffViewStyle, soft_wrap: SoftWrap) {
            init_vim_test(cx, diff_view_style);
            cx.update(|cx| {
                cx.update_global::<SettingsStore, _>(|store, cx| {
                    store.update_user_settings(cx, |settings| {
                        settings.project.all_languages.defaults.soft_wrap = Some(soft_wrap);
                    });
                });
            });
        }

        fn new_line() -> String {
            "y".repeat(300)
        }

        fn old_line() -> String {
            "x".repeat(300)
        }

        fn wraps(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> bool {
            editor.read_with(cx, |editor, cx| {
                !matches!(editor.soft_wrap_mode(cx), editor::SoftWrap::None)
            })
        }

        /// The first display row of the row with `text`, and the number of display rows it
        /// takes.
        fn display_rows(
            editor: &Entity<Editor>,
            text: &str,
            cx: &mut VisualTestContext,
        ) -> (u32, u32) {
            redraw(cx);
            cx.run_until_parked();
            let row = move_to_row_with_text(editor, text, cx);
            editor.update(cx, |editor, cx| {
                let snapshot = editor.display_snapshot(cx);
                let end = Point::new(
                    row,
                    snapshot.buffer_snapshot().line_len(MultiBufferRow(row)),
                );
                let start = snapshot.point_to_display_point(Point::new(row, 0), Bias::Left);
                let end = snapshot.point_to_display_point(end, Bias::Left);
                (start.row().0, end.row().0 - start.row().0 + 1)
            })
        }

        fn max_display_row(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> u32 {
            editor.update(cx, |editor, cx| {
                editor.display_snapshot(cx).max_point().row().0
            })
        }

        fn toggle_split(review: &Entity<ReviewView>, cx: &mut VisualTestContext) {
            let rhs = rhs_editor(review, cx);
            cx.update(|window, cx| {
                rhs.focus_handle(cx)
                    .dispatch_action(&editor::ToggleSplitDiff, window, cx)
            });
            cx.run_until_parked();
        }

        fn resize(width: f32, cx: &mut VisualTestContext) {
            cx.simulate_resize(gpui::size(px(width), px(1000.)));
            cx.run_until_parked();
            redraw(cx);
            cx.run_until_parked();
        }

        fn scroll_x(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> f64 {
            redraw(cx);
            cx.run_until_parked();
            editor.update(cx, |editor, cx| editor.scroll_position(cx).x)
        }

        /// A trackpad scroll by `pixels` to the right over the row with `text`.
        fn scroll_right(
            editor: &Entity<Editor>,
            text: &str,
            pixels: f32,
            cx: &mut VisualTestContext,
        ) {
            redraw(cx);
            let row = move_to_row_with_text(editor, text, cx);
            redraw(cx);
            let position = row_center(editor, row, cx);
            cx.simulate_event(ScrollWheelEvent {
                position,
                delta: ScrollDelta::Pixels(gpui::point(px(-pixels), px(0.))),
                modifiers: Modifiers::none(),
                touch_phase: TouchPhase::Moved,
            });
            cx.run_until_parked();
        }

        /// Wrapping follows the language settings, as in Zed's own diff views (Zed's default
        /// doesn't wrap: vim then moves by whole lines, `ctrl-u` and `ctrl-d` too).
        /// `ToggleSoftWrap` turns it on and off for both sides, and a new left side (after
        /// toggling the split, or after the pane gets too narrow for two sides and wide again)
        /// follows the right side.
        #[gpui::test]
        async fn test_wrapping_follows_the_settings(cx: &mut TestAppContext) {
            init(cx, DiffViewStyle::Split, SoftWrap::None);
            let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
            let rhs = rhs_editor(&review, cx);
            let lhs = lhs_editor(&review, cx);
            assert!(!wraps(&rhs, cx));
            assert!(!wraps(&lhs, cx));
            assert_eq!(display_rows(&rhs, &new_line(), cx).1, 1);
            assert_eq!(display_rows(&lhs, &old_line(), cx).1, 1);

            cx.focus(&rhs);
            dispatch(ToggleSoftWrap, cx);
            assert!(wraps(&rhs, cx));
            assert!(wraps(&lhs, cx));
            assert!(display_rows(&rhs, &new_line(), cx).1 > 1);
            assert!(display_rows(&lhs, &old_line(), cx).1 > 1);

            toggle_split(&review, cx);
            toggle_split(&review, cx);
            let lhs = lhs_editor(&review, cx);
            assert!(wraps(&lhs, cx));
            assert!(display_rows(&lhs, &old_line(), cx).1 > 1);

            resize(500., cx);
            assert!(!splittable(&review, cx).read_with(cx, |editor, _| editor.is_split()));
            assert!(wraps(&rhs, cx));
            resize(1600., cx);
            let new_lhs = lhs_editor(&review, cx);
            assert_ne!(new_lhs, lhs);
            assert!(wraps(&new_lhs, cx));

            cx.focus(&rhs);
            dispatch(ToggleSoftWrap, cx);
            assert!(!wraps(&rhs, cx));
            assert!(!wraps(&new_lhs, cx));
            assert_eq!(display_rows(&new_lhs, &old_line(), cx).1, 1);
        }

        #[gpui::test]
        async fn test_wrapping_settings_wrap_the_diff(cx: &mut TestAppContext) {
            init(cx, DiffViewStyle::Unified, SoftWrap::EditorWidth);
            let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
            let rhs = rhs_editor(&review, cx);
            assert!(wraps(&rhs, cx));
            assert!(display_rows(&rhs, &new_line(), cx).1 > 1);
        }

        /// A long line wrapped into more rows on one side than on the other faces a spacer:
        /// the rows after it stay side by side.
        #[gpui::test]
        async fn test_wrapped_lines_stay_aligned_in_split_view(cx: &mut TestAppContext) {
            init(cx, DiffViewStyle::Split, SoftWrap::EditorWidth);
            let (fs, _workspace, review, cx) = open_review_fixture(cx).await;
            let longer_line = "y".repeat(900);
            fs.insert_file(
                path!("/project/long.txt"),
                format!("short\n{longer_line}\n").into_bytes(),
            )
            .await;
            cx.run_until_parked();
            let rhs = rhs_editor(&review, cx);
            let lhs = lhs_editor(&review, cx);

            let (new_start, new_rows) = display_rows(&rhs, &longer_line, cx);
            let (old_start, old_rows) = display_rows(&lhs, &old_line(), cx);
            assert_eq!(new_start, old_start);
            assert!(old_rows > 1);
            assert!(new_rows > old_rows);
            assert_eq!(
                display_rows(&rhs, "ONE", cx).0,
                display_rows(&lhs, "one", cx).0
            );
            assert_eq!(max_display_row(&rhs, cx), max_display_row(&lhs, cx));
        }

        #[gpui::test]
        async fn test_long_lines_scroll_horizontally_without_wrapping(cx: &mut TestAppContext) {
            assert_long_lines_scroll_horizontally(DiffViewStyle::Unified, cx).await;
        }

        #[gpui::test]
        async fn test_long_lines_scroll_horizontally_without_wrapping_in_split_view(
            cx: &mut TestAppContext,
        ) {
            assert_long_lines_scroll_horizontally(DiffViewStyle::Split, cx).await;
        }

        /// With wrapping off, a trackpad scroll over a long line and `$` scroll the view
        /// horizontally; in split view both sides move together, from either side.
        async fn assert_long_lines_scroll_horizontally(
            diff_view_style: DiffViewStyle,
            cx: &mut TestAppContext,
        ) {
            init(cx, diff_view_style, SoftWrap::None);
            let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
            let rhs = rhs_editor(&review, cx);
            let lhs = (diff_view_style == DiffViewStyle::Split).then(|| lhs_editor(&review, cx));
            cx.focus(&rhs);
            assert!(!wraps(&rhs, cx));
            assert_eq!(scroll_x(&rhs, cx), 0.);

            scroll_right(&rhs, &new_line(), 200., cx);
            let scrolled = scroll_x(&rhs, cx);
            assert!(scrolled > 0., "{diff_view_style:?}");
            if let Some(lhs) = &lhs {
                assert_eq!(scroll_x(lhs, cx), scrolled);
                scroll_right(lhs, &old_line(), 200., cx);
                assert!(scroll_x(&rhs, cx) > scrolled);
                assert_eq!(scroll_x(lhs, cx), scroll_x(&rhs, cx));
            }

            scroll_right(&rhs, &new_line(), -10_000., cx);
            assert_eq!(scroll_x(&rhs, cx), 0., "{diff_view_style:?}");
            cx.focus(&rhs);
            move_to_row_with_text(&rhs, &new_line(), cx);
            cx.simulate_keystrokes("$");
            let scrolled = scroll_x(&rhs, cx);
            assert!(scrolled > 0., "{diff_view_style:?}");
            if let Some(lhs) = &lhs {
                // Each side stops at its own end (the sides' gutters differ in width).
                let lhs_scrolled = scroll_x(lhs, cx);
                assert!(lhs_scrolled > 0. && (lhs_scrolled - scrolled).abs() < 3.);
            }
            cx.simulate_keystrokes("0");
            // Back to the start, but for the gutter's margin, which autoscroll keeps in view.
            assert!(scroll_x(&rhs, cx) < 1., "{diff_view_style:?}");
        }
    }
}
