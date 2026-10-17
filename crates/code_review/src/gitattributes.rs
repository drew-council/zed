//! The `.gitattributes` that decide whether a file is in a review by default: GitHub's and
//! GitLab's `linguist-generated` / `gitlab-generated` and `linguist-vendored`, which collapse a
//! file in a pull request diff.
//!
//! Resolution follows git (and tuicr): `$GIT_COMMON_DIR/info/attributes` wins, then the
//! `.gitattributes` closest to the file, up to the repository root; within one file the last
//! matching line wins. Patterns use gitignore syntax without negation and without the
//! recursive-directory rule, which is what `Gitignore::matched` (as opposed to
//! `matched_path_or_any_parents`) implements. Unsetting lines (`-attr`, `!attr`, `attr=false`)
//! become whitelist globs, so "last match wins" is the `ignore` crate's own ordering.

use collections::{BTreeSet, HashMap};
use fs::Fs;
use git::repository::RepoPath;
use ignore::{
    Match,
    gitignore::{Gitignore, GitignoreBuilder},
};
use std::path::Path;
use util::ResultExt as _;

/// The review-relevant attributes of one file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FileAttributes {
    pub generated: bool,
    pub vendored: bool,
}

const GENERATED_ATTRIBUTES: &[&str] = &["linguist-generated", "gitlab-generated"];
const VENDORED_ATTRIBUTES: &[&str] = &["linguist-vendored"];
const ATTRIBUTES: &[&str] = &[
    "linguist-generated",
    "gitlab-generated",
    "linguist-vendored",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttributeState {
    Set,
    Unset,
}

/// One attributes file, compiled into a matcher per attribute.
struct Layer {
    /// The directory its patterns are relative to, relative to the repository root ("" for the
    /// root and for `info/attributes`).
    directory: String,
    matchers: Vec<(&'static str, Gitignore)>,
}

impl Layer {
    fn parse(directory: &str, contents: &str) -> Option<Self> {
        // Lookups pass paths relative to `directory`, so the matcher needs no real root; `.` is
        // the one root the `ignore` crate never strips.
        let mut builders = ATTRIBUTES
            .iter()
            .map(|name| (*name, GitignoreBuilder::new(".")))
            .collect::<Vec<_>>();
        let mut any_line = false;
        for line in contents.lines() {
            let Some((pattern, assignments)) = parse_line(line) else {
                continue;
            };
            for (name, state) in assignments {
                let Some((_, builder)) = builders.iter_mut().find(|(known, _)| *known == name)
                else {
                    continue;
                };
                let glob = match state {
                    AttributeState::Set => pattern.clone(),
                    AttributeState::Unset => format!("!{pattern}"),
                };
                if builder.add_line(None, &glob).is_ok() {
                    any_line = true;
                }
            }
        }
        if !any_line {
            return None;
        }
        let matchers = builders
            .into_iter()
            .filter_map(|(name, builder)| Some((name, builder.build().log_err()?)))
            .collect();
        Some(Self {
            directory: directory.to_string(),
            matchers,
        })
    }

    /// `relative_path` is relative to this layer's directory.
    fn resolve(&self, attribute: &str, relative_path: &str) -> Option<AttributeState> {
        let (_, matcher) = self.matchers.iter().find(|(name, _)| *name == attribute)?;
        match matcher.matched(Path::new(relative_path), false) {
            Match::Ignore(_) => Some(AttributeState::Set),
            Match::Whitelist(_) => Some(AttributeState::Unset),
            Match::None => None,
        }
    }
}

/// The pattern and the tracked attribute assignments of a line; `None` for blank lines,
/// comments and `[attr]` macro definitions.
fn parse_line(line: &str) -> Option<(String, Vec<(String, AttributeState)>)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') || line.starts_with("[attr]") {
        return None;
    }
    let (pattern, rest) = split_pattern(line)?;
    // Negative patterns are forbidden in gitattributes; the `ignore` crate would read the `!`
    // as a whitelist and invert the meaning.
    if pattern.is_empty() || pattern.starts_with('!') {
        return None;
    }
    let assignments = rest
        .split_whitespace()
        .map(|token| {
            if let Some(name) = token.strip_prefix('-') {
                (name.to_string(), AttributeState::Unset)
            } else if let Some(name) = token.strip_prefix('!') {
                // "Unspecified" overrides lower-precedence lines like an unset does.
                (name.to_string(), AttributeState::Unset)
            } else if let Some((name, value)) = token.split_once('=') {
                let state = if value.eq_ignore_ascii_case("false") {
                    AttributeState::Unset
                } else {
                    AttributeState::Set
                };
                (name.to_string(), state)
            } else {
                (token.to_string(), AttributeState::Set)
            }
        })
        .collect();
    Some((pattern, assignments))
}

