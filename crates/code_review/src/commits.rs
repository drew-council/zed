use crate::target::{BranchBase, CommitRange, CommitSpec};
use anyhow::{Context as _, Result, anyhow, bail};
use buffer_diff::BufferDiff;
use collections::HashMap;
use futures::FutureExt as _;
use git::{
    Oid,
    repository::{CommitData, GitRepository, RepoPath},
    status::{DiffTreeType, FileStatus, StatusCode, TrackedStatus, TreeDiffStatus},
};
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, SharedString, Task, WeakEntity,
};
use language::{
    Buffer, BufferId, Capability, DiskState, File, LanguageRegistry, LineEnding, ReplicaId, Rope,
    TextBuffer,
};
use project::{
    Project, WorktreeId,
    git_store::{
        LocalRepositoryState, Repository, RepositoryState,
        diff_buffer_list::{BranchDiffEvent, DiffBuffer, LoadedDiffBuffer},
    },
};
use std::{future::Future, path::PathBuf, sync::Arc};
use util::{ResultExt as _, paths::PathStyle, rel_path::RelPath};

/// How many commits the Start Review picker lists, following first parents from HEAD.
pub(crate) const RECENT_COMMITS_LIMIT: usize = 50;
/// How many commits of a range the top block shows.
pub(crate) const TOP_BLOCK_COMMIT_LIMIT: usize = 100;

// git knows the empty tree without it being stored in the repository, so root commits can be
// diffed against it.
const EMPTY_TREE_SHA1: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const EMPTY_TREE_SHA256: &str = "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321";

const BINARY_PLACEHOLDER: &str = "(binary file not shown)";

/// A snapshot loads its files in jobs of this many, so that other git work (status refreshes, the
/// git panel) runs in between, and closing the review stops before the remaining files.
const SNAPSHOT_FILES_PER_JOB: usize = 32;

/// Runs `job` on the repository's git job queue with its local backend.
fn local_job<R, Fut>(
    repo: &Entity<Repository>,
    description: &'static str,
    job: impl FnOnce(Arc<dyn GitRepository>) -> Fut + 'static,
    cx: &mut App,
) -> Task<Result<R>>
where
    R: Send + 'static,
    Fut: Future<Output = Result<R>> + 'static,
{
    let receiver = repo.update(cx, |repo, _| {
        repo.send_job(description, None, move |state, _| async move {
            match state {
                RepositoryState::Local(LocalRepositoryState { backend, .. }) => job(backend).await,
                RepositoryState::Remote(_) => {
                    Err(anyhow!("Code review only supports local repositories"))
                }
            }
        })
    });
    cx.spawn(async move |_| receiver.await.context("the git job was canceled")?)
}

pub(crate) struct CommitChain {
    /// Newest first.
    pub(crate) commits: Vec<CommitData>,
    /// Whether the chain stopped at the limit (or at a shallow clone's boundary) before reaching
    /// its end.
    pub(crate) truncated: bool,
}

/// Follows first parents from `from` (HEAD when `None`) until `stop_before`, a root commit or
/// `limit` commits.
pub(crate) fn first_parent_chain(
    repo: &Entity<Repository>,
    from: Option<Oid>,
    stop_before: Option<Oid>,
    limit: usize,
    cx: &mut App,
) -> Task<Result<CommitChain>> {
    local_job(
        repo,
        "code review: list commits",
        move |backend| async move {
            let from = match from {
                Some(from) => from,
                None => {
                    let Some(head) = backend.head_sha().await else {
                        return Ok(CommitChain {
                            commits: Vec::new(),
                            truncated: false,
                        });
                    };
                    head.parse::<Oid>()
                        .with_context(|| format!("invalid HEAD sha {head:?}"))?
                }
            };
            let reader = backend.commit_data_reader()?;
            let mut commits = Vec::new();
            let mut next = Some(from);
            while let Some(sha) = next {
                if Some(sha) == stop_before {
                    break;
                }
                if commits.len() == limit {
                    return Ok(CommitChain {
                        commits,
                        truncated: true,
                    });
                }
                match reader.read(sha).await {
                    Ok(commit) => {
                        next = commit.parents.first().copied();
                        commits.push(commit);
                    }
                    // A shallow clone's history ends at a commit whose parent is missing.
                    Err(error) if !commits.is_empty() => {
                        log::info!("commit history ends before {sha}: {error:#}");
                        return Ok(CommitChain {
                            commits,
                            truncated: true,
                        });
                    }
                    Err(error) => return Err(error),
                }
            }
            Ok(CommitChain {
                commits,
                truncated: false,
            })
        },
        cx,
    )
}

