use crate::{
    comments::CommentBlocks, commits::CommitChain, diff::ReviewDiff, forge::ForgeRepo,
    pull_request::PullRequestState, target::CommitRange, view::ReviewView,
};
use anyhow::Result;
use editor::hover_markdown_style;
use git::repository::CommitData;
use gpui::{Entity, FontWeight, ScrollHandle, Subscription, WeakEntity};
use language::LanguageRegistry;
use markdown::{Markdown, MarkdownElement};
use std::sync::Arc;
use time::{OffsetDateTime, UtcOffset};
use ui::{Disclosure, Tooltip, WithScrollbar as _, prelude::*};
use util::{ResultExt as _, truncate_and_trailoff};

/// The fixed header above a review's diff: the commit messages of a commit review, or the base of
/// a branch review, then the review-level comments. It sits outside the editor because a custom
/// block can't go above the first file header, and editor blocks are lost whenever the left side
/// of a split diff is recreated.
pub(crate) struct ReviewTopBlock {
    content: TopBlockContent,
    expanded: bool,
    view: WeakEntity<ReviewView>,
    diff: Entity<ReviewDiff>,
    comments: Entity<CommentBlocks>,
    scroll_handle: ScrollHandle,
    comments_scroll_handle: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

pub(crate) enum TopBlockContent {
    None,
    Branch {
        branch: SharedString,
        base: SharedString,
    },
    Commits {
        range: CommitRange,
        commits: CommitsState,
    },
    PullRequest {
        repo: ForgeRepo,
        number: u64,
        state: Option<Entity<PullRequestState>>,
    },
}

pub(crate) enum CommitsState {
    Loading,
    Loaded {
        /// Newest first.
        commits: Vec<TopBlockCommit>,
        truncated: bool,
    },
    Failed(SharedString),
}

pub(crate) struct TopBlockCommit {
    pub(crate) data: CommitData,
    /// The message without its subject line, when there is more than the subject.
    pub(crate) body: Option<Entity<Markdown>>,
}

impl ReviewTopBlock {
    pub(crate) fn new(
        content: TopBlockContent,
        view: WeakEntity<ReviewView>,
        diff: Entity<ReviewDiff>,
        comments: Entity<CommentBlocks>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscriptions = vec![
            // The snapshot label appears once the commit diff knows its mode.
            cx.observe(&diff, |_, _, cx| cx.notify()),
            cx.observe(&comments, |_, _, cx| cx.notify()),
        ];
        if let TopBlockContent::PullRequest {
            state: Some(state), ..
        } = &content
        {
            subscriptions.push(cx.observe(state, |_, _, cx| cx.notify()));
        }
        Self {
            content,
            expanded: false,
            view,
            diff,
            comments,
            scroll_handle: ScrollHandle::new(),
            comments_scroll_handle: ScrollHandle::new(),
            _subscriptions: subscriptions,
        }
    }

    #[cfg(test)]
    pub(crate) fn content(&self) -> &TopBlockContent {
        &self.content
    }

    pub(crate) fn has_content(&self) -> bool {
        !matches!(self.content, TopBlockContent::None)
    }

    #[cfg(test)]
    pub(crate) fn is_expanded(&self) -> bool {
        self.expanded
    }

    pub(crate) fn toggle_expanded(&mut self, cx: &mut Context<Self>) {
        self.expanded = !self.expanded;
        cx.notify();
    }

    pub(crate) fn set_branch(
        &mut self,
        branch: SharedString,
        base: SharedString,
        cx: &mut Context<Self>,
    ) {
        self.content = TopBlockContent::Branch { branch, base };
        cx.notify();
    }

    pub(crate) fn set_commits(
        &mut self,
        chain: Result<CommitChain>,
        language_registry: Arc<LanguageRegistry>,
        cx: &mut Context<Self>,
    ) {
        let TopBlockContent::Commits { commits, .. } = &mut self.content else {
            return;
        };
        *commits = match chain {
            Ok(chain) => CommitsState::Loaded {
                commits: chain
                    .commits
                    .into_iter()
                    .map(|data| {
                        let body = commit_body(&data.message).map(|body| {
                            cx.new(|cx| {
                                Markdown::new(
                                    body.into(),
                                    Some(language_registry.clone()),
                                    None,
                                    cx,
                                )
                            })
                        });
                        TopBlockCommit { data, body }
                    })
                    .collect(),
                truncated: chain.truncated,
            },
            Err(error) => CommitsState::Failed(format!("{error:#}").into()),
        };
        cx.notify();
    }

