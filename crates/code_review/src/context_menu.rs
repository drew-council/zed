use crate::{
    AddComment, AddFileComment, CopyCommentAtCursor, CopyPullRequestUrl, CopyReviewAsMarkdown,
    CopySelectedLines, DeleteComment, EditComment, OpenFileAtCursor, ReviewView,
    ToggleFileReviewed, ToggleHunkReviewed,
    comments::{CommentTarget, ItemKey},
    line_cursor::selected_rows,
};
use editor::{
    Bias, DisplayPoint, Editor, SelectionEffects,
    actions::{CopyPermalinkToLine, FindAllReferences, GoToDefinition},
};
use git::repository::RepoPath;
use gpui::{Action, ClipboardItem, Entity, Focusable as _, WeakEntity};
use language::Point;
use ui::{ContextMenu, ContextMenuEntry, ContextMenuItem, DocumentationSide, prelude::*};
use util::ResultExt as _;
use workspace::{Toast, notifications::NotificationId};

/// Replaces Zed's context menu (go to definition, cut, paste…) in a side of the review's diff
/// with the review's own: comments, reviewed marks, copying, and the few code navigation
/// entries a review needs.
pub(crate) fn install(editor: &Entity<Editor>, review: WeakEntity<ReviewView>, cx: &mut App) {
    editor.update(cx, |editor, _| {
        editor.set_custom_context_menu(move |editor, point, window, cx| {
            let review = review.upgrade()?;
            // Like Zed's menu: the entries act at the clicked row, unless it is in the
            // selection, which they act on then.
            move_cursor_unless_selected(editor, point, window, cx);
            let side = cx.entity();
            let focus_handle = editor.focus_handle(cx);
            let menu = ContextMenu::build(window, cx, |menu, _, _| menu.context(focus_handle));
            // The entries depend on this editor, which can't be read while it is being updated
            // (as it is here): they are added right after, before the menu is drawn.
            window.defer(cx, {
                let menu = menu.clone();
                move |_, cx| {
                    let items = menu_items(&review, &side, cx);
                    let review = review.downgrade();
                    let side = side.downgrade();
                    menu.update(cx, |menu, cx| {
                        for item in items {
                            menu.push_item(context_menu_item(item, &review, &side));
                        }
                        cx.notify();
                    });
                }
            });
            Some(menu)
        });
    });
}

fn move_cursor_unless_selected(
    editor: &mut Editor,
    point: DisplayPoint,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    let snapshot = editor.display_snapshot(cx);
    let row = snapshot.display_point_to_point(point, Bias::Left).row;
    if selected_rows(editor, &snapshot).contains(&row) {
        return;
    }
    let point = Point::new(row, 0);
    editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
        selections.select_ranges([point..point])
    });
}

/// An entry of the review's context menu, or a separator.
pub(crate) enum MenuItem {
    Separator,
    Entry(MenuEntry),
}

pub(crate) struct MenuEntry {
    pub(crate) label: SharedString,
    /// Why the entry is disabled, shown next to it.
    pub(crate) disabled: Option<SharedString>,
    /// The action whose key binding the entry shows.
    key: Option<Box<dyn Action>>,
    command: Command,
}