/// Resolves a typed commit or range. A single commit is reviewed against its first parent. In a
/// range `A..B`, A must be on B's first-parent history: otherwise the diff from A would also
/// revert A's own changes, and the range has no list of consecutive commits to show.
pub(crate) fn resolve_commit_spec(
    repo: &Entity<Repository>,
    spec: CommitSpec,
    cx: &mut App,
) -> Task<Result<CommitRange>> {
    local_job(
        repo,
        "code review: resolve commits",
        move |backend| async move {
            match spec {
                CommitSpec::Single(revision) => {
                    let head = match resolve_commits(backend.as_ref(), &[&revision])
                        .await?
                        .as_slice()
                    {
                        [head] => *head,
                        _ => bail!("Unknown revision {revision}"),
                    };
                    let commit = backend
                        .commit_data_reader()?
                        .read(head)
                        .await
                        .with_context(|| format!("reading commit {revision}"))?;
                    Ok(CommitRange {
                        base: commit.parents.first().copied(),
                        head,
                    })
                }
                CommitSpec::Range { from, to } => {
                    let (base, head) = match resolve_commits(backend.as_ref(), &[&from, &to])
                        .await?
                        .as_slice()
                    {
                        [base, head] => (*base, *head),
                        _ => bail!("Unknown revision {from}..{to}"),
                    };
                    if base == head {
                        bail!("{from}..{to} has no commits");
                    }
                    if !is_first_parent_ancestor(backend.as_ref(), base, head).await? {
                        bail!(
                            "{from}..{to} isn't a range of consecutive commits: {from} is not \
                             on {to}'s first-parent history"
                        );
                    }
                    Ok(CommitRange {
                        base: Some(base),
                        head,
                    })
                }
            }
        },
        cx,
    )
}

/// Resolves typed revisions to the commits they name (peeling tags), all in one
/// `git cat-file --batch-check`.
async fn resolve_commits(backend: &dyn GitRepository, revisions: &[&str]) -> Result<Vec<Oid>> {
    let requests = revisions
        .iter()
        .flat_map(|revision| [revision.to_string(), format!("{revision}^{{commit}}")])
        .collect();
    let mut resolved = backend.revparse_batch(requests).await?.into_iter();
    revisions
        .iter()
        .map(|revision| {
            let object = resolved.next().flatten();
            let commit = resolved.next().flatten();
            resolved_commit(revision, object, commit)
        })
        .collect()
}

/// `git cat-file --batch-check` answers "‹name› missing" (which `revparse_batch` turns into
/// `None`) or "‹name› ambiguous" instead of an object name. A tree or blob has no `^{commit}`.
fn resolved_commit(revision: &str, object: Option<String>, commit: Option<String>) -> Result<Oid> {
    if let Some(oid) = commit.as_deref().and_then(|sha| sha.parse::<Oid>().ok()) {
        return Ok(oid);
    }
    let is_ambiguous = |answer: &Option<String>| {
        answer
            .as_deref()
            .is_some_and(|answer| answer.ends_with(" ambiguous"))
    };
    if is_ambiguous(&object) || is_ambiguous(&commit) {
        bail!("Ambiguous revision {revision}");
    }
    if object.is_some() {
        bail!("{revision} is not a commit");
    }
    bail!("Unknown revision {revision}")
}

/// How far back a range's oldest commit may be.
const RANGE_COMMIT_LIMIT: usize = 10_000;

/// Whether following first parents from `descendant` reaches `ancestor`.
async fn is_first_parent_ancestor(
    backend: &dyn GitRepository,
    ancestor: Oid,
    descendant: Oid,
) -> Result<bool> {
    let reader = backend.commit_data_reader()?;
    let mut next = Some(descendant);
    for _ in 0..RANGE_COMMIT_LIMIT {
        let Some(sha) = next else {
            return Ok(false);
        };
        if sha == ancestor {
            return Ok(true);
        }
        next = match reader.read(sha).await {
            Ok(commit) => commit.parents.first().copied(),
            // A shallow clone's history ends at a commit whose parent is missing.
            Err(error) if sha != descendant => {
                log::info!("commit history ends before {sha}: {error:#}");
                None
            }
            Err(error) => return Err(error),
        };
    }
    Ok(false)
}

/// Checks that a branch review can compare HEAD with `base`: the base names a commit that has one
/// merge base with HEAD. `DiffBufferList` only logs a failed comparison and then lists no files.
pub(crate) fn check_branch_base(
    repo: &Entity<Repository>,
    base: BranchBase,
    cx: &mut App,
) -> Task<Result<()>> {
    local_job(
        repo,
        "code review: check base",
        move |backend| async move {
            let base_commit = match resolve_commits(backend.as_ref(), &[base.name().as_ref()])
                .await?
                .as_slice()
            {
                [base_commit] => *base_commit,
                _ => bail!("Unknown revision {base}"),
            };
            backend
                .diff_tree(DiffTreeType::MergeBase {
                    base: base_commit.to_string().into(),
                    head: "HEAD".into(),
                })
                .await
                .map_err(|error| merge_base_error(&base, &error))?;
            Ok(())
        },
        cx,
    )
}

/// Git's `diff --merge-base` stops with "no merge base found" or "multiple merge bases found".
fn merge_base_error(base: &BranchBase, error: &anyhow::Error) -> anyhow::Error {
    let message = format!("{error:#}");
    if message.contains("no merge base") {
        anyhow!("{base} and HEAD have no common history")
    } else if message.contains("multiple merge bases") {
        anyhow!("{base} and HEAD have more than one merge base")
    } else {
        anyhow!("Can't compare HEAD with {base}: {}", message.trim())
    }
}

