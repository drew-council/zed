use crate::comments::{Comment, CommentTarget, CommentType};
use std::fmt::Write as _;

/// The Markdown export's opening line, tuicr's default.
const INTRO: &str = "I reviewed your code and have the following comments. Please address them.";

/// tuicr's heading, kept literally: agents prompted with tuicr's review format look for it.
const COMMENTS_HEADING: &str = "## Local tuicr Comments";

/// What the export says the review covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExportPreamble {
    /// The lines after the intro, e.g. "Reviewing staged + unstaged changes".
    pub banner: Vec<String>,
    /// What review-level comments are about: "Review Comment (scope: …)".
    pub scope: String,
}

/// The review's comments as Markdown in tuicr's numbered `[TYPE] path:line` format (DESIGN
/// §2.8), or `None` without comments to export. Comments submitted to GitHub and comments
/// without text (still being written) are left out; outdated ones are marked.
///
/// Order, as tuicr's: review comments, then files by path; in a file, file comments, then line
/// comments by their last line (old and new lines by number), each in creation order.
pub(crate) fn review_markdown(preamble: &ExportPreamble, comments: &[Comment]) -> Option<String> {
    let mut comments = comments
        .iter()
        .filter(|comment| !comment.is_submitted() && !comment.body.is_empty())
        .collect::<Vec<_>>();
    if comments.is_empty() {
        return None;
    }
    comments.sort_by_key(|comment| match &comment.target {
        CommentTarget::Review => (0, None, None, comment.id),
        CommentTarget::File { path } => (1, Some(path.as_unix_str().to_string()), None, comment.id),
        CommentTarget::Lines { path, lines, .. } => (
            1,
            Some(path.as_unix_str().to_string()),
            Some(*lines.end()),
            comment.id,
        ),
    });

    let mut markdown = String::new();
    writeln!(markdown, "{INTRO}").ok();
    writeln!(markdown).ok();
    if !preamble.banner.is_empty() {
        for line in &preamble.banner {
            writeln!(markdown, "{line}").ok();
        }
        writeln!(markdown).ok();
    }

    let legend = CommentType::ALL
        .into_iter()
        .filter(|comment_type| {
            comments
                .iter()
                .any(|comment| comment.comment_type == Some(*comment_type))
        })
        .map(|comment_type| format!("{} ({})", comment_type.label(), comment_type.definition()))
        .collect::<Vec<_>>();
    if !legend.is_empty() {
        writeln!(markdown, "Comment types: {}", legend.join(", ")).ok();
        writeln!(markdown).ok();
    }

    writeln!(markdown, "{COMMENTS_HEADING}").ok();
    writeln!(markdown).ok();
    for (index, comment) in comments.iter().enumerate() {
        let marker = format!("{}.", index + 1);
        let type_marker = comment
            .comment_type
            .map(|comment_type| format!("**[{}]** ", comment_type.label()))
            .unwrap_or_default();
        let location = comment
            .location()
            .unwrap_or_else(|| format!("Review Comment (scope: {})", preamble.scope));
        // Where tuicr says which commit a comment was made against.
        let state = if comment.is_outdated() {
            " (outdated)"
        } else {
            ""
        };
        let mut lines = comment
            .body
            .split('\n')
            .map(|line| line.trim_end_matches('\r'));
        let first_line = lines.next().unwrap_or_default();
        writeln!(
            markdown,
            "{marker} {type_marker}`{location}`{state} - {first_line}"
        )
        .ok();
        let indent = " ".repeat(marker.len() + 1);
        for line in lines {
            writeln!(markdown, "{indent}{line}").ok();
        }
    }
    Some(markdown)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comments::{CommentId, CommentState, Side};
    use git::repository::RepoPath;

    fn comment(
        id: u64,
        target: CommentTarget,
        comment_type: Option<CommentType>,
        body: &str,
    ) -> Comment {
        Comment {
            id: CommentId(id),
            target,
            comment_type,
            body: body.into(),
            line_text: None,
            state: CommentState::Draft,
            anchor: None,
        }
    }

    fn lines(path: &str, side: Side, start: u32, end: u32) -> CommentTarget {
        CommentTarget::Lines {
            path: RepoPath::new(path).unwrap(),
            side,
            lines: start..=end,
        }
    }

    fn file(path: &str) -> CommentTarget {
        CommentTarget::File {
            path: RepoPath::new(path).unwrap(),
        }
    }

    fn uncommitted() -> ExportPreamble {
        ExportPreamble {
            banner: vec!["Reviewing staged + unstaged changes".into()],
            scope: "staged + unstaged changes".into(),
        }
    }

    #[test]
    fn test_review_markdown_matches_tuicr() {
        assert_eq!(review_markdown(&uncommitted(), &[]), None);

        let mut outdated = comment(
            6,
            lines("src/b.rs", Side::New, 3, 3),
            Some(CommentType::Note),
            "moved?",
        );
        outdated.state = CommentState::Outdated;
        let mut submitted = comment(7, file("src/a.rs"), None, "already on GitHub");
        submitted.state = CommentState::Submitted {
            review_id: "1".into(),
        };
        let comments = [
            comment(
                0,
                lines("src/b.rs", Side::New, 10, 12),
                Some(CommentType::Issue),
                "range\r\nsecond line",
            ),
            comment(
                1,
                CommentTarget::Review,
                Some(CommentType::Issue),
                "Overall",
            ),
            comment(2, lines("src/a.rs", Side::Old, 7, 7), None, "removed line"),
            comment(3, file("src/b.rs"), None, "file comment"),
            comment(
                4,
                lines("src/a.rs", Side::New, 42, 42),
                None,
                "first\nsecond",
            ),
            comment(5, lines("src/a.rs", Side::Old, 7, 9), None, "old range"),
            outdated,
            submitted,
            comment(8, CommentTarget::Review, None, ""),
        ];
        let expected = "\
I reviewed your code and have the following comments. Please address them.

Reviewing staged + unstaged changes

Comment types: ISSUE (problems to fix), NOTE (observations)

## Local tuicr Comments

1. **[ISSUE]** `Review Comment (scope: staged + unstaged changes)` - Overall
2. `src/a.rs:~7` - removed line
3. `src/a.rs:~7-~9` - old range
4. `src/a.rs:42` - first
   second
5. `src/b.rs` - file comment
6. **[NOTE]** `src/b.rs:3` (outdated) - moved?
7. **[ISSUE]** `src/b.rs:10-12` - range
   second line
";
        assert_eq!(
            review_markdown(&uncommitted(), &comments).as_deref(),
            Some(expected)
        );
    }

    #[test]
    fn test_review_markdown_numbering_and_legend() {
        // Untyped comments only: no legend. Continuation lines line up with the text after
        // two-digit numbers too.
        let comments = (0..10)
            .map(|index| {
                comment(
                    index,
                    lines("a.rs", Side::New, index as u32 + 1, index as u32 + 1),
                    None,
                    "one\ntwo",
                )
            })
            .collect::<Vec<_>>();
        let preamble = ExportPreamble {
            banner: vec!["Reviewing commit: abc1234".into()],
            scope: "selected commit range".into(),
        };
        let markdown = review_markdown(&preamble, &comments).unwrap();
        assert!(!markdown.contains("Comment types:"));
        assert!(markdown.contains("Reviewing commit: abc1234\n\n## Local tuicr Comments\n\n1."));
        assert!(markdown.ends_with("10. `a.rs:10` - one\n    two\n"));
        assert!(markdown.contains("9. `a.rs:9` - one\n   two\n"));

        // Every type, in tuicr's order; no banner.
        let comments = [
            comment(0, CommentTarget::Review, Some(CommentType::Praise), "a"),
            comment(1, CommentTarget::Review, Some(CommentType::Suggestion), "b"),
            comment(2, CommentTarget::Review, Some(CommentType::Note), "c"),
            comment(3, CommentTarget::Review, Some(CommentType::Issue), "d"),
        ];
        let preamble = ExportPreamble {
            banner: Vec::new(),
            scope: "x".into(),
        };
        let markdown = review_markdown(&preamble, &comments).unwrap();
        assert!(markdown.starts_with(
            "I reviewed your code and have the following comments. Please address them.\n\n\
             Comment types: ISSUE (problems to fix), SUGGESTION (improvements), \
             NOTE (observations), PRAISE (positive feedback)\n\n"
        ));
    }
}
