use crate::{
    ToggleHeader,
    comments::{
        CommentBlocks, HoveredLink, comment_markdown_style, github_markdown, open_web_link,
        untrusted_markdown,
    },
    commits::CommitChain,
    diff::ReviewDiff,
    forge::{ForgeRepo, PullRequestStatus, RemoteReviewSummary, ReviewDecision, ReviewState},
    pull_request::{PullRequestState, ThreadFilters},
    target::CommitRange,
    view::ReviewView,
};
use anyhow::Result;
use collections::HashMap;
use editor::{
    Anchor,
    display_map::{BlockPlacement, BlockProperties, BlockStyle},
    hover_markdown_style,
};
use git::repository::CommitData;
use gpui::{AnyView, Entity, FontWeight, Subscription, WeakEntity};
use language::LanguageRegistry;
use markdown::{Markdown, MarkdownElement, parser::PARSE_OPTIONS};
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use std::sync::Arc;
use time::{OffsetDateTime, UtcOffset};
use ui::{Checkbox, Disclosure, ToggleState, Tooltip, prelude::*};
use util::{ResultExt as _, truncate_and_trailoff};

/// The review's overview, at the top of the diff: the commit messages of a commit review, the
/// base of a branch review, or a pull request's description and reviews, then the review-level
/// comments. It is a block above the first file (in the right side's editor, drawn across both
/// sides of a split diff), so it scrolls away with the diff like the rest of the document.
/// Nothing in it scrolls on its own.
pub(crate) struct ReviewTopBlock {
    content: TopBlockContent,
    expanded: bool,
    view: WeakEntity<ReviewView>,
    diff: Entity<ReviewDiff>,
    comments: Entity<CommentBlocks>,
    language_registry: Arc<LanguageRegistry>,
    /// A pull request's description and review summaries, rendered.
    markdown: HashMap<SharedString, Entity<Markdown>>,
    hovered_link: HoveredLink,
    _subscriptions: Vec<Subscription>,
}