fn base_tree(range: &CommitRange) -> SharedString {
    match range.base {
        Some(base) => base.to_string().into(),
        None if range.head.as_bytes().len() == 32 => EMPTY_TREE_SHA256.into(),
        None => EMPTY_TREE_SHA1.into(),
    }
}

enum CommitDiffState {
    Loading,
    Failed(SharedString),
    /// The range ends at HEAD: the files changed in the range, shown over the project's buffers.
    Live(HashMap<RepoPath, TreeDiffStatus>),
    /// Historic, read-only buffers of the range's head.
    Snapshot(Vec<SnapshotFile>),
}

struct SnapshotFile {
    repo_path: RepoPath,
    file_status: FileStatus,
    buffer: Entity<Buffer>,
    diff: Entity<BufferDiff>,
}

/// The changed files of a commit range, playing the role `DiffBufferList` plays for the other
/// targets. A range that ends at HEAD when the review opens is live; any other range is a
/// snapshot that never touches the checkout. The mode is decided once.
pub(crate) struct CommitDiffList {
    repo: Entity<Repository>,
    project: Entity<Project>,
    state: CommitDiffState,
    snapshot_statuses: HashMap<BufferId, FileStatus>,
    _load_task: Task<()>,
}

impl EventEmitter<BranchDiffEvent> for CommitDiffList {}

impl CommitDiffList {
    pub(crate) fn new(
        range: CommitRange,
        repo: Entity<Repository>,
        project: Entity<Project>,
        cx: &mut Context<Self>,
    ) -> Self {
        let load_task = cx.spawn({
            let repo = repo.clone();
            let project = project.clone();
            async move |this, cx| {
                if let Err(error) = Self::load(this.clone(), range, repo, project, cx).await {
                    log::error!("loading commits for review: {error:#}");
                    this.update(cx, |this, cx| {
                        this.state = CommitDiffState::Failed(format!("{error:#}").into());
                        cx.emit(BranchDiffEvent::FileListChanged);
                        cx.notify();
                    })
                    .ok();
                }
            }
        });
        Self {
            repo,
            project,
            state: CommitDiffState::Loading,
            snapshot_statuses: HashMap::default(),
            _load_task: load_task,
        }
    }

    async fn load(
        this: WeakEntity<Self>,
        range: CommitRange,
        repo: Entity<Repository>,
        project: Entity<Project>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let (head_sha, tree_diff) = repo.update(cx, |repo, cx| {
            let tree_diff = repo.diff_tree(
                DiffTreeType::Since {
                    base: base_tree(&range),
                    head: range.head.to_string().into(),
                },
                cx,
            );
            (repo.head_sha(), tree_diff)
        });
        let (head_sha, tree_diff) = futures::join!(head_sha, tree_diff);
        let head_sha = head_sha??;
        let entries = tree_diff??.entries;

        if head_sha.is_some_and(|sha| sha == range.head.to_string()) {
            return this.update(cx, |this, cx| {
                this.state = CommitDiffState::Live(entries);
                cx.emit(BranchDiffEvent::FileListChanged);
                cx.notify();
            });
        }

        let mut entries = entries
            .into_iter()
            .filter(|(repo_path, _)| {
                // `git cat-file --batch` takes one revision per line.
                let loadable = !repo_path.as_unix_str().contains('\n');
                if !loadable {
                    log::warn!("skipping {repo_path:?} in a commit review: its path has a newline");
                }
                loadable
            })
            .collect::<Vec<_>>();
        entries.sort_by(|(left, _), (right, _)| left.cmp(right));
        let mut files = Vec::with_capacity(entries.len());
        let mut remaining = entries;
        while !remaining.is_empty() {
            let rest = remaining.split_off(remaining.len().min(SNAPSHOT_FILES_PER_JOB));
            let chunk = std::mem::replace(&mut remaining, rest);
            let texts = cx
                .update(|cx| load_snapshot_texts(&repo, range, &chunk, cx))
                .await?;
            files.extend(
                build_snapshot_files(chunk, texts, range.base.is_some(), &repo, &project, cx)
                    .await?,
            );
        }
        this.update(cx, |this, cx| {
            this.snapshot_statuses = files
                .iter()
                .map(|file| (file.buffer.read(cx).remote_id(), file.file_status))
                .collect();
            this.state = CommitDiffState::Snapshot(files);
            cx.emit(BranchDiffEvent::FileListChanged);
            cx.notify();
        })
    }

    pub(crate) fn repo(&self) -> &Entity<Repository> {
        &self.repo
    }

    pub(crate) fn is_loading(&self) -> bool {
        matches!(self.state, CommitDiffState::Loading)
    }

    pub(crate) fn is_snapshot(&self) -> bool {
        matches!(self.state, CommitDiffState::Snapshot(_))
    }

    pub(crate) fn error(&self) -> Option<SharedString> {
        match &self.state {
            CommitDiffState::Failed(error) => Some(error.clone()),
            _ => None,
        }
    }

