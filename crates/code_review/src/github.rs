//! GitHub's pull request API: authentication, GraphQL and REST requests over Zed's
//! [`HttpClient`], and parsing into the types of [`crate::forge`].

use crate::forge::{
    Actor, ChangedFile, ChangedFileStatus, CommentableLines, DiffSide, Forge, ForgeRepo,
    ForgeRepoRef, HeadStatus, Hunk, HunkRowKind, HunkRows, PendingReview, PullRequestDetails,
    PullRequestListFilter, PullRequestStatus, PullRequestSummary, RemoteComment,
    RemoteReviewSummary, RemoteThread, RepositoryContext, ReviewDecision, ReviewState,
    ThreadAnchor,
};
use anyhow::anyhow;
use collections::HashMap;
use futures::{
    AsyncReadExt as _, FutureExt as _,
    future::{BoxFuture, Either, Shared},
};
use git::{GitHostingProviderRegistry, Oid, RemoteUrl};
use gpui::{App, AppContext as _, BackgroundExecutor, Entity, Global, SharedString, Task};
use http_client::{
    AsyncBody, HttpClient, HttpRequestExt as _, Method, Request, StatusCode,
    http::{HeaderMap, HeaderValue},
};
use project::{Project, git_store::Repository};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::json;
use std::{
    fmt,
    future::Future,
    io,
    ops::Range,
    path::Path,
    pin::pin,
    str::FromStr as _,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use time::OffsetDateTime;

/// Every GraphQL operation, sent whole with an `operationName`.
const QUERIES: &str = include_str!("github/queries.graphql");
const API_VERSION: &str = "2022-11-28";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const GH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_GRAPHQL_PAGES: usize = 20;
/// GitHub lists at most 3000 files of a pull request.
const MAX_FILE_PAGES: u32 = 30;
const FILES_PER_PAGE: usize = 100;

/// Why a GitHub request failed. `Display` is [`GitHubError::hint`].
#[derive(Clone, Debug)]
pub enum GitHubError {
    /// The repository has no GitHub remote.
    Unsupported,
    /// A pull request reference names a repository this checkout doesn't review.
    OtherRepository {
        repo: SharedString,
    },
    Auth {
        host: SharedString,
        failure: AuthFailure,
    },
    /// `reset`: when the limit resets, in Unix seconds.
    RateLimited {
        reset: Option<i64>,
    },
    /// The organization requires its single sign-on to authorize the token.
    Sso {
        url: Option<SharedString>,
    },
    Forbidden {
        what: SharedString,
        message: SharedString,
    },
    NotFound {
        what: SharedString,
    },
    /// A 422: GitHub's `errors` (strings, or the objects' `message`s; an empty string is kept,
    /// GitHub sends one when a review needs a body) and its `message`.
    Validation {
        messages: Vec<String>,
        raw_message: String,
    },
    GraphQl {
        messages: Vec<String>,
    },
    /// The request failed, GitHub answered with an unexpected status, or the response wasn't what
    /// GitHub sends.
    Transport(Arc<anyhow::Error>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthFailure {
    /// GitHub rejected the token (401).
    Rejected,
    /// Neither `GITHUB_TOKEN` nor `gh auth token` gave a token.
    NoToken,
    /// No `GITHUB_TOKEN`, and the GitHub CLI isn't installed.
    NoTokenNoCli,
}

impl GitHubError {
    /// What went wrong and what to do about it, for the user.
    pub fn hint(&self) -> SharedString {
        match self {
            GitHubError::Unsupported => "This repository has no GitHub remote.".into(),
            GitHubError::OtherRepository { repo } => {
                format!("Open this pull request from a checkout of {repo}.").into()
            }
            GitHubError::Auth { host, failure } => match failure {
                AuthFailure::Rejected => format!(
                    "GitHub authentication failed for {host}. Set GITHUB_TOKEN or run \
                     `gh auth login --hostname {host}`."
                ),
                AuthFailure::NoToken => format!(
                    "No GitHub token for {host}. Set GITHUB_TOKEN or run \
                     `gh auth login --hostname {host}`."
                ),
                AuthFailure::NoTokenNoCli => format!(
                    "No GitHub token for {host}. Set GITHUB_TOKEN, or install the GitHub CLI \
                     and run `gh auth login --hostname {host}`."
                ),
            }
            .into(),
            GitHubError::RateLimited { reset } => {
                rate_limit_hint(*reset, OffsetDateTime::now_utc().unix_timestamp()).into()
            }
            GitHubError::Sso { url: Some(url) } => {
                format!("Authorize your GitHub token for this organization's single sign-on: {url}")
                    .into()
            }
            GitHubError::Sso { url: None } => {
                "Authorize your GitHub token for this organization's single sign-on.".into()
            }
            GitHubError::Forbidden { what, message } => {
                format!("GitHub denied access to {what}: {message}").into()
            }
            GitHubError::NotFound { what } => format!(
                "{what} not found on GitHub, or your token can't see it (private repositories \
                 need the `repo` scope)."
            )
            .into(),
            GitHubError::Validation {
                messages,
                raw_message,
            } => validation_hint(messages, raw_message).into(),
            GitHubError::GraphQl { messages } => {
                format!("GitHub API error: {}", messages.join("; ")).into()
            }
            GitHubError::Transport(error) => format!("GitHub request failed: {error:#}").into(),
        }
    }
}

impl fmt::Display for GitHubError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.hint())
    }
}

impl std::error::Error for GitHubError {}

fn transport(error: impl Into<anyhow::Error>) -> GitHubError {
    GitHubError::Transport(Arc::new(error.into()))
}

fn rate_limit_hint(reset: Option<i64>, now: i64) -> String {
    match reset {
        Some(reset) => {
            let minutes = ((reset - now).max(0) + 59) / 60;
            let unit = if minutes == 1 { "minute" } else { "minutes" };
            format!("GitHub's API rate limit is exceeded. Try again in {minutes} {unit}.")
        }
        None => "GitHub's API rate limit is exceeded. Try again later.".into(),
    }
}

/// The texts GitHub sends for a refused review (DESIGN §0, M7, "error hints").
fn validation_hint(messages: &[String], raw_message: &str) -> String {
    let mentions = |needle: &str| {
        raw_message.contains(needle) || messages.iter().any(|message| message.contains(needle))
    };
    if mentions("one pending review per pull request") {
        return "You already have a pending review on this PR. Submit or discard it on GitHub, \
                then retry."
            .into();
    }
    if mentions("could not be resolved") {
        return "GitHub rejected a comment's position (outside the PR diff). Reload the review \
                and check comments on lines outside GitHub's diff hunks."
            .into();
    }
    if mentions("is not part of the pull request") {
        return "GitHub doesn't know the reviewed commit. Reload the review (drafts are kept)."
            .into();
    }
    if let Some(message) = messages.iter().find(|message| {
        message.contains("Can not approve your own pull request")
            || message.contains("Can not request changes on your own pull request")
    }) {
        return message
            .strip_prefix("Review ")
            .unwrap_or(message)
            .to_string();
    }
    if mentions("You need to leave a comment indicating the requested changes") {
        return "Request changes needs a review comment or at least one inline comment.".into();
    }
    if messages.iter().any(|message| message.trim().is_empty()) {
        return "GitHub needs a review comment when there are no inline comments.".into();
    }
    if mentions("was provided invalid value") {
        log::error!("GitHub refused a request as invalid: {raw_message} {messages:?}");
    }
    if messages.is_empty() {
        raw_message.to_string()
    } else {
        messages.join("; ")
    }
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok().map(str::trim)
}

fn rate_limit_reset(headers: &HeaderMap) -> Option<i64> {
    if let Some(reset) = header(headers, "x-ratelimit-reset").and_then(|reset| reset.parse().ok()) {
        return Some(reset);
    }
    let retry_after: i64 = header(headers, "retry-after")?.parse().ok()?;
    Some(OffsetDateTime::now_utc().unix_timestamp() + retry_after)
}

/// `X-GitHub-SSO: required; url=https://github.com/orgs/…/sso?authorization_request=…`
fn sso_url(value: &str) -> Option<SharedString> {
    value
        .split(';')
        .find_map(|part| part.trim().strip_prefix("url="))
        .map(|url| SharedString::from(url.to_string()))
}

#[derive(Default, Deserialize)]
struct RestErrorBody {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    errors: Option<Vec<serde_json::Value>>,
}

fn status_error(
    status: StatusCode,
    headers: &HeaderMap,
    body: &[u8],
    what: &SharedString,
    host: &SharedString,
) -> GitHubError {
    let error_body: RestErrorBody = serde_json::from_slice(body).unwrap_or_default();
    let message = error_body.message.unwrap_or_default();
    match status.as_u16() {
        401 => GitHubError::Auth {
            host: host.clone(),
            failure: AuthFailure::Rejected,
        },
        403 | 429 => {
            if header(headers, "x-ratelimit-remaining") == Some("0")
                || header(headers, "retry-after").is_some()
                || message.to_ascii_lowercase().contains("rate limit")
            {
                GitHubError::RateLimited {
                    reset: rate_limit_reset(headers),
                }
            } else if let Some(sso) = header(headers, "x-github-sso") {
                GitHubError::Sso { url: sso_url(sso) }
            } else {
                GitHubError::Forbidden {
                    what: what.clone(),
                    message: message.into(),
                }
            }
        }
        404 => GitHubError::NotFound { what: what.clone() },
        422 => GitHubError::Validation {
            messages: error_body
                .errors
                .unwrap_or_default()
                .into_iter()
                .map(|error| match error {
                    serde_json::Value::String(message) => message,
                    serde_json::Value::Object(object) => match object.get("message") {
                        Some(serde_json::Value::String(message)) => message.clone(),
                        _ => serde_json::Value::Object(object).to_string(),
                    },
                    other => other.to_string(),
                })
                .collect(),
            raw_message: message,
        },
        _ => transport(anyhow!("GitHub answered {status} for {what}: {message}")),
    }
}

#[derive(Deserialize)]
struct GraphQlResponse {
    data: Option<serde_json::Value>,
    #[serde(default)]
    errors: Vec<GraphQlErrorEntry>,
}

#[derive(Deserialize)]
struct GraphQlErrorEntry {
    #[serde(default)]
    message: String,
    #[serde(rename = "type")]
    kind: Option<String>,
}

/// GraphQL answers 200 for most errors, with typed `errors`.
fn graphql_error(
    errors: Vec<GraphQlErrorEntry>,
    headers: &HeaderMap,
    what: &SharedString,
) -> GitHubError {
    let has = |kind: &str| {
        errors
            .iter()
            .any(|error| error.kind.as_deref() == Some(kind))
    };
    if has("NOT_FOUND") {
        return GitHubError::NotFound { what: what.clone() };
    }
    if has("RATE_LIMITED") {
        return GitHubError::RateLimited {
            reset: rate_limit_reset(headers),
        };
    }
    let forbidden = has("FORBIDDEN");
    let messages = errors
        .into_iter()
        .map(|error| error.message)
        .collect::<Vec<_>>();
    // Resources of an organization the token isn't authorized for come back FORBIDDEN, with an
    // `X-GitHub-SSO` header.
    if let Some(sso) = header(headers, "x-github-sso") {
        return GitHubError::Sso { url: sso_url(sso) };
    }
    if forbidden {
        return GitHubError::Forbidden {
            what: what.clone(),
            message: messages.join("; ").into(),
        };
    }
    GitHubError::GraphQl { messages }
}

/// The API base URLs of a GitHub host: github.com, GitHub Enterprise Cloud with data residency
/// (`*.ghe.com`) or GitHub Enterprise Server.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Endpoints {
    rest: String,
    graphql: String,
}

impl Endpoints {
    fn for_host(host: &str) -> Self {
        if host == "github.com" {
            Self {
                rest: "https://api.github.com".into(),
                graphql: "https://api.github.com/graphql".into(),
            }
        } else if host.ends_with(".ghe.com") {
            Self {
                rest: format!("https://api.{host}"),
                graphql: format!("https://api.{host}/graphql"),
            }
        } else {
            Self {
                rest: format!("https://{host}/api/v3"),
                graphql: format!("https://{host}/api/graphql"),
            }
        }
    }
}

/// The GitHub repository a remote URL points at, and whether the URL is an SSH one. GitHub
/// Enterprise Server hosts are recognised through the git hosting provider registry, which
/// registers the `origin` remote's host when it contains "github".
fn github_repo_for_remote(
    url: &str,
    registry: Option<&GitHostingProviderRegistry>,
) -> Option<(ForgeRepo, bool)> {
    let remote_url = RemoteUrl::from_str(url).ok()?;
    let host = remote_url.host_str()?.to_ascii_lowercase();
    if !is_github_host(&host, registry) {
        return None;
    }
    let mut segments = remote_url
        .path_segments()?
        .filter(|segment| !segment.is_empty());
    let owner = segments.next()?;
    let name = segments.next()?.trim_end_matches(".git");
    if name.is_empty() {
        return None;
    }
    let repo = ForgeRepo {
        host: host.into(),
        owner: owner.to_string().into(),
        name: name.to_string().into(),
    };
    Some((repo, remote_url.scheme() == "ssh"))
}

fn is_github_host(host: &str, registry: Option<&GitHostingProviderRegistry>) -> bool {
    host == "github.com"
        || host.ends_with(".ghe.com")
        || registry.is_some_and(|registry| {
            registry.list_hosting_providers().iter().any(|provider| {
                matches!(provider.name().as_str(), "GitHub" | "GitHub Self-Hosted")
                    && provider
                        .base_url()
                        .host_str()
                        .is_some_and(|provider_host| provider_host.eq_ignore_ascii_case(host))
            })
        })
}

/// A GitHub token. It never reaches the logs: `Debug` redacts it and requests are logged
/// without headers. Clones share whether GitHub rejected it.
#[derive(Clone)]
pub(crate) struct GitHubToken {
    value: Arc<str>,
    revoked: Arc<AtomicBool>,
}