enum Command {
    AddComment,
    FileComment(RepoPath),
    ToggleHunk(RepoPath, SharedString),
    ToggleFile(RepoPath),
    EditComment(crate::comments::CommentId),
    DeleteComment(crate::comments::CommentId),
    CopyText(String, &'static str),
    /// An action of the clicked side's editor or of the review, dispatched from that editor.
    Dispatch(Box<dyn Action>),
}

fn entry(
    label: impl Into<SharedString>,
    key: Option<Box<dyn Action>>,
    command: Command,
) -> MenuEntry {
    MenuEntry {
        label: label.into(),
        disabled: None,
        key,
        command,
    }
}

impl MenuEntry {
    fn disabled_because(mut self, reason: Option<SharedString>) -> Self {
        self.disabled = reason;
        self
    }
}

/// The menu's entries for a right click in `side` (either side of the diff), whose cursor or
/// selection is where the click was.
pub(crate) fn menu_items(
    review: &Entity<ReviewView>,
    side: &Entity<Editor>,
    cx: &mut App,
) -> Vec<MenuItem> {
    let view = review.read(cx);
    let comments = view.comments().clone();
    let is_lhs = view.diff().read(cx).editor().read(cx).lhs_editor() == Some(side);
    let is_snapshot = view.diff().read(cx).is_snapshot(cx);
    let is_pull_request = view.pull_request().is_some();
    let session = view.session().clone();
    let cursor = side.read(cx).selections.newest_anchor().head();
    let hunk = view.hunk_key_at(side, cursor, cx);

    let (target, line_refusal, file_refusal, comment, comment_submitted, body, on_thread) =
        comments.update(cx, |comments, cx| {
            let target = comments.line_comment_target(side, cx);
            let line_refusal = target
                .as_ref()
                .and_then(|target| comments.refusal(target, cx));
            let file_refusal = target.as_ref().and_then(|target| {
                let path = target.path()?.clone();
                comments.refusal(&CommentTarget::File { path }, cx)
            });
            let comment = comments.comment_at_cursor_of(side, cx);
            let comment_submitted = comment.is_some_and(|id| comments.is_submitted(id, cx));
            let item = comments.item_at_cursor_of(side, cx);
            let body = item.as_ref().and_then(|item| comments.item_body(item, cx));
            let on_thread = matches!(item, Some(ItemKey::Remote(_)));
            (
                target,
                line_refusal,
                file_refusal,
                comment,
                comment_submitted,
                body,
                on_thread,
            )
        });
    let path = target.as_ref().and_then(|target| target.path().cloned());
    let line_count = match &target {
        Some(CommentTarget::Lines { lines, .. }) => Some(lines.clone().count()),
        Some(CommentTarget::File { .. } | CommentTarget::Review) | None => None,
    };
    let session = session.read(cx);
    let marks_loaded = session.is_loaded();
    let not_loaded = || Some(SharedString::from("The review's marks haven't loaded yet"));

    let mut items = Vec::new();
    items.push(MenuItem::Entry(
        entry(
            if line_count.is_some_and(|count| count > 1) {
                "Comment on Selected Lines"
            } else {
                "Add Comment"
            },
            Some(AddComment.boxed_clone()),
            Command::AddComment,
        )
        .disabled_because(match &target {
            None => Some("No line here to comment on".into()),
            Some(_) => line_refusal,
        }),
    ));
    if let Some(path) = &path {
        items.push(MenuItem::Entry(
            entry(
                "Comment on File",
                Some(AddFileComment.boxed_clone()),
                Command::FileComment(path.clone()),
            )
            .disabled_because(file_refusal),
        ));
    }

    let threads_are_read_only = || {
        Some(SharedString::from(
            "Threads on GitHub can't be changed in the review",
        ))
    };
    let submitted = || Some(SharedString::from("Submitted comments can't be changed"));
    match comment {
        Some(id) => {
            items.push(MenuItem::Separator);
            items.push(MenuItem::Entry(
                entry(
                    "Edit Comment",
                    Some(EditComment.boxed_clone()),
                    Command::EditComment(id),
                )
                .disabled_because(if comment_submitted {
                    submitted()
                } else {
                    None
                }),
            ));
            items.push(MenuItem::Entry(
                entry("Delete Comment", None, Command::DeleteComment(id))
                    .disabled_because(if comment_submitted { submitted() } else { None }),
            ));
        }
        None if on_thread => {
            items.push(MenuItem::Separator);
            for (label, action) in [
                ("Edit Comment", EditComment.boxed_clone()),
                ("Delete Comment", DeleteComment.boxed_clone()),
            ] {
                items.push(MenuItem::Entry(
                    entry(label, None, Command::Dispatch(action))
                        .disabled_because(threads_are_read_only()),
                ));
            }
        }
        None => {}
    }
    if let Some(body) = body {
        if comment.is_none() && !on_thread {
            items.push(MenuItem::Separator);
        }
        items.push(MenuItem::Entry(entry(
            "Copy Comment",
            Some(CopyCommentAtCursor.boxed_clone()),
            Command::CopyText(body, "Comment copied to clipboard"),
        )));
    }

    items.push(MenuItem::Separator);
    let hunk_entry = match &hunk {
        Some((path, key)) => entry(
            if session.is_hunk_reviewed(path, key) {
                "Mark Hunk as Not Reviewed"
            } else {
                "Mark Hunk as Reviewed"
            },
            Some(ToggleHunkReviewed.boxed_clone()),
            Command::ToggleHunk(path.clone(), key.clone()),
        )
        .disabled_because(if marks_loaded { None } else { not_loaded() }),
        None => entry(
            "Mark Hunk as Reviewed",
            Some(ToggleHunkReviewed.boxed_clone()),
            Command::Dispatch(ToggleHunkReviewed.boxed_clone()),
        )
        .disabled_because(Some("Not on a changed line".into())),
    };
    items.push(MenuItem::Entry(hunk_entry));
    if let Some(path) = &path {
        items.push(MenuItem::Entry(
            entry(
                if session.is_file_reviewed(path) {
                    "Mark File as Not Reviewed"
                } else {
                    "Mark File as Reviewed"
                },
                Some(ToggleFileReviewed.boxed_clone()),
                Command::ToggleFile(path.clone()),
            )
            .disabled_because(if marks_loaded { None } else { not_loaded() }),
        ));
    }

    items.push(MenuItem::Separator);
    items.push(MenuItem::Entry(
        entry(
            if line_count.is_some_and(|count| count > 1) {
                "Copy Selected Lines"
            } else {
                "Copy Line"
            },
            Some(CopySelectedLines.boxed_clone()),
            Command::Dispatch(CopySelectedLines.boxed_clone()),
        )
        .disabled_because(line_count.is_none().then(|| "No lines to copy here".into())),
    ));
    items.push(MenuItem::Entry(entry(
        "Copy Review as Markdown",
        Some(CopyReviewAsMarkdown.boxed_clone()),
        Command::Dispatch(CopyReviewAsMarkdown.boxed_clone()),
    )));
    if is_pull_request {
        // The left side shows the diff base, which has no file to link to.
        if !is_lhs {
            items.push(MenuItem::Entry(entry(
                "Copy Permalink to Line",
                Some(CopyPermalinkToLine.boxed_clone()),
                Command::Dispatch(CopyPermalinkToLine.boxed_clone()),
            )));
        }
        items.push(MenuItem::Entry(entry(
            "Copy Pull Request URL",
            Some(CopyPullRequestUrl.boxed_clone()),
            Command::Dispatch(CopyPullRequestUrl.boxed_clone()),
        )));
    }

    items.push(MenuItem::Separator);
    items.push(MenuItem::Entry(entry(
        "Open File at Line",
        Some(OpenFileAtCursor.boxed_clone()),
        Command::Dispatch(OpenFileAtCursor.boxed_clone()),
    )));
    let no_language_server = if is_snapshot {
        Some(SharedString::from(
            "A read-only snapshot has no language server",
        ))
    } else if is_lhs {
        Some(SharedString::from(
            "The old side of the diff has no language server",
        ))
    } else {
        None
    };
    items.push(MenuItem::Entry(
        entry(
            "Go to Definition",
            Some(GoToDefinition::default().boxed_clone()),
            Command::Dispatch(GoToDefinition::default().boxed_clone()),
        )
        .disabled_because(no_language_server.clone()),
    ));
    items.push(MenuItem::Entry(
        entry(
            "Find All References",
            Some(FindAllReferences::default().boxed_clone()),
            Command::Dispatch(FindAllReferences::default().boxed_clone()),
        )
        .disabled_because(no_language_server),
    ));
    items
}

fn context_menu_item(
    item: MenuItem,
    review: &WeakEntity<ReviewView>,
    side: &WeakEntity<Editor>,
) -> ContextMenuItem {
    let MenuEntry {
        label,
        disabled,
        key,
        command,
    } = match item {
        MenuItem::Separator => return ContextMenuItem::Separator,
        MenuItem::Entry(entry) => entry,
    };
    let review = review.clone();
    let side = side.clone();
    ContextMenuEntry::new(label)
        .when_some(key, |entry, key| entry.action(key))
        .disabled(disabled.is_some())
        .when_some(disabled, |entry, reason| {
            entry.documentation_aside(DocumentationSide::Right, move |_| {
                Label::new(reason.clone())
                    .size(LabelSize::Small)
                    .into_any_element()
            })
        })
        .handler(move |window, cx| {
            let (Some(review), Some(side)) = (review.upgrade(), side.upgrade()) else {
                return;
            };
            run(&command, review, side, window, cx);
        })
        .into()
}

fn run(
    command: &Command,
    review: Entity<ReviewView>,
    side: Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    let comments = review.read(cx).comments().clone();
    match command {
        Command::AddComment => comments.update(cx, |comments, cx| {
            comments.add_line_comment_on(side, None, window, cx)
        }),
        Command::FileComment(path) => comments.update(cx, |comments, cx| {
            comments.add_file_comment_from(path.clone(), side, window, cx)
        }),
        Command::ToggleHunk(path, key) => {
            review.update(cx, |review, cx| review.toggle_hunk(path, key.clone(), cx))
        }
        Command::ToggleFile(path) => review.update(cx, |review, cx| review.toggle_file(path, cx)),
        Command::EditComment(id) => {
            comments.update(cx, |comments, cx| comments.edit_from(*id, side, window, cx))
        }
        Command::DeleteComment(id) => {
            comments.update(cx, |comments, cx| comments.confirm_delete(*id, window, cx))
        }
        Command::CopyText(text, message) => {
            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
            let message = *message;
            let workspace = review.read(cx).workspace().clone();
            workspace
                .update(cx, |workspace, cx| {
                    workspace.show_toast(
                        Toast::new(NotificationId::unique::<MenuEntry>(), message).autohide(),
                        cx,
                    )
                })
                .log_err();
        }
        Command::Dispatch(action) => {
            window.focus(&side.focus_handle(cx), cx);
            window.dispatch_action(action.boxed_clone(), cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        pull_request::tests::review::{Fixture, open_review},
        test_support::{
            click_selector, cursor_line, init_vim_test, is_folded, is_reviewed, lhs_editor,
            move_to_row_with_text, open_review_fixture, path_at_cursor, redraw, rhs_editor,
            row_center,
        },
    };
    use gpui::{Modifiers, MouseButton, TestAppContext, VisualTestContext};
    use settings::DiffViewStyle;

    /// The menu's entries for `side` as "label" or "label (reason it's disabled)", separators
    /// as "-".
    fn entries(
        review: &Entity<ReviewView>,
        side: &Entity<Editor>,
        cx: &mut VisualTestContext,
    ) -> Vec<String> {
        cx.update(|_, cx| {
            menu_items(review, side, cx)
                .into_iter()
                .map(|item| match item {
                    MenuItem::Separator => "-".to_string(),
                    MenuItem::Entry(entry) => match entry.disabled {
                        Some(reason) => format!("{} ({reason})", entry.label),
                        None => entry.label.to_string(),
                    },
                })
                .collect()
        })
    }

    fn comment_bodies(review: &Entity<ReviewView>, cx: &VisualTestContext) -> Vec<String> {
        review.read_with(cx, |review, cx| {
            review
                .session()
                .read(cx)
                .comments()
                .iter()
                .map(|comment| comment.body.clone())
                .collect()
        })
    }

    fn right_click(editor: &Entity<Editor>, row: u32, cx: &mut VisualTestContext) {
        let position = row_center(editor, row, cx);
        cx.simulate_mouse_move(position, None, Modifiers::none());
        redraw(cx);
        cx.simulate_mouse_down(position, MouseButton::Right, Modifiers::none());
        cx.simulate_mouse_up(position, MouseButton::Right, Modifiers::none());
        cx.run_until_parked();
        redraw(cx);
    }

    /// Which entries the menu has, and which are disabled (and why), depend on where it opens.
    #[gpui::test]
    async fn test_context_menu_entries(cx: &mut TestAppContext) {
        init_vim_test(cx, DiffViewStyle::Split);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);
        let lhs = lhs_editor(&review, cx);

        move_to_row_with_text(&rhs, "LINE 2", cx);
        assert_eq!(
            entries(&review, &rhs, cx),
            [
                "Add Comment",
                "Comment on File",
                "-",
                "Mark Hunk as Reviewed",
                "Mark File as Reviewed",
                "-",
                "Copy Line",
                "Copy Review as Markdown",
                "-",
                "Open File at Line",
                "Go to Definition",
                "Find All References",
            ]
        );

        // A context row has no hunk; the old side has no language server.
        move_to_row_with_text(&lhs, "line 3", cx);
        let lhs_entries = entries(&review, &lhs, cx);
        assert!(lhs_entries.contains(&"Mark Hunk as Reviewed (Not on a changed line)".into()));
        assert!(lhs_entries.contains(
            &"Go to Definition (The old side of the diff has no language server)".into()
        ));

        // On a comment: edit, delete and copy it.
        move_to_row_with_text(&rhs, "line 3", cx);
        cx.simulate_keystrokes("c");
        cx.simulate_input("three");
        cx.simulate_keystrokes("ctrl-enter");
        let rhs_entries = entries(&review, &rhs, cx);
        for entry in ["Edit Comment", "Delete Comment", "Copy Comment"] {
            assert!(rhs_entries.contains(&entry.into()), "{rhs_entries:?}");
        }

        // A reviewed hunk and file say so.
        move_to_row_with_text(&rhs, "LINE 2", cx);
        cx.simulate_keystrokes("shift-r");
        assert!(entries(&review, &rhs, cx).contains(&"Mark Hunk as Not Reviewed".into()));
        review.update(cx, |review, cx| {
            review.toggle_file(&RepoPath::new("a.rs").unwrap(), cx)
        });
        cx.run_until_parked();
        assert!(entries(&review, &rhs, cx).contains(&"Mark File as Not Reviewed".into()));
    }

    /// A right click opens the review's menu at the clicked row: comment there, mark the file
    /// reviewed, comment on a selection it is in.
    #[gpui::test]
    async fn test_context_menu_actions(cx: &mut TestAppContext) {
        init_vim_test(cx, DiffViewStyle::Unified);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);
        let line_3 = move_to_row_with_text(&rhs, "line 3", cx);

        right_click(&rhs, line_3 + 1, cx);
        assert_eq!(cursor_line(&rhs, cx), "line 4");
        click_selector("MENU_ITEM-Add Comment", cx);
        cx.simulate_input("four");
        cx.simulate_keystrokes("ctrl-enter");
        assert_eq!(comment_bodies(&review, cx), ["four"]);
        review.read_with(cx, |review, cx| {
            let comment = review.session().read(cx).comments().iter().next().unwrap();
            assert_eq!(comment.location().as_deref(), Some("a.rs:4"));
        });

        // The comment's own entries act on it.
        right_click(&rhs, line_3 + 1, cx);
        click_selector("MENU_ITEM-Copy Comment", cx);
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("four")
        );

        right_click(&rhs, line_3, cx);
        click_selector("MENU_ITEM-Mark File as Reviewed", cx);
        assert!(is_reviewed(&review, "a.rs", cx));
        // Like the header's checkbox: the file folds, the cursor doesn't move on (unlike `r`).
        assert!(is_folded(&review, "a.rs", cx));
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("a.rs"));
        review.update(cx, |review, cx| {
            review.toggle_file(&RepoPath::new("a.rs").unwrap(), cx)
        });
        cx.run_until_parked();