    /// The status of the buffer's file in the range, rather than against HEAD.
    pub(crate) fn status_for_buffer_id(&self, buffer_id: BufferId, cx: &App) -> Option<FileStatus> {
        match &self.state {
            CommitDiffState::Snapshot(_) => self.snapshot_statuses.get(&buffer_id).copied(),
            CommitDiffState::Live(entries) => {
                let (repo, repo_path) = self
                    .project
                    .read(cx)
                    .git_store()
                    .read(cx)
                    .repository_and_path_for_buffer_id(buffer_id, cx)?;
                (repo == self.repo)
                    .then(|| entries.get(&repo_path).map(tree_status_to_file_status))
                    .flatten()
            }
            CommitDiffState::Loading | CommitDiffState::Failed(_) => None,
        }
    }

    pub(crate) fn load_buffers(&mut self, cx: &mut Context<Self>) -> Vec<DiffBuffer> {
        match &self.state {
            CommitDiffState::Loading | CommitDiffState::Failed(_) => Vec::new(),
            CommitDiffState::Snapshot(files) => files
                .iter()
                .map(|file| {
                    let loaded = LoadedDiffBuffer {
                        display_buffer: file.buffer.clone(),
                        main_buffer: file.buffer.clone(),
                        diff: file.diff.clone(),
                        conflict_set: None,
                    };
                    DiffBuffer {
                        repo_path: file.repo_path.clone(),
                        file_status: file.file_status,
                        load: async move { Ok(loaded) }.boxed_local(),
                    }
                })
                .collect(),
            CommitDiffState::Live(entries) => {
                let git_store = self.project.read(cx).git_store().clone();
                entries
                    .iter()
                    .filter_map(|(repo_path, status)| {
                        let project_path = self
                            .repo
                            .read(cx)
                            .repo_path_to_project_path(repo_path, cx)?;
                        let old_oid = match status {
                            TreeDiffStatus::Added => None,
                            TreeDiffStatus::Modified { old } | TreeDiffStatus::Deleted { old } => {
                                Some(*old)
                            }
                        };
                        let project = self.project.clone();
                        let git_store = git_store.clone();
                        let repo = self.repo.clone();
                        let mut cx = cx.to_async();
                        let load = async move {
                            let buffer = project
                                .update(&mut cx, |project, cx| {
                                    project.open_buffer(project_path, cx)
                                })
                                .await?;
                            let diff = git_store
                                .update(&mut cx, |git_store, cx| {
                                    git_store.open_diff_since(old_oid, buffer.clone(), repo, cx)
                                })
                                .await?;
                            Ok(LoadedDiffBuffer {
                                display_buffer: buffer.clone(),
                                main_buffer: buffer,
                                diff,
                                conflict_set: None,
                            })
                        }
                        .boxed_local();
                        Some(DiffBuffer {
                            repo_path: repo_path.clone(),
                            file_status: tree_status_to_file_status(status),
                            load,
                        })
                    })
                    .collect()
            }
        }
    }
}

struct SnapshotTexts {
    old: Option<Vec<u8>>,
    new: Option<Vec<u8>>,
}

/// Loads both sides of every file in one `git cat-file --batch` process.
fn load_snapshot_texts(
    repo: &Entity<Repository>,
    range: CommitRange,
    entries: &[(RepoPath, TreeDiffStatus)],
    cx: &mut App,
) -> Task<Result<Vec<SnapshotTexts>>> {
    let mut revisions = Vec::new();
    let mut slots = Vec::with_capacity(entries.len());
    for (repo_path, status) in entries {
        let path = repo_path.as_unix_str();
        let mut push = |revision: String| {
            revisions.push(revision);
            revisions.len() - 1
        };
        let new = (!matches!(status, TreeDiffStatus::Deleted { .. }))
            .then(|| push(format!("{}:{path}", range.head)));
        let old = match (status, range.base) {
            (TreeDiffStatus::Added, _) | (_, None) => None,
            (_, Some(base)) => Some(push(format!("{base}:{path}"))),
        };
        slots.push((old, new));
    }
    local_job(
        repo,
        "code review: load snapshot",
        move |backend| async move {
            let mut contents = backend.load_revisions(revisions).await?;
            let mut take = |slot: Option<usize>| {
                slot.and_then(|index| contents.get_mut(index).and_then(Option::take))
            };
            Ok(slots
                .into_iter()
                .map(|(old, new)| SnapshotTexts {
                    old: take(old),
                    new: take(new),
                })
                .collect())
        },
        cx,
    )
}