impl GitHubToken {
    fn new(value: &str) -> Self {
        Self {
            value: value.into(),
            revoked: Arc::default(),
        }
    }

    fn revoke(&self) {
        self.revoked.store(true, Ordering::SeqCst);
    }

    fn is_revoked(&self) -> bool {
        self.revoked.load(Ordering::SeqCst)
    }
}

impl fmt::Debug for GitHubToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("GitHubToken(<redacted>)")
    }
}

#[derive(Clone, Default)]
enum TokenSource {
    /// `GITHUB_TOKEN` in Zed's environment, then in the project's (direnv), then
    /// `gh auth token --hostname <host>`.
    #[default]
    Environment,
    #[cfg(test)]
    Fake(Arc<dyn Fn(&str) -> Result<String, GitHubError> + Send + Sync>),
}

/// Tokens by host, resolved once per session. A token GitHub rejected, or a failed lookup, is
/// looked up again the next time.
#[derive(Default)]
struct GitHubTokens {
    source: TokenSource,
    tokens: HashMap<SharedString, Shared<Task<Result<GitHubToken, GitHubError>>>>,
}

impl Global for GitHubTokens {}

fn token_for_host(
    host: &SharedString,
    project: &Entity<Project>,
    work_dir: &Arc<Path>,
    cx: &mut App,
) -> Shared<Task<Result<GitHubToken, GitHubError>>> {
    let tokens = cx.default_global::<GitHubTokens>();
    if let Some(token) = tokens.tokens.get(host) {
        let usable = match token.peek() {
            None => true,
            Some(Ok(token)) => !token.is_revoked(),
            Some(Err(_)) => false,
        };
        if usable {
            return token.clone();
        }
    }
    let task = match tokens.source.clone() {
        TokenSource::Environment => {
            let process_token = std::env::var("GITHUB_TOKEN").ok();
            let environment = non_empty(process_token.as_deref()).is_none().then(|| {
                project
                    .read(cx)
                    .environment()
                    .clone()
                    .update(cx, |environment, cx| {
                        environment.directory_environment(work_dir.clone(), cx)
                    })
            });
            let executor = cx.background_executor().clone();
            let host = host.clone();
            cx.background_spawn(async move {
                let environment = match environment {
                    Some(environment) => environment.await,
                    None => None,
                };
                resolve_token(&host, process_token, environment, |environment| {
                    gh_auth_token(host.clone(), environment, executor)
                })
                .await
            })
        }
        #[cfg(test)]
        TokenSource::Fake(token) => {
            let token = token(host.as_ref());
            cx.background_spawn(async move { token.map(|token| GitHubToken::new(&token)) })
        }
    }
    .shared();
    cx.default_global::<GitHubTokens>()
        .tokens
        .insert(host.clone(), task.clone());
    task
}

fn non_empty(token: Option<&str>) -> Option<&str> {
    token.map(str::trim).filter(|token| !token.is_empty())
}

enum GhFailure {
    /// The GitHub CLI isn't installed.
    Missing,
    Failed(String),
}

async fn resolve_token<F>(
    host: &SharedString,
    process_token: Option<String>,
    environment: Option<HashMap<String, String>>,
    run_gh: impl FnOnce(Option<HashMap<String, String>>) -> F,
) -> Result<GitHubToken, GitHubError>
where
    F: Future<Output = Result<String, GhFailure>>,
{
    if let Some(token) = non_empty(process_token.as_deref()) {
        return Ok(GitHubToken::new(token));
    }
    if let Some(token) = environment
        .as_ref()
        .and_then(|environment| non_empty(environment.get("GITHUB_TOKEN").map(String::as_str)))
    {
        return Ok(GitHubToken::new(token));
    }
    let failure = match run_gh(environment).await {
        Ok(output) => match non_empty(Some(&output)) {
            Some(token) => return Ok(GitHubToken::new(token)),
            None => AuthFailure::NoToken,
        },
        Err(GhFailure::Missing) => AuthFailure::NoTokenNoCli,
        Err(GhFailure::Failed(message)) => {
            log::warn!("`gh auth token --hostname {host}` failed: {message}");
            AuthFailure::NoToken
        }
    };
    Err(GitHubError::Auth {
        host: host.clone(),
        failure,
    })
}

/// Runs `gh auth token --hostname <host>` in the project's environment, so a `gh` that is only on
/// the login shell's `PATH` is found. Its output is the token: never log it.
async fn gh_auth_token(
    host: SharedString,
    environment: Option<HashMap<String, String>>,
    executor: BackgroundExecutor,
) -> Result<String, GhFailure> {
    let mut command = util::command::new_command("gh");
    command
        .args(["auth", "token", "--hostname", host.as_ref()])
        .kill_on_drop(true);
    if let Some(environment) = environment {
        command.envs(environment);
    }
    let output = pin!(command.output());
    let timeout = pin!(executor.timer(GH_TIMEOUT));
    let output = match futures::future::select(output, timeout).await {
        Either::Left((output, _)) => output,
        Either::Right(_) => return Err(GhFailure::Failed("timed out".into())),
    };
    match output {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(GhFailure::Missing),
        Err(error) => Err(GhFailure::Failed(error.to_string())),
        Ok(output) if !output.status.success() => Err(GhFailure::Failed(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        )),
        Ok(output) => String::from_utf8(output.stdout)
            .map_err(|_| GhFailure::Failed("its output isn't UTF-8".into())),
    }
}

