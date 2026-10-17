mod blocks;
mod view;

pub(crate) use blocks::{CommentBlocks, CommentBlocksEvent};

use git::repository::RepoPath;
use gpui::SharedString;
use std::ops::{Range, RangeInclusive};
use ui::Color;

/// A comment's number within its review, in creation order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct CommentId(pub u64);

/// The side of the diff a line comment is on: the old (removed) lines or the new ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Side {
    Old,
    New,
}

/// tuicr's comment types. Comments are untyped unless given one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum CommentType {
    Issue,
    Suggestion,
    Note,
    Praise,
}

impl CommentType {
    const CYCLE: [Option<Self>; 5] = [
        None,
        Some(Self::Issue),
        Some(Self::Suggestion),
        Some(Self::Note),
        Some(Self::Praise),
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Issue => "ISSUE",
            Self::Suggestion => "SUGGESTION",
            Self::Note => "NOTE",
            Self::Praise => "PRAISE",
        }
    }

    pub(crate) fn color(self) -> Color {
        match self {
            Self::Issue => Color::Error,
            Self::Suggestion => Color::Info,
            Self::Note => Color::Warning,
            Self::Praise => Color::Success,
        }
    }

    /// The type after (or before) `current` in the cycle untyped → issue → suggestion → note →
    /// praise → untyped.
    pub(crate) fn cycle(current: Option<Self>, forward: bool) -> Option<Self> {
        let count = Self::CYCLE.len();
        let index = Self::CYCLE
            .iter()
            .position(|comment_type| *comment_type == current)
            .unwrap_or(0);
        let next = if forward {
            (index + 1) % count
        } else {
            (index + count - 1) % count
        };
        Self::CYCLE[next]
    }
}

/// What a comment is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CommentTarget {
    /// The whole review (GitHub's review body).
    Review,
    File {
        path: RepoPath,
    },
    Lines {
        path: RepoPath,
        side: Side,
        /// 1-based line numbers on `side` when the comment was made.
        lines: RangeInclusive<u32>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CommentState {
    Draft,
    /// The commented lines changed while the review was closed.
    #[allow(dead_code, reason = "set once comments are persisted")]
    Outdated,
    /// Posted to GitHub as part of a review, so it can't change any more.
    #[allow(dead_code, reason = "set once reviews can be submitted to GitHub")]
    Submitted {
        review_id: SharedString,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct Comment {
    pub id: CommentId,
    pub target: CommentTarget,
    pub comment_type: Option<CommentType>,
    /// The saved text (trimmed). Edits in an open comment editor aren't part of it until the
    /// comment is left.
    pub body: String,
    /// The text of the commented lines when the comment was made, to tell whether they changed
    /// since.
    #[allow(dead_code, reason = "compared once comments are persisted")]
    pub line_text: Option<String>,
    pub state: CommentState,
    /// Where a line comment's lines are now, in the buffer of their side (the file's buffer for
    /// the new side). Anchors follow edits; they aren't stored.
    pub anchor: Option<Range<language::Anchor>>,
}

pub(crate) struct NewComment {
    pub target: CommentTarget,
    pub line_text: Option<String>,
    pub anchor: Option<Range<language::Anchor>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommentEvent {
    Added(CommentId),
    Changed(CommentId),
    Removed(CommentId),
}

/// The comments of one review, in creation order.
#[derive(Default)]
pub(crate) struct CommentStore {
    comments: Vec<Comment>,
    next_id: u64,
}

impl CommentStore {
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Comment> {
        self.comments.iter()
    }

    pub(crate) fn get(&self, id: CommentId) -> Option<&Comment> {
        self.comments.iter().find(|comment| comment.id == id)
    }

    pub(crate) fn get_mut(&mut self, id: CommentId) -> Option<&mut Comment> {
        self.comments.iter_mut().find(|comment| comment.id == id)
    }

    pub(crate) fn insert(&mut self, new_comment: NewComment) -> CommentId {
        let id = CommentId(self.next_id);
        self.next_id += 1;
        self.comments.push(Comment {
            id,
            target: new_comment.target,
            comment_type: None,
            body: String::new(),
            line_text: new_comment.line_text,
            state: CommentState::Draft,
            anchor: new_comment.anchor,
        });
        id
    }

    pub(crate) fn remove(&mut self, id: CommentId) -> Option<Comment> {
        let index = self.comments.iter().position(|comment| comment.id == id)?;
        Some(self.comments.remove(index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_comment_type_cycle() {
        let mut comment_type = None;
        let mut seen = Vec::new();
        for _ in 0..5 {
            comment_type = CommentType::cycle(comment_type, true);
            seen.push(comment_type);
        }
        assert_eq!(
            seen,
            [
                Some(CommentType::Issue),
                Some(CommentType::Suggestion),
                Some(CommentType::Note),
                Some(CommentType::Praise),
                None,
            ]
        );
        assert_eq!(CommentType::cycle(None, false), Some(CommentType::Praise));
        assert_eq!(CommentType::cycle(Some(CommentType::Issue), false), None);
    }
}