async fn build_snapshot_files(
    entries: Vec<(RepoPath, TreeDiffStatus)>,
    texts: Vec<SnapshotTexts>,
    has_base: bool,
    repo: &Entity<Repository>,
    project: &Entity<Project>,
    cx: &mut AsyncApp,
) -> Result<Vec<SnapshotFile>> {
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    let mut files = Vec::with_capacity(entries.len());
    for ((repo_path, status), texts) in entries.into_iter().zip(texts) {
        // A side that git lists but that isn't a blob, like a submodule's commit.
        let missing_side = match status {
            TreeDiffStatus::Added | TreeDiffStatus::Modified { .. } => texts.new.is_none(),
            TreeDiffStatus::Deleted { .. } => false,
        } || match status {
            TreeDiffStatus::Modified { .. } | TreeDiffStatus::Deleted { .. } => {
                has_base && texts.old.is_none()
            }
            TreeDiffStatus::Added => false,
        };
        if missing_side {
            log::warn!("skipping {repo_path:?} in a commit review: it isn't a file on both sides");
            continue;
        }
        let (worktree_id, path) = cx
            .update(|cx| {
                project_path_for_repo_path(repo.read(cx), project.read(cx), &repo_path, cx)
            })
            .context("the project has no worktrees")?;
        let is_deleted = texts.new.is_none();
        let is_binary = texts.old.as_deref().is_some_and(is_binary_content)
            || texts.new.as_deref().is_some_and(is_binary_content);
        // A binary file shows a placeholder line, added or deleted, so that it is still listed.
        let (old_text, new_text) = if is_binary {
            if is_deleted {
                (Some(BINARY_PLACEHOLDER.to_string()), String::new())
            } else {
                (None, BINARY_PLACEHOLDER.to_string())
            }
        } else {
            (
                texts.old.map(lossy_string),
                texts.new.map(lossy_string).unwrap_or_default(),
            )
        };
        let display_name = repo_path
            .file_name()
            .map(ToString::to_string)
            .unwrap_or_else(|| repo_path.as_unix_str().to_string());
        let blob = Arc::new(GitBlob {
            repo_path: repo_path.clone(),
            path,
            worktree_id,
            is_deleted,
            is_binary,
            display_name,
        });
        let buffer = build_buffer(new_text, blob, &language_registry, cx).await?;
        let diff = build_buffer_diff(old_text, &buffer, &language_registry, cx).await;
        files.push(SnapshotFile {
            repo_path,
            file_status: snapshot_file_status(&status),
            buffer,
            diff,
        });
    }
    Ok(files)
}

/// Git's heuristic. Text in other encodings (Latin-1, Shift-JIS) shows decoded lossily rather than
/// hidden.
fn is_binary_content(content: &[u8]) -> bool {
    content.iter().take(8000).any(|byte| *byte == 0)
}

fn lossy_string(content: Vec<u8>) -> String {
    String::from_utf8(content)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
}

/// Like `CommitView`'s statuses: the change is committed, nothing is pending in the worktree.
fn snapshot_file_status(status: &TreeDiffStatus) -> FileStatus {
    let index_status = match status {
        TreeDiffStatus::Added => StatusCode::Added,
        TreeDiffStatus::Modified { .. } => StatusCode::Modified,
        TreeDiffStatus::Deleted { .. } => StatusCode::Deleted,
    };
    FileStatus::Tracked(TrackedStatus {
        index_status,
        worktree_status: StatusCode::Unmodified,
    })
}

// Copied from `DiffBufferList`, where it is private.
fn tree_status_to_file_status(status: &TreeDiffStatus) -> FileStatus {
    let status_code = match status {
        TreeDiffStatus::Added => StatusCode::Added,
        TreeDiffStatus::Modified { .. } => StatusCode::Modified,
        TreeDiffStatus::Deleted { .. } => StatusCode::Deleted,
    };
    FileStatus::Tracked(TrackedStatus {
        index_status: status_code,
        worktree_status: status_code,
    })
}

// `GitBlob`, `worktree_id_for_repo_path`, `build_buffer` and `build_buffer_diff` are adapted from
// `git_ui`'s `CommitView`, where they are crate-private.

/// The file of a historic buffer. It isn't local, so no language server attaches to it.
struct GitBlob {
    repo_path: RepoPath,
    /// Relative to the worktree when the file is in one (the project may be a subdirectory of the
    /// repository), else the repository path.
    path: Arc<RelPath>,
    worktree_id: WorktreeId,
    is_deleted: bool,
    is_binary: bool,
    display_name: String,
}

impl File for GitBlob {
    fn as_local(&self) -> Option<&dyn language::LocalFile> {
        None
    }

    fn disk_state(&self) -> DiskState {
        DiskState::Historic {
            was_deleted: self.is_deleted,
        }
    }

    fn path_style(&self, _: &App) -> PathStyle {
        PathStyle::local()
    }

    fn path(&self) -> &Arc<RelPath> {
        &self.path
    }

    fn full_path(&self, _: &App) -> PathBuf {
        self.repo_path.as_std_path().to_path_buf()
    }

    fn file_name<'a>(&'a self, _: &'a App) -> &'a str {
        self.display_name.as_ref()
    }

    fn worktree_id(&self, _: &App) -> WorktreeId {
        self.worktree_id
    }

    fn to_proto(&self, _: &App) -> language::proto::File {
        language::proto::File {
            worktree_id: self.worktree_id.to_proto(),
            entry_id: None,
            path: self.path.as_unix_str().to_owned(),
            mtime: None,
            is_deleted: self.is_deleted,
            is_historic: true,
        }
    }

    fn is_private(&self) -> bool {
        false
    }

    fn can_open(&self) -> bool {
        !self.is_binary
    }
}

fn project_path_for_repo_path(
    repository: &Repository,
    project: &Project,
    path: &RepoPath,
    cx: &App,
) -> Option<(WorktreeId, Arc<RelPath>)> {
    if let Some(project_path) = repository.repo_path_to_project_path(path, cx) {
        return Some((project_path.worktree_id, project_path.path));
    }
    let worktree_id = project
        .find_worktree(&repository.work_directory_abs_path, cx)
        .map(|(worktree, _)| worktree.read(cx).id())
        .or_else(|| {
            project
                .worktrees(cx)
                .next()
                .map(|worktree| worktree.read(cx).id())
        })?;
    Some((worktree_id, path.as_ref().clone()))
}