    /// The reviewed commits' short shas, oldest first, once they are loaded (and not too many to
    /// list).
    pub(crate) fn commit_shas(&self) -> Option<Vec<String>> {
        let TopBlockContent::Commits {
            commits: CommitsState::Loaded { commits, truncated },
            ..
        } = &self.content
        else {
            return None;
        };
        if *truncated || commits.is_empty() {
            return None;
        }
        Some(
            commits
                .iter()
                .rev()
                .map(|commit| commit.data.sha.display_short())
                .collect(),
        )
    }

    /// The tab title of a commit review: the commit, or the size and ends of the range.
    pub(crate) fn commits_title(&self) -> Option<SharedString> {
        let TopBlockContent::Commits { range, commits } = &self.content else {
            return None;
        };
        let CommitsState::Loaded { commits, truncated } = commits else {
            return Some(range.head.display_short().into());
        };
        let title = match (commits.as_slice(), *truncated) {
            ([], _) => range.head.display_short(),
            ([commit], false) => format!(
                "{} {}",
                commit.data.sha.display_short(),
                truncate_and_trailoff(&commit.data.subject, 20)
            ),
            ([newest, ..], true) => format!(
                "{}+ commits up to {}",
                commits.len(),
                newest.data.sha.display_short()
            ),
            ([newest, .., oldest], false) => format!(
                "{} commits {}–{}",
                commits.len(),
                oldest.data.sha.display_short(),
                newest.data.sha.display_short()
            ),
        };
        Some(title.into())
    }

    fn render_branch(
        &self,
        branch: &SharedString,
        base: &SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        h_flex()
            .gap_1()
            .child(
                Label::new(format!("Changes on {branch} since its merge base with"))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Button::new("review-change-base", base.clone())
                    .label_size(LabelSize::Small)
                    .end_icon(
                        Icon::new(IconName::ChevronDown)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .tooltip(Tooltip::text("Change Base"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.view
                            .update(cx, |view, cx| view.open_base_picker(window, cx))
                            .log_err();
                    })),
            )
            .into_any_element()
    }