/// The key of a pull request description's markdown; summaries are keyed by their id.
const DESCRIPTION_KEY: &str = "description";

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
        language_registry: Arc<LanguageRegistry>,
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
            subscriptions.push(cx.observe(state, |this, _, cx| {
                this.sync_pull_request_markdown(cx);
                cx.notify();
            }));
        }
        // A pull request's description is what the review is about; commit messages stay out
        // of the way until asked for.
        let expanded = matches!(content, TopBlockContent::PullRequest { .. });
        let mut this = Self {
            content,
            expanded,
            view,
            diff,
            comments,
            language_registry,
            markdown: HashMap::default(),
            hovered_link: HoveredLink::default(),
            _subscriptions: subscriptions,
        };
        this.sync_pull_request_markdown(cx);
        this
    }

    /// Renders the pull request's description and summaries again where their text changed.
    fn sync_pull_request_markdown(&mut self, cx: &mut Context<Self>) {
        let TopBlockContent::PullRequest {
            state: Some(state), ..
        } = &self.content
        else {
            return;
        };
        let state = state.read(cx);
        let sources = std::iter::once((
            SharedString::from(DESCRIPTION_KEY),
            github_markdown(&state.details().body),
        ))
        .chain(
            state
                .visible_summaries()
                .map(|summary| (summary.id.clone(), github_markdown(&summary.body))),
        )
        .collect::<Vec<_>>();
        let mut previous = std::mem::take(&mut self.markdown);
        for (key, source) in sources {
            let markdown = match previous.remove(&key) {
                Some(markdown) => {
                    if markdown.read(cx).source() != source.as_ref() {
                        markdown.update(cx, |markdown, cx| markdown.replace(source, cx));
                    }
                    markdown
                }
                None => {
                    let language_registry = self.language_registry.clone();
                    cx.new(|cx| Markdown::new(source, Some(language_registry), None, cx))
                }
            };
            self.markdown.insert(key, markdown);
        }
    }

    #[cfg(test)]
    pub(crate) fn markdown_source(&self, key: &str, cx: &App) -> Option<SharedString> {
        self.markdown
            .get(key)
            .map(|markdown| markdown.read(cx).source().to_string().into())
    }

    #[cfg(test)]
    pub(crate) fn markdown_keys(&self) -> Vec<SharedString> {
        let mut keys = self.markdown.keys().cloned().collect::<Vec<_>>();
        keys.sort();
        keys
    }

    #[cfg(test)]
    pub(crate) fn content(&self) -> &TopBlockContent {
        &self.content
    }

    pub(crate) fn has_content(&self) -> bool {
        !matches!(self.content, TopBlockContent::None)
    }

    /// Whether there's nothing to show: an uncommitted review without review-level comments.
    /// The diff then starts with its first file.
    pub(crate) fn is_empty(&self, cx: &App) -> bool {
        !self.has_content() && self.comments.read(cx).review_comment_views(cx).is_empty()
    }

    /// The block that shows the overview above the first file of the diff's right side, drawn
    /// across both sides of a split diff (`Editor::set_full_width_block`). It takes the height
    /// of its content once rendered.
    pub(crate) fn block_properties(top_block: &Entity<Self>) -> BlockProperties<Anchor> {
        // Weak: the overview holds the diff, whose editor holds the block.
        let view = AnyView::from(top_block.clone()).downgrade();
        BlockProperties {
            placement: BlockPlacement::Above(Anchor::Min),
            height: Some(1),
            // Not moved by the diff's horizontal scroll.
            style: BlockStyle::Sticky,
            priority: 0,
            render: Arc::new(move |cx| {
                // Opaque over the whole block: the left side's spacer is under it.
                div()
                    .size_full()
                    .bg(cx.theme().colors().editor_background)
                    .block_mouse_except_scroll()
                    .children(view.upgrade())
                    .into_any_element()
            }),
        }
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

    /// The title of a commit review: the commit, or the size and ends of the range.
    pub(crate) fn commits_title(&self) -> Option<SharedString> {
        self.commits_title_with_subject(|subject| subject.to_string())
    }

    /// [`Self::commits_title`] with a single commit's subject cut to fit in a tab.
    pub(crate) fn commits_tab_title(&self) -> Option<SharedString> {
        self.commits_title_with_subject(|subject| truncate_and_trailoff(subject, 20))
    }

    fn commits_title_with_subject(
        &self,
        subject_text: impl Fn(&str) -> String,
    ) -> Option<SharedString> {
        let TopBlockContent::Commits { range, commits } = &self.content else {
            return None;
        };
        let CommitsState::Loaded { commits, truncated } = commits else {
            return Some(range.head.display_short().into());
        };
        let title = match (commits.as_slice(), *truncated) {
            ([], _) => range.head.display_short(),
            // GitHub's squash merges end their subject with the pull request's number, which the
            // cut subject would lose.
            ([commit], false) => match split_pull_request_suffix(&commit.data.subject) {
                Some((subject, number)) => format!("#{number} {}", subject_text(subject)),
                None => format!(
                    "{} {}",
                    commit.data.sha.display_short(),
                    subject_text(&commit.data.subject)
                ),
            },
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

    /// The tab tooltip of a commit review: a single commit's sha and whole subject, else
    /// [`Self::commits_title`].
    pub(crate) fn commits_tooltip(&self) -> Option<SharedString> {
        if let TopBlockContent::Commits {
            commits:
                CommitsState::Loaded {
                    commits,
                    truncated: false,
                },
            ..
        } = &self.content
            && let [commit] = commits.as_slice()
        {
            return Some(
                format!(
                    "{} {}",
                    commit.data.sha.display_short(),
                    commit.data.subject
                )
                .into(),
            );
        }
        self.commits_title()
    }

    fn render_branch(
        &self,
        branch: &SharedString,
        base: &SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        h_flex()
            .w_full()
            .min_w_0()
            .gap_1()
            .child(
                Label::new(format!("Changes on {branch} since its merge base with"))
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate(),
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(state) = state else {
            return Label::new(format!("{repo}#{number}"))
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element();
        };
        let (details, summaries, hidden_bot_summaries, load_errors) = {
            let pull_request = state.read(cx);
            (
                pull_request.details().clone(),
                pull_request
                    .visible_summaries()
                    .cloned()
                    .collect::<Vec<_>>(),
                pull_request.hidden_bot_summaries(),
                pull_request.load_errors().to_vec(),
            )
        };
        // Another branch checked out, or the pull request moved on.
        let local_head = self
            .view
            .upgrade()
            .and_then(|view| view.read(cx).head_sha(cx));
        let moved_head = local_head.and_then(|local_head| {
            (details.head_oid.to_string() != local_head.as_ref()).then(|| {
                format!(
                    "Checked out {}, pull request head {}",
                    local_head.get(..7).unwrap_or(&local_head),
                    details.head_oid.display_short()
                )
            })
        });
        let state_badge = if details.is_draft {
            Some(("Draft", Color::Muted))
        } else {
            match details.state {
                PullRequestStatus::Open => None,
                PullRequestStatus::Closed => Some(("Closed", Color::Error)),
                PullRequestStatus::Merged => Some(("Merged", Color::Accent)),
            }
        };
        let decision = details.review_decision.map(|decision| match decision {
            ReviewDecision::Approved => ("Approved", Color::Success),
            ReviewDecision::ChangesRequested => ("Changes requested", Color::Error),
            ReviewDecision::ReviewRequired => ("Review required", Color::Muted),
        });
        let url = details.url.clone();
        let header = h_flex()
            .gap_1()
            .min_w_0()
            .child(
                div()
                    .debug_selector(|| "review-pull-request-disclosure".into())
                    .child(
                        Disclosure::new("review-pull-request-description", self.expanded)
                            .tooltip(Tooltip::for_action_title(
                                if self.expanded {
                                    "Hide Description"
                                } else {
                                    "Show Description"
                                },
                                &ToggleHeader,
                            ))
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_expanded(cx))),
                    ),
            )
            .child(
                Label::new(details.title.clone())
                    .size(LabelSize::Small)
                    .weight(FontWeight::SEMIBOLD)
                    .truncate(),
            )
            .child(
                Label::new(format!("#{number}"))
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .flex_none(),
            )
            .children(state_badge.map(|(label, color)| badge(label, color, cx)))
            .child(
                Label::new(format!(
                    "{} wants to merge {} into {}",
                    details.author.login, details.head_ref, details.base_ref
                ))
                .size(LabelSize::Small)
                .color(Color::Muted)
                .truncate(),
            )
            .children(decision.map(|(label, color)| badge(label, color, cx)))
            .child(div().flex_1())
            .child(
                IconButton::new("review-pull-request-open", IconName::ArrowUpRight)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .tooltip({
                        let url = url.clone();
                        move |_, cx| Tooltip::with_meta("Open on GitHub", None, url.clone(), cx)
                    })
                    .on_click({
                        let url = url.clone();
                        move |_, window, cx| open_web_link(&url, window, cx)
                    }),
            );
        let warnings = moved_head
            .into_iter()
            .chain(
                details
                    .pending_review
                    .is_some()
                    .then(|| "You have a pending review on GitHub".to_string()),
            )
            .map(|warning| {
                h_flex()
                    .gap_1()
                    .child(
                        Icon::new(IconName::Warning)
                            .size(IconSize::XSmall)
                            .color(Color::Warning),
                    )
                    .child(
                        Label::new(warning)
                            .size(LabelSize::Small)
                            .color(Color::Warning),
                    )
            })
            .collect::<Vec<_>>();

        let body = self.expanded.then(|| {
            let markdown_style = comment_markdown_style(window, cx);
            let timezone = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
            let description = self
                .markdown
                .get(DESCRIPTION_KEY)
                .filter(|_| !details.body.trim().is_empty())
                .cloned();
            let summaries = summaries
                .iter()
                .filter_map(|summary| {
                    let markdown = self.markdown.get(&summary.id)?.clone();
                    let body = untrusted_markdown(
                        markdown,
                        markdown_style.clone(),
                        summary.id.clone(),
                        |this: &mut Self| &mut this.hovered_link,
                        cx,
                    );
                    Some(render_summary(summary, body, timezone, cx))
                })
                .collect::<Vec<_>>();
            v_flex()
                .gap_1()
                .child(
                    Button::new("review-pull-request-url", url.clone())
                        .label_size(LabelSize::Small)
                        .color(Color::Muted)
                        .tooltip(Tooltip::text("Open on GitHub"))
                        .on_click({
                            let url = url.clone();
                            move |_, window, cx| open_web_link(&url, window, cx)
                        }),
                )
                .child(
                    v_flex()
                        .id("review-pull-request-description-text")
                        .debug_selector(|| "review-pull-request-description-text".into())
                        .w_full()
                        .gap_1()
                        .child(match description {
                            Some(description) => div()
                                .w_full()
                                .text_sm()
                                .child(untrusted_markdown(
                                    description,
                                    markdown_style,
                                    DESCRIPTION_KEY,
                                    |this: &mut Self| &mut this.hovered_link,
                                    cx,
                                ))
                                .into_any_element(),
                            None => Label::new("No description provided.")
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .into_any_element(),
                        })
                        .when(!summaries.is_empty(), |list| {
                            list.child(
                                Label::new("Reviews")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .children(summaries)
                        })
                        .when(hidden_bot_summaries > 0, |list| {
                            list.child(
                                Label::new(if hidden_bot_summaries == 1 {
                                    "1 bot review hidden".to_string()
                                } else {
                                    format!("{hidden_bot_summaries} bot reviews hidden")
                                })
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                            )
                        }),
                )
                .child(self.render_thread_filters(state, cx))
                .children(load_errors.iter().map(|error| {
                    Label::new(error.clone())
                        .size(LabelSize::Small)
                        .color(Color::Error)
                }))
        });

        v_flex()
            .gap_1()
            .child(header)
            .children(warnings)
            .children(body)
            .into_any_element()
    }

    /// "Show threads: Resolved (n) · Outdated (n) · Bots (n)", and Reload.
    fn render_thread_filters(
        &self,
        state: &Entity<PullRequestState>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let pull_request = state.read(cx);
        let filters = pull_request.filters();
        let counts = pull_request.thread_counts();
        let is_loading = pull_request.is_loading();
        let toggle =
            |id: &'static str, label: String, checked: bool, update: fn(&mut ThreadFilters)| {
                let state = state.downgrade();
                Checkbox::new(id, ToggleState::from(checked))
                    .label(label)
                    .label_size(LabelSize::Small)
                    .on_click(move |_, _, cx| {
                        state
                            .update(cx, |pull_request, cx| {
                                let mut filters = pull_request.filters();
                                update(&mut filters);
                                pull_request.set_filters(filters, cx);
                            })
                            .log_err();
                    })
            };
        h_flex()
            .gap_3()
            .child(
                Label::new("Show:")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(toggle(
                "review-show-resolved-threads",
                format!("Resolved threads ({})", counts.resolved),
                filters.show_resolved,
                |filters| filters.show_resolved = !filters.show_resolved,
            ))
            .child(toggle(
                "review-show-outdated-threads",
                format!("Outdated threads ({})", counts.outdated),
                filters.show_outdated,
                |filters| filters.show_outdated = !filters.show_outdated,
            ))
            .child(toggle(
                "review-show-bot-reviews",
                format!("Bots ({})", counts.bots),
                filters.show_bots,
                |filters| filters.show_bots = !filters.show_bots,
            ))
            .child(div().flex_1())
            .when(is_loading, |row| {
                row.child(
                    Label::new("Loading from GitHub…")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .child(
                Button::new("review-reload-pull-request", "Reload")
                    .label_size(LabelSize::Small)
                    .disabled(is_loading)
                    .tooltip(Tooltip::text("Load the Pull Request from GitHub Again"))
                    .on_click({
                        let state = state.downgrade();
                        move |_, _, cx| {
                            state
                                .update(cx, |pull_request, cx| pull_request.reload(cx))
                                .log_err();
                        }
                    }),
            )
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
            .w_full()
            .min_w_0()
            .gap_1()
            .child(
                Disclosure::new("review-commit-messages", self.expanded)
                    .tooltip(Tooltip::for_action_title(
                        if self.expanded {
                            "Hide Commit Messages"
                        } else {
                            "Show Commit Messages"
                        },
                        &ToggleHeader,
                    ))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_expanded(cx))),
            )
            .child(Label::new(summary).size(LabelSize::Small).truncate())
            .when(is_snapshot, |header| {
                header.child(
                    Label::new("· Read-only snapshot")
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .flex_none(),
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
                v_flex()
                    .id("review-commit-messages-list")
                    .w_full()
                    .children(commits.iter().map(|commit| {
                        let body = commit.body.clone().map(|body| {
                            untrusted_markdown(
                                body,
                                markdown_style.clone(),
                                SharedString::from(commit.data.sha.to_string()),
                                |this: &mut Self| &mut this.hovered_link,
                                cx,
                            )
                        });
                        render_commit(commit, body, timezone, cx)
                    }))
                    .when(*truncated, |list| {
                        list.child(
                            Label::new("…and more commits")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
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

fn badge(label: &'static str, color: Color, cx: &App) -> impl IntoElement {
    div()
        .flex_none()
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(color.color(cx).opacity(0.6))
        .child(Label::new(label).size(LabelSize::XSmall).color(color))
}

fn render_summary(
    summary: &RemoteReviewSummary,
    body: MarkdownElement,
    timezone: UtcOffset,
    cx: &App,
) -> AnyElement {
    let state = match summary.state {
        ReviewState::Commented => "commented",
        ReviewState::Approved => "approved",
        ReviewState::ChangesRequested => "requested changes",
        ReviewState::Dismissed => "review dismissed",
    };
    let time = summary.submitted_at.map(|submitted_at| {
        time_format::format_localized_timestamp(
            submitted_at,
            OffsetDateTime::now_utc(),
            timezone,
            time_format::TimestampFormat::Relative,
        )
    });
    v_flex()
        .w_full()
        .py_1()
        .border_t_1()
        .border_color(cx.theme().colors().border_variant)
        .child(
            h_flex()
                .gap_1()
                .child(
                    Label::new(summary.author.login.clone())
                        .size(LabelSize::Small)
                        .weight(FontWeight::SEMIBOLD),
                )
                .when(summary.author.is_bot, |line| {
                    line.child(badge("BOT", Color::Muted, cx))
                })
                .child(
                    Label::new(match time {
                        Some(time) => format!("{state} · {time}"),
                        None => state.to_string(),
                    })
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
                ),
        )
        .child(div().pt_0p5().text_sm().child(body))
        .into_any_element()
}

fn render_commit(
    commit: &TopBlockCommit,
    body: Option<MarkdownElement>,
    timezone: UtcOffset,
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
                .items_start()
                .gap_2()
                .child(
                    Label::new(data.sha.display_short())
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .buffer_font(cx)
                        .flex_none(),
                )
                // Wrapped: the header above truncates the subject, this is where it's read whole.
                .child(
                    div().flex_1().min_w_0().child(
                        Label::new(data.subject.clone())
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD),
                    ),
                ),
        )
        .child(
            Label::new(format!("{} · {date}", data.author_name))
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
        .when_some(body, |element, body| {
            element.child(div().pt_1().text_sm().child(body))
        })
        .into_any_element()
}

/// The commit message after its subject line.
fn commit_body(message: &str) -> Option<String> {
    let body = message.split_once('\n')?.1.trim();
    (!body.is_empty()).then(|| join_bare_list_markers(body))
}

/// Commit messages are hard-wrapped, and wrapping a list item whose first word doesn't fit on the
/// line (usually a long URL) leaves its marker alone on a line. Markdown reads that as an empty
/// item followed by a paragraph outside the list, so such a marker gets its next line back.
fn join_bare_list_markers(body: &str) -> String {
    let events = Parser::new_ext(body, PARSE_OPTIONS)
        .into_offset_iter()
        .collect::<Vec<_>>();
    let mut joins = Vec::new();
    for (index, (event, range)) in events.iter().enumerate() {
        if !matches!(event, Event::Start(Tag::Item))
            || !matches!(events.get(index + 1), Some((Event::End(TagEnd::Item), _)))
        {
            continue;
        }
        let Some((next, next_range)) = events
            .iter()
            .skip(index + 2)
            .find(|(event, _)| !matches!(event, Event::End(TagEnd::List(_))))
        else {
            continue;
        };
        if !starts_paragraph(next) {
            continue;
        }
        let Some(marker) = body.get(range.clone()) else {
            continue;
        };
        let marker_end = range.start + marker.trim_end().len();
        let Some(gap) = body.get(marker_end..next_range.start) else {
            continue;
        };
        // Only a line break and indentation: anything else (a quote's `>`, a blank line) means
        // the paragraph isn't the marker's wrapped text.
        if gap.matches('\n').count() == 1 && gap.chars().all(char::is_whitespace) {
            joins.push(marker_end..next_range.start);
        }
    }
    let mut joined = body.to_string();
    for range in joins.into_iter().rev() {
        joined.replace_range(range, " ");
    }
    joined
}

/// Whether the event begins a paragraph, which in a tight list has no paragraph event of its own.
fn starts_paragraph(event: &Event) -> bool {
    matches!(
        event,
        Event::Start(
            Tag::Paragraph
                | Tag::Emphasis
                | Tag::Strong
                | Tag::Strikethrough
                | Tag::Superscript
                | Tag::Subscript
                | Tag::Link { .. }
                | Tag::Image { .. }
        ) | Event::Text(_)
            | Event::Code(_)
            | Event::InlineHtml(_)
            | Event::FootnoteReference(_)
    )
}

/// A subject ending like GitHub's squash merges, "Subject (#123)": the subject without that
/// suffix, and the pull request's number.
fn split_pull_request_suffix(subject: &str) -> Option<(&str, &str)> {
    let (subject, number) = subject.strip_suffix(')')?.rsplit_once(" (#")?;
    let subject = subject.trim_end();
    (!subject.is_empty() && !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()))
        .then_some((subject, number))
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
            } => {
                let (repo, number, state) = (repo.clone(), *number, state.clone());
                Some(self.render_pull_request(&repo, number, state.as_ref(), window, cx))
            }
        };
        let review_comments = self.comments.read(cx).review_comment_views(cx);
        if content.is_none() && review_comments.is_empty() {
            return div().into_any_element();
        }
        // The bottom of the block has the thread toggles and the review comments.
        let hovered_link = self.hovered_link.render_below(cx);
        v_flex()
            .id("review-overview")
            .debug_selector(|| "review-overview".into())
            .relative()
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
                        .id("review-comments-list")
                        .w_full()
                        .gap_1()
                        .child(
                            Label::new("Review comments")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .children(review_comments),
                )
            })
            .children(hovered_link)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unindent::Unindent as _;

    /// Each list item's text, as rendered.
    fn list_items(source: &str) -> Vec<String> {
        let mut items = Vec::new();
        let mut depth = 0;
        for event in Parser::new_ext(source, PARSE_OPTIONS) {
            match event {
                Event::Start(Tag::Item) => {
                    depth += 1;
                    items.push(String::new());
                }
                Event::End(TagEnd::Item) => depth -= 1,
                Event::Text(text) | Event::Code(text) if depth > 0 => {
                    if let Some(item) = items.last_mut() {
                        item.push_str(&text);
                    }
                }
                Event::SoftBreak if depth > 0 => {
                    if let Some(item) = items.last_mut() {
                        item.push(' ');
                    }
                }
                _ => {}
            }
        }
        items
    }

    #[test]
    fn test_commit_body_joins_wrapped_list_markers() {
        let message = "
            Subject (#1)

            ## Links

            -
            https://example.com/pull/1#issuecomment-1
            - Passing run:
            https://example.com/runs/2

            1.
            https://example.com/a/very/long/url
            2. Second
        "
        .unindent();
        let body = commit_body(&message).expect("the message has a body");
        assert_eq!(
            body,
            "
            ## Links

            - https://example.com/pull/1#issuecomment-1
            - Passing run:
            https://example.com/runs/2

            1. https://example.com/a/very/long/url
            2. Second"
                .unindent()
        );
        assert_eq!(
            list_items(&body),
            [
                "https://example.com/pull/1#issuecomment-1",
                "Passing run: https://example.com/runs/2",
                "https://example.com/a/very/long/url",
                "Second",
            ]
        );
    }

    #[test]
    fn test_commit_body_keeps_other_bare_list_markers() {
        for body in [
            // A list item, a heading, a quoted line or a blank line after the marker: not its
            // wrapped text.
            "-\n- item",
            // A setext heading's underline.
            "Heading\n-\ntext",
            "-\n# Heading",
            "> -\n> quoted",
            "-\n\nparagraph",
            "```\n-\ncode\n```",
            "- item\n  more of the item",
        ] {
            let message = format!("Subject\n\n{body}");
            assert_eq!(commit_body(&message).as_deref(), Some(body), "{body:?}");
        }

        let nested = "- outer\n  - first\n  -\n  https://example.com/nested";
        let message = format!("Subject\n\n{nested}");
        let body = commit_body(&message).expect("the message has a body");
        assert_eq!(body, "- outer\n  - first\n  - https://example.com/nested");
        assert_eq!(
            list_items(&body),
            ["outer", "first", "https://example.com/nested"]
        );
    }

    #[test]
    fn test_pull_request_description_list_of_links() {
        let description = SharedString::from(
            "## Links\n\n\
             - https://example.com/pull/1#issuecomment-1\n\
             - Passing run: https://example.com/runs/2\n",
        );
        assert_eq!(
            list_items(&github_markdown(&description)),
            [
                "https://example.com/pull/1#issuecomment-1",
                "Passing run: https://example.com/runs/2",
            ]
        );
    }
}