        // Inside a selection, the selection stays and is commented on.
        move_to_row_with_text(&rhs, "LINE 2", cx);
        cx.simulate_keystrokes("shift-v j j");
        right_click(&rhs, line_3, cx);
        click_selector("MENU_ITEM-Comment on Selected Lines", cx);
        cx.simulate_input("two to four");
        cx.simulate_keystrokes("ctrl-enter");
        review.read_with(cx, |review, cx| {
            let comment = review.session().read(cx).comments().iter().nth(1).unwrap();
            assert_eq!(comment.location().as_deref(), Some("a.rs:2-4"));
        });
    }

    /// In a pull request review, the comment gate disables "Add Comment" on lines GitHub
    /// refuses, threads can be copied but not changed, and the pull request's links are there.
    #[gpui::test]
    async fn test_context_menu_in_a_pull_request_review(cx: &mut TestAppContext) {
        let (_fs, _workspace, review, _requests, cx) = open_review(Fixture::default(), cx).await;
        let rhs = rhs_editor(&review, cx);

        move_to_row_with_text(&rhs, "ENGINE 100", cx);
        let refused = entries(&review, &rhs, cx);
        assert_eq!(
            refused[0],
            "Add Comment (GitHub only accepts comments within its diff hunks)"
        );
        assert!(refused.contains(&"Copy Permalink to Line".into()));
        assert!(refused.contains(&"Copy Pull Request URL".into()));

        move_to_row_with_text(&rhs, "ENGINE 59", cx);
        assert_eq!(entries(&review, &rhs, cx)[0], "Add Comment");

        move_to_row_with_text(&rhs, "ENGINE 134", cx);
        let on_thread = entries(&review, &rhs, cx);
        for entry in [
            "Edit Comment (Threads on GitHub can't be changed in the review)",
            "Delete Comment (Threads on GitHub can't be changed in the review)",
            "Copy Comment",
        ] {
            assert!(on_thread.contains(&entry.into()), "{on_thread:?}");
        }
    }
}
