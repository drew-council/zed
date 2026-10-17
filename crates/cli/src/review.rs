//! `zed --review TARGET [REPO_DIR]`, sent to Zed as a `zed://review` link plus the repository's
//! path (Zed checks the target and the repository).

use anyhow::{Context as _, Result, anyhow, bail};
use std::{io, path::Path};

pub struct ReviewRequest {
    pub url: String,
    pub repo_dir: String,
}

/// Whether the command line asks for a review, also when clap couldn't parse it.
pub fn requested() -> bool {
    std::env::args_os()
        .skip(1)
        .any(|argument| is_review_flag(&argument.to_string_lossy()))
}

fn is_review_flag(argument: &str) -> bool {
    argument == "--review" || argument.starts_with("--review=")
}

/// The review `--review TARGET` asks for, in the directory `paths` names (default: the current
/// one). Clap passes everything after the first path through as a path, `--review` too.
pub fn request(target: Option<&str>, paths: &[String]) -> Result<Option<ReviewRequest>> {
    if paths.iter().any(|path| is_review_flag(path)) {
        bail!("--review must come before the repository path");
    }
    let Some(target) = target else {
        return Ok(None);
    };
    if target.trim().is_empty() {
        bail!(
            "--review needs a TARGET: uncommitted, branch, branch:BASE, commit:REV, \
             commits:A..B, pr:N, OWNER/REPO#N or a pull request URL"
        );
    }
    if let Some(option) = paths.iter().skip(1).find(|path| path.starts_with('-')) {
        bail!("Options must come before the repository path (got {option})");
    }
    let repo_dir = match paths {
        [] => std::env::current_dir().context("Couldn't read the current directory")?,
        [path] => Path::new(path).to_path_buf(),
        _ => bail!("--review takes one repository path"),
    };
    let repo_dir = repo_dir
        .canonicalize()
        .map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => anyhow!("{} doesn't exist", repo_dir.display()),
            _ => anyhow!("{}: {error}", repo_dir.display()),
        })?;
    if !repo_dir.is_dir() {
        bail!("{} isn't a directory", repo_dir.display());
    }
    let repo_dir = repo_dir
        .to_str()
        .with_context(|| format!("{} isn't valid UTF-8", repo_dir.display()))?
        .to_string();
    Ok(Some(ReviewRequest {
        url: review_url(&repo_dir, target),
        repo_dir,
    }))
}

/// Zed answers a `--review` request with a ping before anything else. A Zed that doesn't know
/// `--review` (still running after an update) takes the link for an unknown URL and exits with
/// 0 at once, which would look like a submitted review.
pub fn check_exit(pinged: bool, status: i32) -> Result<()> {
    if !pinged && status == 0 {
        bail!("This Zed doesn't support --review: restart Zed after updating it");
    }
    Ok(())
}

fn review_url(repo_dir: &str, target: &str) -> String {
    format!(
        "zed://review?repo={}&target={}",
        encode(repo_dir),
        encode(target)
    )
}

/// Percent-encodes every byte except unreserved characters and `/`.
fn encode(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_review_url() {
        assert_eq!(
            review_url("/home/me/repo two", "branch:origin/main"),
            "zed://review?repo=/home/me/repo%20two&target=branch%3Aorigin/main"
        );
        assert_eq!(
            review_url("/r", "o/r#1"),
            "zed://review?repo=/r&target=o/r%231"
        );
        assert_eq!(encode("a+b&c=d?é"), "a%2Bb%26c%3Dd%3F%C3%A9");
        assert_eq!(encode("A-z_0.9~"), "A-z_0.9~");
    }

    #[test]
    fn test_request() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().canonicalize().unwrap();
        let path = path.to_str().unwrap().to_string();

        assert!(request(None, &[path.clone()]).unwrap().is_none());
        let review = request(Some("uncommitted"), &[path.clone()])
            .unwrap()
            .unwrap();
        assert_eq!(review.repo_dir, path);
        assert_eq!(
            review.url,
            format!("zed://review?repo={}&target=uncommitted", encode(&path))
        );

        let error = |target: Option<&str>, paths: &[&str]| {
            let paths: Vec<String> = paths.iter().map(|path| path.to_string()).collect();
            request(target, &paths).err().unwrap().to_string()
        };
        assert_eq!(
            error(None, &[&path, "--review", "uncommitted"]),
            "--review must come before the repository path"
        );
        assert_eq!(
            error(None, &[&path, "--review=uncommitted"]),
            "--review must come before the repository path"
        );
        assert_eq!(
            error(Some("uncommitted"), &[&path, &path]),
            "--review takes one repository path"
        );
        assert_eq!(
            error(Some("pr:1"), &[&path, "--wait"]),
            "Options must come before the repository path (got --wait)"
        );
        assert_eq!(
            error(Some("uncommitted"), &[&path, "-n"]),
            "Options must come before the repository path (got -n)"
        );
        for blank in ["", "  "] {
            assert!(
                error(Some(blank), &[&path]).starts_with("--review needs a TARGET: uncommitted"),
                "{blank:?}"
            );
        }
        let missing = format!("{path}/missing");
        assert_eq!(
            error(Some("uncommitted"), &[&missing]),
            format!("{missing} doesn't exist")
        );
        let file = format!("{path}/file");
        std::fs::write(&file, "").unwrap();
        assert_eq!(
            error(Some("uncommitted"), &[&file]),
            format!("{file} isn't a directory")
        );
        // A file in the way of a path's directory.
        let below_file = format!("{file}/x");
        assert!(
            error(Some("uncommitted"), &[&below_file]).starts_with(&format!("{below_file}: ")),
            "{}",
            error(Some("uncommitted"), &[&below_file])
        );
    }

    #[test]
    fn test_check_exit() {
        assert!(check_exit(true, 0).is_ok());
        assert!(check_exit(true, 2).is_ok());
        assert!(check_exit(false, 1).is_ok());
        assert_eq!(
            check_exit(false, 0).unwrap_err().to_string(),
            "This Zed doesn't support --review: restart Zed after updating it"
        );
    }
}
