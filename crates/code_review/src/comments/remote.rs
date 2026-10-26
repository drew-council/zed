use crate::forge::{DiffSide, ForgeRepo, RemoteThread, ThreadAnchor};
use editor::hover_markdown_style;
use gpui::{Entity, deferred};
use language::LanguageRegistry;
use markdown::{Markdown, MarkdownElement, MarkdownStyle};
use std::{ops::Range, sync::Arc};
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

/// How review comments and pull request text render: as on GitHub, where a line break in the
/// text is a line break (CommonMark would join the lines into one paragraph line).
pub(crate) fn comment_markdown_style(window: &Window, cx: &App) -> MarkdownStyle {
    MarkdownStyle {
        soft_break_as_hard_break: true,
        ..hover_markdown_style(window, cx)
    }
}

/// GitHub-authored Markdown as the review renders it: GitHub's collapsible sections
/// (`<details>`, `<summary>`), which bots use a lot, would show as raw HTML (HTML isn't parsed,
/// as it could load remote images), so their tags go and a summary becomes a bold line. Only
/// lines that start with one of these tags change, and nothing inside fenced code blocks.
/// References to issues and pull requests link to them, as on GitHub (see [`link_references`]);
/// `repo` is the pull request's repository, which `#123` refers to.
pub(crate) fn github_markdown(source: &SharedString, repo: &ForgeRepo) -> SharedString {
    link_references(&without_collapsible_tags(source), repo)
}

fn without_collapsible_tags(source: &SharedString) -> SharedString {
    let mut fence: Option<(char, usize)> = None;
    let mut changed = false;
    let mut lines = Vec::new();
    for line in source.split('\n') {
        let trimmed = line.trim_start();
        let fence_marker = ['`', '~'].into_iter().find_map(|marker| {
            let length = trimmed.chars().take_while(|c| *c == marker).count();
            (length >= 3).then_some((marker, length))
        });
        match (fence, fence_marker) {
            (None, Some(marker)) => fence = Some(marker),
            (Some((open, open_length)), Some((marker, length)))
                if marker == open
                    && length >= open_length
                    && trimmed[length..].trim().is_empty() =>
            {
                fence = None
            }
            _ => {}
        }
        if fence.is_none()
            && fence_marker.is_none()
            && let Some(replaced) = strip_collapsible_tags(trimmed)
        {
            changed = true;
            lines.push(replaced);
        } else {
            lines.push(line.to_string());
        }
    }
    if changed {
        lines.join("\n").into()
    } else {
        source.clone()
    }
}

/// A line starting with `<details>`, `</details>`, `<summary>` or `</summary>` without these
/// tags, a summary in bold; `None` for any other line.
fn strip_collapsible_tags(line: &str) -> Option<String> {
    fn tag_at<'a>(text: &'a str, name: &str) -> Option<&'a str> {
        let rest = text.get(..name.len() + 1)?;
        if !rest.eq_ignore_ascii_case(&format!("<{name}")) {
            return None;
        }
        let after = &text[name.len() + 1..];
        if !after.starts_with(['>', ' ', '\t']) {
            return None;
        }
        let end = after.find('>')?;
        Some(&after[end + 1..])
    }
    let mut rest = line;
    let mut output = String::new();
    let mut found = false;
    loop {
        rest = rest.trim_start();
        if let Some(after) = tag_at(rest, "details").or_else(|| tag_at(rest, "/details")) {
            rest = after;
        } else if let Some(after) = tag_at(rest, "summary") {
            let (summary, after) = match after.to_ascii_lowercase().find("</summary>") {
                Some(end) => (&after[..end], &after[end + "</summary>".len()..]),
                None => (after, ""),
            };
            let summary = summary.trim();
            if !summary.is_empty() {
                output.push_str(&format!("**{summary}**"));
            }
            rest = after;
        } else if let Some(after) = tag_at(rest, "/summary") {
            rest = after;
        } else {
            break;
        }
        found = true;
    }
    if !found {
        return None;
    }
    let rest = rest.trim_end();
    let rest = rest
        .len()
        .checked_sub("</details>".len())
        .filter(|start| {
            rest.get(*start..)
                .is_some_and(|end| end.eq_ignore_ascii_case("</details>"))
        })
        .map_or(rest, |start| rest[..start].trim_end());
    if !output.is_empty() && !rest.is_empty() {
        output.push(' ');
    }
    output.push_str(rest);
    Some(output)
}

