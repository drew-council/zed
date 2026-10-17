use crate::{
    AddReviewComment, CopyReviewAsMarkdown, NextComment, NextFile, NextHunk, PreviousComment,
    PreviousFile, PreviousHunk, ReviewView, SubmitReview, ToggleSingleFileMode,
    view::{copy_review_as_markdown, submit_review},
};
use gpui::{Action, Entity, EventEmitter, Focusable as _, Subscription, WeakEntity};
use ui::{Divider, TintColor, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    Pane, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView, Workspace, item::ItemHandle,
};

/// Puts a [`ReviewToolbar`] in every pane of the workspace, including the panes split off later.
/// Zed adds its own toolbar items the same way (`zed::initialize_pane`), so `zed.rs` needs no
/// change. Ours are added after Zed's (whose handlers run after ours), so that the search bar's
/// buttons come first in the row.
pub(crate) fn register(workspace: &Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let panes = workspace.panes().to_vec();
    cx.defer_in(window, move |_, window, cx| {
        for pane in &panes {
            add_to_pane(pane, window, cx);
        }
    });
    cx.subscribe_in(
        &cx.entity(),
        window,
        |_, _, event: &workspace::Event, window, cx| {
            if let workspace::Event::PaneAdded(pane) = event {
                let pane = pane.clone();
                cx.defer_in(window, move |_, window, cx| add_to_pane(&pane, window, cx));
            }
        },
    )
    .detach();
}

fn add_to_pane(pane: &Entity<Pane>, window: &mut Window, cx: &mut Context<Workspace>) {
    let item = cx.new(|_| ReviewToolbar::default());
    pane.update(cx, |pane, cx| {
        pane.toolbar()
            .update(cx, |toolbar, cx| toolbar.add_item(item, window, cx))
    });
}

/// The review tab's part of the pane's toolbar row, after the search bar's collapse and
/// split/unified buttons: what is reviewed and commented so far (cut short when the pane is
/// narrow), then at the right end the navigation buttons, the review's own actions and, last,
/// submitting or finishing the review.
#[derive(Default)]
pub(crate) struct ReviewToolbar {
    review: Option<WeakEntity<ReviewView>>,
    _observation: Option<Subscription>,
}

impl ReviewToolbar {
    fn review(&self) -> Option<Entity<ReviewView>> {
        self.review.as_ref()?.upgrade()
    }

    /// The review's view actions are handled by its diff: it takes the focus first (the toolbar
    /// doesn't), and the action goes there.
    fn dispatch(&self, action: Box<dyn Action>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(review) = self.review() else {
            return;
        };
        window.focus(&review.focus_handle(cx), cx);
        window.defer(cx, move |window, cx| window.dispatch_action(action, cx));
    }

    /// Submitting and copying act on this toolbar's review, which isn't necessarily the active
    /// one (the workspace actions' choice) when the toolbar is in another pane.
    fn submit(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(review) = self.review() else {
            return;
        };
        let workspace = review.read(cx).workspace().clone();
        workspace
            .update(cx, |workspace, cx| {
                submit_review(workspace, review, window, cx)
            })
            .log_err();
    }

    fn copy_as_markdown(&self, cx: &mut Context<Self>) {
        let Some(review) = self.review() else {
            return;
        };
        let workspace = review.read(cx).workspace().clone();
        workspace
            .update(cx, |workspace, cx| {
                copy_review_as_markdown(workspace, &review, cx)
            })
            .log_err();
    }

    /// "3/12 reviewed · 5 comments · 2 threads" (threads in pull request reviews only).
    pub(crate) fn summary_text(&self, cx: &App) -> Option<String> {
        let review = self.review()?;
        let review = review.read(cx);
        let summary = review.summary(cx);
        let mut text = format!(
            "{}/{} reviewed · {}",
            summary.reviewed,
            summary.total,
            counted(review.draft_comment_count(cx), "comment")
        );
        if review.pull_request().is_some() {
            text.push_str(&format!(
                " · {}",
                counted(review.shown_thread_count(cx), "thread")
            ));
        }
        Some(text)
    }
}

fn counted(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

impl EventEmitter<ToolbarItemEvent> for ReviewToolbar {}

impl ToolbarItemView for ReviewToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        let review = active_pane_item.and_then(|item| item.act_as::<ReviewView>(cx));
        // Marks, comments, threads and single-file mode all notify the review.
        self._observation = review
            .as_ref()
            .map(|review| cx.observe(review, |_, _, cx| cx.notify()));
        self.review = review.as_ref().map(Entity::downgrade);
        cx.notify();
        // On the left, which takes the row's width: the right side doesn't shrink, and would
        // squeeze the search bar's buttons out of a narrow pane.
        if review.is_some() {
            ToolbarItemLocation::PrimaryLeft
        } else {
            ToolbarItemLocation::Hidden
        }
    }
}