/// Splits the pattern off a line. Patterns with whitespace are double-quoted, with `\"` and `\\`
/// escapes.
fn split_pattern(line: &str) -> Option<(String, &str)> {
    if let Some(quoted) = line.strip_prefix('"') {
        let mut pattern = String::new();
        let mut chars = quoted.char_indices();
        while let Some((index, character)) = chars.next() {
            match character {
                '\\' => {
                    if let Some((_, escaped)) = chars.next() {
                        pattern.push(escaped);
                    }
                }
                '"' => return Some((pattern, &quoted[index + 1..])),
                _ => pattern.push(character),
            }
        }
        // Git rejects an unterminated quote.
        return None;
    }
    let end = line.find(char::is_whitespace).unwrap_or(line.len());
    Some((line[..end].to_string(), &line[end..]))
}

/// The attribute files that apply to a set of paths.
#[derive(Default)]
pub(crate) struct GitAttributes {
    info: Option<Layer>,
    /// `.gitattributes` files by directory relative to the root ("" for the root).
    directories: HashMap<String, Layer>,
}

impl GitAttributes {
    /// `files` maps directories (relative to the root, "" for the root) to the contents of their
    /// `.gitattributes`.
    pub(crate) fn parse<'a>(
        info: Option<&str>,
        files: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Self {
        Self {
            info: info.and_then(|contents| Layer::parse("", contents)),
            directories: files
                .into_iter()
                .filter_map(|(directory, contents)| {
                    Some((directory.to_string(), Layer::parse(directory, contents)?))
                })
                .collect(),
        }
    }

    /// Reads `info/attributes` and the `.gitattributes` of every directory above `paths` in the
    /// working tree.
    pub(crate) async fn load(
        fs: &dyn Fs,
        work_directory: &Path,
        common_directory: &Path,
        paths: &[RepoPath],
    ) -> Self {
        let info = read_file(fs, &common_directory.join("info").join("attributes")).await;
        let directories = paths
            .iter()
            .flat_map(|path| ancestors(path.as_unix_str()))
            .collect::<BTreeSet<_>>();
        let mut files = Vec::new();
        for directory in directories {
            let path = if directory.is_empty() {
                work_directory.join(".gitattributes")
            } else {
                work_directory.join(&directory).join(".gitattributes")
            };
            if let Some(contents) = read_file(fs, &path).await {
                files.push((directory, contents));
            }
        }
        Self::parse(
            info.as_deref(),
            files
                .iter()
                .map(|(directory, contents)| (directory.as_str(), contents.as_str())),
        )
    }

    pub(crate) fn attributes(&self, path: &RepoPath) -> FileAttributes {
        let path = path.as_unix_str();
        FileAttributes {
            generated: GENERATED_ATTRIBUTES
                .iter()
                .any(|attribute| self.resolve(attribute, path) == Some(AttributeState::Set)),
            vendored: VENDORED_ATTRIBUTES
                .iter()
                .any(|attribute| self.resolve(attribute, path) == Some(AttributeState::Set)),
        }
    }

    /// The first definitive answer, from the highest-precedence layer down.
    fn resolve(&self, attribute: &str, path: &str) -> Option<AttributeState> {
        if let Some(state) = self
            .info
            .as_ref()
            .and_then(|layer| layer.resolve(attribute, path))
        {
            return Some(state);
        }
        for directory in ancestors(path) {
            let Some(layer) = self.directories.get(&directory) else {
                continue;
            };
            let relative_path = if layer.directory.is_empty() {
                path
            } else {
                path.strip_prefix(&layer.directory)
                    .and_then(|rest| rest.strip_prefix('/'))
                    .unwrap_or(path)
            };
            if let Some(state) = layer.resolve(attribute, relative_path) {
                return Some(state);
            }
        }
        None
    }
}

/// The directories containing `path`, closest first, ending with the root ("").
fn ancestors(path: &str) -> Vec<String> {
    let mut directories = Vec::new();
    let mut rest = path;
    while let Some((parent, _)) = rest.rsplit_once('/') {
        directories.push(parent.to_string());
        rest = parent;
    }
    directories.push(String::new());
    directories
}