/// `source` with GitHub's references to issues and pull requests as links to them, as GitHub
/// autolinks them: `#123` (in `repo`), `owner/repo#123` and `GH-123`, at word boundaries, and not
/// in code, links or URLs. GitHub only links a reference to an issue that exists, which isn't
/// known here, so `step #1` links too. An issue's URL also reaches a pull request with that
/// number: GitHub redirects it.
fn link_references(source: &SharedString, repo: &ForgeRepo) -> SharedString {
    use pulldown_cmark::{Event, Parser, Tag, TagEnd};

    // Text as the markdown crate parses it, in runs of adjacent text events (one only ends at
    // markup, a line break or an escape), so that a reference is never split by markup it would
    // break, and a URL is seen whole.
    let mut runs: Vec<Range<usize>> = Vec::new();
    let mut in_run = false;
    let mut skipped_depth = 0_usize;
    for (event, range) in
        Parser::new_ext(source, markdown::parser::PARSE_OPTIONS).into_offset_iter()
    {
        match event {
            Event::Start(
                Tag::Link { .. }
                | Tag::Image { .. }
                | Tag::CodeBlock(_)
                | Tag::HtmlBlock
                | Tag::MetadataBlock(_),
            ) => skipped_depth += 1,
            Event::End(
                TagEnd::Link
                | TagEnd::Image
                | TagEnd::CodeBlock
                | TagEnd::HtmlBlock
                | TagEnd::MetadataBlock(_),
            ) => skipped_depth = skipped_depth.saturating_sub(1),
            // Text that differs from its source (an escape, an entity, a smart quote) isn't
            // where it seems to be in the source.
            Event::Text(text)
                if skipped_depth == 0 && source.get(range.clone()) == Some(text.as_ref()) =>
            {
                match runs.last_mut() {
                    Some(run) if in_run && run.end == range.start => run.end = range.end,
                    _ => runs.push(range),
                }
                in_run = true;
                continue;
            }
            _ => {}
        }
        in_run = false;
    }

    let mut references = Vec::new();
    for run in runs {
        let mut finder = linkify::LinkFinder::new();
        finder.kinds(&[linkify::LinkKind::Url]);
        let urls = finder
            .links(&source[run.clone()])
            .map(|link| run.start + link.start()..run.start + link.end())
            .collect::<Vec<_>>();
        references.extend(
            references_in(source, run)
                .filter(|(range, _)| !urls.iter().any(|url| overlaps(url, range))),
        );
    }
    if references.is_empty() {
        return source.clone();
    }

    references.sort_by_key(|(range, _)| range.start);
    let mut output = String::with_capacity(source.len() + references.len() * 64);
    let mut copied = 0;
    for (range, reference) in references {
        if range.start < copied {
            continue;
        }
        let (owner, name) = reference
            .repo
            .unwrap_or((repo.owner.as_ref(), repo.name.as_ref()));
        output.push_str(&source[copied..range.start]);
        output.push_str(&format!(
            "[{}](https://{}/{owner}/{name}/issues/{})",
            &source[range.clone()],
            repo.host,
            reference.number
        ));
        copied = range.end;
    }
    output.push_str(&source[copied..]);
    output.into()
}

/// An issue or pull request a text refers to.
struct IssueReference<'a> {
    /// Another repository's owner and name, as written.
    repo: Option<(&'a str, &'a str)>,
    number: u64,
}

/// The references in `run`, a range of plain text in `source`, by their range in `source`.
fn references_in(
    source: &str,
    run: Range<usize>,
) -> impl Iterator<Item = (Range<usize>, IssueReference<'_>)> {
    let text = &source[run.clone()];
    let numbered = text.match_indices('#').filter_map(move |(index, _)| {
        let hash = run.start + index;
        let end = number_end(source, hash + 1, run.end)?;
        let number = source[hash + 1..end].parse().ok()?;
        let before = &source[run.start..hash];
        if before.ends_with(is_repo_name_char) {
            let name_start = before.trim_end_matches(is_repo_name_char).len();
            let owner_part = before[..name_start].strip_suffix('/')?;
            let owner_start = owner_part
                .trim_end_matches(|c: char| c.is_ascii_alphanumeric() || c == '-')
                .len();
            let owner = &owner_part[owner_start..];
            let name = &before[name_start..];
            if owner.is_empty()
                || owner.starts_with('-')
                || !starts_reference(source, run.start + owner_start)
            {
                return None;
            }
            Some((
                run.start + owner_start..end,
                IssueReference {
                    repo: Some((owner, name)),
                    number,
                },
            ))
        } else {
            starts_reference(source, hash)
                .then_some((hash..end, IssueReference { repo: None, number }))
        }
    });
    let prefixed = text
        .match_indices("GH-")
        .filter_map(move |(index, prefix)| {
            let start = run.start + index;
            let number_start = start + prefix.len();
            let end = number_end(source, number_start, run.end)?;
            let number = source[number_start..end].parse().ok()?;
            starts_reference(source, start)
                .then_some((start..end, IssueReference { repo: None, number }))
        });
    numbered.chain(prefixed)
}

