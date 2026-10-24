mod blocks;
mod remote;
mod view;

pub(crate) use blocks::{
    CommentBlocks, CommentBlocksEvent, ItemKey, RemoteItem, RemoteKey, keep_scroll_at_start,
    stay_at_start,
};
pub(crate) use remote::{
    HoveredLink, RemoteThreadContent, comment_markdown_style, github_markdown, open_web_link,
    thread_location, untrusted_markdown,
};

use anyhow::{Context as _, Result};
use git::repository::RepoPath;
use gpui::SharedString;
use serde::{Deserialize, Serialize};
use std::ops::RangeInclusive;
use ui::Color;

/// A comment's number within its review, in creation order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct CommentId(pub u64);

/// The side of the diff a line comment is on: the old (removed) lines or the new ones. Old lines
/// come first where both sides are at the same place.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Side {
    Old,
    New,
}

/// tuicr's comment types. Comments are untyped unless given one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

    /// Every type, in the order the export's legend lists them.
    pub(crate) const ALL: [Self; 4] = [Self::Issue, Self::Suggestion, Self::Note, Self::Praise];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Issue => "ISSUE",
            Self::Suggestion => "SUGGESTION",
            Self::Note => "NOTE",
            Self::Praise => "PRAISE",
        }
    }

    /// What the type means, as the export's legend explains it to the reader.
    pub(crate) fn definition(self) -> &'static str {
        match self {
            Self::Issue => "problems to fix",
            Self::Suggestion => "improvements",
            Self::Note => "observations",
            Self::Praise => "positive feedback",
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
        /// The side of the last line, which the comment shows below.
        side: Side,
        /// 1-based line numbers, the first on `start_side`, the last on `side`. They follow the
        /// comment's lines while the review is open, and are what a review opened later looks
        /// for.
        lines: RangeInclusive<u32>,
        /// The side of the first line. A range from a removed line to an added one, or back,
        /// has its ends on different sides (GitHub's `start_side`): its two numbers count on
        /// different sides then, so they aren't a range of either side's lines.
        start_side: Side,
    },
}

impl CommentTarget {
    pub(crate) fn path(&self) -> Option<&RepoPath> {
        match self {
            Self::Review => None,
            Self::File { path } | Self::Lines { path, .. } => Some(path),
        }
    }

    /// Lines with both ends on `side`.
    pub(crate) fn lines(path: RepoPath, side: Side, lines: RangeInclusive<u32>) -> Self {
        Self::Lines {
            path,
            side,
            lines,
            start_side: side,
        }
    }

    /// A range whose ends are on different sides of the diff.
    pub(crate) fn is_mixed(&self) -> bool {
        matches!(self, Self::Lines { side, start_side, .. } if side != start_side)
    }

    /// Whether the target spans more than one line.
    pub(crate) fn is_range(&self) -> bool {
        matches!(self, Self::Lines { lines, .. } if lines.start() != lines.end()) || self.is_mixed()
    }

    /// Where the target is, as the export and the review panel show it: `path:12`,
    /// `path:12-14`, `path:~12` (old side), `path:~12-14` (from an old line to a new one),
    /// `path`, or `None` for the whole review.
    pub(crate) fn location(&self) -> Option<String> {
        match self {
            Self::Review => None,
            Self::File { path } => Some(path.as_unix_str().to_string()),
            Self::Lines {
                path,
                side,
                lines,
                start_side,
            } => {
                let path = path.as_unix_str();
                let marker = |side: &Side| match side {
                    Side::Old => "~",
                    Side::New => "",
                };
                Some(if self.is_range() {
                    format!(
                        "{path}:{}{}-{}{}",
                        marker(start_side),
                        lines.start(),
                        marker(side),
                        lines.end()
                    )
                } else {
                    format!("{path}:{}{}", marker(side), lines.end())
                })
            }
        }
    }