#[derive(Deserialize)]
struct RepositoryData<T> {
    repository: Option<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestData<T> {
    pull_request: Option<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Connection<N> {
    #[serde(default)]
    page_info: Option<PageInfo>,
    nodes: Vec<Option<N>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
struct ActorNode {
    #[serde(rename = "__typename")]
    typename: String,
    login: String,
}

#[derive(Deserialize)]
struct LoginNode {
    login: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepositoryContextData {
    viewer: LoginNode,
    repository: Option<RepositoryNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepositoryNode {
    name_with_owner: String,
    url: String,
    ssh_url: String,
    is_fork: bool,
    parent: Option<ParentNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ParentNode {
    name_with_owner: String,
    url: String,
    ssh_url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenPullRequestsField {
    pull_requests: Connection<PullRequestSummaryNode>,
}

#[derive(Deserialize)]
struct SearchData {
    search: SearchField,
}

#[derive(Deserialize)]
struct SearchField {
    nodes: Vec<Option<serde_json::Value>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestSummaryNode {
    number: u64,
    title: String,
    url: String,
    is_draft: bool,
    is_cross_repository: bool,
    head_ref_name: String,
    base_ref_name: String,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
    author: Option<ActorNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestDetailsNode {
    id: String,
    number: u64,
    title: String,
    url: String,
    body: String,
    state: String,
    is_draft: bool,
    author: Option<ActorNode>,
    base_ref_name: String,
    base_ref_oid: String,
    head_ref_name: String,
    head_ref_oid: String,
    is_cross_repository: bool,
    head_repository: Option<NameWithOwnerNode>,
    review_decision: Option<String>,
    viewer_did_author: bool,
    pending_reviews: Connection<PendingReviewNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NameWithOwnerNode {
    name_with_owner: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingReviewNode {
    id: String,
    database_id: Option<u64>,
    viewer_did_author: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestHeadNode {
    head_ref_oid: String,
    state: String,
    pending_reviews: Connection<PendingReviewNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewThreadsField {
    review_threads: Connection<ThreadNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadNode {
    id: String,
    is_resolved: bool,
    is_outdated: bool,
    subject_type: Option<String>,
    path: String,
    line: Option<u32>,
    start_line: Option<u32>,
    original_line: Option<u32>,
    original_start_line: Option<u32>,
    diff_side: Option<String>,
    start_diff_side: Option<String>,
    comments: CommentConnection,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommentConnection {
    total_count: u32,
    nodes: Vec<Option<CommentNode>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommentNode {
    id: String,
    database_id: Option<u64>,
    body: String,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    url: String,
    state: Option<String>,
    commit: Option<CommitNode>,
    author: Option<ActorNode>,
}

#[derive(Deserialize)]
struct CommitNode {
    oid: String,
}

#[derive(Deserialize)]
struct ReviewsField {
    reviews: Connection<ReviewNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewNode {
    id: String,
    state: String,
    body: String,
    #[serde(default, with = "time::serde::rfc3339::option")]
    submitted_at: Option<OffsetDateTime>,
    url: String,
    author: Option<ActorNode>,
}

/// An entry of REST `GET /repos/{owner}/{repo}/pulls/{number}/files`.
#[derive(Deserialize)]
struct FileEntry {
    filename: String,
    previous_filename: Option<String>,
    status: String,
    patch: Option<String>,
}

fn actor(node: Option<ActorNode>) -> Actor {
    match node {
        // GraphQL names bots without the "[bot]" suffix REST uses (`github-actions`).
        Some(node) => Actor {
            is_bot: node.typename == "Bot" || node.login.ends_with("[bot]"),
            login: node.login.into(),
        },
        None => Actor {
            login: "ghost".into(),
            is_bot: false,
        },
    }
}

/// Commit ids from GitHub become refs in git commands later, so only full ids pass.
fn parse_full_oid(value: &str) -> Result<Oid, GitHubError> {
    if !matches!(value.len(), 40 | 64) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(transport(anyhow!(
            "GitHub returned an invalid commit id {value:?}"
        )));
    }
    value.parse().map_err(transport)
}

fn parse_pull_request_status(state: &str) -> Result<PullRequestStatus, GitHubError> {
    match state {
        "OPEN" => Ok(PullRequestStatus::Open),
        "CLOSED" => Ok(PullRequestStatus::Closed),
        "MERGED" => Ok(PullRequestStatus::Merged),
        other => Err(transport(anyhow!(
            "GitHub returned an unknown pull request state {other:?}"
        ))),
    }
}

fn parse_side(side: Option<&str>) -> Option<DiffSide> {
    match side? {
        "LEFT" => Some(DiffSide::Left),
        "RIGHT" => Some(DiffSide::Right),
        _ => None,
    }
}

fn pending_review(reviews: Connection<PendingReviewNode>) -> Option<PendingReview> {
    reviews
        .nodes
        .into_iter()
        .flatten()
        .find(|review| review.viewer_did_author)
        .and_then(|review| {
            Some(PendingReview {
                node_id: review.id.into(),
                database_id: review.database_id?,
            })
        })
}

fn repo_from_name_with_owner(
    host: &SharedString,
    name_with_owner: &str,
) -> Result<ForgeRepo, GitHubError> {
    let (owner, name) = name_with_owner.split_once('/').ok_or_else(|| {
        transport(anyhow!(
            "GitHub returned an invalid repository name {name_with_owner:?}"
        ))
    })?;
    Ok(ForgeRepo {
        host: host.clone(),
        owner: owner.to_string().into(),
        name: name.to_string().into(),
    })
}

fn summary(node: PullRequestSummaryNode) -> PullRequestSummary {
    PullRequestSummary {
        number: node.number,
        title: node.title.into(),
        url: node.url.into(),
        author: actor(node.author),
        head_ref: node.head_ref_name.into(),
        base_ref: node.base_ref_name.into(),
        is_draft: node.is_draft,
        is_cross_repository: node.is_cross_repository,
        updated_at: node.updated_at,
    }
}

/// A thread as the review shows it, or `None` for a thread of the viewer's pending review.
///
/// GitHub's anchors: a FILE thread reports line 1; a thread without `line` is outdated; a thread
/// is single-line exactly when `startDiffSide` is null (its `startLine` is then a head-mapped
/// RIGHT number, even for a LEFT thread); a range starting on the LEFT has its old line number in
/// `originalStartLine` (`startLine` holds the RIGHT number of that row).
fn parse_thread(node: ThreadNode) -> Option<RemoteThread> {
    let first = node.comments.nodes.iter().flatten().next()?;
    if first.state.as_deref() == Some("PENDING") {
        return None;
    }
    let anchor_commit = first
        .commit
        .as_ref()
        .and_then(|commit| parse_full_oid(&commit.oid).ok());
    let anchor = thread_anchor(&node);
    let mut pending_replies = 0;
    let comments = node
        .comments
        .nodes
        .into_iter()
        .flatten()
        .filter(|comment| {
            let pending = comment.state.as_deref() == Some("PENDING");
            pending_replies += u32::from(pending);
            !pending
        })
        .map(|comment| RemoteComment {
            id: comment.id.into(),
            database_id: comment.database_id,
            author: actor(comment.author),
            body: comment.body.into(),
            created_at: comment.created_at,
            url: comment.url.into(),
        })
        .collect();
    Some(RemoteThread {
        id: node.id.into(),
        path: node.path,
        anchor,
        anchor_commit,
        is_resolved: node.is_resolved,
        is_outdated: node.is_outdated,
        comments,
        total_comments: node.comments.total_count.saturating_sub(pending_replies),
    })
}

fn thread_anchor(node: &ThreadNode) -> ThreadAnchor {
    if node.subject_type.as_deref() == Some("FILE") {
        return ThreadAnchor::File;
    }
    let Some(line) = node.line else {
        return ThreadAnchor::Outdated {
            original_line: node.original_line,
            original_start: node
                .original_start_line
                .filter(|start| Some(*start) != node.original_line),
        };
    };
    let side = parse_side(node.diff_side.as_deref()).unwrap_or(DiffSide::Right);
    let Some(start_side) = parse_side(node.start_diff_side.as_deref()) else {
        return ThreadAnchor::Line { side, line };
    };
    let start = match start_side {
        DiffSide::Right => node.start_line,
        DiffSide::Left => node.original_start_line,
    };
    match start {
        Some(start) if (start_side, start) != (side, line) => ThreadAnchor::Range {
            start_side,
            start,
            side,
            line,
        },
        _ => ThreadAnchor::Line { side, line },
    }
}

/// A review summary, or `None` for a pending review or one without a body.
fn parse_summary(node: ReviewNode) -> Option<RemoteReviewSummary> {
    let state = match node.state.as_str() {
        "COMMENTED" => ReviewState::Commented,
        "APPROVED" => ReviewState::Approved,
        "CHANGES_REQUESTED" => ReviewState::ChangesRequested,
        "DISMISSED" => ReviewState::Dismissed,
        "PENDING" => return None,
        other => {
            log::warn!("skipping a review in the unknown state {other:?}");
            return None;
        }
    };
    if node.body.trim().is_empty() {
        return None;
    }
    Some(RemoteReviewSummary {
        id: node.id.into(),
        author: actor(node.author),
        state,
        body: node.body.into(),
        submitted_at: node.submitted_at,
        url: node.url.into(),
    })
}

fn changed_file(entry: FileEntry) -> ChangedFile {
    let status = match entry.status.as_str() {
        "added" => ChangedFileStatus::Added,
        "removed" => ChangedFileStatus::Removed,
        "modified" => ChangedFileStatus::Modified,
        "renamed" => ChangedFileStatus::Renamed,
        "copied" => ChangedFileStatus::Copied,
        "unchanged" => ChangedFileStatus::Unchanged,
        _ => ChangedFileStatus::Changed,
    };
    ChangedFile {
        hunks: entry.patch.as_deref().map(parse_patch),
        path: entry.filename,
        previous_path: entry.previous_filename,
        status,
    }
}

/// The hunks of a unified diff without file headers, as in GitHub's `patch`.
fn parse_patch(patch: &str) -> Vec<Hunk> {
    let mut hunks = Vec::new();
    // The hunk being read, with the old and new line numbers of its next row.
    let mut current: Option<(Hunk, u32, u32)> = None;
    for line in patch.lines() {
        if line.starts_with("@@") {
            hunks.extend(current.take().map(|(hunk, _, _)| hunk));
            match parse_hunk_header(line) {
                Some((old, new)) => {
                    let (old_line, new_line) = (old.start, new.start);
                    current = Some((
                        Hunk {
                            old,
                            new,
                            rows: Vec::new(),
                        },
                        old_line,
                        new_line,
                    ));
                }
                None => log::warn!("skipping a hunk with the header {line:?}"),
            }
            continue;
        }
        let Some((hunk, old_line, new_line)) = current.as_mut() else {
            continue;
        };
        let kind = match line.as_bytes().first() {
            Some(b'+') => HunkRowKind::Added,
            Some(b'-') => HunkRowKind::Deleted,
            // "\ No newline at end of file"
            Some(b'\\') => continue,
            _ => HunkRowKind::Context,
        };
        match hunk.rows.last_mut() {
            Some(rows) if rows.kind == kind => rows.len += 1,
            _ => hunk.rows.push(HunkRows {
                kind,
                old_start: *old_line,
                new_start: *new_line,
                len: 1,
            }),
        }
        if kind != HunkRowKind::Added {
            *old_line += 1;
        }
        if kind != HunkRowKind::Deleted {
            *new_line += 1;
        }
    }
    hunks.extend(current.map(|(hunk, _, _)| hunk));
    hunks
}

/// `@@ -5,20 +5,32 @@ context` → the old and new line ranges. A missing count is 1.
fn parse_hunk_header(line: &str) -> Option<(Range<u32>, Range<u32>)> {
    let (ranges, _) = line.strip_prefix("@@ -")?.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    Some((parse_hunk_range(old)?, parse_hunk_range(new)?))
}

fn parse_hunk_range(range: &str) -> Option<Range<u32>> {
    let (start, count) = match range.split_once(',') {
        Some((start, count)) => (start.parse().ok()?, count.parse().ok()?),
        None => (range.parse().ok()?, 1),
    };
    Some(start..start.checked_add(count)?)
}

/// Requests to one GitHub host with one token.
struct GitHubClient {
    http: Arc<dyn HttpClient>,
    host: SharedString,
    endpoints: Endpoints,
    token: GitHubToken,
}

impl GitHubClient {
    fn new(http: Arc<dyn HttpClient>, host: SharedString, token: GitHubToken) -> Self {
        Self {
            endpoints: Endpoints::for_host(&host),
            http,
            host,
            token,
        }
    }

    /// Sends a request and returns the headers and body of a successful response. `what` names
    /// the resource in errors; `label` names the request in the debug log, which never holds
    /// headers or bodies.
    async fn send(
        &self,
        method: Method,
        url: &str,
        body: Option<Vec<u8>>,
        what: &SharedString,
        label: &str,
    ) -> Result<(HeaderMap, Vec<u8>), GitHubError> {
        let mut authorization =
            HeaderValue::from_str(&format!("Bearer {}", self.token.value)).map_err(transport)?;
        // Keeps the token out of the HTTP stack's debug output.
        authorization.set_sensitive(true);
        let mut request = Request::builder()
            .method(method.clone())
            .uri(url)
            .header("Authorization", authorization)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .timeout(REQUEST_TIMEOUT);
        if body.is_some() {
            request = request.header("Content-Type", "application/json");
        }
        let request = request
            .body(body.map(AsyncBody::from).unwrap_or_default())
            .map_err(transport)?;
        let mut response = self.http.send(request).await.map_err(|error| {
            transport(error.context(format!("{label}: couldn't reach {}", self.host)))
        })?;
        let status = response.status();
        log::debug!("GitHub {method} {url} ({label}): {status}");
        let mut bytes = Vec::new();
        response
            .body_mut()
            .read_to_end(&mut bytes)
            .await
            .map_err(|error| {
                transport(
                    anyhow::Error::from(error).context(format!("{label}: reading the response")),
                )
            })?;
        if status.is_success() {
            return Ok((response.headers().clone(), bytes));
        }
        if status == StatusCode::UNAUTHORIZED {
            self.token.revoke();
        }
        Err(status_error(
            status,
            response.headers(),
            &bytes,
            what,
            &self.host,
        ))
    }

    async fn graphql<D: DeserializeOwned>(
        &self,
        operation: &'static str,
        variables: serde_json::Value,
        what: &SharedString,
    ) -> Result<D, GitHubError> {
        let body = serde_json::to_vec(&json!({
            "query": QUERIES,
            "operationName": operation,
            "variables": variables,
        }))
        .map_err(transport)?;
        let (headers, bytes) = self
            .send(
                Method::POST,
                &self.endpoints.graphql,
                Some(body),
                what,
                operation,
            )
            .await?;
        let response: GraphQlResponse = parse_json(&bytes, operation)?;
        if !response.errors.is_empty() {
            return Err(graphql_error(response.errors, &headers, what));
        }
        let data = response
            .data
            .ok_or_else(|| transport(anyhow!("{operation}: GitHub returned no data")))?;
        serde_json::from_value(data).map_err(|error| {
            transport(
                anyhow::Error::from(error)
                    .context(format!("parsing GitHub's {operation} response")),
            )
        })
    }

    async fn rest_get<T: DeserializeOwned>(
        &self,
        path: &str,
        what: &SharedString,
    ) -> Result<T, GitHubError> {
        let url = format!("{}/{path}", self.endpoints.rest);
        let (_, bytes) = self.send(Method::GET, &url, None, what, path).await?;
        parse_json(&bytes, path)
    }

    /// Every node of a pull request's connection, page by page (at most
    /// [`MAX_GRAPHQL_PAGES`]).
    async fn pull_request_pages<F: DeserializeOwned, N>(
        &self,
        operation: &'static str,
        repo: &ForgeRepo,
        number: u64,
        connection: impl Fn(F) -> Connection<N>,
    ) -> Result<Vec<N>, GitHubError> {
        let what = pull_request_name(repo, number);
        let mut nodes = Vec::new();
        let mut after: Option<String> = None;
        for _ in 0..MAX_GRAPHQL_PAGES {
            let data: RepositoryData<PullRequestData<F>> = self
                .graphql(
                    operation,
                    json!({
                        "owner": repo.owner,
                        "name": repo.name,
                        "number": number,
                        "after": after,
                    }),
                    &what,
                )
                .await?;
            let field = data
                .repository
                .and_then(|repository| repository.pull_request)
                .ok_or_else(|| GitHubError::NotFound { what: what.clone() })?;
            let page = connection(field);
            nodes.extend(page.nodes.into_iter().flatten());
            match page.page_info {
                Some(PageInfo {
                    has_next_page: true,
                    end_cursor: Some(cursor),
                }) => after = Some(cursor),
                _ => return Ok(nodes),
            }
        }
        log::warn!("{operation} of {what}: stopped after {MAX_GRAPHQL_PAGES} pages");
        Ok(nodes)
    }

    /// The context of the repository `remote` points at. With `follow_parent`, a fork's pull
    /// requests are its parent's.
    async fn repository_context(
        &self,
        remote: &GitHubRemote,
        reference: Option<&ForgeRepoRef>,
        follow_parent: bool,
    ) -> Result<RepositoryContext, GitHubError> {
        let what = SharedString::from(remote.repo.to_string());
        let data: RepositoryContextData = self
            .graphql(
                "RepositoryContext",
                json!({ "owner": remote.repo.owner, "name": remote.repo.name }),
                &what,
            )
            .await?;
        let repository = data
            .repository
            .ok_or_else(|| GitHubError::NotFound { what: what.clone() })?;
        let origin = repo_from_name_with_owner(&self.host, &repository.name_with_owner)?;
        let (canonical, https_url, ssh_url) = match repository.parent {
            Some(parent) if follow_parent && repository.is_fork => (
                repo_from_name_with_owner(&self.host, &parent.name_with_owner)?,
                parent.url,
                parent.ssh_url,
            ),
            _ => (origin.clone(), repository.url, repository.ssh_url),
        };
        if let Some(reference) = reference
            && !reference.matches(&canonical)
        {
            return Err(GitHubError::OtherRepository {
                repo: reference.to_string().into(),
            });
        }
        Ok(RepositoryContext {
            origin,
            canonical,
            viewer: data.viewer.login.into(),
            canonical_https_url: https_url,
            canonical_ssh_url: ssh_url,
            origin_is_ssh: remote.is_ssh,
        })
    }

    async fn list_pull_requests(
        &self,
        repo: &ForgeRepo,
        filter: PullRequestListFilter,
    ) -> Result<Vec<PullRequestSummary>, GitHubError> {
        let what = SharedString::from(repo.to_string());
        let nodes = match filter {
            // Not `search`: its index lags behind new pull requests.
            PullRequestListFilter::Open => {
                let data: RepositoryData<OpenPullRequestsField> = self
                    .graphql(
                        "OpenPullRequests",
                        json!({ "owner": repo.owner, "name": repo.name }),
                        &what,
                    )
                    .await?;
                data.repository
                    .ok_or_else(|| GitHubError::NotFound { what: what.clone() })?
                    .pull_requests
                    .nodes
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
            }
            PullRequestListFilter::ReviewRequested => {
                let query =
                    format!("repo:{repo} is:pr is:open review-requested:@me sort:updated-desc");
                let data: SearchData = self
                    .graphql("SearchPullRequests", json!({ "query": query }), &what)
                    .await?;
                data.search
                    .nodes
                    .into_iter()
                    .flatten()
                    // Hits that aren't pull requests come back as `{}`.
                    .filter(|node| node.as_object().is_none_or(|node| !node.is_empty()))
                    .map(serde_json::from_value::<PullRequestSummaryNode>)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| {
                        transport(
                            anyhow::Error::from(error).context("parsing GitHub's search results"),
                        )
                    })?
            }
        };
        Ok(nodes.into_iter().map(summary).collect())
    }

    async fn pull_request(
        &self,
        repo: &ForgeRepo,
        number: u64,
    ) -> Result<PullRequestDetails, GitHubError> {
        let what = pull_request_name(repo, number);
        let data: RepositoryData<PullRequestData<PullRequestDetailsNode>> = self
            .graphql(
                "PullRequestDetails",
                json!({ "owner": repo.owner, "name": repo.name, "number": number }),
                &what,
            )
            .await?;
        let node = data
            .repository
            .and_then(|repository| repository.pull_request)
            .ok_or_else(|| GitHubError::NotFound { what: what.clone() })?;
        Ok(PullRequestDetails {
            node_id: node.id.into(),
            number: node.number,
            title: node.title.into(),
            url: node.url.into(),
            body: node.body.into(),
            author: actor(node.author),
            state: parse_pull_request_status(&node.state)?,
            is_draft: node.is_draft,
            base_ref: node.base_ref_name.into(),
            base_oid: parse_full_oid(&node.base_ref_oid)?,
            head_ref: node.head_ref_name.into(),
            head_oid: parse_full_oid(&node.head_ref_oid)?,
            head_repository: node
                .head_repository
                .map(|head| repo_from_name_with_owner(&self.host, &head.name_with_owner))
                .transpose()?,
            is_cross_repository: node.is_cross_repository,
            review_decision: match node.review_decision.as_deref() {
                Some("APPROVED") => Some(ReviewDecision::Approved),
                Some("CHANGES_REQUESTED") => Some(ReviewDecision::ChangesRequested),
                Some("REVIEW_REQUIRED") => Some(ReviewDecision::ReviewRequired),
                _ => None,
            },
            viewer_is_author: node.viewer_did_author,
            pending_review: pending_review(node.pending_reviews),
        })
    }

    async fn pull_request_head(
        &self,
        repo: &ForgeRepo,
        number: u64,
    ) -> Result<HeadStatus, GitHubError> {
        let what = pull_request_name(repo, number);
        let data: RepositoryData<PullRequestData<PullRequestHeadNode>> = self
            .graphql(
                "PullRequestHead",
                json!({ "owner": repo.owner, "name": repo.name, "number": number }),
                &what,
            )
            .await?;
        let node = data
            .repository
            .and_then(|repository| repository.pull_request)
            .ok_or_else(|| GitHubError::NotFound { what: what.clone() })?;
        Ok(HeadStatus {
            head_oid: parse_full_oid(&node.head_ref_oid)?,
            state: parse_pull_request_status(&node.state)?,
            pending_review: pending_review(node.pending_reviews),
        })
    }

    async fn review_threads(
        &self,
        repo: &ForgeRepo,
        number: u64,
    ) -> Result<Vec<RemoteThread>, GitHubError> {
        let nodes = self
            .pull_request_pages(
                "ReviewThreads",
                repo,
                number,
                |field: ReviewThreadsField| field.review_threads,
            )
            .await?;
        Ok(nodes.into_iter().filter_map(parse_thread).collect())
    }

    async fn review_summaries(
        &self,
        repo: &ForgeRepo,
        number: u64,
    ) -> Result<Vec<RemoteReviewSummary>, GitHubError> {
        let nodes = self
            .pull_request_pages("ReviewSummaries", repo, number, |field: ReviewsField| {
                field.reviews
            })
            .await?;
        Ok(nodes.into_iter().filter_map(parse_summary).collect())
    }

    /// The pull request's files with GitHub's hunks. Comments must fall inside these hunks,
    /// which can differ from the local diff's.
    async fn changed_files(
        &self,
        repo: &ForgeRepo,
        number: u64,
    ) -> Result<CommentableLines, GitHubError> {
        let what = pull_request_name(repo, number);
        let mut files = Vec::new();
        let mut truncated = false;
        for page in 1..=MAX_FILE_PAGES {
            let entries: Vec<FileEntry> = self
                .rest_get(
                    &format!(
                        "repos/{}/{}/pulls/{number}/files?per_page={FILES_PER_PAGE}&page={page}",
                        repo.owner, repo.name
                    ),
                    &what,
                )
                .await?;
            let full_page = entries.len() == FILES_PER_PAGE;
            files.extend(entries.into_iter().map(changed_file));
            if !full_page {
                break;
            }
            truncated = page == MAX_FILE_PAGES;
        }
        Ok(CommentableLines::new(files, truncated))
    }
}

fn pull_request_name(repo: &ForgeRepo, number: u64) -> SharedString {
    format!("{repo}#{number}").into()
}

fn parse_json<T: DeserializeOwned>(bytes: &[u8], label: &str) -> Result<T, GitHubError> {
    serde_json::from_slice(bytes).map_err(|error| {
        transport(anyhow::Error::from(error).context(format!("parsing GitHub's {label} response")))
    })
}

pub(crate) struct GitHubForge {
    client: Arc<GitHubClient>,
    context: RepositoryContext,
}

impl Forge for GitHubForge {
    fn context(&self) -> &RepositoryContext {
        &self.context
    }

    fn list_pull_requests(
        &self,
        filter: PullRequestListFilter,
    ) -> BoxFuture<'static, anyhow::Result<Vec<PullRequestSummary>>> {
        let client = self.client.clone();
        let repo = self.context.canonical.clone();
        async move { Ok(client.list_pull_requests(&repo, filter).await?) }.boxed()
    }

    fn pull_request(&self, number: u64) -> BoxFuture<'static, anyhow::Result<PullRequestDetails>> {
        let client = self.client.clone();
        let repo = self.context.canonical.clone();
        async move { Ok(client.pull_request(&repo, number).await?) }.boxed()
    }

    fn pull_request_head(&self, number: u64) -> BoxFuture<'static, anyhow::Result<HeadStatus>> {
        let client = self.client.clone();
        let repo = self.context.canonical.clone();
        async move { Ok(client.pull_request_head(&repo, number).await?) }.boxed()
    }

    fn review_threads(&self, number: u64) -> BoxFuture<'static, anyhow::Result<Vec<RemoteThread>>> {
        let client = self.client.clone();
        let repo = self.context.canonical.clone();
        async move { Ok(client.review_threads(&repo, number).await?) }.boxed()
    }

    fn review_summaries(
        &self,
        number: u64,
    ) -> BoxFuture<'static, anyhow::Result<Vec<RemoteReviewSummary>>> {
        let client = self.client.clone();
        let repo = self.context.canonical.clone();
        async move { Ok(client.review_summaries(&repo, number).await?) }.boxed()
    }

    fn changed_files(&self, number: u64) -> BoxFuture<'static, anyhow::Result<CommentableLines>> {
        let client = self.client.clone();
        let repo = self.context.canonical.clone();
        async move { Ok(client.changed_files(&repo, number).await?) }.boxed()
    }
}

/// A remote of the checkout that points at GitHub.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GitHubRemote {
    pub(crate) name: String,
    pub(crate) repo: ForgeRepo,
    pub(crate) is_ssh: bool,
}

/// The GitHub remotes, preferred first: the current branch's upstream remote, `origin`,
/// `upstream`, then the others by name.
fn github_remotes(
    urls: &HashMap<String, String>,
    branch_remote: Option<&str>,
    registry: Option<&GitHostingProviderRegistry>,
) -> Vec<GitHubRemote> {
    let mut remotes = urls
        .iter()
        .filter_map(|(name, url)| {
            let (repo, is_ssh) = github_repo_for_remote(url, registry)?;
            Some(GitHubRemote {
                name: name.clone(),
                repo,
                is_ssh,
            })
        })
        .collect::<Vec<_>>();
    remotes.sort_by_cached_key(|remote| {
        let rank = if Some(remote.name.as_str()) == branch_remote {
            0
        } else if remote.name == "origin" {
            1
        } else if remote.name == "upstream" {
            2
        } else {
            3
        };
        (rank, remote.name.clone())
    });
    remotes
}

/// The remote to resolve, and whether to follow a fork to its parent: a reference that names a
/// remote's repository picks that remote as is.
fn choose_remote<'a>(
    remotes: &'a [GitHubRemote],
    reference: Option<&ForgeRepoRef>,
) -> Result<(&'a GitHubRemote, bool), GitHubError> {
    let preferred = remotes.first().ok_or(GitHubError::Unsupported)?;
    if let Some(reference) = reference
        && let Some(remote) = remotes
            .iter()
            .find(|remote| reference.matches(&remote.repo))
    {
        return Ok((remote, false));
    }
    Ok((preferred, true))
}

/// The checkout's GitHub remotes, preferred first (see [`github_remotes`]).
pub(crate) fn remotes(
    repository: &Entity<Repository>,
    cx: &mut App,
) -> Task<anyhow::Result<Vec<GitHubRemote>>> {
    let branch_remote = repository
        .read(cx)
        .branch
        .as_ref()
        .and_then(|branch| branch.upstream.as_ref())
        .and_then(|upstream| upstream.remote_name())
        .map(str::to_string);
    let remote_urls = repository.update(cx, |repository, _| repository.remote_urls());
    let registry = GitHostingProviderRegistry::try_global(cx);
    cx.spawn(async move |_| {
        let remote_urls = remote_urls
            .await
            .map_err(|_| anyhow!("the repository was closed"))?
            .map_err(|error| error.context("listing the repository's remotes"))?;
        Ok(github_remotes(
            &remote_urls,
            branch_remote.as_deref(),
            registry.as_deref(),
        ))
    })
}

/// What a resolved forge depends on; any change resolves it again.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ForgeKey {
    work_dir: Arc<Path>,
    origin_url: Option<String>,
    upstream_url: Option<String>,
    branch_remote: Option<String>,
    reference: Option<ForgeRepoRef>,
}

#[derive(Default)]
struct GitHubForges(HashMap<ForgeKey, Shared<Task<Result<Arc<GitHubForge>, GitHubError>>>>);

impl Global for GitHubForges {}

pub(crate) fn resolve(
    repository: &Entity<Repository>,
    reference: Option<ForgeRepoRef>,
    project: &Entity<Project>,
    cx: &mut App,
) -> Task<anyhow::Result<Arc<dyn Forge>>> {
    let snapshot = repository.read(cx).snapshot();
    let branch_remote = snapshot
        .branch
        .as_ref()
        .and_then(|branch| branch.upstream.as_ref())
        .and_then(|upstream| upstream.remote_name())
        .map(str::to_string);
    let key = ForgeKey {
        work_dir: snapshot.work_directory_abs_path,
        origin_url: snapshot.remote_origin_url,
        upstream_url: snapshot.remote_upstream_url,
        branch_remote: branch_remote.clone(),
        reference: reference.clone(),
    };
    let cached = cx
        .default_global::<GitHubForges>()
        .0
        .get(&key)
        .filter(|forge| match forge.peek() {
            None => true,
            Some(Ok(forge)) => !forge.client.token.is_revoked(),
            Some(Err(_)) => false,
        })
        .cloned();
    let forge = match cached {
        Some(forge) => forge,
        None => {
            let forge = resolve_uncached(
                repository,
                reference,
                branch_remote,
                project,
                key.work_dir.clone(),
                cx,
            )
            .shared();
            cx.default_global::<GitHubForges>()
                .0
                .insert(key, forge.clone());
            forge
        }
    };
    cx.spawn(async move |_| {
        let forge: Arc<dyn Forge> = forge.await?;
        Ok(forge)
    })
}

fn resolve_uncached(
    repository: &Entity<Repository>,
    reference: Option<ForgeRepoRef>,
    branch_remote: Option<String>,
    project: &Entity<Project>,
    work_dir: Arc<Path>,
    cx: &mut App,
) -> Task<Result<Arc<GitHubForge>, GitHubError>> {
    let remote_urls = repository.update(cx, |repository, _| repository.remote_urls());
    let registry = GitHostingProviderRegistry::try_global(cx);
    let http = cx.http_client();
    let project = project.clone();
    cx.spawn(async move |cx| {
        let remote_urls = remote_urls
            .await
            .map_err(|_| transport(anyhow!("the repository was closed")))?
            .map_err(|error| transport(error.context("listing the repository's remotes")))?;
        let remotes = github_remotes(&remote_urls, branch_remote.as_deref(), registry.as_deref());
        let (remote, follow_parent) = choose_remote(&remotes, reference.as_ref())?;
        let token = cx
            .update(|cx| token_for_host(&remote.repo.host, &project, &work_dir, cx))
            .await?;
        let client = Arc::new(GitHubClient::new(http, remote.repo.host.clone(), token));
        let context = client
            .repository_context(remote, reference.as_ref(), follow_parent)
            .await?;
        Ok(Arc::new(GitHubForge { client, context }))
    })
}

#[cfg(test)]
pub(crate) fn set_fake_token_source(
    cx: &mut App,
    token: impl Fn(&str) -> Result<String, GitHubError> + Send + Sync + 'static,
) {
    let tokens = cx.default_global::<GitHubTokens>();
    tokens.source = TokenSource::Fake(Arc::new(token));
    tokens.tokens.clear();
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use http_client::{FakeHttpClient, Response};
    use serde_json::Value;
    use std::sync::Mutex;

    pub(crate) const PR1_THREADS: &str = include_str!("../test_data/github/pr1-threads.json");
    pub(crate) const PR1_SUMMARIES: &str = include_str!("../test_data/github/pr1-summaries.json");
    pub(crate) const PR1_DETAILS: &str = include_str!("../test_data/github/pr1-details.json");
    pub(crate) const PR1_HEAD: &str = include_str!("../test_data/github/pr1-head.json");
    pub(crate) const PR1_FILES: &str = include_str!("../test_data/github/pr1-files.json");
    pub(crate) const PR4_FILES: &str = include_str!("../test_data/github/pr4-files.json");
    pub(crate) const PR999_DETAILS: &str = include_str!("../test_data/github/pr999-details.json");
    pub(crate) const OPEN_PULL_REQUESTS: &str =
        include_str!("../test_data/github/open-pull-requests.json");
    pub(crate) const FORK_CONTEXT: &str = include_str!("../test_data/github/fork-context.json");

    pub(crate) const PR1_HEAD_OID: &str = "3f22f1b3bd52ed79a655a055209707e356fa8253";
    pub(crate) const PR1_BASE_OID: &str = "deea36cf8fa38276ef75801d05773c4c35c48d45";
    pub(crate) const TOKEN: &str = "secret-token-value";

    pub(crate) struct SentRequest {
        pub(crate) method: Method,
        pub(crate) uri: String,
        pub(crate) headers: HeaderMap,
        pub(crate) body: Value,
    }

    impl SentRequest {
        pub(crate) fn operation(&self) -> Option<&str> {
            self.body["operationName"].as_str()
        }

        pub(crate) fn variables(&self) -> &Value {
            &self.body["variables"]
        }
    }

    pub(crate) struct Reply {
        pub(crate) status: u16,
        pub(crate) headers: Vec<(&'static str, String)>,
        pub(crate) body: String,
    }

    impl Reply {
        pub(crate) fn json(body: impl Into<String>) -> Self {
            Self::status(200, body)
        }

        pub(crate) fn status(status: u16, body: impl Into<String>) -> Self {
            Self {
                status,
                headers: Vec::new(),
                body: body.into(),
            }
        }

        pub(crate) fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
            self.headers.push((name, value.into()));
            self
        }
    }

    pub(crate) type SentRequests = Arc<Mutex<Vec<SentRequest>>>;

    /// A fake GitHub that answers every request with `respond` and records the requests.
    pub(crate) fn fake_github(
        respond: impl Fn(&SentRequest) -> Reply + Send + Sync + 'static,
    ) -> (Arc<dyn HttpClient>, SentRequests) {
        let requests = SentRequests::default();
        let respond = Arc::new(respond);
        let client = FakeHttpClient::create({
            let requests = requests.clone();
            move |request| {
                let requests = requests.clone();
                let respond = respond.clone();
                async move {
                    let (parts, mut body) = request.into_parts();
                    let mut bytes = Vec::new();
                    body.read_to_end(&mut bytes).await?;
                    let sent = SentRequest {
                        method: parts.method,
                        uri: parts.uri.to_string(),
                        headers: parts.headers,
                        body: if bytes.is_empty() {
                            Value::Null
                        } else {
                            serde_json::from_slice(&bytes)?
                        },
                    };
                    let reply = respond(&sent);
                    requests.lock().unwrap().push(sent);
                    let mut response = Response::builder().status(reply.status);
                    for (name, value) in reply.headers {
                        response = response.header(name, value);
                    }
                    Ok(response.body(AsyncBody::from(reply.body))?)
                }
            }
        });
        (client, requests)
    }

    /// A pull request's files as `GET pulls/N/files` lists them (one page).
    pub(crate) fn changed_files_fixture(text: &str) -> CommentableLines {
        let entries: Vec<FileEntry> = serde_json::from_str(text).unwrap();
        CommentableLines::new(entries.into_iter().map(changed_file), false)
    }

    /// PR A's review threads, parsed.
    pub(crate) fn pr1_threads() -> Vec<RemoteThread> {
        let (http, _) = fake_github(|_| Reply::json(PR1_THREADS));
        let client = GitHubClient::new(http, "github.com".into(), GitHubToken::new(TOKEN));
        let repo = ForgeRepo {
            host: "github.com".into(),
            owner: "drew-council".into(),
            name: "code-review-sandbox".into(),
        };
        futures::executor::block_on(client.review_threads(&repo, 1)).unwrap()
    }

    pub(crate) fn repository_context_reply(request: &SentRequest) -> Reply {
        let variables = request.variables();
        let repository = |name_with_owner: &str, parent: Value| {
            json!({ "data": {
                "viewer": { "login": "drew-council" },
                "repository": {
                    "nameWithOwner": name_with_owner,
                    "url": format!("https://github.com/{name_with_owner}"),
                    "sshUrl": format!("git@github.com:{name_with_owner}.git"),
                    "isFork": !parent.is_null(),
                    "parent": parent,
                },
            } })
            .to_string()
        };
        match (variables["owner"].as_str(), variables["name"].as_str()) {
            (Some("drew-council"), Some("zed")) => Reply::json(FORK_CONTEXT),
            (Some("drew-council"), Some("code-review-sandbox")) => {
                Reply::json(repository("drew-council/code-review-sandbox", Value::Null))
            }
            (Some("zed-industries"), Some("zed")) => {
                Reply::json(repository("zed-industries/zed", Value::Null))
            }
            _ => Reply::json(
                r#"{"data":{"viewer":{"login":"drew-council"},"repository":null},"errors":[{"type":"NOT_FOUND","message":"Could not resolve to a Repository"}]}"#,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::forge::{error_message, github_error, resolve_forge};
    use anyhow::Context as _;
    use futures::{AsyncWriteExt as _, executor::block_on};
    use gpui::TestAppContext;
    use http_client::{FakeHttpClient, Response, Url};
    use project::FakeFs;
    use serde_json::Value;
    use std::{
        cell::Cell,
        sync::{Mutex, atomic::AtomicUsize},
    };
    use util::{command::Stdio, path};

    fn sandbox() -> ForgeRepo {
        ForgeRepo {
            host: "github.com".into(),
            owner: "drew-council".into(),
            name: "code-review-sandbox".into(),
        }
    }

    fn client(http: Arc<dyn HttpClient>) -> GitHubClient {
        GitHubClient::new(http, "github.com".into(), GitHubToken::new(TOKEN))
    }

    fn fixture(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn test_endpoints() {
        let endpoints = |rest: &str, graphql: &str| Endpoints {
            rest: rest.into(),
            graphql: graphql.into(),
        };
        assert_eq!(
            Endpoints::for_host("github.com"),
            endpoints("https://api.github.com", "https://api.github.com/graphql")
        );
        assert_eq!(
            Endpoints::for_host("octo.ghe.com"),
            endpoints(
                "https://api.octo.ghe.com",
                "https://api.octo.ghe.com/graphql"
            )
        );
        assert_eq!(
            Endpoints::for_host("github.example.com"),
            endpoints(
                "https://github.example.com/api/v3",
                "https://github.example.com/api/graphql"
            )
        );
    }

    #[test]
    fn test_github_repo_for_remote() {
        let registry = GitHostingProviderRegistry::new();
        registry
            .register_hosting_provider(Arc::new(git_hosting_providers::Github::public_instance()));
        registry
            .register_hosting_provider(Arc::new(git_hosting_providers::Gitlab::public_instance()));
        registry.register_hosting_provider(Arc::new(git_hosting_providers::Github::new(
            "GitHub Self-Hosted",
            Url::parse("https://github.example.com").unwrap(),
        )));
        let parse = |url: &str, registry: Option<&GitHostingProviderRegistry>| {
            github_repo_for_remote(url, registry).map(|(repo, is_ssh)| {
                format!(
                    "{} {repo} {}",
                    repo.host,
                    if is_ssh { "ssh" } else { "https" }
                )
            })
        };
        let cases = [
            (
                "git@github.com:drew-council/zed.git",
                Some("github.com drew-council/zed ssh"),
            ),
            (
                "ssh://git@github.com/drew-council/zed.git",
                Some("github.com drew-council/zed ssh"),
            ),
            (
                "https://github.com/zed-industries/zed",
                Some("github.com zed-industries/zed https"),
            ),
            (
                "https://x-access-token@github.com/zed-industries/zed.git/",
                Some("github.com zed-industries/zed https"),
            ),
            (
                "git@github.example.com:team/app.git",
                Some("github.example.com team/app ssh"),
            ),
            (
                "https://octo.ghe.com/team/app.git",
                Some("octo.ghe.com team/app https"),
            ),
            ("https://gitlab.com/team/app.git", None),
            ("https://github.com/only-an-owner", None),
            ("/srv/git/app.git", None),
        ];
        for (url, expected) in cases {
            assert_eq!(parse(url, Some(&registry)).as_deref(), expected, "{url}");
        }
        // Enterprise Server hosts are only known through the registry.
        assert_eq!(parse("git@github.example.com:team/app.git", None), None);
        assert_eq!(
            parse("https://octo.ghe.com/team/app.git", None).as_deref(),
            Some("octo.ghe.com team/app https")
        );
    }

    #[test]
    fn test_remote_preference() {
        let urls = HashMap::from_iter(
            [
                ("origin", "git@github.com:drew-council/zed.git"),
                ("upstream", "https://github.com/zed-industries/zed.git"),
                ("mirror", "https://gitlab.com/drew/zed.git"),
                ("colleague", "https://github.com/colleague/zed.git"),
                ("archive", "https://github.com/archive/zed.git"),
            ]
            .map(|(name, url)| (name.to_string(), url.to_string())),
        );
        let names = |branch_remote: Option<&str>| {
            github_remotes(&urls, branch_remote, None)
                .into_iter()
                .map(|remote| remote.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(None), ["origin", "upstream", "archive", "colleague"]);
        assert_eq!(
            names(Some("colleague")),
            ["colleague", "origin", "upstream", "archive"]
        );

        let remotes = github_remotes(&urls, None, None);
        assert!(remotes[0].repo.is_same_repo(&ForgeRepo {
            host: "GitHub.com".into(),
            owner: "Drew-Council".into(),
            name: "ZED".into(),
        }));
        assert!(!remotes[0].repo.is_same_repo(&remotes[1].repo));
        let reference = |owner: &str, name: &str| ForgeRepoRef {
            host: None,
            owner: owner.to_string().into(),
            name: name.to_string().into(),
        };
        let chosen = |reference: Option<ForgeRepoRef>| {
            let (remote, follow_parent) = choose_remote(&remotes, reference.as_ref()).unwrap();
            (remote.name.clone(), follow_parent)
        };
        assert_eq!(chosen(None), ("origin".to_string(), true));
        assert_eq!(
            chosen(Some(reference("Colleague", "ZED"))),
            ("colleague".to_string(), false)
        );
        // Not a remote: resolved from the preferred remote, then checked against its parent.
        assert_eq!(
            chosen(Some(reference("someone", "zed"))),
            ("origin".to_string(), true)
        );
        assert!(matches!(
            choose_remote(&[], None),
            Err(GitHubError::Unsupported)
        ));
    }

    #[test]
    fn test_token_precedence() {
        let host = SharedString::from("github.com");
        let directory = |token: &str| {
            Some(HashMap::from_iter([(
                "GITHUB_TOKEN".to_string(),
                token.to_string(),
            )]))
        };
        let gh_ran = Cell::new(false);
        let gh = |output: &'static str| {
            let gh_ran = &gh_ran;
            move |_| {
                gh_ran.set(true);
                async move { Ok::<_, GhFailure>(output.to_string()) }
            }
        };

        let token = block_on(resolve_token(
            &host,
            Some("process".into()),
            directory("directory"),
            gh("gh"),
        ))
        .unwrap();
        assert_eq!(&*token.value, "process");
        assert!(!gh_ran.get());

        let token = block_on(resolve_token(
            &host,
            Some(" \n".into()),
            directory("directory"),
            gh("gh"),
        ))
        .unwrap();
        assert_eq!(&*token.value, "directory");
        assert!(!gh_ran.get());

        // `gh` runs in the project's environment.
        let environment = HashMap::from_iter([("PATH".to_string(), "/nix/bin".to_string())]);
        let token = block_on(resolve_token(
            &host,
            None,
            Some(environment),
            |environment| {
                assert_eq!(environment.unwrap()["PATH"], "/nix/bin");
                async { Ok::<_, GhFailure>("gh-token\n".to_string()) }
            },
        ))
        .unwrap();
        assert_eq!(&*token.value, "gh-token");

        let failure = |result: Result<GitHubToken, GitHubError>| match result {
            Err(GitHubError::Auth { host, failure }) => {
                assert_eq!(host, "github.com");
                failure
            }
            other => panic!("expected an auth error, got {other:?}"),
        };
        let error = block_on(resolve_token(&host, None, None, |_| async {
            Err::<String, _>(GhFailure::Failed(
                "no oauth token found for github.com".into(),
            ))
        }));
        assert_eq!(failure(error), AuthFailure::NoToken);
        let error = block_on(resolve_token(&host, None, None, |_| async {
            Err::<String, _>(GhFailure::Missing)
        }));
        assert_eq!(failure(error), AuthFailure::NoTokenNoCli);
        let error = block_on(resolve_token(&host, None, None, |_| async {
            Ok::<_, GhFailure>("\n".to_string())
        }));
        assert_eq!(failure(error), AuthFailure::NoToken);

        let hint = |failure| {
            GitHubError::Auth {
                host: host.clone(),
                failure,
            }
            .hint()
        };
        assert_eq!(
            hint(AuthFailure::NoToken),
            "No GitHub token for github.com. Set GITHUB_TOKEN or run \
             `gh auth login --hostname github.com`."
        );
        assert_eq!(
            hint(AuthFailure::NoTokenNoCli),
            "No GitHub token for github.com. Set GITHUB_TOKEN, or install the GitHub CLI and \
             run `gh auth login --hostname github.com`."
        );
    }

    #[test]
    fn test_token_stays_out_of_debug_output() {
        let token = GitHubToken::new(TOKEN);
        assert!(!format!("{token:?}").contains(TOKEN));

        let (http, _) = fake_github(|_| Reply::status(401, r#"{"message":"Bad credentials"}"#));
        let client = client(http);
        let error = block_on(client.pull_request(&sandbox(), 1)).unwrap_err();
        assert!(!format!("{error:?} {error}").contains(TOKEN));
    }

    #[test]
    fn test_graphql_request() {
        let (http, requests) = fake_github(|_| Reply::json(PR1_HEAD));
        let client = client(http);
        let head = block_on(client.pull_request_head(&sandbox(), 1)).unwrap();
        assert_eq!(
            head,
            HeadStatus {
                head_oid: PR1_HEAD_OID.parse().unwrap(),
                state: PullRequestStatus::Open,
                pending_review: None,
            }
        );

        let requests = requests.lock().unwrap();
        let [request] = requests.as_slice() else {
            panic!("expected one request");
        };
        assert_eq!(request.method, Method::POST);
        assert_eq!(request.uri, "https://api.github.com/graphql");
        assert_eq!(request.headers["authorization"], format!("Bearer {TOKEN}"));
        assert!(request.headers["authorization"].is_sensitive());
        assert_eq!(request.headers["x-github-api-version"], "2022-11-28");
        assert_eq!(request.headers["accept"], "application/vnd.github+json");
        assert_eq!(request.headers["content-type"], "application/json");
        assert_eq!(request.body["query"], QUERIES);
        assert_eq!(request.operation(), Some("PullRequestHead"));
        assert_eq!(
            request.variables(),
            &json!({ "owner": "drew-council", "name": "code-review-sandbox", "number": 1 })
        );
    }

    #[test]
    fn test_list_pull_requests() {
        let (http, requests) = fake_github(|request| match request.operation() {
            Some("OpenPullRequests") => Reply::json(OPEN_PULL_REQUESTS),
            Some("SearchPullRequests") => {
                let pull_request =
                    fixture(OPEN_PULL_REQUESTS)["data"]["repository"]["pullRequests"]["nodes"][1]
                        .clone();
                Reply::json(
                    json!({ "data": { "search": { "nodes": [{}, pull_request, null] } } })
                        .to_string(),
                )
            }
            _ => Reply::status(500, "{}"),
        });
        let client = client(http);

        let open =
            block_on(client.list_pull_requests(&sandbox(), PullRequestListFilter::Open)).unwrap();
        assert_eq!(
            open.iter()
                .map(|pull_request| pull_request.number)
                .collect::<Vec<_>>(),
            [4, 1, 3, 2]
        );
        assert_eq!(
            open[0].author,
            Actor {
                login: "github-actions".into(),
                is_bot: true,
            }
        );
        assert_eq!(open[0].head_ref, "bot/api-probe");
        let pr_a = &open[1];
        assert_eq!(
            pr_a.title,
            "Add template filters and retire the legacy renderer"
        );
        assert_eq!(
            pr_a.url,
            "https://github.com/drew-council/code-review-sandbox/pull/1"
        );
        assert_eq!(pr_a.head_ref, "feature/template-filters");
        assert_eq!(pr_a.base_ref, "main");
        assert!(!pr_a.author.is_bot && !pr_a.is_draft && !pr_a.is_cross_repository);

        let requested =
            block_on(client.list_pull_requests(&sandbox(), PullRequestListFilter::ReviewRequested))
                .unwrap();
        assert_eq!(
            requested
                .iter()
                .map(|pull_request| pull_request.number)
                .collect::<Vec<_>>(),
            [1]
        );

        let requests = requests.lock().unwrap();
        assert_eq!(
            requests[0].variables(),
            &json!({ "owner": "drew-council", "name": "code-review-sandbox" })
        );
        assert_eq!(
            requests[1].variables(),
            &json!({ "query": "repo:drew-council/code-review-sandbox is:pr is:open review-requested:@me sort:updated-desc" })
        );
    }

    #[test]
    fn test_pull_request_details() {
        let details_with = |edit: fn(&mut Value)| {
            let mut details = fixture(PR1_DETAILS);
            edit(&mut details["data"]["repository"]["pullRequest"]);
            let (http, _) = fake_github(move |_| Reply::json(details.to_string()));
            block_on(client(http).pull_request(&sandbox(), 1))
        };

        let details = details_with(|_| {}).unwrap();
        assert_eq!(details.node_id, "PR_kwDOVM9_N88AAAABI92VhA");
        assert_eq!(details.number, 1);
        assert_eq!(
            details.title,
            "Add template filters and retire the legacy renderer"
        );
        assert!(details.body.starts_with("## Summary\n"));
        assert_eq!(details.author.login, "drew-council");
        assert_eq!(details.state, PullRequestStatus::Open);
        assert!(!details.is_draft);
        assert_eq!(details.base_ref, "main");
        assert_eq!(details.base_oid.to_string(), PR1_BASE_OID);
        assert_eq!(details.head_ref, "feature/template-filters");
        assert_eq!(details.head_oid.to_string(), PR1_HEAD_OID);
        assert_eq!(details.head_repository, Some(sandbox()));
        assert!(!details.is_cross_repository);
        assert_eq!(details.review_decision, None);
        assert!(details.viewer_is_author);
        assert_eq!(details.pending_review, None);

        let details = details_with(|pull_request| {
            pull_request["pendingReviews"]["nodes"] = json!([
                { "id": "PRR_pending", "databaseId": 5555705425u64, "viewerDidAuthor": true }
            ]);
            pull_request["headRepository"] = Value::Null;
            pull_request["reviewDecision"] = json!("CHANGES_REQUESTED");
            pull_request["state"] = json!("MERGED");
        })
        .unwrap();
        assert_eq!(
            details.pending_review,
            Some(PendingReview {
                node_id: "PRR_pending".into(),
                database_id: 5555705425,
            })
        );
        assert_eq!(details.head_repository, None);
        assert_eq!(
            details.review_decision,
            Some(ReviewDecision::ChangesRequested)
        );
        assert_eq!(details.state, PullRequestStatus::Merged);

        // Commit ids become git arguments: only full ids pass.
        for oid in ["3f22f1b", "--upload-pack=x", ""] {
            let error = {
                let mut details = fixture(PR1_DETAILS);
                details["data"]["repository"]["pullRequest"]["headRefOid"] = json!(oid);
                let (http, _) = fake_github(move |_| Reply::json(details.to_string()));
                block_on(client(http).pull_request(&sandbox(), 1)).unwrap_err()
            };
            assert!(
                matches!(error, GitHubError::Transport(_)),
                "{oid:?}: {error:?}"
            );
        }
    }

    #[test]
    fn test_review_threads() {
        let (http, _) = fake_github(|_| Reply::json(PR1_THREADS));
        let threads = block_on(client(http).review_threads(&sandbox(), 1)).unwrap();
        let rows = threads
            .iter()
            .map(|thread| {
                (
                    thread.path.as_str(),
                    thread.anchor.clone(),
                    thread.is_resolved,
                    thread.is_outdated,
                    thread.comments.len(),
                    thread.is_by_bot(),
                )
            })
            .collect::<Vec<_>>();
        let line = |side, line| ThreadAnchor::Line { side, line };
        assert_eq!(
            rows,
            [
                (
                    "src/engine.rs",
                    line(DiffSide::Right, 134),
                    false,
                    false,
                    2,
                    false
                ),
                (
                    "src/filters.rs",
                    ThreadAnchor::Range {
                        start_side: DiffSide::Right,
                        start: 20,
                        side: DiffSide::Right,
                        line: 25,
                    },
                    false,
                    false,
                    1,
                    false
                ),
                (
                    "src/util/strings.rs",
                    line(DiffSide::Right, 48),
                    true,
                    false,
                    1,
                    false
                ),
                (
                    "src/engine.rs",
                    ThreadAnchor::Outdated {
                        original_line: Some(223),
                        original_start: None,
                    },
                    false,
                    true,
                    1,
                    false
                ),
                (
                    "src/engine.rs",
                    line(DiffSide::Left, 193),
                    false,
                    false,
                    1,
                    false
                ),
                ("docs/guide.md", ThreadAnchor::File, false, false, 1, false),
                (
                    "src/filters.rs",
                    line(DiffSide::Right, 38),
                    false,
                    false,
                    1,
                    true
                ),
            ]
        );

        let head: Oid = PR1_HEAD_OID.parse().unwrap();
        let outdated_commit: Oid = "5c5cdf448832bafac6e57e6dc7ece99b69ada5ce".parse().unwrap();
        assert_eq!(
            threads
                .iter()
                .map(|thread| thread.anchor_commit)
                .collect::<Vec<_>>(),
            [
                Some(head),
                Some(head),
                Some(head),
                Some(outdated_commit),
                Some(head),
                Some(head),
                Some(head)
            ]
        );
        let first = &threads[0];
        assert_eq!(first.id, "PRRT_kwDOVM9_N86tcJxI");
        assert_eq!(first.total_comments, 2);
        assert_eq!(first.comments[0].database_id, Some(4301011954));
        assert_eq!(first.comments[0].author.login, "drew-council");
        assert_eq!(
            first.comments[0].url,
            "https://github.com/drew-council/code-review-sandbox/pull/1#discussion_r4301011954"
        );
        assert!(first.comments[1].body.starts_with("Good call"));
        assert_eq!(
            threads[6].author().map(|author| author.login.clone()),
            Some("github-actions".into())
        );
    }

    fn thread_node(fields: Value) -> ThreadNode {
        let mut node = json!({
            "id": "PRRT_1",
            "isResolved": false,
            "isOutdated": false,
            "subjectType": "LINE",
            "path": "src/engine.rs",
            "line": null,
            "startLine": null,
            "originalLine": null,
            "originalStartLine": null,
            "diffSide": "RIGHT",
            "startDiffSide": null,
            "comments": {
                "totalCount": 1,
                "nodes": [{
                    "id": "PRRC_1",
                    "databaseId": 1,
                    "body": "Comment",
                    "createdAt": "2026-10-17T03:00:00Z",
                    "url": "https://github.com/o/r/pull/1#discussion_r1",
                    "state": "SUBMITTED",
                    "commit": { "oid": PR1_HEAD_OID },
                    "author": { "__typename": "User", "login": "reviewer" },
                }],
            },
        });
        for (key, value) in fields.as_object().unwrap() {
            node[key] = value.clone();
        }
        serde_json::from_value(node).unwrap()
    }

    #[test]
    fn test_thread_anchors() {
        let anchor = |fields: Value| parse_thread(thread_node(fields)).unwrap().anchor;
        // A LEFT single-line thread on a context row: `startLine` is the row's RIGHT number.
        assert_eq!(
            anchor(json!({ "line": 9, "diffSide": "LEFT", "startLine": 10 })),
            ThreadAnchor::Line {
                side: DiffSide::Left,
                line: 9,
            }
        );
        // A single-line thread made on an older commit: `startLine` is mapped to the head.
        assert_eq!(
            anchor(json!({ "line": 41, "startLine": 42 })),
            ThreadAnchor::Line {
                side: DiffSide::Right,
                line: 41,
            }
        );
        // From a LEFT context row: its old number is `originalStartLine`.
        assert_eq!(
            anchor(json!({
                "line": 194,
                "startLine": 190,
                "originalStartLine": 189,
                "startDiffSide": "LEFT",
            })),
            ThreadAnchor::Range {
                start_side: DiffSide::Left,
                start: 189,
                side: DiffSide::Right,
                line: 194,
            }
        );
        assert_eq!(
            anchor(json!({
                "line": 43,
                "diffSide": "LEFT",
                "startLine": 26,
                "originalStartLine": 29,
                "startDiffSide": "LEFT",
            })),
            ThreadAnchor::Range {
                start_side: DiffSide::Left,
                start: 29,
                side: DiffSide::Left,
                line: 43,
            }
        );
        assert_eq!(
            anchor(json!({ "line": 10, "startLine": 5, "startDiffSide": "RIGHT" })),
            ThreadAnchor::Range {
                start_side: DiffSide::Right,
                start: 5,
                side: DiffSide::Right,
                line: 10,
            }
        );
        // A one-line "range" GitHub stores with a start side.
        assert_eq!(
            anchor(json!({ "line": 8, "startLine": 8, "startDiffSide": "RIGHT" })),
            ThreadAnchor::Line {
                side: DiffSide::Right,
                line: 8,
            }
        );
        assert_eq!(
            anchor(json!({
                "originalLine": 30,
                "originalStartLine": 25,
                "startDiffSide": "RIGHT",
                "isOutdated": true,
            })),
            ThreadAnchor::Outdated {
                original_line: Some(30),
                original_start: Some(25),
            }
        );
        // File threads report line 1.
        assert_eq!(
            anchor(json!({ "subjectType": "FILE", "line": 1, "originalLine": 1 })),
            ThreadAnchor::File
        );

        let thread = parse_thread(thread_node(json!({ "line": 3 }))).unwrap();
        assert_eq!(thread.anchor_commit, Some(PR1_HEAD_OID.parse().unwrap()));
        assert!(!thread.is_by_bot());

        // The viewer's pending review shows up in `reviewThreads`.
        let mut pending = thread_node(json!({ "line": 3 }));
        if let Some(Some(comment)) = pending.comments.nodes.first_mut() {
            comment.state = Some("PENDING".into());
        }
        assert_eq!(parse_thread(pending), None);

        let mut thread = thread_node(json!({ "line": 3 }));
        let reply = json!({
            "id": "PRRC_2",
            "databaseId": 2,
            "body": "Pending reply",
            "createdAt": "2026-10-17T03:01:00Z",
            "url": "https://github.com/o/r/pull/1#discussion_r2",
            "state": "PENDING",
            "commit": null,
            "author": { "__typename": "Bot", "login": "copilot-pull-request-reviewer" },
        });
        thread
            .comments
            .nodes
            .push(Some(serde_json::from_value(reply).unwrap()));
        thread.comments.total_count = 2;
        let thread = parse_thread(thread).unwrap();
        assert_eq!(thread.comments.len(), 1);
        assert_eq!(thread.total_comments, 1);
    }

    #[test]
    fn test_review_thread_pages() {
        let threads = fixture(PR1_THREADS);
        let nodes = threads["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"]
            .as_array()
            .unwrap()
            .clone();
        let page = |nodes: &[Value], next: Option<&str>| {
            json!({ "data": { "repository": { "pullRequest": { "reviewThreads": {
                "pageInfo": { "hasNextPage": next.is_some(), "endCursor": next },
                "nodes": nodes,
            } } } } })
            .to_string()
        };
        let (first_page, second_page) =
            (page(&nodes[..3], Some("CURSOR")), page(&nodes[3..], None));
        let (http, requests) = fake_github(move |request| {
            if request.variables()["after"].is_null() {
                Reply::json(first_page.clone())
            } else {
                Reply::json(second_page.clone())
            }
        });
        let threads = block_on(client(http).review_threads(&sandbox(), 1)).unwrap();
        assert_eq!(threads.len(), 7);
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].variables()["after"], "CURSOR");

        let endless = page(&nodes[..1], Some("AGAIN"));
        let (http, requests) = fake_github(move |_| Reply::json(endless.clone()));
        let threads = block_on(client(http).review_threads(&sandbox(), 1)).unwrap();
        assert_eq!(threads.len(), MAX_GRAPHQL_PAGES);
        assert_eq!(requests.lock().unwrap().len(), MAX_GRAPHQL_PAGES);
    }

    #[test]
    fn test_review_summaries() {
        let mut summaries = fixture(PR1_SUMMARIES);
        summaries["data"]["repository"]["pullRequest"]["reviews"]["nodes"]
            .as_array_mut()
            .unwrap()
            .push(json!({
                "id": "PRR_pending",
                "state": "PENDING",
                "body": "Not submitted yet",
                "submittedAt": null,
                "url": "https://github.com/drew-council/code-review-sandbox/pull/1",
                "author": { "__typename": "User", "login": "drew-council" },
            }));
        let (http, _) = fake_github(move |_| Reply::json(summaries.to_string()));
        let summaries = block_on(client(http).review_summaries(&sandbox(), 1)).unwrap();
        // Reviews without a body (from replies and single comments) and pending ones are left out.
        let rows = summaries
            .iter()
            .map(|summary| {
                (
                    summary.id.as_ref(),
                    summary.state,
                    summary.author.login.as_ref(),
                    summary.author.is_bot,
                    summary.submitted_at.is_some(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            [
                (
                    "PRR_kwDOVM9_N88AAAABSyT2YQ",
                    ReviewState::Commented,
                    "drew-council",
                    false,
                    true
                ),
                (
                    "PRR_kwDOVM9_N88AAAABSyUC4w",
                    ReviewState::Commented,
                    "github-actions",
                    true,
                    true
                ),
            ]
        );
        assert!(summaries[0].body.starts_with("Initial pass."));
        assert!(summaries[1].body.starts_with("## Automated review"));
    }

    #[test]
    fn test_hunk_headers() {
        assert_eq!(
            parse_hunk_header("@@ -5,20 +5,32 @@ fn render()"),
            Some((5..25, 5..37))
        );
        assert_eq!(parse_hunk_header("@@ -0,0 +1,5 @@"), Some((0..0, 1..6)));
        assert_eq!(parse_hunk_header("@@ -1 +1 @@"), Some((1..2, 1..2)));
        assert_eq!(parse_hunk_header("@@ -1,3 +0,0 @@"), Some((1..4, 0..0)));
        assert_eq!(parse_hunk_header("@@ -1,x +1 @@"), None);
        assert_eq!(parse_hunk_header("@@ -4294967295,2 +1 @@"), None);

        let hunks = parse_patch(
            "@@ -1,4 +1,5 @@\n a\n-b\n+B\n+C\n c\n d\n\\ No newline at end of file\n@@ -10 +11 @@\n-x\n+y",
        );
        let rows = |kind, old_start, new_start, len| HunkRows {
            kind,
            old_start,
            new_start,
            len,
        };
        assert_eq!(
            hunks,
            [
                Hunk {
                    old: 1..5,
                    new: 1..6,
                    rows: vec![
                        rows(HunkRowKind::Context, 1, 1, 1),
                        rows(HunkRowKind::Deleted, 2, 2, 1),
                        rows(HunkRowKind::Added, 3, 2, 2),
                        rows(HunkRowKind::Context, 3, 4, 2),
                    ],
                },
                Hunk {
                    old: 10..11,
                    new: 11..12,
                    rows: vec![
                        rows(HunkRowKind::Deleted, 10, 11, 1),
                        rows(HunkRowKind::Added, 11, 11, 1),
                    ],
                },
            ]
        );

        let file = changed_file(FileEntry {
            filename: "docs/design-notes.md".into(),
            previous_filename: Some("docs/notes.md".into()),
            status: "renamed".into(),
            patch: None,
        });
        assert_eq!(file.status, ChangedFileStatus::Renamed);
        assert_eq!(file.hunks, None);
    }

    fn files_fixture(text: &str) -> CommentableLines {
        let (http, _) = {
            let text = text.to_string();
            fake_github(move |_| Reply::json(text.clone()))
        };
        block_on(client(http).changed_files(&sandbox(), 4)).unwrap()
    }

    #[test]
    fn test_changed_file_pages() {
        let entry = |index: usize| {
            json!({
                "filename": format!("src/file{index}.rs"),
                "previous_filename": null,
                "status": "added",
                "patch": "@@ -0,0 +1 @@\n+x",
            })
        };
        let (http, requests) = fake_github(move |request| {
            let entries = if request.uri.ends_with("&page=1") {
                (0..FILES_PER_PAGE).map(entry).collect::<Vec<_>>()
            } else {
                (FILES_PER_PAGE..FILES_PER_PAGE + 3).map(entry).collect()
            };
            Reply::json(Value::from(entries).to_string())
        });
        let files = block_on(client(http).changed_files(&sandbox(), 4)).unwrap();
        assert_eq!(files.files().count(), FILES_PER_PAGE + 3);
        assert!(!files.is_truncated());
        let requests = requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .map(|request| (request.method.clone(), request.uri.as_str()))
                .collect::<Vec<_>>(),
            [
                (
                    Method::GET,
                    "https://api.github.com/repos/drew-council/code-review-sandbox/pulls/4/files?per_page=100&page=1"
                ),
                (
                    Method::GET,
                    "https://api.github.com/repos/drew-council/code-review-sandbox/pulls/4/files?per_page=100&page=2"
                ),
            ]
        );
        assert_eq!(requests[0].headers["x-github-api-version"], "2022-11-28");
        assert!(requests[0].headers.get("content-type").is_none());
        drop(requests);

        // GitHub lists at most 3000 files.
        let (http, requests) = fake_github(move |_| {
            Reply::json(Value::from((0..FILES_PER_PAGE).map(entry).collect::<Vec<_>>()).to_string())
        });
        let files = block_on(client(http).changed_files(&sandbox(), 4)).unwrap();
        assert!(files.is_truncated());
        assert!(files.contains_file("not/listed.rs"));
        assert!(!files.allows("not/listed.rs", DiffSide::Right, 1));
        assert_eq!(requests.lock().unwrap().len(), MAX_FILE_PAGES as usize);
    }

    #[test]
    fn test_commentable_lines() {
        use DiffSide::{Left, Right};

        // PR D (#4) of the sandbox; the cases are from the live verification.
        let files = files_fixture(PR4_FILES);
        let engine = "src/engine.rs";
        // Hunk `-3,8 +3,9`: context rows up to R11 count, R2 and R12 don't.
        assert!(files.allows(engine, Right, 10));
        assert!(files.allows(engine, Right, 3));
        assert!(files.allows(engine, Right, 11));
        assert!(!files.allows(engine, Right, 12));
        assert!(!files.allows(engine, Right, 2));
        // A LEFT context row.
        assert!(files.allows(engine, Left, 9));
        assert!(!files.allows(engine, Right, 100));
        assert!(!files.allows(engine, Left, 100));
        // Ranges may span hunks, but both ends must be commentable and in diff order.
        assert!(files.allows_range(engine, (Right, 8), (Right, 192)));
        assert!(!files.allows_range(engine, (Right, 8), (Right, 15)));
        assert!(files.allows_range(engine, (Left, 191), (Right, 195)));
        assert!(files.allows_range(engine, (Left, 189), (Right, 194)));
        assert!(files.allows_range(engine, (Left, 189), (Left, 192)));
        assert!(!files.allows_range(engine, (Right, 192), (Left, 193)));
        assert!(!files.allows_range(engine, (Right, 10), (Right, 8)));
        assert!(files.allows_range(engine, (Right, 8), (Right, 8)));

        let strings = "src/util/strings.rs";
        assert!(!files.allows(strings, Right, 35));
        assert!(files.allows_range(strings, (Left, 29), (Left, 43)));

        // Renamed files: the old path finds the new one.
        assert!(files.allows("src/settings.rs", Left, 40));
        assert!(files.allows("src/config.rs", Left, 40));
        assert!(files.allows("src/settings.rs", Left, 38));
        assert!(!files.allows("src/settings.rs", Right, 10));
        assert_eq!(files.current_path("src/config.rs"), "src/settings.rs");
        assert_eq!(files.current_path("src/settings.rs"), "src/settings.rs");
        // A pure rename has no patch: file comments only.
        assert!(files.contains_file("docs/design-notes.md"));
        assert!(files.contains_file("docs/notes.md"));
        assert!(!files.allows("docs/design-notes.md", Right, 1));
        assert!(!files.allows("docs/notes.md", Left, 1));
        // Added files have no LEFT side.
        assert!(files.allows("src/cache.rs", Right, 10));
        assert!(!files.allows("src/cache.rs", Left, 10));
        // Files outside the pull request.
        assert!(!files.contains_file("src/render.rs"));
        assert!(!files.allows("src/render.rs", Right, 1));
        assert!(!files.is_truncated());

        // PR A (#1).
        let files = files_fixture(PR1_FILES);
        // GitHub merged two local hunks into `-43,15 +48,19`: R59 sits between them.
        assert!(files.allows(engine, Right, 59));
        // In that hunk the second change's deletion (L54) comes after the first one's
        // additions (R51–R55).
        assert!(files.allows_range(engine, (Right, 55), (Left, 54)));
        assert!(!files.allows_range(engine, (Left, 54), (Right, 55)));
        // Deleted files have no RIGHT side.
        let legacy = "src/legacy.rs";
        assert!(files.allows(legacy, Left, 7));
        assert!(!files.allows(legacy, Right, 7));
        assert!(files.allows_range(legacy, (Left, 3), (Left, 12)));
        // Generated and vendored files are like any other.
        assert!(files.allows("gen/schema.rs", Right, 30));
        assert!(files.allows("vendor/lib.js", Right, 17));
    }

    fn error_for(reply: Reply) -> GitHubError {
        let reply = Arc::new(Mutex::new(Some(reply)));
        let (http, _) = fake_github(move |_| reply.lock().unwrap().take().unwrap());
        block_on(client(http).changed_files(&sandbox(), 4)).unwrap_err()
    }

    #[test]
    fn test_errors() {
        let (http, _) = fake_github(|_| Reply::status(401, r#"{"message":"Bad credentials"}"#));
        let client = client(http);
        let error = block_on(client.changed_files(&sandbox(), 4)).unwrap_err();
        assert_eq!(
            error.hint(),
            "GitHub authentication failed for github.com. Set GITHUB_TOKEN or run \
             `gh auth login --hostname github.com`."
        );
        assert!(client.token.is_revoked());

        let error = error_for(
            Reply::status(
                403,
                r#"{"message":"API rate limit exceeded for user ID 1."}"#,
            )
            .header("x-ratelimit-remaining", "0")
            .header("x-ratelimit-reset", "1760000000"),
        );
        assert!(matches!(
            error,
            GitHubError::RateLimited {
                reset: Some(1760000000)
            }
        ));
        assert_eq!(
            rate_limit_hint(Some(1760000090), 1760000000),
            "GitHub's API rate limit is exceeded. Try again in 2 minutes."
        );

        let error = error_for(
            Reply::status(
                403,
                r#"{"message":"Resource protected by organization SAML enforcement."}"#,
            )
            .header(
                "x-github-sso",
                "required; url=https://github.com/orgs/acme/sso?authorization_request=A1",
            ),
        );
        assert_eq!(
            error.hint(),
            "Authorize your GitHub token for this organization's single sign-on: \
             https://github.com/orgs/acme/sso?authorization_request=A1"
        );

        let error = error_for(Reply::status(
            403,
            r#"{"message":"Resource not accessible by integration"}"#,
        ));
        assert_eq!(
            error.hint(),
            "GitHub denied access to drew-council/code-review-sandbox#4: Resource not \
             accessible by integration"
        );

        let error = error_for(Reply::status(404, r#"{"message":"Not Found"}"#));
        assert_eq!(
            error.hint(),
            "drew-council/code-review-sandbox#4 not found on GitHub, or your token can't see it \
             (private repositories need the `repo` scope)."
        );

        let error = error_for(Reply::status(502, "<html>Bad gateway</html>"));
        assert!(matches!(error, GitHubError::Transport(_)), "{error:?}");
        assert!(error.hint().contains("502"), "{}", error.hint());

        let error = error_for(Reply::json("[{"));
        assert!(matches!(error, GitHubError::Transport(_)), "{error:?}");

        // GraphQL answers 200 with typed errors.
        let (http, _) = fake_github(|_| Reply::json(PR999_DETAILS));
        let error = block_on(self::client(http).pull_request(&sandbox(), 999)).unwrap_err();
        assert!(
            matches!(&error, GitHubError::NotFound { what } if what == "drew-council/code-review-sandbox#999"),
            "{error:?}"
        );
        let (http, _) = fake_github(|_| {
            Reply::json(
                r#"{"errors":[{"type":"RATE_LIMITED","message":"API rate limit exceeded"}]}"#,
            )
            .header("x-ratelimit-reset", "1760000000")
        });
        let error = block_on(self::client(http).pull_request(&sandbox(), 1)).unwrap_err();
        assert!(matches!(
            error,
            GitHubError::RateLimited {
                reset: Some(1760000000)
            }
        ));
        let (http, _) = fake_github(|_| {
            Reply::json(r#"{"data":null,"errors":[{"message":"Something went wrong"}]}"#)
        });
        let error = block_on(self::client(http).pull_request(&sandbox(), 1)).unwrap_err();
        assert_eq!(error.hint(), "GitHub API error: Something went wrong");
    }

    #[test]
    fn test_validation_hints() {
        let hint = |body: &str| {
            status_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                &HeaderMap::new(),
                body.as_bytes(),
                &"drew-council/code-review-sandbox#2".into(),
                &"github.com".into(),
            )
            .hint()
            .to_string()
        };
        let pending = "You already have a pending review on this PR. Submit or discard it on \
                       GitHub, then retry.";
        assert_eq!(
            hint(
                r#"{"message":"Unprocessable Entity","errors":["User can only have one pending review per pull request"],"status":"422"}"#
            ),
            pending
        );
        // `POST /pulls/{n}/comments` sends objects.
        assert_eq!(
            hint(
                r#"{"message":"Validation Failed","errors":[{"resource":"PullRequestReview","code":"custom","field":"user_id","message":"user_id can only have one pending review per pull request"}]}"#
            ),
            pending
        );
        let position = "GitHub rejected a comment's position (outside the PR diff). Reload the \
                        review and check comments on lines outside GitHub's diff hunks.";
        assert_eq!(
            hint(r#"{"message":"Unprocessable Entity","errors":["Line could not be resolved"]}"#),
            position
        );
        assert_eq!(
            hint(
                r#"{"message":"Unprocessable Entity","errors":["Path could not be resolved and Line could not be resolved"]}"#
            ),
            position
        );
        assert_eq!(
            hint(
                r#"{"message":"Unprocessable Entity","errors":["Start position could not be resolved"]}"#
            ),
            position
        );
        assert_eq!(
            hint(
                r#"{"message":"Unprocessable Entity","errors":["The commitOID is not part of the pull request"]}"#
            ),
            "GitHub doesn't know the reviewed commit. Reload the review (drafts are kept)."
        );
        assert_eq!(
            hint(
                r#"{"message":"Unprocessable Entity","errors":["Review Can not approve your own pull request"]}"#
            ),
            "Can not approve your own pull request"
        );
        assert_eq!(
            hint(
                r#"{"message":"Unprocessable Entity","errors":["Review Can not request changes on your own pull request"]}"#
            ),
            "Can not request changes on your own pull request"
        );
        assert_eq!(
            hint(
                r#"{"message":"Unprocessable Entity","errors":["Review You need to leave a comment indicating the requested changes."]}"#
            ),
            "Request changes needs a review comment or at least one inline comment."
        );
        // A COMMENT review without a body or comments gets an empty error.
        assert_eq!(
            hint(r#"{"message":"Unprocessable Entity","errors":[""],"status":"422"}"#),
            "GitHub needs a review comment when there are no inline comments."
        );
        assert_eq!(
            hint(r#"{"message":"Invalid request.\n\n\"event\" wasn't supplied."}"#),
            "Invalid request.\n\n\"event\" wasn't supplied."
        );
        assert_eq!(
            hint(
                r#"{"message":"Unprocessable Entity","errors":["Variable $event of type PullRequestReviewEvent was provided invalid value"]}"#
            ),
            "Variable $event of type PullRequestReviewEvent was provided invalid value"
        );
    }

    async fn project_with_remotes(
        remotes: &[(&str, &str)],
        cx: &mut TestAppContext,
    ) -> (Entity<Project>, Entity<Repository>) {
        crate::init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({ ".git": {}, "file.txt": "text\n" }),
        )
        .await;
        for (name, url) in remotes {
            fs.set_remote_for_repo(Path::new(path!("/project/.git")), *name, *url);
        }
        let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;
        cx.run_until_parked();
        let repository =
            project.read_with(cx, |project, cx| project.active_repository(cx).unwrap());
        (project, repository)
    }

    #[gpui::test]
    async fn test_tokens_are_shared_per_host(cx: &mut TestAppContext) {
        let (project, _) = project_with_remotes(&[], cx).await;
        let lookups = Arc::new(AtomicUsize::new(0));
        let fail = Arc::new(AtomicBool::new(false));
        cx.update(|cx| {
            set_fake_token_source(cx, {
                let lookups = lookups.clone();
                let fail = fail.clone();
                move |host| {
                    let lookup = lookups.fetch_add(1, Ordering::SeqCst) + 1;
                    if fail.load(Ordering::SeqCst) {
                        Err(GitHubError::Auth {
                            host: host.to_string().into(),
                            failure: AuthFailure::NoToken,
                        })
                    } else {
                        Ok(format!("token-{lookup}"))
                    }
                }
            })
        });
        let work_dir: Arc<Path> = Path::new(path!("/project")).into();
        let token = |host: &str, cx: &mut TestAppContext| {
            let host = SharedString::from(host.to_string());
            cx.update(|cx| token_for_host(&host, &project, &work_dir, cx))
        };

        // Two callers at once share one lookup.
        let (first, second) = (token("github.com", cx), token("github.com", cx));
        let (first, second) = (first.await.unwrap(), second.await.unwrap());
        assert_eq!(&*first.value, "token-1");
        assert!(Arc::ptr_eq(&first.value, &second.value));
        assert_eq!(&*token("github.com", cx).await.unwrap().value, "token-1");
        assert_eq!(lookups.load(Ordering::SeqCst), 1);

        assert_eq!(
            &*token("github.example.com", cx).await.unwrap().value,
            "token-2"
        );

        // A token GitHub rejected is looked up again.
        first.revoke();
        let third = token("github.com", cx).await.unwrap();
        assert_eq!(&*third.value, "token-3");

        // So is a failed lookup, once the problem is fixed.
        fail.store(true, Ordering::SeqCst);
        third.revoke();
        assert!(token("github.com", cx).await.is_err());
        fail.store(false, Ordering::SeqCst);
        assert_eq!(&*token("github.com", cx).await.unwrap().value, "token-5");
        assert_eq!(lookups.load(Ordering::SeqCst), 5);
    }

    #[gpui::test]
    async fn test_resolve_forge(cx: &mut TestAppContext) {
        let (project, repository) = project_with_remotes(
            &[
                ("origin", "git@github.com:drew-council/zed.git"),
                ("mirror", "https://gitlab.com/drew-council/zed.git"),
            ],
            cx,
        )
        .await;
        let (http, requests) = fake_github(|request| match request.operation() {
            Some("RepositoryContext") => repository_context_reply(request),
            Some("OpenPullRequests") => Reply::json(OPEN_PULL_REQUESTS),
            _ => Reply::status(500, "{}"),
        });
        cx.update(|cx| {
            cx.set_http_client(http);
            set_fake_token_source(cx, |_| Ok(TOKEN.into()));
        });
        let resolve = |reference: Option<(&str, &str)>, cx: &mut TestAppContext| {
            let reference = reference.map(|(owner, name)| ForgeRepoRef {
                host: None,
                owner: owner.to_string().into(),
                name: name.to_string().into(),
            });
            cx.update(|cx| resolve_forge(&repository, reference, &project, cx))
        };
        let context_requests = |requests: &SentRequests| {
            requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request.operation() == Some("RepositoryContext"))
                .count()
        };

        // A fork's checkout reviews the parent's pull requests.
        let forge = resolve(None, cx).await.unwrap();
        assert_eq!(
            forge.context(),
            &RepositoryContext {
                origin: ForgeRepo {
                    host: "github.com".into(),
                    owner: "drew-council".into(),
                    name: "zed".into(),
                },
                canonical: ForgeRepo {
                    host: "github.com".into(),
                    owner: "zed-industries".into(),
                    name: "zed".into(),
                },
                viewer: "drew-council".into(),
                canonical_https_url: "https://github.com/zed-industries/zed".into(),
                canonical_ssh_url: "git@github.com:zed-industries/zed.git".into(),
                origin_is_ssh: true,
            }
        );
        cx.background_executor
            .spawn(forge.list_pull_requests(PullRequestListFilter::Open))
            .await
            .unwrap();
        let list_variables = requests
            .lock()
            .unwrap()
            .last()
            .map(|request| request.variables().clone());
        assert_eq!(
            list_variables,
            Some(json!({ "owner": "zed-industries", "name": "zed" }))
        );

        // Resolved once.
        let again = resolve(None, cx).await.unwrap();
        assert!(Arc::ptr_eq(&forge, &again));
        assert_eq!(context_requests(&requests), 1);

        // References: the upstream (the fork's parent) or the fork itself.
        let upstream = resolve(Some(("Zed-Industries", "zed")), cx).await.unwrap();
        assert_eq!(
            upstream.context().canonical.to_string(),
            "zed-industries/zed"
        );
        let fork = resolve(Some(("drew-council", "zed")), cx).await.unwrap();
        assert_eq!(fork.context().canonical.to_string(), "drew-council/zed");
        assert_eq!(
            fork.context().canonical_ssh_url,
            "git@github.com:drew-council/zed.git"
        );

        let Err(error) = resolve(Some(("someone", "else")), cx).await else {
            panic!("expected an error");
        };
        assert!(matches!(
            github_error(&error),
            Some(GitHubError::OtherRepository { .. })
        ));
        assert_eq!(
            error_message(&error),
            "Open this pull request from a checkout of someone/else."
        );
    }

    #[gpui::test]
    async fn test_resolve_forge_after_rejected_token(cx: &mut TestAppContext) {
        let (project, repository) =
            project_with_remotes(&[("origin", "https://github.com/zed-industries/zed")], cx).await;
        let reject = Arc::new(AtomicBool::new(false));
        let (http, requests) = fake_github({
            let reject = reject.clone();
            move |request| {
                if reject.load(Ordering::SeqCst) {
                    Reply::status(401, r#"{"message":"Bad credentials"}"#)
                } else {
                    repository_context_reply(request)
                }
            }
        });
        let lookups = Arc::new(AtomicUsize::new(0));
        cx.update(|cx| {
            cx.set_http_client(http);
            set_fake_token_source(cx, {
                let lookups = lookups.clone();
                move |_| Ok(format!("token-{}", lookups.fetch_add(1, Ordering::SeqCst)))
            });
        });
        let resolve = |cx: &mut TestAppContext| {
            cx.update(|cx| resolve_forge(&repository, None, &project, cx))
        };

        let forge = resolve(cx).await.unwrap();
        assert_eq!(forge.context().canonical.to_string(), "zed-industries/zed");
        reject.store(true, Ordering::SeqCst);
        let error = cx
            .background_executor
            .spawn(forge.pull_request(1))
            .await
            .unwrap_err();
        assert_eq!(
            error_message(&error),
            "GitHub authentication failed for github.com. Set GITHUB_TOKEN or run \
             `gh auth login --hostname github.com`."
        );

        // After `gh auth login`, resolving again looks the token up again.
        reject.store(false, Ordering::SeqCst);
        let forge = resolve(cx).await.unwrap();
        assert_eq!(lookups.load(Ordering::SeqCst), 2);
        let last_authorization = requests.lock().unwrap().last().map(|request| {
            request.headers["authorization"]
                .to_str()
                .unwrap()
                .to_string()
        });
        assert_eq!(last_authorization.as_deref(), Some("Bearer token-1"));
        assert_eq!(forge.context().canonical.to_string(), "zed-industries/zed");
    }

    #[gpui::test]
    async fn test_resolve_forge_without_github_remote(cx: &mut TestAppContext) {
        let (project, repository) =
            project_with_remotes(&[("origin", "https://gitlab.com/team/app.git")], cx).await;
        let (http, requests) = fake_github(|_| Reply::status(500, "{}"));
        cx.update(|cx| {
            cx.set_http_client(http);
            set_fake_token_source(cx, |_| Ok(TOKEN.into()));
        });
        let Err(error) = cx
            .update(|cx| resolve_forge(&repository, None, &project, cx))
            .await
        else {
            panic!("expected an error");
        };
        assert!(matches!(
            github_error(&error),
            Some(GitHubError::Unsupported)
        ));
        assert_eq!(
            error_message(&error),
            "This repository has no GitHub remote."
        );
        assert!(requests.lock().unwrap().is_empty());
    }

    /// An `HttpClient` that sends requests with curl, for the live test. The token travels in
    /// curl's configuration on stdin, never in its arguments.
    fn curl_http_client() -> Arc<dyn HttpClient> {
        FakeHttpClient::create(|request| async move {
            let (parts, mut body) = request.into_parts();
            let mut bytes = Vec::new();
            body.read_to_end(&mut bytes).await?;
            let quote = |text: &str| {
                text.replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('\n', "\\n")
                    .replace('\r', "\\r")
                    .replace('\t', "\\t")
            };
            let mut config = format!(
                "url = \"{}\"\nrequest = \"{}\"\nsilent\nshow-error\ninclude\nhttp1.1\nheader = \"Expect:\"\n",
                quote(&parts.uri.to_string()),
                parts.method
            );
            for (name, value) in &parts.headers {
                config.push_str(&format!(
                    "header = \"{name}: {}\"\n",
                    quote(value.to_str()?)
                ));
            }
            if !bytes.is_empty() {
                config.push_str(&format!(
                    "data-binary = \"{}\"\n",
                    quote(std::str::from_utf8(&bytes)?)
                ));
            }
            let mut child = util::command::new_command("curl")
                .args(["--config", "-"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()?;
            let mut stdin = child.stdin.take().context("curl's stdin")?;
            stdin.write_all(config.as_bytes()).await?;
            drop(stdin);
            let output = child.output().await?;
            anyhow::ensure!(
                output.status.success(),
                "curl failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let header_end = output
                .stdout
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .context("no end of the response headers")?;
            let head = std::str::from_utf8(&output.stdout[..header_end])?;
            let mut lines = head.split("\r\n");
            let status: u16 = lines
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .context("no status line")?
                .parse()?;
            let mut response = Response::builder().status(status);
            for line in lines {
                if let Some((name, value)) = line.split_once(':') {
                    response = response.header(name.trim(), value.trim());
                }
            }
            Ok(response.body(AsyncBody::from(output.stdout[header_end + 4..].to_vec()))?)
        })
    }

    /// Reads the sandbox repository's pull requests on GitHub (read-only). Run by hand, with
    /// `GITHUB_TOKEN` set or `gh` logged in:
    /// `cargo test -p code_review live_github -- --ignored`
    #[gpui::test]
    #[ignore = "talks to GitHub"]
    async fn live_github_sandbox(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (project, repository) = project_with_remotes(
            &[(
                "origin",
                "https://github.com/drew-council/code-review-sandbox.git",
            )],
            cx,
        )
        .await;
        cx.update(|cx| cx.set_http_client(curl_http_client()));
        let forge = cx
            .update(|cx| resolve_forge(&repository, None, &project, cx))
            .await
            .unwrap();
        assert_eq!(forge.context().canonical, sandbox());
        assert!(!forge.context().viewer.is_empty());

        let open = forge
            .list_pull_requests(PullRequestListFilter::Open)
            .await
            .unwrap();
        let numbers = open
            .iter()
            .map(|pull_request| pull_request.number)
            .collect::<Vec<_>>();
        assert!(
            [1, 2, 3, 4].iter().all(|number| numbers.contains(number)) && !numbers.contains(&5),
            "{numbers:?}"
        );
        forge
            .list_pull_requests(PullRequestListFilter::ReviewRequested)
            .await
            .unwrap();

        let details = forge.pull_request(1).await.unwrap();
        assert_eq!(details.head_oid.to_string(), PR1_HEAD_OID);
        assert_eq!(details.base_oid.to_string(), PR1_BASE_OID);
        let head = forge.pull_request_head(1).await.unwrap();
        assert_eq!(head.head_oid, details.head_oid);

        let threads = forge.review_threads(1).await.unwrap();
        let anchors = threads
            .iter()
            .map(|thread| (thread.path.as_str(), thread.anchor.clone()))
            .collect::<Vec<_>>();
        assert!(anchors.contains(&(
            "src/engine.rs",
            ThreadAnchor::Line {
                side: DiffSide::Left,
                line: 193
            }
        )));
        assert!(anchors.contains(&("docs/guide.md", ThreadAnchor::File)));
        assert_eq!(threads.len(), 7, "{anchors:?}");
        let summaries = forge.review_summaries(1).await.unwrap();
        assert_eq!(summaries.len(), 2);

        let files = forge.changed_files(4).await.unwrap();
        assert!(files.allows("src/engine.rs", DiffSide::Right, 10));
        assert!(!files.allows("src/engine.rs", DiffSide::Right, 12));

        let error = forge.pull_request(999).await.unwrap_err();
        assert!(matches!(
            github_error(&error),
            Some(GitHubError::NotFound { .. })
        ));
    }
}