fn is_repo_name_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
}

/// Where the issue number starting at `start` ends (before `limit`), if a number (not starting
/// with 0) starts there and a word doesn't go on after it.
fn number_end(source: &str, start: usize, limit: usize) -> Option<usize> {
    let digits = source[start..limit]
        .bytes()
        .take_while(u8::is_ascii_digit)
        .count();
    let end = start + digits;
    if digits == 0 || source[start..].starts_with('0') {
        return None;
    }
    // Emphasis delimiters, which may close emphasis around the reference, don't continue it.
    let after = source[end..].trim_start_matches(['*', '_', '~']);
    if after.starts_with(|c: char| c.is_alphanumeric()) {
        return None;
    }
    Some(end)
}

/// Whether a reference may start at `start`: at the start of a line or after a space or an
/// opening bracket or quote, possibly with emphasis delimiters in between (`**#123**`), but not
/// inside a word (`abc#123`, `a_#1`) or a path (`a/b/c#1`).
fn starts_reference(source: &str, start: usize) -> bool {
    source[..start]
        .trim_end_matches(['*', '_', '~'])
        .chars()
        .next_back()
        .is_none_or(|c| c.is_whitespace() || matches!(c, '(' | '[' | '{' | '"' | '\''))
}

fn overlaps(left: &Range<usize>, right: &Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
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
    /// The pull request's repository, which `#123` in the comments refers to.
    pub repo: ForgeRepo,
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

    /// Keeps the rendered markdown of threads that didn't change. Returns whether any thread
    /// changed, came or went.
    pub(crate) fn set_threads(
        &mut self,
        title: Option<SharedString>,
        threads: Vec<RemoteThreadContent>,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut previous = std::mem::take(&mut self.threads);
        let mut changed = false;
        self.threads = threads
            .into_iter()
            .map(|content| {
                if let Some(index) = previous.iter().position(|entry| entry.content == content) {
                    return previous.swap_remove(index);
                }
                changed = true;
                let bodies = content
                    .thread
                    .comments
                    .iter()
                    .map(|comment| {
                        let language_registry = self.language_registry.clone();
                        let source = github_markdown(&comment.body, &content.repo);
                        cx.new(|cx| Markdown::new(source, Some(language_registry), None, cx))
                    })
                    .collect();
                ThreadEntry { content, bodies }
            })
            .collect();
        self.title = title;
        cx.notify();
        changed || !previous.is_empty()
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

    #[cfg(test)]
    pub(crate) fn body_sources(&self, cx: &App) -> Vec<SharedString> {
        self.threads
            .iter()
            .flat_map(|entry| &entry.bodies)
            .map(|body| body.read(cx).source().clone())
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
        let markdown_style = comment_markdown_style(window, cx);
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

    fn repo() -> ForgeRepo {
        ForgeRepo {
            host: "github.com".into(),
            owner: "o".into(),
            name: "r".into(),
        }
    }

    #[test]
    fn test_github_markdown_links_references() {
        let markdown = |source: &str| github_markdown(&source.into(), &repo()).to_string();
        let issue = |number: u32| format!("https://github.com/o/r/issues/{number}");
        assert_eq!(
            markdown("Fixes #12."),
            format!("Fixes [#12]({}).", issue(12))
        );
        assert_eq!(
            markdown("#1 first\nthen #2\n> quoted #3\n- item #4\n# Heading #5"),
            format!(
                "[#1]({}) first\nthen [#2]({})\n> quoted [#3]({})\n- item [#4]({})\n# Heading [#5]({})",
                issue(1),
                issue(2),
                issue(3),
                issue(4),
                issue(5)
            )
        );
        assert_eq!(
            markdown("(#1), [#2], **#3**, _#4_, ~~#5~~, \"#6\", '#7'"),
            format!(
                "([#1]({})), [[#2]({})], **[#3]({})**, _[#4]({})_, ~~[#5]({})~~, \"[#6]({})\", '[#7]({})'",
                issue(1),
                issue(2),
                issue(3),
                issue(4),
                issue(5),
                issue(6),
                issue(7)
            )
        );
        assert_eq!(
            markdown("See zed-industries/zed.dev#7 and GH-8"),
            format!(
                "See [zed-industries/zed.dev#7](https://github.com/zed-industries/zed.dev/issues/7) \
                 and [GH-8]({})",
                issue(8)
            )
        );
        assert_eq!(
            markdown("| a | #1 |\n|---|---|\n| o2/r_2#2 | b |"),
            format!(
                "| a | [#1]({}) |\n|---|---|\n| [o2/r_2#2](https://github.com/o2/r_2/issues/2) | b |",
                issue(1)
            )
        );
        // A URL and a reference in one text: only the reference links.
        assert_eq!(
            markdown("https://example.com/page#1 and #2"),
            format!("https://example.com/page#1 and [#2]({})", issue(2))
        );
        // Collapsible sections go first, then their summary's references link.
        assert_eq!(
            markdown("<details><summary>See #1</summary>"),
            format!("**See [#1]({})**", issue(1))
        );
        // The pull request's host.
        let enterprise = ForgeRepo {
            host: "ghe.corp".into(),
            ..repo()
        };
        assert_eq!(
            github_markdown(&"#3 and a/b#4".into(), &enterprise).as_ref(),
            "[#3](https://ghe.corp/o/r/issues/3) and [a/b#4](https://ghe.corp/a/b/issues/4)"
        );

        for source in [
            // Inside words, paths and URLs.
            "abc#12",
            "issue#12 x-#1 foo_#2 a.b#3",
            "a/b/c#1",
            "path/o/r#1",
            "-o/r#1",
            "#12abc #3_x GH-4a XGH-5 gh-6",
            "https://github.com/o/r/pull/1#12",
            "see https://example.com/#3.",
            "<https://example.com/#3>",
            // Not numbers.
            "#0 #01 # #x GH- #99999999999999999999999",
            // Code, links and images.
            "`#12` and `` o/r#1 ``",
            "```\n#12\n```",
            "~~~md\nGH-1\n~~~",
            "    #12 indented code",
            "[see #12](https://example.com)",
            "[#12][ref]\n\n[ref]: https://example.com",
            "![#12](image.png)",
            // Escapes, entities and HTML.
            "\\#12",
            "&#35;12",
            "<span title=\"#12\">x</span>",
            "<div>\n#12\n</div>",
            "",
        ] {
            assert_eq!(markdown(source), source, "{source}");
        }
    }

    #[test]
    fn test_github_markdown_drops_collapsible_tags() {
        let markdown = |source: &str| github_markdown(&source.into(), &repo()).to_string();
        assert_eq!(
            markdown(
                "Summary\n\n<details>\n<summary>Notes</summary>\n\nHidden text\n</details>\nAfter"
            ),
            "Summary\n\n\n**Notes**\n\nHidden text\n\nAfter"
        );
        assert_eq!(
            markdown("<details open><summary> Two words </summary>\nText</details>"),
            "**Two words**\nText</details>"
        );
        assert_eq!(markdown("<DETAILS><Summary>Caps</SUMMARY>"), "**Caps**");
        assert_eq!(markdown("<details><summary>A</summary>tail"), "**A** tail");
        assert_eq!(
            markdown("  <summary>\n  Multi\n  </summary>"),
            "\n  Multi\n"
        );
        // Not at the start of a line, other tags, and fenced code stay as they are.
        for source in [
            "Use `<details>` for this",
            "<detailsx>",
            "<div>x</div>",
            "```html\n<details>\n<summary>Code</summary>\n```\n",
            "~~~~\n```\n<details>\n~~~~",
        ] {
            assert_eq!(markdown(source), source);
        }
        assert_eq!(markdown("```\ncode\n```\n<details>"), "```\ncode\n```\n");
    }
}
