use crate::forge::{DiffSide, RemoteThread, ThreadAnchor};
use editor::hover_markdown_style;
use gpui::Entity;
use language::LanguageRegistry;
use markdown::{Markdown, MarkdownElement};
use std::sync::Arc;
use time::{OffsetDateTime, UtcOffset};
use ui::{Tooltip, prelude::*};

/// A review thread of the pull request on GitHub, in a block of the diff (read-only), or a file's
/// outdated and moved threads together in one block at the file.
pub(crate) struct RemoteThreadView {
    /// The heading of a file's outdated and moved threads.
    title: Option<SharedString>,
    threads: Vec<ThreadEntry>,
    /// The thread at the cursor, which `]t` moved to.
    targeted: bool,
    language_registry: Arc<LanguageRegistry>,
}

/// What a [`RemoteThreadView`] shows of a thread.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RemoteThreadContent {
    pub thread: Arc<RemoteThread>,
    /// Where the thread is, when its block's place doesn't say it: a range, an old line, a file,
    /// or the original lines of an outdated or moved thread.
    pub location: Option<SharedString>,
}

struct ThreadEntry {
    content: RemoteThreadContent,
    bodies: Vec<Entity<Markdown>>,
}

impl RemoteThreadView {
    pub(crate) fn new(
        title: Option<SharedString>,
        threads: Vec<RemoteThreadContent>,
        language_registry: Arc<LanguageRegistry>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            title: None,
            threads: Vec::new(),
            targeted: false,
            language_registry,
        };
        this.set_threads(title, threads, cx);
        this
    }

    /// Keeps the rendered markdown of threads that didn't change.
    pub(crate) fn set_threads(
        &mut self,
        title: Option<SharedString>,
        threads: Vec<RemoteThreadContent>,
        cx: &mut Context<Self>,
    ) {
        let mut previous = std::mem::take(&mut self.threads);
        self.threads = threads
            .into_iter()
            .map(|content| {
                if let Some(index) = previous.iter().position(|entry| entry.content == content) {
                    return previous.swap_remove(index);
                }
                let bodies = content
                    .thread
                    .comments
                    .iter()
                    .map(|comment| {
                        let language_registry = self.language_registry.clone();
                        cx.new(|cx| {
                            Markdown::new(comment.body.clone(), Some(language_registry), None, cx)
                        })
                    })
                    .collect();
                ThreadEntry { content, bodies }
            })
            .collect();
        self.title = title;
        cx.notify();
    }

    pub(crate) fn threads(&self) -> impl Iterator<Item = &Arc<RemoteThread>> {
        self.threads.iter().map(|entry| &entry.content.thread)
    }

    pub(crate) fn set_targeted(&mut self, targeted: bool, cx: &mut Context<Self>) {
        if self.targeted != targeted {
            self.targeted = targeted;
            cx.notify();
        }
    }

    #[cfg(test)]
    pub(crate) fn is_targeted(&self) -> bool {
        self.targeted
    }

    #[cfg(test)]
    pub(crate) fn locations(&self) -> Vec<Option<SharedString>> {
        self.threads
            .iter()
            .map(|entry| entry.content.location.clone())
            .collect()
    }

    fn render_thread(
        &self,
        index: usize,
        entry: &ThreadEntry,
        show_source: bool,
        window: &mut Window,
        cx: &App,
    ) -> impl IntoElement {
        let thread = &entry.content.thread;
        let url = thread
            .comments
            .first()
            .map(|comment| comment.url.clone())
            .filter(|url| !url.is_empty());
        let timezone = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
        let now = OffsetDateTime::now_utc();
        let markdown_style = hover_markdown_style(window, cx);
        let more = (thread.total_comments as usize).saturating_sub(thread.comments.len());
        let header = h_flex()
            .gap_2()
            .when(show_source, |header| {
                header.child(
                    h_flex()
                        .gap_1()
                        .child(
                            Icon::new(IconName::Github)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new("GitHub")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
            })
            .when(thread.is_resolved, |header| {
                header.child(badge("RESOLVED", Color::Success, cx))
            })
            .when(thread.is_outdated, |header| {
                header.child(badge("OUTDATED", Color::Warning, cx))
            })
            .when_some(entry.content.location.clone(), |header, location| {
                header.child(
                    Label::new(location)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .child(div().flex_1())
            .when_some(url, |header, url| {
                header.child(
                    IconButton::new(("review-thread-open", index), IconName::ArrowUpRight)
                        .icon_size(IconSize::XSmall)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text("Open on GitHub"))
                        .on_click(move |_, _, cx| cx.open_url(&url)),
                )
            });
        v_flex()
            .w_full()
            .gap_1()
            .child(header)
            .children(thread.comments.iter().zip(&entry.bodies).enumerate().map(
                |(comment_index, (comment, body))| {
                    let time = time_format::format_localized_timestamp(
                        comment.created_at,
                        now,
                        timezone,
                        time_format::TimestampFormat::Relative,
                    );
                    v_flex()
                        .w_full()
                        .when(comment_index > 0, |reply| {
                            reply
                                .pt_1()
                                .border_t_1()
                                .border_color(cx.theme().colors().border_variant)
                        })
                        .child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Label::new(comment.author.login.clone())
                                        .size(LabelSize::Small)
                                        .weight(gpui::FontWeight::SEMIBOLD),
                                )
                                .when(comment.author.is_bot, |line| {
                                    line.child(badge("BOT", Color::Muted, cx))
                                })
                                .child(
                                    Label::new(format!("· {time}"))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                ),
                        )
                        .child(
                            div()
                                .w_full()
                                .child(MarkdownElement::new(body.clone(), markdown_style.clone())),
                        )
                },
            ))
            .when(more > 0, |thread| {
                thread.child(
                    Label::new(if more == 1 {
                        "…1 more comment on GitHub".to_string()
                    } else {
                        format!("…{more} more comments on GitHub")
                    })
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
                )
            })
    }
}

/// Where a thread is, for its block: nothing for one new line (the block is right below it),
/// else the lines, old or new, and for an outdated or moved thread its original place.
pub(crate) fn thread_location(thread: &RemoteThread, is_moved: bool) -> Option<SharedString> {
    let lines = |side: DiffSide, start: u32, end: u32| {
        let (one, many) = match side {
            DiffSide::Left => ("Old line", "Old lines"),
            DiffSide::Right => ("Line", "Lines"),
        };
        if start == end {
            format!("{one} {end}")
        } else {
            format!("{many} {start}–{end}")
        }
    };
    let at_commit = thread
        .anchor_commit
        .map(|commit| format!(" at {}", commit.display_short()))
        .unwrap_or_default();
    let location = match &thread.anchor {
        ThreadAnchor::File => "File comment".to_string(),
        ThreadAnchor::Outdated {
            original_line,
            original_start,
        } => match (original_start, original_line) {
            (Some(start), Some(line)) if start < line => {
                format!("Originally lines {start}–{line}{at_commit}")
            }
            (_, Some(line)) => format!("Originally line {line}{at_commit}"),
            (_, None) => "Outdated".to_string(),
        },
        ThreadAnchor::Line { side, line } => {
            let location = lines(*side, *line, *line);
            if is_moved {
                format!("{location}{at_commit}")
            } else if *side == DiffSide::Right {
                return None;
            } else {
                location
            }
        }
        ThreadAnchor::Range {
            start_side,
            start,
            side,
            line,
        } => {
            let location = if start_side == side {
                lines(*side, *start, *line)
            } else {
                format!(
                    "{} – {}",
                    lines(*start_side, *start, *start),
                    lines(*side, *line, *line).to_lowercase()
                )
            };
            if is_moved {
                format!("{location}{at_commit}")
            } else {
                location
            }
        }
    };
    Some(location.into())
}

fn badge(label: &'static str, color: Color, cx: &App) -> impl IntoElement {
    div()
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(color.color(cx).opacity(0.6))
        .child(
            Label::new(label)
                .size(LabelSize::XSmall)
                .color(color)
                .buffer_font(cx),
        )
}

impl Render for RemoteThreadView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = self.title.clone();
        let show_source_per_thread = title.is_none();
        let threads = self
            .threads
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                self.render_thread(index, entry, show_source_per_thread, window, cx)
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let colors = cx.theme().colors();
        let border = if self.targeted {
            colors.border_focused
        } else {
            colors.border_variant
        };
        h_flex()
            .w_full()
            .items_stretch()
            .overflow_hidden()
            .rounded_sm()
            .border_1()
            .border_color(border)
            .bg(colors.editor_background)
            .child(
                div()
                    .w(px(3.))
                    .flex_none()
                    .bg(colors.text_muted.opacity(0.5)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .text_sm()
                    .when_some(title, |card, title| {
                        card.child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Icon::new(IconName::Github)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .child(
                                    Label::new(title).size(LabelSize::Small).color(Color::Muted),
                                ),
                        )
                    })
                    .children(threads),
            )
    }
}