async fn build_buffer(
    mut text: String,
    blob: Arc<dyn File>,
    language_registry: &Arc<LanguageRegistry>,
    cx: &mut AsyncApp,
) -> Result<Entity<Buffer>> {
    let line_ending = LineEnding::detect(&text);
    LineEnding::normalize(&mut text);
    let text = Rope::from(text);
    let language = cx.update(|cx| language_registry.language_for_file(&blob, Some(&text), cx));
    let language = match language {
        Some(language) => language_registry
            .load_language(language)
            .await
            .ok()
            .and_then(|language| language.log_err()),
        None => None,
    };
    Ok(cx.new(|cx| {
        let buffer = TextBuffer::new_normalized(
            ReplicaId::LOCAL,
            cx.entity_id().as_non_zero_u64().into(),
            line_ending,
            text,
        );
        let mut buffer = Buffer::build(buffer, Some(blob), Capability::ReadOnly, cx);
        buffer.set_language_registry(language_registry.clone());
        buffer.set_language_async(language, cx);
        buffer
    }))
}

async fn build_buffer_diff(
    old_text: Option<String>,
    buffer: &Entity<Buffer>,
    language_registry: &Arc<LanguageRegistry>,
    cx: &mut AsyncApp,
) -> Entity<BufferDiff> {
    let old_text = old_text.map(|mut old_text| {
        LineEnding::normalize(&mut old_text);
        Arc::<str>::from(old_text)
    });
    let (snapshot, language) = buffer.read_with(cx, |buffer, _| {
        (buffer.text_snapshot(), buffer.language().cloned())
    });
    let diff =
        cx.new(|cx| BufferDiff::new(&snapshot, language, Some(language_registry.clone()), cx));
    diff.update(cx, |diff, cx| diff.set_base_text(old_text, snapshot, cx))
        .await;
    diff
}

#[cfg(test)]
mod tests {
    use crate::{
        ReviewCommits, ReviewView,
        test_support::{
            C1, C2, C3, active_review, commit_data, cursor_line, cursor_point, dispatch, dot_git,
            excerpt_paths, init_vim_test, insert_commit_ref, move_to_row_with_text, open_workspace,
            path_at_cursor, record_commit, repository, rhs_editor, tab_title,
        },
    };
    use editor::{Editor, test::editor_test_context::assert_state_with_diff};
    use fs::Fs as _;
    use git::repository::RepoPath;
    use gpui::{Entity, TestAppContext, VisualContext as _, VisualTestContext};
    use language::{Capability, DiskState};
    use project::{FakeFs, Project, ProjectPath};
    use serde_json::json;
    use settings::DiffViewStyle;
    use std::path::Path;
    use unindent::Unindent as _;
    use util::{path, rel_path::RelPath};
    use workspace::{MultiWorkspace, SplitDirection, Workspace, item::Item as _};

    const C1_LIB: &str = "fn a() {}\n";
    const C2_LIB: &str = "fn a() {}\nfn b() {}\n";
    const C3_LIB: &str = "fn a() {}\nfn b() { 1 }\n";
    const WORKTREE_LIB: &str = "fn a() {}\nfn b() { 1 }\nfn c() {}\n";

    fn review_commits(commits: &str, cx: &mut VisualTestContext) {
        dispatch(
            ReviewCommits {
                commits: Some(commits.into()),
            },
            cx,
        );
    }

    fn is_snapshot(review: &Entity<ReviewView>, cx: &mut VisualTestContext) -> bool {
        review.read_with(cx, |review, cx| review.diff().read(cx).is_snapshot(cx))
    }

    fn head(fs: &FakeFs) -> Option<String> {
        fs.with_git_state(dot_git(), false, |state| state.refs.get("HEAD").cloned())
            .unwrap()
    }

