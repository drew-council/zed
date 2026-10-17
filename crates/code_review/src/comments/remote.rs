use crate::forge::{DiffSide, RemoteThread, ThreadAnchor};
use editor::hover_markdown_style;
use gpui::{Entity, deferred};
use language::LanguageRegistry;
use markdown::{Markdown, MarkdownElement, MarkdownStyle};
use std::sync::Arc;
use time::{OffsetDateTime, UtcOffset};
use ui::{LinkPreview, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{Toast, Workspace, notifications::NotificationId};

/// Opens a link only when it is a web page. Pull requests, their reviews and commits can be
/// written by anyone, and a link there may name a local file or another application's URL scheme
/// behind any text; so may a URL a GitHub server sent.
pub(crate) fn open_web_link(url: &str, window: &mut Window, cx: &mut App) {
    // What was checked is what opens: the parsed URL, not the text it came from.
    if let Some(web_link) = web_link(url) {
        cx.open_url(web_link.as_str());
        return;
    }
    log::warn!("not opening {url:?}: only web links open from a review");
    if let Some(workspace) = Workspace::for_window(window, cx) {
        let message = format!(
            "Not opening {}: only http and https links open from a review",
            util::truncate_and_trailoff(url, 80)
        );
        workspace.update(cx, |workspace, cx| {
            workspace.show_toast(
                Toast::new(NotificationId::unique::<HoveredLink>(), message).autohide(),
                cx,
            )
        });
    }
}

fn web_link(url: &str) -> Option<http_client::Url> {
    http_client::Url::parse(url)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https"))
}

#[cfg(test)]
fn is_web_link(url: &str) -> bool {
    web_link(url).is_some()
}

/// The link under the mouse in one of a view's markdown texts (by the text's key), whose
/// destination the view shows.
#[derive(Default)]
pub(crate) struct HoveredLink(Option<(SharedString, SharedString)>);

impl HoveredLink {
    /// Whether anything changed.
    fn set(&mut self, key: &SharedString, url: Option<SharedString>) -> bool {
        match url {
            Some(url) => {
                let hovered = Some((key.clone(), url));
                let changed = self.0 != hovered;
                self.0 = hovered;
                changed
            }
            // Every text hears every mouse move; only the one that was hovered clears it.
            None if self.0.as_ref().is_some_and(|(hovered, _)| hovered == key) => {
                self.0 = None;
                true
            }
            None => false,
        }
    }

    /// Where the hovered link goes, in the bottom left corner of a `relative` container.
    pub(crate) fn render(&self, cx: &mut App) -> Option<impl IntoElement + use<>> {
        let (_, url) = self.0.as_ref()?;
        Some(
            div()
                .absolute()
                .bottom_0()
                .left_0()
                .max_w_full()
                .overflow_hidden()
                .child(LinkPreview::new(url, cx)),
        )
    }

    /// Where the hovered link goes, hanging below the bottom left corner of a `relative`
    /// container (over whatever follows it), so that it covers none of the container.
    pub(crate) fn render_below(&self, cx: &mut App) -> Option<impl IntoElement + use<>> {
        let (_, url) = self.0.as_ref()?;
        Some(deferred(
            div()
                .absolute()
                .top_full()
                .left_0()
                .max_w_full()
                .overflow_hidden()
                .child(LinkPreview::new(url, cx)),
        ))
    }
}

/// Markdown someone else wrote: its links show where they go while hovered (in the view's
/// [`HoveredLink`], under `key`) and only open web pages.
pub(crate) fn untrusted_markdown<V: 'static>(
    markdown: Entity<Markdown>,
    style: MarkdownStyle,
    key: impl Into<SharedString>,
    hovered_link: fn(&mut V) -> &mut HoveredLink,
    cx: &Context<V>,
) -> MarkdownElement {
    let view = cx.weak_entity();
    let key = key.into();
    MarkdownElement::new(markdown, style)
        .on_url_click(|url, window, cx| open_web_link(&url, window, cx))
        .on_url_hover(move |url, _, cx| {
            view.update(cx, |view, cx| {
                if hovered_link(view).set(&key, url) {
                    cx.notify();
                }
            })
            .log_err();
        })
}

/// A review thread of the pull request on GitHub, in a block of the diff (read-only), or a file's
/// outdated and moved threads together in one block at the file.
pub(crate) struct RemoteThreadView {
    /// The heading of a file's outdated and moved threads.
    title: Option<SharedString>,
    threads: Vec<ThreadEntry>,
    /// The thread at the cursor, which `]t` moved to.
    targeted: bool,
    language_registry: Arc<LanguageRegistry>,
    hovered_link: HoveredLink,
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
            hovered_link: HoveredLink::default(),
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
        cx: &Context<Self>,
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
                        .tooltip({
                            let url = url.clone();
                            move |_, cx| Tooltip::with_meta("Open on GitHub", None, url.clone(), cx)
                        })
                        .on_click(move |_, window, cx| open_web_link(&url, window, cx)),
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
                        .child(div().w_full().child(untrusted_markdown(
                            body.clone(),
                            markdown_style.clone(),
                            format!("{index}-{comment_index}"),
                            |view: &mut Self| &mut view.hovered_link,
                            cx,
                        )))
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
    let at = |commit: Option<git::Oid>| {
        commit
            .map(|commit| format!(" at {}", commit.display_short()))
            .unwrap_or_default()
    };
    let at_commit = at(thread.anchor_commit);
    let location = match &thread.anchor {
        ThreadAnchor::File => "File comment".to_string(),
        // The original lines count in the commit the thread was made on; pushes since moved
        // `anchor_commit` along until the lines changed.
        ThreadAnchor::Outdated {
            original_line,
            original_start,
        } => match (original_start, original_line) {
            (Some(start), Some(line)) if start < line => format!(
                "Originally lines {start}–{line}{}",
                at(thread.original_commit.or(thread.anchor_commit))
            ),
            (_, Some(line)) => format!(
                "Originally line {line}{}",
                at(thread.original_commit.or(thread.anchor_commit))
            ),
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
        let hovered_link = self.hovered_link.render(cx);
        let colors = cx.theme().colors();
        let border = if self.targeted {
            colors.border_focused
        } else {
            colors.border_variant
        };
        h_flex()
            .relative()
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
            .children(hovered_link)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_only_web_links_open() {
        assert!(is_web_link("https://github.com/o/r/pull/1"));
        assert!(is_web_link("http://ghe.corp:8080/o/r"));
        for url in [
            "file:///etc/passwd",
            "zed://review?repo=/tmp",
            "vscode://file/x",
            "javascript:alert(1)",
            "/relative/path",
            "#anchor",
            "",
        ] {
            assert!(!is_web_link(url), "{url}");
        }
    }

    #[test]
    fn test_hovered_link_per_text() {
        let mut hovered = HoveredLink::default();
        let (description, summary) = (SharedString::from("a"), SharedString::from("b"));
        assert!(hovered.set(&description, Some("https://x".into())));
        assert!(!hovered.set(&description, Some("https://x".into())));
        // Another text, not hovered, doesn't clear it; the hovered one does.
        assert!(!hovered.set(&summary, None));
        assert_eq!(hovered.0, Some((description.clone(), "https://x".into())));
        assert!(hovered.set(&description, None));
        assert_eq!(hovered.0, None);
    }
}
