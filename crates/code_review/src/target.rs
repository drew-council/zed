use crate::{
    forge::{ForgeRepo, ForgeRepoRef},
    session::ReviewKey,
};
use anyhow::{Result, bail};
use git::Oid;
use gpui::SharedString;
use http_client::Url;
use project::git_store::RepositorySnapshot;
use std::fmt;

/// What a review covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReviewTarget {
    /// HEAD → working tree, staged and unstaged changes together.
    Uncommitted,
    /// merge-base(base, HEAD) → working tree, uncommitted and untracked changes included.
    Branch { base: BranchBase },
    /// One commit or a contiguous range of commits.
    Commits(CommitRange),
    /// A pull request checked out in the repository: merge-base(base, HEAD) → working tree, which
    /// is the pull request's diff while HEAD is its head.
    PullRequest(PullRequestTarget),
}

/// A pull request of a forge repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequestTarget {
    /// The repository the pull request belongs to (not the one its head branch is in).
    pub repo: ForgeRepo,
    pub number: u64,
    /// The base branch's commit when the pull request was loaded. It comes from the forge, so it
    /// goes through the same check as a typed branch base.
    pub base: BranchBase,
}

impl PullRequestTarget {
    pub fn new(repo: ForgeRepo, number: u64, base_oid: Oid) -> Result<Self> {
        Ok(Self {
            repo,
            number,
            base: BranchBase::new(base_oid.to_string())?,
        })
    }
}

/// The ref a branch review compares against, e.g. "origin/main". It is passed by name to
/// `git diff --merge-base`, so the review follows fetches. Every base is built by
/// [`BranchBase::new`], which rejects anything git would parse as an option.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BranchBase(SharedString);

impl BranchBase {
    pub fn new(base: impl Into<SharedString>) -> Result<Self> {
        let base = base.into();
        if !is_plain_revision(&base) {
            bail!("Invalid base {base:?}");
        }
        Ok(Self(base))
    }

    pub fn name(&self) -> &SharedString {
        &self.0
    }
}

impl fmt::Display for BranchBase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Revisions end up in git's arguments and in line-based `git cat-file --batch` input.
fn is_plain_revision(revision: &str) -> bool {
    !revision.is_empty()
        && !revision.starts_with('-')
        && !revision
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
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
    pub fn branch(base: impl Into<SharedString>) -> Result<Self> {
        Ok(Self::Branch {
            base: BranchBase::new(base)?,
        })
    }

    /// Whether a review of `other` is the same review as one of `self`, so its tab can be reused.
    /// A pull request is the same review whatever its base commit is now.
    pub fn is_same_review(&self, other: &Self) -> bool {
        match (self, other) {
            (ReviewTarget::PullRequest(this), ReviewTarget::PullRequest(other)) => {
                this.repo.is_same_repo(&other.repo) && this.number == other.number
            }
            _ => self == other,
        }
    }

    /// The key review state (reviewed marks, later drafts) is stored under. Paths come last, and
    /// git ref names can't contain `:`, so keys can't collide.
    pub fn review_key(&self, repo: &RepositorySnapshot) -> ReviewKey {
        let key = match self {
            ReviewTarget::Uncommitted => {
                format!("uncommitted:{}", repo.work_directory_abs_path.display())
            }
            // A branch is checked out in one worktree at a time, but any number of worktrees can
            // have a detached HEAD.
            ReviewTarget::Branch { base } => match &repo.branch {
                Some(branch) => format!(
                    "branch:{base}:{}:{}",
                    branch.name(),
                    repo.common_dir_abs_path.display()
                ),
                None => format!(
                    "branch:{base}:HEAD:{}",
                    repo.work_directory_abs_path.display()
                ),
            },
            ReviewTarget::Commits(range) => {
                let base = range.base.map(|base| base.to_string()).unwrap_or_default();
                format!(
                    "commits:{base}..{}:{}",
                    range.head,
                    repo.common_dir_abs_path.display()
                )
            }
            // Independent of the checkout, so a review follows its pull request into another
            // worktree or clone. GitHub ignores the case of owners and names.
            ReviewTarget::PullRequest(pull_request) => format!(
                "pr:{}/{}/{}#{}",
                pull_request.repo.host.to_lowercase(),
                pull_request.repo.owner.to_lowercase(),
                pull_request.repo.name.to_lowercase(),
                pull_request.number
            ),
        };
        ReviewKey(key.into())
    }

    pub fn empty_label(&self) -> SharedString {
        match self {
            ReviewTarget::Uncommitted => "No uncommitted changes".into(),
            ReviewTarget::Branch { base } => format!("No changes against {base}").into(),
            ReviewTarget::Commits(_) => "No changes in these commits".into(),
            ReviewTarget::PullRequest(_) => "No changes in this pull request".into(),
        }
    }
}

