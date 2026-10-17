use crate::session::ReviewKey;
use anyhow::{Result, bail};
use git::Oid;
use gpui::SharedString;
use project::git_store::RepositorySnapshot;

/// What a review covers. Pull request targets will be added alongside these.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReviewTarget {
    /// HEAD → working tree, staged and unstaged changes together.
    Uncommitted,
    /// merge-base(base, HEAD) → working tree, uncommitted and untracked changes included. `base`
    /// is a ref passed verbatim to `git diff --merge-base` (e.g. "origin/main"), so the review
    /// follows fetches.
    Branch { base: SharedString },
    /// One commit or a contiguous range of commits.
    Commits(CommitRange),
}

/// The commits reachable from `head` but not from `base`, like git's `base..head`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CommitRange {
    /// The first parent of the oldest reviewed commit; `None` when that commit is a root commit.
    pub base: Option<Oid>,
    /// The newest reviewed commit.
    pub head: Oid,
}

impl ReviewTarget {
    /// Whether a review of `other` is the same review as one of `self`, so its tab can be reused.
    pub fn is_same_review(&self, other: &Self) -> bool {
        self == other
    }

    /// The key review state (reviewed marks, later drafts) is stored under. Paths come last, and
    /// git ref names can't contain `:`, so keys can't collide.
    pub fn review_key(&self, repo: &RepositorySnapshot) -> ReviewKey {
        let key = match self {
            ReviewTarget::Uncommitted => {
                format!("uncommitted:{}", repo.work_directory_abs_path.display())
            }
            ReviewTarget::Branch { base } => {
                let branch = repo.branch.as_ref().map_or("HEAD", |branch| branch.name());
                format!(
                    "branch:{base}:{branch}:{}",
                    repo.common_dir_abs_path.display()
                )
            }
            ReviewTarget::Commits(range) => {
                let base = range.base.map(|base| base.to_string()).unwrap_or_default();
                format!(
                    "commits:{base}..{}:{}",
                    range.head,
                    repo.common_dir_abs_path.display()
                )
            }
        };
        ReviewKey(key.into())
    }

    pub fn empty_label(&self) -> SharedString {
        match self {
            ReviewTarget::Uncommitted => "No uncommitted changes".into(),
            ReviewTarget::Branch { base } => format!("No changes against {base}").into(),
            ReviewTarget::Commits(_) => "No changes in these commits".into(),
        }
    }
}

/// A commit or commit range as typed by the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CommitSpec {
    Single(String),
    /// git's two-dot form `from..to`: the commits reachable from `to` but not from `from`.
    Range { from: String, to: String },
}

/// Parses "abc1234", "HEAD~2" or "A..B". Revisions end up in line-based `git cat-file --batch`
/// input and in git arguments, so whitespace and leading dashes are rejected.
pub(crate) fn parse_commit_spec(input: &str) -> Result<CommitSpec> {
    let input = input.trim();
    if input.is_empty() {
        bail!("No commit given");
    }
    if input.contains("...") {
        bail!("Use A..B for a commit range; A...B is not supported");
    }
    let spec = match input.split_once("..") {
        Some((from, to)) => CommitSpec::Range {
            from: validate_revision(from)?,
            to: validate_revision(to)?,
        },
        None => CommitSpec::Single(validate_revision(input)?),
    };
    Ok(spec)
}

fn validate_revision(revision: &str) -> Result<String> {
    if revision.is_empty() {
        bail!("A commit range needs a commit on both sides of \"..\"");
    }
    if revision.chars().any(char::is_whitespace) {
        bail!("Invalid revision {revision:?}");
    }
    if revision.starts_with('-') {
        bail!("Invalid revision {revision:?}");
    }
    Ok(revision.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_commit_spec() {
        assert_eq!(
            parse_commit_spec("abc1234").unwrap(),
            CommitSpec::Single("abc1234".into())
        );
        assert_eq!(
            parse_commit_spec("HEAD").unwrap(),
            CommitSpec::Single("HEAD".into())
        );
        assert_eq!(
            parse_commit_spec("HEAD~3..HEAD").unwrap(),
            CommitSpec::Range {
                from: "HEAD~3".into(),
                to: "HEAD".into()
            }
        );
        assert_eq!(
            parse_commit_spec(" A..B \n").unwrap(),
            CommitSpec::Range {
                from: "A".into(),
                to: "B".into()
            }
        );
        for invalid in ["", "  ", "-x", "A..-x", "A...B", "a b", "a\nb", "..B", "A..", ".."] {
            assert!(
                parse_commit_spec(invalid).is_err(),
                "{invalid:?} should be rejected"
            );
        }
    }
}