    /// History C1 → C2 → C3 (HEAD), plus uncommitted edits. C1 adds `lib.rs` with `a`, C2 adds
    /// `b` and `x.txt`, C3 changes `b`.
    async fn history_fixture(
        cx: &mut TestAppContext,
    ) -> (
        std::sync::Arc<FakeFs>,
        Entity<Workspace>,
        &mut VisualTestContext,
    ) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "lib.rs": WORKTREE_LIB,
                "untracked.txt": "not in any commit\n",
                "x.txt": "x\ny\n",
            }),
        )
        .await;
        let (workspace, cx) = open_workspace(fs.clone(), cx).await;
        let repo = repository(&workspace, cx);
        record_commit(&fs, &repo, &[("lib.rs", C1_LIB.into())], C1, cx).await;
        record_commit(
            &fs,
            &repo,
            &[("lib.rs", C2_LIB.into()), ("x.txt", "x\n".into())],
            C2,
            cx,
        )
        .await;
        let c3 = [("lib.rs", C3_LIB.into()), ("x.txt", "x\n".into())];
        fs.set_head_for_repo(dot_git(), &c3, C3);
        fs.set_index_for_repo(dot_git(), &c3);
        // The fake lists a range's files by comparing these blobs with HEAD's contents, so every
        // range below changes the same files.
        fs.set_merge_base_content_for_repo(dot_git(), &[("lib.rs", C1_LIB.into())]);
        fs.set_commit_data(
            dot_git(),
            [
                (commit_data(C1, None, "Add a"), false),
                (commit_data(C2, Some(C1), "Add b\n\nAnd x."), false),
                (commit_data(C3, Some(C2), "Tweak b"), false),
            ],
        );
        for sha in [C1, C2, C3] {
            insert_commit_ref(&fs, sha, sha);
        }
        cx.run_until_parked();
        (fs, workspace, cx)
    }

    #[gpui::test]
    async fn test_review_commits_at_head_is_live(cx: &mut TestAppContext) {
        crate::init_test(cx);
        let (fs, workspace, cx) = history_fixture(cx).await;

        review_commits(&format!("{C1}..{C3}"), cx);
        let review = active_review(&workspace, cx);
        assert!(!is_snapshot(&review, cx));
        assert_eq!(tab_title(&review, cx), "Review: 2 commits 2222222–3333333");
        // Only the range's files, over the working tree: uncommitted edits to them show.
        assert_eq!(excerpt_paths(&review, cx), ["lib.rs", "x.txt"]);
        let editor = rhs_editor(&review, cx);
        assert_state_with_diff(
            &editor,
            cx,
            &"
                  ˇfn a() {}
                + fn b() { 1 }
                + fn c() {}

                + x
                + y
            "
            .unindent(),
        );
        editor.read_with(cx, |editor, cx| {
            assert!(editor.read_only(cx));
            for buffer in editor.buffer().read(cx).all_buffers() {
                let file = buffer.read(cx).file().unwrap().clone();
                assert!(
                    file.as_local().is_some(),
                    "{:?} isn't a project file",
                    file.path()
                );
                assert_eq!(buffer.read(cx).capability(), Capability::ReadWrite);
            }
        });

        fs.insert_file(path!("/project/x.txt"), "x\n".into()).await;
        cx.run_until_parked();
        assert_state_with_diff(
            &editor,
            cx,
            &"
                  ˇfn a() {}
                + fn b() { 1 }
                + fn c() {}

                + x
            "
            .unindent(),
        );
    }

    #[gpui::test]
    async fn test_review_commit_not_at_head_is_snapshot(cx: &mut TestAppContext) {
        init_vim_test(cx, DiffViewStyle::Unified);
        let (fs, workspace, cx) = history_fixture(cx).await;

        review_commits(C2, cx);
        let review = active_review(&workspace, cx);
        assert!(is_snapshot(&review, cx));
        assert_eq!(tab_title(&review, cx), "Review: 2222222 Add b");
        assert!(
            review
                .read_with(cx, |review, cx| review.tab_tooltip_text(cx))
                .unwrap()
                .contains("Read-only snapshot")
        );
        assert_eq!(excerpt_paths(&review, cx), ["lib.rs", "x.txt"]);
        let editor = rhs_editor(&review, cx);
        // C1 → C2, neither C3 nor the working tree.
        assert_state_with_diff(
            &editor,
            cx,
            &"
                  ˇfn a() {}
                + fn b() {}

                + x
            "
            .unindent(),
        );
        editor.read_with(cx, |editor, cx| {
            assert!(editor.read_only(cx));
            for buffer in editor.buffer().read(cx).all_buffers() {
                let buffer = buffer.read(cx);
                assert_eq!(buffer.capability(), Capability::ReadOnly);
                let file = buffer.file().unwrap();
                assert!(file.as_local().is_none());
                assert!(matches!(file.disk_state(), DiskState::Historic { .. }));
            }
        });

        // The line cursor and file navigation work on historic buffers too.
        cx.focus(&review);
        cx.simulate_keystrokes("j");
        assert_eq!(cursor_line(&editor, cx), "fn b() {}");
        cx.simulate_keystrokes("] f");
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("x.txt"));
        assert_eq!(cursor_line(&editor, cx), "x");
        cx.simulate_keystrokes("[ f");
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("lib.rs"));

        // The checkout never moves.
        assert_eq!(head(&fs).as_deref(), Some(C3));
        assert_eq!(
            fs.load(path!("/project/lib.rs").as_ref()).await.unwrap(),
            WORKTREE_LIB
        );

        // `e` opens the working tree's file at the same row.
        move_to_row_with_text(&editor, "fn b() {}", cx);
        cx.simulate_keystrokes("e");
        cx.run_until_parked();
        let opened = workspace.update(cx, |workspace, cx| {
            workspace.active_item_as::<Editor>(cx).unwrap()
        });
        opened.read_with(cx, |opened, cx| {
            let buffer = opened.buffer().read(cx).as_singleton().unwrap();
            let buffer = buffer.read(cx);
            assert!(buffer.file().unwrap().as_local().is_some());
            assert_eq!(buffer.text(), WORKTREE_LIB);
        });
        assert_eq!(cursor_point(&opened, cx).row, 1);
    }

    #[gpui::test]
    async fn test_snapshot_shows_text_that_isnt_utf8(cx: &mut TestAppContext) {
        crate::init_test(cx);
        let (fs, workspace, cx) = history_fixture(cx).await;
        let latin1 = b"caf\xe9\n".to_vec();
        fs.with_git_state(dot_git(), true, |state| {
            let path = RepoPath::new("latin1.txt").unwrap();
            state.head_contents.insert(path.clone(), latin1.clone());
            for snapshot in &mut state.commit_history {
                if snapshot.sha == C2 {
                    snapshot.head_contents.insert(path.clone(), latin1.clone());
                }
            }
        })
        .unwrap();
        cx.run_until_parked();

        review_commits(C2, cx);
        let review = active_review(&workspace, cx);
        assert!(is_snapshot(&review, cx));
        assert_eq!(
            excerpt_paths(&review, cx),
            ["latin1.txt", "lib.rs", "x.txt"]
        );
        let text = rhs_editor(&review, cx).read_with(cx, |editor, cx| editor.text(cx));
        assert!(text.contains("caf\u{fffd}"), "{text:?}");
        assert!(!text.contains("binary file"), "{text:?}");
    }

    #[gpui::test]
    async fn test_snapshot_paths_are_relative_to_the_worktree(cx: &mut TestAppContext) {
        crate::init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({ ".git": {}, "sub": { "lib.rs": WORKTREE_LIB } }),
        )
        .await;
        // The project is a subdirectory of the repository.
        let project = Project::test(fs.clone(), [Path::new(path!("/project/sub"))], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        cx.run_until_parked();
        cx.focus(&workspace);
        let repo = repository(&workspace, cx);
        record_commit(&fs, &repo, &[("sub/lib.rs", C1_LIB.into())], C1, cx).await;
        record_commit(&fs, &repo, &[("sub/lib.rs", C2_LIB.into())], C2, cx).await;
        let c3 = [("sub/lib.rs", C3_LIB.into())];
        fs.set_head_for_repo(dot_git(), &c3, C3);
        fs.set_index_for_repo(dot_git(), &c3);
        fs.set_merge_base_content_for_repo(dot_git(), &[("sub/lib.rs", C1_LIB.into())]);
        fs.set_commit_data(
            dot_git(),
            [
                (commit_data(C1, None, "Add a"), false),
                (commit_data(C2, Some(C1), "Add b"), false),
                (commit_data(C3, Some(C2), "Tweak b"), false),
            ],
        );
        for sha in [C1, C2, C3] {
            insert_commit_ref(&fs, sha, sha);
        }
        cx.run_until_parked();

        review_commits(C2, cx);
        let review = active_review(&workspace, cx);
        assert!(is_snapshot(&review, cx));
        assert_eq!(excerpt_paths(&review, cx), ["lib.rs"]);
        let worktree_id = project.read_with(cx, |project, cx| {
            project.worktrees(cx).next().unwrap().read(cx).id()
        });
        assert_eq!(
            review.read_with(cx, |review, cx| review.active_project_path(cx)),
            Some(ProjectPath {
                worktree_id,
                path: RelPath::from_unix_str("lib.rs").unwrap().into_arc(),
            })
        );
    }

    #[gpui::test]
    async fn test_split_keeps_a_live_review_live(cx: &mut TestAppContext) {
        crate::init_test(cx);
        let (fs, workspace, cx) = history_fixture(cx).await;
        review_commits(&format!("{C1}..{C3}"), cx);
        let review = active_review(&workspace, cx);
        assert!(!is_snapshot(&review, cx));

        // A commit moves HEAD past the reviewed range.
        fs.set_head_for_repo(
            dot_git(),
            &[("lib.rs", WORKTREE_LIB.into()), ("x.txt", "x\ny\n".into())],
            "4444444444444444444444444444444444444444",
        );
        cx.run_until_parked();
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        let split = workspace.update_in(cx, |workspace, window, cx| {
            workspace.split_and_clone(pane, SplitDirection::Right, window, cx)
        });
        cx.run_until_parked();
        split.await.unwrap();
        let clone = active_review(&workspace, cx);
        assert_ne!(clone, review);
        assert!(!is_snapshot(&clone, cx));
        assert_eq!(excerpt_paths(&clone, cx), excerpt_paths(&review, cx));
    }

    #[gpui::test]
    async fn test_root_commit_review(cx: &mut TestAppContext) {
        crate::init_test(cx);
        let (_fs, workspace, cx) = history_fixture(cx).await;

        // C1 has no parent: everything it has is added.
        review_commits(C1, cx);
        let review = active_review(&workspace, cx);
        assert_eq!(
            review.read_with(cx, |review, _| review.target().clone()),
            crate::ReviewTarget::Commits(crate::CommitRange {
                base: None,
                head: crate::test_support::oid(C1),
            })
        );
        assert!(is_snapshot(&review, cx));
        assert_eq!(tab_title(&review, cx), "Review: 1111111 Add a");
        // The fake lists HEAD's files; `x.txt` isn't in C1, so it is skipped.
        let editor = rhs_editor(&review, cx);
        assert_state_with_diff(
            &editor,
            cx,
            &"
                + ˇfn a() {}
            "
            .unindent(),
        );
    }
}
