use super::{
    Comment, CommentBlocks, CommentId, CommentTarget, CommentType, Side,
    remote::comment_markdown_style,
};
use crate::{CycleCommentType, CycleCommentTypeBackward, DiscardCommentChanges, SaveComment};
use editor::Editor;
use gpui::{Entity, Subscription, WeakEntity};
use language::LanguageRegistry;
use markdown::{Markdown, MarkdownElement};
use std::sync::Arc;
use ui::{Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{CloseActiveItem, CloseItemInAllPanes, SaveIntent};

/// How a comment editor is left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Leave {
    /// Save the text.
    Keep,
    /// Forget the edits (`:q!`); a new comment is deleted.
    Discard,
}

pub(crate) struct EditingState {
    pub(crate) editor: Entity<Editor>,
    /// The diff editor that had focus when editing began, which gets it back.
    pub(crate) return_to: WeakEntity<Editor>,
    /// Never saved: leaving it empty or discarding it deletes it.
    pub(crate) is_new: bool,
    /// Vim can only switch the editor to insert mode once it has been painted, which its first
    /// focus event comes after.
    pub(crate) enter_insert_mode: bool,
    pub(crate) _subscription: Subscription,
}

/// One comment, in a block below its lines or in the review's top block: its saved text as
/// markdown, or an editor while it is edited.
pub(crate) struct CommentView {
    id: CommentId,
    blocks: WeakEntity<CommentBlocks>,
    comment_type: Option<CommentType>,
    location: Option<SharedString>,
    is_outdated: bool,
    is_submitted: bool,
    markdown: Entity<Markdown>,
    editing: Option<EditingState>,
    /// The comment below the cursor line, which `i`, `Enter` and `dd` act on.
    targeted: bool,
}

impl CommentView {
    pub(crate) fn new(
        comment: &Comment,
        blocks: WeakEntity<CommentBlocks>,
        language_registry: Arc<LanguageRegistry>,
        cx: &mut Context<Self>,
    ) -> Self {
        let body = SharedString::from(comment.body.clone());
        let markdown = cx.new(|cx| Markdown::new(body, Some(language_registry), None, cx));
        Self {
            id: comment.id,
            blocks,
            comment_type: comment.comment_type,
            location: location_label(&comment.target),
            is_outdated: comment.is_outdated(),
            is_submitted: comment.is_submitted(),
            markdown,
            editing: None,
            targeted: false,
        }
    }

    pub(crate) fn update_comment(&mut self, comment: &Comment, cx: &mut Context<Self>) {
        self.comment_type = comment.comment_type;
        self.location = location_label(&comment.target);
        self.is_outdated = comment.is_outdated();
        self.is_submitted = comment.is_submitted();
        if self.markdown.read(cx).source().as_ref() != comment.body {
            let body = SharedString::from(comment.body.clone());
            self.markdown
                .update(cx, |markdown, cx| markdown.replace(body, cx));
        }
        cx.notify();
    }

    pub(crate) fn editing(&self) -> Option<&EditingState> {
        self.editing.as_ref()
    }

    pub(crate) fn editor(&self) -> Option<&Entity<Editor>> {
        self.editing.as_ref().map(|editing| &editing.editor)
    }

    pub(crate) fn set_return_to(&mut self, return_to: WeakEntity<Editor>) {
        if let Some(editing) = &mut self.editing {
            editing.return_to = return_to;
        }
    }

    pub(crate) fn start_editing(&mut self, editing: EditingState, cx: &mut Context<Self>) {
        self.editing = Some(editing);
        cx.notify();
    }

    pub(crate) fn take_editing(&mut self, cx: &mut Context<Self>) -> Option<EditingState> {
        cx.notify();
        self.editing.take()
    }