    /// The target with its new lines numbered by `map` (old lines are numbered in the diff
    /// base on every side): all of them, or the end of a mixed range that is on the new side.
    pub(crate) fn map_new_lines<E>(
        &self,
        map: impl FnOnce(&RepoPath, RangeInclusive<u32>) -> Result<RangeInclusive<u32>, E>,
    ) -> Result<Self, E> {
        let Self::Lines {
            path,
            side,
            lines,
            start_side,
        } = self
        else {
            return Ok(self.clone());
        };
        let (start, end) = (*lines.start(), *lines.end());
        let lines = match (start_side, side) {
            (Side::Old, Side::Old) => lines.clone(),
            (Side::New, Side::New) => map(path, lines.clone())?,
            (Side::Old, Side::New) => start..=*map(path, end..=end)?.end(),
            (Side::New, Side::Old) => *map(path, start..=start)?.start()..=end,
        };
        Ok(Self::Lines {
            path: path.clone(),
            side: *side,
            lines,
            start_side: *start_side,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CommentState {
    Draft,
    /// The commented lines had changed when the review was opened again. The comment stays (on
    /// the lines with the same numbers) until it is deleted.
    Outdated,
    /// Posted to GitHub as part of a review, so it can't change any more.
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
    /// The text of the commented lines, kept up to date like the line numbers, to tell whether
    /// they changed while the review was closed.
    pub line_text: Option<String>,
    pub state: CommentState,
}

impl Comment {
    pub(crate) fn is_outdated(&self) -> bool {
        self.state == CommentState::Outdated
    }

    pub(crate) fn is_submitted(&self) -> bool {
        matches!(self.state, CommentState::Submitted { .. })
    }

    /// Where the comment is ([`CommentTarget::location`]).
    pub(crate) fn location(&self) -> Option<String> {
        self.target.location()
    }
}

pub(crate) struct NewComment {
    pub target: CommentTarget,
    pub line_text: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CommentEvent {
    Added(CommentId),
    Changed(CommentId),
    /// The lines of line comments moved, or their text changed, as one tab saw them.
    Moved(Vec<CommentId>),
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
        self.next_id = self.next_id.saturating_add(1);
        self.comments.push(Comment {
            id,
            target: new_comment.target,
            comment_type: None,
            body: String::new(),
            line_text: new_comment.line_text,
            state: CommentState::Draft,
        });
        id
    }

    /// Adds comments read from the database, keeping their ids. New ids continue after
    /// `max_stored_id`, the largest id stored, which may belong to a row that couldn't be read (a
    /// newer version wrote it): a new comment with that id would overwrite it.
    pub(crate) fn restore(
        &mut self,
        comments: impl IntoIterator<Item = Comment>,
        max_stored_id: Option<u64>,
    ) {
        if let Some(max_stored_id) = max_stored_id {
            self.next_id = self.next_id.max(max_stored_id.saturating_add(1));
        }
        for comment in comments {
            if self.get(comment.id).is_some() {
                continue;
            }
            self.next_id = self.next_id.max(comment.id.0.saturating_add(1));
            self.comments.push(comment);
        }
        self.comments.sort_by_key(|comment| comment.id);
    }

    pub(crate) fn remove(&mut self, id: CommentId) -> Option<Comment> {
        let index = self.comments.iter().position(|comment| comment.id == id)?;
        Some(self.comments.remove(index))
    }
}

/// A comment as stored in the database (JSON, so that fields can be added without a migration:
/// new ones need `#[serde(default)]`).
#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct StoredComment {
    target: StoredTarget,
    #[serde(default)]
    comment_type: Option<CommentType>,
    body: String,
    #[serde(default)]
    line_text: Option<String>,
    #[serde(default)]
    state: StoredState,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredTarget {
    Review,
    File {
        path: String,
    },
    Lines {
        path: String,
        side: Side,
        start: u32,
        end: u32,
        /// Only for a range whose ends are on different sides.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start_side: Option<Side>,
    },
}

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredState {
    #[default]
    Draft,
    Outdated,
    Submitted {
        review_id: String,
    },
}

impl Comment {
    pub(crate) fn to_json(&self) -> Result<String> {
        let target = match &self.target {
            CommentTarget::Review => StoredTarget::Review,
            CommentTarget::File { path } => StoredTarget::File {
                path: path.as_unix_str().to_string(),
            },
            CommentTarget::Lines {
                path,
                side,
                lines,
                start_side,
            } => StoredTarget::Lines {
                path: path.as_unix_str().to_string(),
                side: *side,
                start: *lines.start(),
                end: *lines.end(),
                start_side: (start_side != side).then_some(*start_side),
            },
        };
        let state = match &self.state {
            CommentState::Draft => StoredState::Draft,
            CommentState::Outdated => StoredState::Outdated,
            CommentState::Submitted { review_id } => StoredState::Submitted {
                review_id: review_id.to_string(),
            },
        };
        let stored = StoredComment {
            target,
            comment_type: self.comment_type,
            body: self.body.clone(),
            line_text: self.line_text.clone(),
            state,
        };
        Ok(serde_json::to_string(&stored)?)
    }

    pub(crate) fn from_json(id: CommentId, json: &str) -> Result<Self> {
        let stored: StoredComment =
            serde_json::from_str(json).context("parsing a stored comment")?;
        let target = match stored.target {
            StoredTarget::Review => CommentTarget::Review,
            StoredTarget::File { path } => CommentTarget::File {
                path: RepoPath::new(&path)?,
            },
            StoredTarget::Lines {
                path,
                side,
                start,
                end,
                start_side,
            } => {
                let start_side = start_side.unwrap_or(side);
                // A mixed range's ends count on different sides, so their order says nothing.
                anyhow::ensure!(
                    1 <= start && 1 <= end && (start <= end || start_side != side),
                    "invalid line range {start}-{end}"
                );
                CommentTarget::Lines {
                    path: RepoPath::new(&path)?,
                    side,
                    lines: start..=end,
                    start_side,
                }
            }
        };
        let state = match stored.state {
            StoredState::Draft => CommentState::Draft,
            StoredState::Outdated => CommentState::Outdated,
            StoredState::Submitted { review_id } => CommentState::Submitted {
                review_id: review_id.into(),
            },
        };
        Ok(Self {
            id,
            target,
            comment_type: stored.comment_type,
            body: stored.body,
            line_text: stored.line_text,
            state,
        })
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

    #[test]
    fn test_comment_json_round_trip() {
        let comments = [
            Comment {
                id: CommentId(0),
                target: CommentTarget::Review,
                comment_type: Some(CommentType::Praise),
                body: "Nice".into(),
                line_text: None,
                state: CommentState::Draft,
            },
            Comment {
                id: CommentId(3),
                target: CommentTarget::File {
                    path: RepoPath::new("src/a b.rs").unwrap(),
                },
                comment_type: None,
                body: "multi\nline".into(),
                line_text: None,
                state: CommentState::Submitted {
                    review_id: "42".into(),
                },
            },
            Comment {
                id: CommentId(7),
                target: CommentTarget::lines(RepoPath::new("lib.rs").unwrap(), Side::Old, 3..=5),
                comment_type: Some(CommentType::Issue),
                body: "x".into(),
                line_text: Some("a\nb\nc".into()),
                state: CommentState::Outdated,
            },
            Comment {
                id: CommentId(8),
                target: CommentTarget::Lines {
                    path: RepoPath::new("lib.rs").unwrap(),
                    side: Side::Old,
                    lines: RangeInclusive::new(30, 4),
                    start_side: Side::New,
                },
                comment_type: None,
                body: "mixed".into(),
                line_text: Some("a\nb".into()),
                state: CommentState::Draft,
            },
        ];
        for comment in comments {
            let json = comment.to_json().unwrap();
            let restored = Comment::from_json(comment.id, &json).unwrap();
            assert_eq!(restored.target, comment.target);
            assert_eq!(restored.comment_type, comment.comment_type);
            assert_eq!(restored.body, comment.body);
            assert_eq!(restored.line_text, comment.line_text);
            assert_eq!(restored.state, comment.state);
        }

        // Missing optional fields default; bad ranges are refused.
        let minimal = r#"{"target":{"kind":"lines","path":"a.rs","side":"new","start":2,"end":2},"body":"b"}"#;
        let comment = Comment::from_json(CommentId(1), minimal).unwrap();
        assert_eq!(comment.state, CommentState::Draft);
        assert_eq!(comment.location().as_deref(), Some("a.rs:2"));
        let reversed = r#"{"target":{"kind":"lines","path":"a.rs","side":"new","start":3,"end":2},"body":"b"}"#;
        assert!(Comment::from_json(CommentId(1), reversed).is_err());
        // A range from old line 6 to new line 8; the side is only stored when it differs.
        let mixed = r#"{"target":{"kind":"lines","path":"a.rs","side":"new","start":6,"end":8,"start_side":"old"},"body":"b"}"#;
        let comment = Comment::from_json(CommentId(1), mixed).unwrap();
        assert!(comment.target.is_mixed());
        assert_eq!(comment.location().as_deref(), Some("a.rs:~6-8"));
        assert!(
            comment
                .to_json()
                .unwrap()
                .contains(r#""start":6,"end":8,"start_side":"old""#)
        );
        let one_side = Comment::from_json(CommentId(1), minimal).unwrap();
        assert!(!one_side.to_json().unwrap().contains("start_side"));
    }
}