async fn read_file(fs: &dyn Fs, path: &Path) -> Option<String> {
    if !fs.is_file(path).await {
        return None;
    }
    fs.load(path).await.log_err()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use serde_json::json;
    use util::path;

    fn attributes(attributes: &GitAttributes, path: &str) -> FileAttributes {
        attributes.attributes(&RepoPath::new(path).unwrap())
    }

    const GENERATED: FileAttributes = FileAttributes {
        generated: true,
        vendored: false,
    };
    const VENDORED: FileAttributes = FileAttributes {
        generated: false,
        vendored: true,
    };
    const NONE: FileAttributes = FileAttributes {
        generated: false,
        vendored: false,
    };

    #[test]
    fn test_patterns_and_assignments() {
        let parsed = GitAttributes::parse(
            None,
            [(
                "",
                "# comment\n\
                 [attr]binary -diff -merge -text\n\
                 *.lock linguist-generated\n\
                 dist/** gitlab-generated=true\n\
                 vendor/** linguist-vendored\n\
                 vendor/ours.js -linguist-vendored\n\
                 \"with space.txt\" linguist-generated\n\
                 keep.lock linguist-generated=false\n\
                 maybe.lock !linguist-generated\n\
                 !negated.txt linguist-generated\n",
            )],
        );
        assert_eq!(attributes(&parsed, "Cargo.lock"), GENERATED);
        assert_eq!(attributes(&parsed, "nested/deep/yarn.lock"), GENERATED);
        assert_eq!(attributes(&parsed, "dist/app/main.js"), GENERATED);
        assert_eq!(attributes(&parsed, "vendor/x.js"), VENDORED);
        assert_eq!(attributes(&parsed, "vendor/ours.js"), NONE);
        assert_eq!(attributes(&parsed, "with space.txt"), GENERATED);
        assert_eq!(attributes(&parsed, "keep.lock"), NONE);
        assert_eq!(attributes(&parsed, "maybe.lock"), NONE);
        assert_eq!(attributes(&parsed, "negated.txt"), NONE);
        assert_eq!(attributes(&parsed, "src/main.rs"), NONE);
    }

    #[test]
    fn test_precedence() {
        let parsed = GitAttributes::parse(
            Some("local.txt linguist-generated\nsrc/kept.rs -linguist-vendored\n"),
            [
                ("", "*.txt linguist-vendored\nsrc/** linguist-vendored\n"),
                (
                    "src",
                    "gen.rs linguist-generated\n*.txt -linguist-vendored\n",
                ),
                ("src/inner", "gen.rs -linguist-generated\n"),
            ],
        );
        // The closest file wins; `info/attributes` beats them all.
        assert_eq!(attributes(&parsed, "notes.txt"), VENDORED);
        assert_eq!(attributes(&parsed, "src/notes.txt"), NONE);
        assert_eq!(
            attributes(&parsed, "src/gen.rs"),
            FileAttributes {
                generated: true,
                vendored: true
            }
        );
        assert_eq!(attributes(&parsed, "src/inner/gen.rs"), VENDORED);
        assert_eq!(attributes(&parsed, "src/kept.rs"), NONE);
        assert_eq!(
            attributes(&parsed, "local.txt"),
            FileAttributes {
                generated: true,
                vendored: true
            }
        );
    }

    #[gpui::test]
    async fn test_load_from_fs(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": { "info": { "attributes": "local.rs linguist-generated\n" } },
                ".gitattributes": "Cargo.lock linguist-generated\n",
                "src": {
                    ".gitattributes": "generated/** linguist-generated\n",
                    "generated": { "api.rs": "" },
                    "main.rs": "",
                },
                "third_party": { ".gitattributes": "* linguist-vendored\n", "lib.c": "" },
            }),
        )
        .await;
        let paths = [
            "Cargo.lock",
            "src/generated/api.rs",
            "src/main.rs",
            "third_party/lib.c",
            "local.rs",
        ]
        .map(|path| RepoPath::new(path).unwrap());
        let loaded = GitAttributes::load(
            fs.as_ref(),
            Path::new(path!("/project")),
            Path::new(path!("/project/.git")),
            &paths,
        )
        .await;
        assert_eq!(attributes(&loaded, "Cargo.lock"), GENERATED);
        assert_eq!(attributes(&loaded, "src/generated/api.rs"), GENERATED);
        assert_eq!(attributes(&loaded, "src/main.rs"), NONE);
        assert_eq!(attributes(&loaded, "third_party/lib.c"), VENDORED);
        assert_eq!(attributes(&loaded, "local.rs"), GENERATED);
    }
}