/// A pull request as the user named it: `123`, `#123`, `pr:123`, `owner/repo#123` or its URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequestReference {
    /// The repository, when the reference names one; otherwise the checkout's pull requests.
    pub repo: Option<ForgeRepoRef>,
    pub number: u64,
}

impl fmt::Display for PullRequestReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.repo {
            Some(repo) => write!(formatter, "{repo}#{}", self.number),
            None => write!(formatter, "#{}", self.number),
        }
    }
}

/// Parses `123`, `#123`, `pr:123`, `owner/repo#123` and `https://host/owner/repo/pull/123[/…]`.
pub fn parse_pull_request_reference(input: &str) -> Result<PullRequestReference> {
    let input = input.trim();
    let invalid = || {
        anyhow::anyhow!(
            "{input:?} isn't a pull request: use a number, owner/repo#123 or a pull request URL"
        )
    };
    if input.starts_with("https://") || input.starts_with("http://") {
        let url = Url::parse(input).map_err(|_| invalid())?;
        let host = url.host_str().ok_or_else(invalid)?.to_ascii_lowercase();
        // An Enterprise Server's web server (and API) may be on another port.
        let host = match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host,
        };
        let mut segments = url
            .path_segments()
            .ok_or_else(invalid)?
            .filter(|segment| !segment.is_empty());
        let (Some(owner), Some(name), Some("pull"), Some(number)) = (
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
        ) else {
            return Err(invalid());
        };
        return Ok(PullRequestReference {
            repo: Some(repo_reference(Some(host), owner, name).ok_or_else(invalid)?),
            number: parse_pull_request_number(number).ok_or_else(invalid)?,
        });
    }
    if let Some(number) = input
        .strip_prefix("pr:")
        .or_else(|| input.strip_prefix('#'))
    {
        return Ok(PullRequestReference {
            repo: None,
            number: parse_pull_request_number(number).ok_or_else(invalid)?,
        });
    }
    if let Some((repo, number)) = input.split_once('#') {
        let (owner, name) = repo.split_once('/').ok_or_else(invalid)?;
        return Ok(PullRequestReference {
            repo: Some(repo_reference(None, owner, name).ok_or_else(invalid)?),
            number: parse_pull_request_number(number).ok_or_else(invalid)?,
        });
    }
    Ok(PullRequestReference {
        repo: None,
        number: parse_pull_request_number(input).ok_or_else(invalid)?,
    })
}

/// A positive number in plain digits (`u64::from_str` would also take a `+`).
fn parse_pull_request_number(number: &str) -> Option<u64> {
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    number.parse().ok().filter(|number| *number > 0)
}

fn repo_reference(host: Option<String>, owner: &str, name: &str) -> Option<ForgeRepoRef> {
    let is_valid = |part: &str| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character))
    };
    let name = name.strip_suffix(".git").unwrap_or(name);
    (is_valid(owner) && is_valid(name)).then(|| ForgeRepoRef {
        host: host.map(SharedString::from),
        owner: owner.to_string().into(),
        name: name.to_string().into(),
    })
}