    fn render_pull_request(
        &self,
        repo: &ForgeRepo,
        number: u64,
        state: Option<&Entity<PullRequestState>>,
        cx: &App,
    ) -> AnyElement {
        let details = state.map(|state| state.read(cx).details());
        // Another branch checked out, or the pull request moved on.
        let local_head = self
            .view
            .upgrade()
            .and_then(|view| view.read(cx).head_sha(cx));
        let moved_head = details.zip(local_head).and_then(|(details, local_head)| {
            (details.head_oid.to_string() != local_head.as_ref()).then(|| {
                format!(
                    "Checked out {}, pull request head {}",
                    local_head.get(..7).unwrap_or(&local_head),
                    details.head_oid.display_short()
                )
            })
        });
        h_flex()
            .gap_1()
            .min_w_0()
            .child(
                Label::new(format!("{repo}#{number}"))
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .flex_none(),
            )
            .when_some(details, |line, details| {
                line.child(
                    Label::new(details.title.clone())
                        .size(LabelSize::Small)
                        .truncate(),
                )
                .child(
                    Label::new(format!(
                        "{} wants to merge {} into {}",
                        details.author.login, details.head_ref, details.base_ref
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate(),
                )
            })
            .when_some(moved_head, |line, moved_head| {
                line.child(
                    Icon::new(IconName::Warning)
                        .size(IconSize::XSmall)
                        .color(Color::Warning),
                )
                .child(
                    Label::new(moved_head)
                        .size(LabelSize::Small)
                        .color(Color::Warning)
                        .flex_none(),
                )
            })
            .into_any_element()
    }

    fn render_commits(
        &self,
        commits: &CommitsState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_snapshot = self.diff.read(cx).is_snapshot(cx);
        let summary = self.commits_title().unwrap_or_default();
        let header = h_flex()
            .gap_1()
            .child(
                Disclosure::new("review-commit-messages", self.expanded)
                    .tooltip(Tooltip::text(if self.expanded {
                        "Hide Commit Messages"
                    } else {
                        "Show Commit Messages"
                    }))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_expanded(cx))),
            )
            .child(Label::new(summary).size(LabelSize::Small))
            .when(is_snapshot, |header| {
                header.child(
                    Label::new("· Read-only snapshot")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            });

        let body = self.expanded.then(|| match commits {
            CommitsState::Loading => Label::new("Loading commits…")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
            CommitsState::Failed(error) => Label::new(error.clone())
                .size(LabelSize::Small)
                .color(Color::Error)
                .into_any_element(),
            CommitsState::Loaded { commits, truncated } => {
                let timezone = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
                let markdown_style = hover_markdown_style(window, cx);
                div()
                    .relative()
                    .w_full()
                    .child(
                        v_flex()
                            .id("review-commit-messages-list")
                            .w_full()
                            .max_h(window.line_height() * 16.)
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll_handle)
                            .children(commits.iter().map(|commit| {
                                render_commit(commit, timezone, markdown_style.clone(), cx)
                            }))
                            .when(*truncated, |list| {
                                list.child(
                                    Label::new("…and more commits")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                            }),
                    )
                    .vertical_scrollbar_for(&self.scroll_handle, window, cx)
                    .into_any_element()
            }
        });

        v_flex()
            .gap_1()
            .child(header)
            .children(body)
            .into_any_element()
    }
}

fn render_commit(
    commit: &TopBlockCommit,
    timezone: UtcOffset,
    markdown_style: markdown::MarkdownStyle,
    cx: &App,
) -> AnyElement {
    let data = &commit.data;
    let date = OffsetDateTime::from_unix_timestamp(data.commit_timestamp)
        .unwrap_or_else(|_| OffsetDateTime::now_utc());
    let date = time_format::format_localized_timestamp(
        date,
        OffsetDateTime::now_utc(),
        timezone,
        time_format::TimestampFormat::MediumAbsolute,
    );
    v_flex()
        .w_full()
        .py_1()
        .child(
            h_flex()
                .gap_2()
                .child(
                    Label::new(data.sha.display_short())
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .buffer_font(cx),
                )
                .child(
                    Label::new(data.subject.clone())
                        .size(LabelSize::Small)
                        .weight(FontWeight::SEMIBOLD),
                ),
        )
        .child(
            Label::new(format!("{} · {date}", data.author_name))
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
        .when_some(commit.body.clone(), |element, body| {
            element.child(
                div()
                    .pt_1()
                    .text_sm()
                    .child(MarkdownElement::new(body, markdown_style)),
            )
        })
        .into_any_element()
}

/// The commit message after its subject line.
fn commit_body(message: &str) -> Option<String> {
    let body = message.split_once('\n')?.1.trim();
    (!body.is_empty()).then(|| body.to_string())
}

impl Render for ReviewTopBlock {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match &self.content {
            TopBlockContent::None => None,
            TopBlockContent::Branch { branch, base } => Some(self.render_branch(branch, base, cx)),
            TopBlockContent::Commits { commits, .. } => {
                Some(self.render_commits(commits, window, cx))
            }
            TopBlockContent::PullRequest {
                repo,
                number,
                state,
            } => Some(self.render_pull_request(repo, *number, state.as_ref(), cx)),
        };
        let review_comments = self.comments.read(cx).review_comment_views(cx);
        if content.is_none() && review_comments.is_empty() {
            return div().into_any_element();
        }
        v_flex()
            .w_full()
            .flex_none()
            .px_3()
            .py_1()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .bg(cx.theme().colors().editor_background)
            .children(content)
            .when(!review_comments.is_empty(), |top_block| {
                top_block.child(
                    v_flex()
                        .gap_1()
                        .child(
                            Label::new("Review comments")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            div()
                                .relative()
                                .w_full()
                                .child(
                                    v_flex()
                                        .id("review-comments-list")
                                        .w_full()
                                        .gap_1()
                                        .max_h(window.line_height() * 16.)
                                        .overflow_y_scroll()
                                        .track_scroll(&self.comments_scroll_handle)
                                        .children(review_comments),
                                )
                                .vertical_scrollbar_for(&self.comments_scroll_handle, window, cx),
                        ),
                )
            })
            .into_any_element()
    }
}