    /// Whether vim should switch to insert mode now; asks only once.
    pub(crate) fn take_enter_insert_mode(&mut self) -> bool {
        self.editing
            .as_mut()
            .is_some_and(|editing| std::mem::take(&mut editing.enter_insert_mode))
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
    pub(crate) fn markdown_source(&self, cx: &App) -> SharedString {
        self.markdown.read(cx).source().to_string().into()
    }

    #[cfg(test)]
    pub(crate) fn is_outdated(&self) -> bool {
        self.is_outdated
    }

    #[cfg(test)]
    pub(crate) fn location(&self) -> Option<SharedString> {
        self.location.clone()
    }

    /// The type and state badges and what the comment is on, when there is anything to say.
    fn render_header(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if self.comment_type.is_none()
            && self.location.is_none()
            && !self.is_outdated
            && !self.is_submitted
        {
            return None;
        }
        Some(
            h_flex()
                .gap_2()
                .when_some(self.comment_type, |header, comment_type| {
                    header.child(type_badge(comment_type, cx))
                })
                .when(self.is_outdated, |header| {
                    header.child(
                        div()
                            .id(("review-comment-outdated", self.id.0))
                            .child(state_badge("OUTDATED", Color::Warning, cx))
                            .tooltip(Tooltip::text(
                                "The commented lines changed while the review was closed",
                            )),
                    )
                })
                .when(self.is_submitted, |header| {
                    header.child(state_badge("SUBMITTED", Color::Muted, cx))
                })
                .when_some(self.location.clone(), |header, location| {
                    header.child(
                        Label::new(location)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                }),
        )
    }

    fn render_buttons(&self) -> Option<impl IntoElement> {
        if self.editing.is_some() || self.is_submitted {
            return None;
        }
        let id = self.id;
        let blocks = self.blocks.clone();
        Some(
            h_flex()
                .flex_none()
                .gap_0p5()
                .child(
                    IconButton::new(("review-comment-edit", id.0), IconName::Pencil)
                        .icon_size(IconSize::XSmall)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text("Edit Comment"))
                        .on_click({
                            let blocks = blocks.clone();
                            move |_, window, cx| {
                                blocks
                                    .update(cx, |blocks, cx| blocks.edit(id, window, cx))
                                    .log_err();
                            }
                        }),
                )
                .child(
                    IconButton::new(("review-comment-delete", id.0), IconName::Trash)
                        .icon_size(IconSize::XSmall)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text("Delete Comment"))
                        .on_click(move |_, window, cx| {
                            blocks
                                .update(cx, |blocks, cx| blocks.confirm_delete(id, window, cx))
                                .log_err();
                        }),
                ),
        )
    }
}

fn leave(
    blocks: &WeakEntity<CommentBlocks>,
    id: CommentId,
    leave: Leave,
    window: &mut Window,
    cx: &mut App,
) {
    blocks
        .update(cx, |blocks, cx| blocks.leave(id, leave, window, cx))
        .log_err();
}

fn cycle_type(blocks: &WeakEntity<CommentBlocks>, id: CommentId, forward: bool, cx: &mut App) {
    blocks
        .update(cx, |blocks, cx| blocks.cycle_type(id, forward, cx))
        .log_err();
}

/// A comment's block sits below its last line, so a single line needs no label. The line numbers
/// follow edits while the review is open.
fn location_label(target: &CommentTarget) -> Option<SharedString> {
    match target {
        CommentTarget::Review => None,
        CommentTarget::File { .. } => Some("File comment".into()),
        CommentTarget::Lines { lines, side, .. } if lines.start() != lines.end() => {
            let label = match side {
                Side::New => "Lines",
                Side::Old => "Old lines",
            };
            Some(format!("{label} {}–{}", lines.start(), lines.end()).into())
        }
        CommentTarget::Lines {
            side: Side::Old,
            lines,
            ..
        } => Some(format!("Old line {}", lines.start()).into()),
        CommentTarget::Lines { .. } => None,
    }
}