/// Checks a branch name that comes from a forge (a pull request's head or base branch) before it
/// reaches git: git's `check-ref-format --branch` rules, and nothing git could read as an option
/// or as one of its own refs (`origin/HEAD` is a symbolic ref to the default branch).
pub(crate) fn validate_branch_name(name: &str) -> Result<()> {
    let is_pseudo_ref = name.ends_with("HEAD")
        && name
            .chars()
            .all(|character| character.is_ascii_uppercase() || character == '_');
    let is_valid = is_plain_revision(name)
        && !is_pseudo_ref
        && name != "@"
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.ends_with('.')
        && !name.contains("..")
        && !name.contains("//")
        && !name.contains("@{")
        && !name.chars().any(|character| "~^:?*[\\".contains(character))
        && name
            .split('/')
            .all(|component| !component.starts_with('.') && !component.ends_with(".lock"));
    if !is_valid {
        bail!("Invalid branch name {name:?}");
    }
    Ok(())
}

/// A commit or commit range as typed by the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CommitSpec {
    Single(String),
    /// git's two-dot form `from..to`: the commits reachable from `to` but not from `from`.
    Range {
        from: String,
        to: String,
    },
}

/// Parses "abc1234", "HEAD~2" or "A..B".
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
    if !is_plain_revision(revision) {
        bail!("Invalid revision {revision:?}");
    }
    Ok(revision.to_string())
}

/// A review target as named on the command line (`zed --review TARGET`) or in a `zed://review`
/// link, before anything in the repository is resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TargetSpec {
    Uncommitted,
    /// `None`: the repository's default branch.
    Branch {
        base: Option<BranchBase>,
    },
    Commits(CommitSpec),
    PullRequest(PullRequestReference),
}

const MAX_TARGET_LENGTH: usize = 1024;