/// An icon button of the toolbar, found by tests through `selector`.
fn toolbar_button(
    selector: &'static str,
    icon: IconName,
    tooltip: &'static str,
    action: &dyn Action,
    focus_handle: &gpui::FocusHandle,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div().debug_selector(|| selector.into()).child(
        IconButton::new(selector, icon)
            .icon_size(IconSize::Small)
            .tooltip(Tooltip::for_action_title_in(tooltip, action, focus_handle))
            .on_click(on_click),
    )
}

impl Render for ReviewToolbar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(review) = self.review() else {
            return div().into_any_element();
        };
        let focus_handle = review.focus_handle(cx);
        let summary = self.summary_text(cx).unwrap_or_default();
        let review = review.read(cx);
        let single_file = review.single_file_position(cx);
        let is_pull_request = review.pull_request().is_some();
        let this = cx.weak_entity();
        let dispatcher = |action: Box<dyn Action>| {
            let this = this.clone();
            move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| {
                this.update(cx, |this, cx| {
                    this.dispatch(action.boxed_clone(), window, cx)
                })
                .log_err();
            }
        };
        let navigation =
            |label: &'static str,
             previous: (&'static str, &'static str, Box<dyn Action>),
             next: (&'static str, &'static str, Box<dyn Action>)| {
                h_flex()
                    .gap_0p5()
                    .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
                    .child(toolbar_button(
                        previous.0,
                        IconName::ArrowUp,
                        previous.1,
                        previous.2.as_ref(),
                        &focus_handle,
                        dispatcher(previous.2.boxed_clone()),
                    ))
                    .child(toolbar_button(
                        next.0,
                        IconName::ArrowDown,
                        next.1,
                        next.2.as_ref(),
                        &focus_handle,
                        dispatcher(next.2.boxed_clone()),
                    ))
            };

        // What the single-file bar said: which file of how many, and its path.
        let summary = match &single_file {
            Some((position, count, path)) => {
                format!("{summary} · single file {position} of {count}: {path}")
            }
            None => summary,
        };
        let controls = h_flex()
            .flex_none()
            .gap_2()
            .child(navigation(
                "File",
                (
                    "review-toolbar-previous-file",
                    "Previous File",
                    PreviousFile.boxed_clone(),
                ),
                (
                    "review-toolbar-next-file",
                    "Next File",
                    NextFile.boxed_clone(),
                ),
            ))
            .child(navigation(
                "Hunk",
                (
                    "review-toolbar-previous-hunk",
                    "Previous Hunk",
                    PreviousHunk.boxed_clone(),
                ),
                (
                    "review-toolbar-next-hunk",
                    "Next Hunk",
                    NextHunk.boxed_clone(),
                ),
            ))
            .child(navigation(
                "Comment",
                (
                    "review-toolbar-previous-comment",
                    "Previous Comment",
                    PreviousComment.boxed_clone(),
                ),
                (
                    "review-toolbar-next-comment",
                    "Next Comment",
                    NextComment.boxed_clone(),
                ),
            ))
            .child(Divider::vertical())
            .child(
                div()
                    .debug_selector(|| "review-toolbar-add-review-comment".into())
                    .child(
                        Button::new("review-toolbar-add-review-comment", "Review Comment")
                            .label_size(LabelSize::Small)
                            .start_icon(
                                Icon::new(IconName::Plus)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .tooltip(Tooltip::for_action_title_in(
                                "Add a Comment on the Whole Review",
                                &AddReviewComment,
                                &focus_handle,
                            ))
                            .on_click(dispatcher(AddReviewComment.boxed_clone())),
                    ),
            )
            .child(
                div()
                    .debug_selector(|| "review-toolbar-single-file".into())
                    .child(
                        IconButton::new("review-toolbar-single-file", IconName::File)
                            .icon_size(IconSize::Small)
                            .toggle_state(single_file.is_some())
                            .tooltip(Tooltip::for_action_title_in(
                                if single_file.is_some() {
                                    "Show All Files"
                                } else {
                                    "Show Only the File at the Cursor"
                                },
                                &ToggleSingleFileMode,
                                &focus_handle,
                            ))
                            .on_click(dispatcher(ToggleSingleFileMode.boxed_clone())),
                    ),
            )
            .child(toolbar_button(
                "review-toolbar-copy-markdown",
                IconName::Copy,
                "Copy Review as Markdown",
                &CopyReviewAsMarkdown,
                &focus_handle,
                {
                    let this = this.clone();
                    move |_, _, cx| {
                        this.update(cx, |this, cx| this.copy_as_markdown(cx))
                            .log_err();
                    }
                },
            ))
            .child(Divider::vertical())
            .child(
                div()
                    .debug_selector(|| "review-toolbar-submit".into())
                    .child(
                        Button::new(
                            "review-toolbar-submit",
                            if is_pull_request {
                                "Submit Review"
                            } else {
                                "Finish Review"
                            },
                        )
                        .style(ButtonStyle::Tinted(TintColor::Accent))
                        .label_size(LabelSize::Small)
                        .tooltip(Tooltip::for_action_title_in(
                            if is_pull_request {
                                "Submit the Review to GitHub"
                            } else {
                                "Finish the Review: Copy Its Comments as Markdown"
                            },
                            &SubmitReview,
                            &focus_handle,
                        ))
                        .on_click(move |_, window, cx| {
                            this.update(cx, |this, cx| this.submit(window, cx))
                                .log_err();
                        }),
                    ),
            );

        h_flex()
            .flex_1()
            .min_w_0()
            .gap_2()
            .child(Divider::vertical())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .debug_selector(|| "review-toolbar-summary".into())
                    .child(
                        Label::new(summary)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    ),
            )
            .child(controls)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ReviewOutcome,
        pull_request::tests::review::{Fixture, open_review},
        submit::SubmitReviewModal,
        test_support::{
            active_review, click_selector, cursor_point, dispatch, init_vim_test,
            move_to_row_with_text, open_review_fixture, path_at_cursor, redraw, rhs_editor,
        },
    };
    use gpui::{TestAppContext, VisualTestContext};
    use language::Point;
    use settings::DiffViewStyle;

    fn toolbar(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<ReviewToolbar> {
        workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_pane()
                .read(cx)
                .toolbar()
                .read(cx)
                .item_of_type::<ReviewToolbar>()
                .expect("every pane has a review toolbar")
        })
    }

    fn summary(toolbar: &Entity<ReviewToolbar>, cx: &mut VisualTestContext) -> Option<String> {
        toolbar.read_with(cx, |toolbar, cx| toolbar.summary_text(cx))
    }

    fn comment(keys: &str, text: &str, cx: &mut VisualTestContext) {
        cx.simulate_keystrokes(keys);
        cx.simulate_input(text);
        cx.simulate_keystrokes("ctrl-enter");
    }

    /// The toolbar shows for review tabs (split clones too), with counts that follow the
    /// review, and not for other items.
    #[gpui::test]
    async fn test_toolbar_follows_the_review(cx: &mut TestAppContext) {
        init_vim_test(cx, DiffViewStyle::Unified);
        let (_fs, workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);
        let toolbar = toolbar(&workspace, cx);
        assert_eq!(
            summary(&toolbar, cx).as_deref(),
            Some("0/5 reviewed · 0 comments")
        );
        redraw(cx);
        assert!(cx.debug_bounds("review-toolbar-summary").is_some());

        cx.simulate_keystrokes("r");
        move_to_row_with_text(&rhs, "line 3", cx);
        comment("c", "one", cx);
        assert_eq!(
            summary(&toolbar, cx).as_deref(),
            Some("1/5 reviewed · 1 comment")
        );
        comment("; c", "overall", cx);
        assert_eq!(
            summary(&toolbar, cx).as_deref(),
            Some("1/5 reviewed · 2 comments")
        );

        // A split clone shares the marks and comments, and has its own toolbar.
        dispatch(workspace::SplitRight::default(), cx);
        let clone = active_review(&workspace, cx);
        assert_ne!(clone, review);
        let clone_toolbar = self::toolbar(&workspace, cx);
        assert_ne!(clone_toolbar, toolbar);
        assert_eq!(
            summary(&clone_toolbar, cx).as_deref(),
            Some("1/5 reviewed · 2 comments")
        );
        // Its buttons act on the clone, not on the review in the other pane.
        let original_position = cursor_point(&rhs, cx);
        click_selector("review-toolbar-next-file", cx);
        assert_eq!(cursor_point(&rhs, cx), original_position);
        assert_eq!(path_at_cursor(&clone, cx).as_deref(), Some("added.txt"));

        // Another item hides it.
        let project_path = workspace.read_with(cx, |workspace, cx| {
            workspace
                .project()
                .read(cx)
                .find_project_path(util::path!("/project/a.rs"), cx)
                .unwrap()
        });
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_path(project_path, None, true, window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();
        assert_eq!(summary(&clone_toolbar, cx), None);
        // The other pane still shows the review.
        assert_eq!(
            summary(&toolbar, cx).as_deref(),
            Some("1/5 reviewed · 2 comments")
        );
    }

    /// Each button does what its key does.
    #[gpui::test]
    async fn test_toolbar_buttons(cx: &mut TestAppContext) {
        init_vim_test(cx, DiffViewStyle::Unified);
        let (_fs, workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);
        move_to_row_with_text(&rhs, "line 3", cx);
        comment("c", "three", cx);
        move_to_row_with_text(&rhs, "new 2", cx);
        comment("c", "new two", cx);
        let start = move_to_row_with_text(&rhs, "line 1", cx);

        let position =
            |cx: &mut VisualTestContext| (path_at_cursor(&review, cx), cursor_point(&rhs, cx).row);
        let set_cursor = |cx: &mut VisualTestContext| {
            rhs.update_in(cx, |editor, window, cx| {
                editor.change_selections(Default::default(), window, cx, |selections| {
                    selections.select_ranges([Point::new(start, 0)..Point::new(start, 0)])
                });
            });
            cx.run_until_parked();
        };
        for (selector, keys) in [
            ("review-toolbar-next-file", "] f"),
            ("review-toolbar-previous-file", "[ f"),
            ("review-toolbar-next-hunk", "] c"),
            ("review-toolbar-previous-hunk", "[ c"),
            ("review-toolbar-next-comment", "] t"),
            ("review-toolbar-previous-comment", "[ t"),
        ] {
            set_cursor(cx);
            cx.simulate_keystrokes(keys);
            let by_key = position(cx);
            set_cursor(cx);
            // From elsewhere: the button focuses the diff first.
            dispatch(crate::FocusPanel, cx);
            click_selector(selector, cx);
            assert_eq!(position(cx), by_key, "{selector}");
        }
        set_cursor(cx);
        click_selector("review-toolbar-next-file", cx);
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("added.txt"));

        click_selector("review-toolbar-single-file", cx);
        assert_eq!(
            review.read_with(cx, |review, cx| review.single_file_position(cx)),
            Some((2, 5, "added.txt".into()))
        );
        click_selector("review-toolbar-single-file", cx);
        assert_eq!(
            review.read_with(cx, |review, cx| review.single_file_position(cx)),
            None
        );

        click_selector("review-toolbar-copy-markdown", cx);
        let copied = cx.read_from_clipboard().and_then(|item| item.text());
        assert!(copied.as_deref().is_some_and(|text| text.contains("three")));

        click_selector("review-toolbar-add-review-comment", cx);
        cx.simulate_input("overall");
        cx.simulate_keystrokes("ctrl-enter");
        assert_eq!(
            toolbar(&workspace, cx).read_with(cx, |toolbar, cx| toolbar.summary_text(cx)),
            Some("0/5 reviewed · 3 comments".to_string())
        );

        // A local review is finished: its Markdown goes to whoever waits for it.
        let session = review.read_with(cx, |review, _| review.session().clone());
        let mut outcome = session.update(cx, |session, _| session.wait_for_outcome());
        click_selector("review-toolbar-submit", cx);
        match outcome.try_recv() {
            Ok(Some(ReviewOutcome::Finished {
                markdown: Some(markdown),
            })) => assert!(markdown.contains("overall"), "{markdown}"),
            other => panic!("the review wasn't finished: {other:?}"),
        }
    }

    /// A pull request review's toolbar counts its threads, and its primary button opens the
    /// submit modal.
    #[gpui::test]
    async fn test_toolbar_of_a_pull_request_review(cx: &mut TestAppContext) {
        let (_fs, workspace, _review, _requests, cx) = open_review(Fixture::default(), cx).await;
        let toolbar = toolbar(&workspace, cx);
        let summary = summary(&toolbar, cx).unwrap();
        assert!(summary.ends_with(" threads"), "{summary}");
        assert!(!summary.ends_with(" 0 threads"), "{summary}");
        click_selector("review-toolbar-submit", cx);
        assert!(workspace.read_with(cx, |workspace, cx| {
            workspace.active_modal::<SubmitReviewModal>(cx).is_some()
        }));
    }
}