fn state_badge(label: &'static str, color: Color, cx: &App) -> impl IntoElement {
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

fn type_badge(comment_type: CommentType, cx: &App) -> impl IntoElement {
    let color = comment_type.color().color(cx);
    div()
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(color.opacity(0.6))
        .bg(color.opacity(0.12))
        .child(
            Label::new(comment_type.label())
                .size(LabelSize::XSmall)
                .color(comment_type.color())
                .buffer_font(cx),
        )
}

impl Render for CommentView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let id = self.id;
        let colors = cx.theme().colors();
        let accent = self
            .comment_type
            .map_or(colors.border, |comment_type| comment_type.color().color(cx));
        let border = if self.editing.is_some() || self.targeted {
            colors.border_focused
        } else {
            colors.border_variant
        };
        let body = match &self.editing {
            Some(editing) => div()
                .w_full()
                .min_h(window.line_height())
                .child(editing.editor.clone())
                .into_any_element(),
            None => div()
                .w_full()
                .child(MarkdownElement::new(
                    self.markdown.clone(),
                    comment_markdown_style(window, cx),
                ))
                .into_any_element(),
        };
        let blocks = self.blocks.clone();

        h_flex()
            .key_context("ReviewComment")
            .when(self.editing.is_some(), |card| {
                card.on_action({
                    let blocks = blocks.clone();
                    move |_: &SaveComment, window, cx| leave(&blocks, id, Leave::Keep, window, cx)
                })
                .on_action({
                    let blocks = blocks.clone();
                    move |_: &DiscardCommentChanges, window, cx| {
                        leave(&blocks, id, Leave::Discard, window, cx)
                    }
                })
                // Vim's `Escape` in normal mode; the editor passes it on when it has nothing to
                // dismiss.
                .on_action({
                    let blocks = blocks.clone();
                    move |_: &editor::actions::Cancel, window, cx| {
                        leave(&blocks, id, Leave::Keep, window, cx)
                    }
                })
                .on_action({
                    let blocks = blocks.clone();
                    move |_: &workspace::Save, window, cx| {
                        leave(&blocks, id, Leave::Keep, window, cx)
                    }
                })
                // Vim's `:q`, `:wq`, `:x`, `ZZ` keep the text, `:q!` and `ZQ` discard it. The
                // review tab stays open either way.
                .on_action({
                    let blocks = blocks.clone();
                    move |action: &CloseActiveItem, window, cx| {
                        let mode = if action.save_intent == Some(SaveIntent::Skip) {
                            Leave::Discard
                        } else {
                            Leave::Keep
                        };
                        leave(&blocks, id, mode, window, cx)
                    }
                })
                // Vim's `:bd` keeps the text and `:bd!` discards it, like `:q` and `:q!`.
                .on_action({
                    let blocks = blocks.clone();
                    move |action: &CloseItemInAllPanes, window, cx| {
                        let mode = if action.save_intent == Some(SaveIntent::Skip) {
                            Leave::Discard
                        } else {
                            Leave::Keep
                        };
                        leave(&blocks, id, mode, window, cx)
                    }
                })
                .on_action({
                    let blocks = blocks.clone();
                    move |_: &CycleCommentType, _, cx| cycle_type(&blocks, id, true, cx)
                })
                .on_action({
                    let blocks = blocks.clone();
                    move |_: &CycleCommentTypeBackward, _, cx| cycle_type(&blocks, id, false, cx)
                })
            })
            .w_full()
            .items_stretch()
            .overflow_hidden()
            .rounded_sm()
            .border_1()
            .border_color(border)
            .bg(colors.editor_background)
            .child(div().w(px(3.)).flex_none().bg(accent))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .px_2()
                    .py_1()
                    .gap_0p5()
                    .text_sm()
                    .children(self.render_header(cx))
                    .child(body),
            )
            .children(
                self.render_buttons()
                    .map(|buttons| div().flex_none().pt_0p5().pr_1().child(buttons)),
            )
    }
}