/// Parses `uncommitted`, `branch`, `branch:BASE`, `commit:REV`, `commits:A..B`, `pr:N`,
/// `OWNER/REPO#N` and pull request URLs.
pub(crate) fn parse_target_spec(input: &str) -> Result<TargetSpec> {
    let input = input.trim();
    if input.len() > MAX_TARGET_LENGTH {
        bail!("The review target is longer than {MAX_TARGET_LENGTH} bytes");
    }
    if input == "uncommitted" {
        return Ok(TargetSpec::Uncommitted);
    }
    if input == "branch" {
        return Ok(TargetSpec::Branch { base: None });
    }
    if let Some(base) = input.strip_prefix("branch:") {
        return Ok(TargetSpec::Branch {
            base: Some(BranchBase::new(base.to_string())?),
        });
    }
    if let Some(commit) = input.strip_prefix("commit:") {
        return match parse_commit_spec(commit)? {
            spec @ CommitSpec::Single(_) => Ok(TargetSpec::Commits(spec)),
            CommitSpec::Range { .. } => bail!("commit: takes one commit; use commits:A..B"),
        };
    }
    if let Some(commits) = input.strip_prefix("commits:") {
        return match parse_commit_spec(commits)? {
            spec @ CommitSpec::Range { .. } => Ok(TargetSpec::Commits(spec)),
            CommitSpec::Single(_) => bail!("commits: takes a range A..B; use commit:REV"),
        };
    }
    // A short commit sha can be all digits too, so a bare number is neither.
    if !input.is_empty() && input.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!(
            "{input:?} isn't a review target: did you mean pr:{input}? (A commit is commit:{input}.)"
        );
    }
    let is_pull_request = input.starts_with("pr:")
        || input.starts_with("https://")
        || input.starts_with("http://")
        || input.contains('#');
    if is_pull_request {
        return parse_pull_request_reference(input)
            .map(TargetSpec::PullRequest)
            .map_err(|_| {
                anyhow::anyhow!(
                    "{input:?} isn't a pull request: use pr:N, OWNER/REPO#N or a pull request URL"
                )
            });
    }
    bail!(
        "Unknown review target {input:?}. Use uncommitted, branch, branch:BASE, commit:REV, \
         commits:A..B, pr:N, OWNER/REPO#N or a pull request URL."
    )
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
        for invalid in [
            "", "  ", "-x", "A..-x", "A...B", "a b", "a\nb", "a\0b", "..B", "A..", "..",
        ] {
            assert!(
                parse_commit_spec(invalid).is_err(),
                "{invalid:?} should be rejected"
            );
        }
    }

    #[test]
    fn test_parse_pull_request_reference() {
        let parse = |input: &str| parse_pull_request_reference(input).unwrap();
        let in_repo = |host: Option<&str>, owner: &str, name: &str, number| PullRequestReference {
            repo: Some(ForgeRepoRef {
                host: host.map(|host| host.to_string().into()),
                owner: owner.to_string().into(),
                name: name.to_string().into(),
            }),
            number,
        };
        let here = |number| PullRequestReference { repo: None, number };
        assert_eq!(parse("123"), here(123));
        assert_eq!(parse(" #7 "), here(7));
        assert_eq!(parse("pr:42"), here(42));
        assert_eq!(
            parse("zed-industries/zed#41"),
            in_repo(None, "zed-industries", "zed", 41)
        );
        assert_eq!(
            parse("https://github.com/drew-council/code-review-sandbox/pull/2"),
            in_repo(Some("github.com"), "drew-council", "code-review-sandbox", 2)
        );
        assert_eq!(
            parse("https://GitHub.com/o/r.js/pull/9/files?diff=split#r123"),
            in_repo(Some("github.com"), "o", "r.js", 9)
        );
        assert_eq!(
            parse("http://ghe.example.com/team/app/pull/3/"),
            in_repo(Some("ghe.example.com"), "team", "app", 3)
        );
        assert_eq!(
            parse("https://ghe.example.com:8443/team/app/pull/3"),
            in_repo(Some("ghe.example.com:8443"), "team", "app", 3)
        );
        assert_eq!(
            parse("https://github.com:443/o/r/pull/3"),
            in_repo(Some("github.com"), "o", "r", 3)
        );
        assert_eq!(parse(&format!("#{}", u64::MAX)), here(u64::MAX));
        assert_eq!(parse("#1").to_string(), "#1");
        assert_eq!(parse("o/r#1").to_string(), "o/r#1");

        for invalid in [
            "",
            "#",
            "0",
            "#0",
            "pr:",
            "+5",
            "-5",
            "1.5",
            "12a",
            "abc",
            "18446744073709551616",
            "o/r#",
            "o#1",
            "o/r/x#1",
            "/r#1",
            "o/../r#1",
            "o/r#1#2",
            "pr:#1",
            "https://github.com/o/r/issues/1",
            "https://github.com/o/r/pull/",
            "https://github.com/o/r/pull/x",
            "https://github.com/o/pull/1",
            "ftp://github.com/o/r/pull/1",
        ] {
            assert!(
                parse_pull_request_reference(invalid).is_err(),
                "{invalid:?} should be rejected"
            );
        }
    }

    #[test]
    fn test_validate_branch_name() {
        for valid in [
            "main",
            "feature/template-filters",
            "fix/truncate-ellipsis",
            "release-1.0",
            "user@host",
            "a.b",
            "ünïcode",
            "x+y",
            "HEADS",
            "fix-HEAD",
            "feature/HEAD",
        ] {
            assert!(validate_branch_name(valid).is_ok(), "{valid:?} should pass");
        }
        for invalid in [
            "",
            "HEAD",
            "FETCH_HEAD",
            "ORIG_HEAD",
            "-x",
            "--upload-pack=x",
            "@",
            "a b",
            "a\tb",
            "a\nb",
            "a\u{7f}",
            "/a",
            "a/",
            "a//b",
            "a..b",
            "a.",
            ".a",
            "a/.b",
            "a.lock",
            "a.lock/b",
            "a@{1}",
            "a~1",
            "a^",
            "a:b",
            "a?",
            "a*",
            "a[b",
            "a\\b",
        ] {
            assert!(
                validate_branch_name(invalid).is_err(),
                "{invalid:?} should be rejected"
            );
        }
    }

    #[test]
    fn test_parse_target_spec() {
        let parse = |input: &str| parse_target_spec(input).unwrap();
        let error = |input: &str| parse_target_spec(input).unwrap_err().to_string();
        let base = |name: &str| Some(BranchBase::new(name).unwrap());
        assert_eq!(parse("uncommitted"), TargetSpec::Uncommitted);
        assert_eq!(parse(" uncommitted\n"), TargetSpec::Uncommitted);
        assert_eq!(parse("branch"), TargetSpec::Branch { base: None });
        assert_eq!(
            parse("branch:origin/main"),
            TargetSpec::Branch {
                base: base("origin/main")
            }
        );
        assert_eq!(
            parse("commit:HEAD~1"),
            TargetSpec::Commits(CommitSpec::Single("HEAD~1".into()))
        );
        assert_eq!(
            parse("commits:HEAD~2..HEAD"),
            TargetSpec::Commits(CommitSpec::Range {
                from: "HEAD~2".into(),
                to: "HEAD".into()
            })
        );
        let pull_request =
            |input: &str| TargetSpec::PullRequest(parse_pull_request_reference(input).unwrap());
        for input in [
            "pr:6",
            "#6",
            "drew-council/code-review-sandbox#6",
            "https://github.com/drew-council/code-review-sandbox/pull/6",
            "https://github.com/o/r/pull/6/files?diff=split#r1",
        ] {
            assert_eq!(parse(input), pull_request(input), "{input}");
        }

        assert_eq!(error("branch:"), "Invalid base \"\"");
        assert_eq!(error("branch:-x"), "Invalid base \"-x\"");
        assert_eq!(error("branch:a b"), "Invalid base \"a b\"");
        assert_eq!(
            error("commit:a..b"),
            "commit: takes one commit; use commits:A..B"
        );
        assert_eq!(
            error("commits:a"),
            "commits: takes a range A..B; use commit:REV"
        );
        assert_eq!(
            error("commits:a...b"),
            "Use A..B for a commit range; A...B is not supported"
        );
        assert_eq!(error("commit:"), "No commit given");
        assert_eq!(
            error("123"),
            "\"123\" isn't a review target: did you mean pr:123? (A commit is commit:123.)"
        );
        for invalid in [
            "pr:0",
            "pr:",
            "pr:x",
            "o#1",
            "https://github.com/o/r/issues/1",
        ] {
            assert_eq!(
                error(invalid),
                format!(
                    "{invalid:?} isn't a pull request: use pr:N, OWNER/REPO#N or a pull request URL"
                )
            );
        }
        for unknown in ["", "bogus", "Uncommitted", "branches", "HEAD"] {
            assert!(
                error(unknown).starts_with("Unknown review target"),
                "{unknown:?}: {}",
                error(unknown)
            );
        }
        assert_eq!(
            error(&format!("branch:{}", "a".repeat(1018))),
            "The review target is longer than 1024 bytes"
        );
        assert!(parse_target_spec(&format!("branch:{}", "a".repeat(1017))).is_ok());
    }

    #[test]
    fn test_branch_base() {
        for valid in ["origin/main", "main", "HEAD~2", "v1.0^{commit}"] {
            assert_eq!(BranchBase::new(valid).unwrap().name(), valid);
        }
        for invalid in ["", "-x", "--output=/tmp/x", "a b", "main\n", "a\tb", "a\0b"] {
            assert!(
                BranchBase::new(invalid).is_err(),
                "{invalid:?} should be rejected"
            );
        }
    }
}
