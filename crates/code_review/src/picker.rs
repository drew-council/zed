use crate::{
    StartReview,
    commits::{RECENT_COMMITS_LIMIT, first_parent_chain},
    target::{BranchBase, CommitRange, ReviewTarget},
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
use project::git_store::Repository;
use std::sync::Arc;
use time::{OffsetDateTime, UtcOffset};
use ui::{HighlightedLabel, KeyBinding, ListItem, ListItemSpacing, ListSubHeader, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

pub(crate) fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &StartReview, window, cx| {
        StartReviewModal::toggle(workspace, StartReviewFocus::Default, window, cx);
    });
}

/// Which row the Start Review picker selects when it opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartReviewFocus {
    /// "Uncommitted changes".
    Default,
    /// The newest commit, once the commits have loaded.
    Commits,
}

/// The Start Review picker: uncommitted changes, the current branch against its base, and recent
/// commits, where multi-selection picks a range.
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
        workspace.toggle_modal(window, cx, |window, cx| {
            Self::new(workspace_handle, repo, focus, window, cx)
        });
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        repo: Entity<Repository>,
        focus: StartReviewFocus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let default_branch = repo.update(cx, |repo, _| repo.default_branch(true));
        let recent_commits = first_parent_chain(&repo, None, None, RECENT_COMMITS_LIMIT, cx);
        let delegate = StartReviewDelegate::new(workspace, repo, focus, cx.focus_handle());
        let picker = cx.new(|cx| {
            Picker::list(delegate, window, cx)
                .initial_width(rems(34.))
                .show_scrollbar(true)
        });
        picker.update(cx, |picker, cx| {
            picker.delegate.focus_handle = picker.focus_handle(cx);
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
                .ok();
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
                .ok();
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
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("StartReview")
            .w(rems(34.))
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Section {
    Local,
    // Milestone 7 adds the pull request section after the local one.
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
    Status(SharedString),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryKey {
    Uncommitted,
    Branch,
    Commit(usize),
}

impl Entry {
    fn key(&self) -> Option<EntryKey> {
        match self {
            Entry::Uncommitted { .. } => Some(EntryKey::Uncommitted),
            Entry::Branch { .. } => Some(EntryKey::Branch),
            Entry::Commit { index, .. } => Some(EntryKey::Commit(*index)),
            Entry::Header(_) | Entry::Status(_) => None,
        }
    }
}

const UNCOMMITTED_CANDIDATE: usize = 0;
const BRANCH_CANDIDATE: usize = 1;
const FIRST_COMMIT_CANDIDATE: usize = 2;

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
    repo: Entity<Repository>,
    default_base: BaseState,
    commits: RecentCommits,
    selection: CommitSelection,
    entries: Vec<Entry>,
    selected_index: usize,
    query: String,
    /// Set until the commits load when the picker opens on them.
    select_first_commit: bool,
    focus_handle: FocusHandle,
    timezone: UtcOffset,
}

impl StartReviewDelegate {
    fn new(
        workspace: WeakEntity<Workspace>,
        repo: Entity<Repository>,
        focus: StartReviewFocus,
        focus_handle: FocusHandle,
    ) -> Self {
        Self {
            workspace,
            repo,
            default_base: BaseState::Loading,
            commits: RecentCommits::Loading,
            selection: CommitSelection::default(),
            entries: Vec::new(),
            selected_index: 0,
            query: String::new(),
            select_first_commit: focus == StartReviewFocus::Commits,
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
        candidates
    }

    /// Builds the rows from matched candidates (in candidate order) and keeps the selection on
    /// the same row when only the data changed.
    fn set_matches(&mut self, query: String, matches: Vec<(usize, Vec<usize>)>) {
        let previous_key = (query == self.query)
            .then(|| self.entries.get(self.selected_index).and_then(Entry::key))
            .flatten();
        self.query = query;

        let local_entries = matches
            .into_iter()
            .map(|(candidate, positions)| match candidate {
                UNCOMMITTED_CANDIDATE => Entry::Uncommitted { positions },
                BRANCH_CANDIDATE => Entry::Branch { positions },
                candidate => Entry::Commit {
                    index: candidate - FIRST_COMMIT_CANDIDATE,
                    positions,
                },
            })
            .collect::<Vec<_>>();
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
        // Milestone 7 appends the pull request section here.
        self.entries = entries;

        let wanted_key =
            if self.select_first_commit && matches!(self.commits, RecentCommits::Loaded(_)) {
                self.select_first_commit = false;
                Some(EntryKey::Commit(0))
            } else {
                previous_key
            };
        self.selected_index = wanted_key
            .and_then(|key| {
                self.entries
                    .iter()
                    .position(|entry| entry.key() == Some(key))
            })
            .or_else(|| self.entries.iter().position(|entry| entry.key().is_some()))
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

    fn render_commit(
        &self,
        ix: usize,
        index: usize,
        positions: &[usize],
        selected: bool,
        checkbox: Option<AnyElement>,
    ) -> Option<AnyElement> {
        let commit = self.recent_commits().get(index)?;
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
            ListItem::new(("start-review-commit", ix))
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
        "Review uncommitted changes, the branch, or commits…".into()
    }

    fn match_count(&self) -> usize {
        self.entries.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn can_select(&self, ix: usize, _window: &mut Window, _cx: &mut Context<Picker<Self>>) -> bool {
        self.entries
            .get(ix)
            .is_some_and(|entry| entry.key().is_some())
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
        let Some(key) = self.entries.get(self.selected_index).and_then(Entry::key) else {
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
        }
    }

    fn supports_multi_select(&self) -> bool {
        true
    }

    fn is_item_selected(&self, ix: usize) -> bool {
        match self.entries.get(ix) {
            Some(Entry::Commit { index, .. }) => self.selection.contains(*index),
            _ => false,
        }
    }

    fn toggle_item_selected(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        if let Some(Entry::Commit { index, .. }) = self.entries.get(ix) {
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
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let row = |label: AnyElement| {
            ListItem::new(("start-review-entry", ix))
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(label)
                .into_any_element()
        };
        match self.entries.get(ix)? {
            Entry::Header(Section::Local) => Some(
                div()
                    .pt_1()
                    .child(ListSubHeader::new("Local").inset(true))
                    .into_any_element(),
            ),
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
                self.render_commit(ix, *index, positions, selected, None)
            }
            Entry::Status(text) => Some(
                ListItem::new(("start-review-status", ix))
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
        ix: usize,
        selected: bool,
        checkbox: AnyElement,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        match self.entries.get(ix)? {
            Entry::Commit { index, positions } => {
                self.render_commit(ix, *index, positions, selected, Some(checkbox))
            }
            _ => self.render_match(ix, selected, window, cx),
        }
    }

    fn render_footer(
        &self,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        let focus_handle = &self.focus_handle;
        let selected = self.entries.get(self.selected_index).and_then(Entry::key);
        let selected_count = self.selection.len();
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
                .child(
                    Label::new(match selected_count {
                        0 => String::new(),
                        1 => "1 commit selected".to_string(),
                        count => format!("{count} commits selected"),
                    })
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
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
        test_support::{
            C1, C2, C3, active_review, commit_data, dispatch, dot_git, oid, open_workspace,
            review_count,
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
            picker.delegate.entries().iter().map(Entry::key).collect()
        })
    }

    fn selected_key(
        picker: &Entity<Picker<StartReviewDelegate>>,
        cx: &mut VisualTestContext,
    ) -> Option<EntryKey> {
        picker.read_with(cx, |picker, _| {
            picker
                .delegate
                .entries()
                .get(picker.delegate.selected_index())
                .and_then(Entry::key)
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
