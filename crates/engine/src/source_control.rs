//! Pure source-control remote and branch normalization.
//!
//! Process execution and provider calls intentionally live outside this layer so
//! remote parsing and head selector construction remain deterministic and testable.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::Mutex;

use zeron_proto::{
    ChangeRequestState, ChangeRequestSummary, PullRequestChecksState, PullRequestListItem,
    PullRequestReviewState, SourceControlConnection, Space,
};

const GIT_TIMEOUT: Duration = Duration::from_secs(10);
const GITHUB_TIMEOUT: Duration = Duration::from_secs(20);
const GIT_OUTPUT_LIMIT: usize = 64 * 1024;
const GITHUB_OUTPUT_LIMIT: usize = 1024 * 1024;
const GITHUB_RESULT_LIMIT: &str = "20";
const PULL_REQUEST_RESULT_LIMIT: &str = "50";
const GITHUB_JSON_FIELDS: &str = "number,title,url,state,baseRefName,headRefName,updatedAt,isCrossRepository,headRepositoryOwner,author";
const GITHUB_LIST_JSON_FIELDS: &str = "number,title,url,state,baseRefName,headRefName,updatedAt,isCrossRepository,headRepositoryOwner,author,isDraft,reviewDecision,statusCheckRollup,body,additions,deletions,changedFiles";
const PULL_REQUEST_CACHE_TTL: Duration = Duration::from_secs(60);
const PULL_REQUEST_CACHE_CAPACITY: usize = 256;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PullRequestFetchState {
    #[default]
    Open,
    All,
    Closed,
    Merged,
}

impl PullRequestFetchState {
    fn github_arg(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::All => "all",
            Self::Closed => "closed",
            Self::Merged => "merged",
        }
    }

    fn bitbucket_states(self) -> &'static [&'static str] {
        match self {
            Self::Open => &["OPEN"],
            Self::All => &["OPEN", "MERGED", "DECLINED"],
            Self::Closed => &["DECLINED"],
            Self::Merged => &["MERGED"],
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct PullRequestListOptions {
    pub state: PullRequestFetchState,
    pub authored: bool,
    pub host: Option<String>,
    pub project: Option<String>,
    pub refresh: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PullRequestCacheKey {
    provider: String,
    repository: String,
    state: PullRequestFetchState,
    authored: bool,
}

impl PullRequestCacheKey {
    fn new(
        provider: impl Into<String>,
        repository: impl Into<String>,
        state: PullRequestFetchState,
        authored: bool,
    ) -> Self {
        Self {
            provider: provider.into(),
            repository: repository.into(),
            state,
            authored,
        }
    }
}

struct PullRequestCacheEntry {
    fetched_at: Instant,
    items: Vec<PullRequestListItem>,
}

#[derive(Clone, Default)]
pub struct PullRequestCache {
    entries: Arc<Mutex<HashMap<PullRequestCacheKey, PullRequestCacheEntry>>>,
    bitbucket_user: Arc<Mutex<Option<(Instant, String, Option<BitbucketUser>)>>>,
}

impl PullRequestCache {
    async fn bitbucket_user(
        &self,
        client: &reqwest::Client,
        email: &str,
        token: &str,
    ) -> Option<BitbucketUser> {
        let mut cached = self.bitbucket_user.lock().await;
        if let Some((fetched_at, cached_email, user)) = cached.as_ref()
            && cached_email == email
            && fetched_at.elapsed() < PULL_REQUEST_CACHE_TTL
        {
            return user.clone();
        }
        let user = bitbucket_user(client, email, token).await;
        *cached = Some((Instant::now(), email.to_owned(), user.clone()));
        user
    }

    async fn get_or_fetch<F, Fut>(
        &self,
        key: PullRequestCacheKey,
        refresh: bool,
        fetch: F,
    ) -> Option<Vec<PullRequestListItem>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Option<Vec<PullRequestListItem>>>,
    {
        let mut entries = self.entries.lock().await;
        if !refresh
            && let Some(entry) = entries.get(&key)
            && entry.fetched_at.elapsed() < PULL_REQUEST_CACHE_TTL
        {
            return Some(entry.items.clone());
        }

        let items = fetch().await?;
        if entries.len() >= PULL_REQUEST_CACHE_CAPACITY
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.fetched_at)
                .map(|(key, _)| key.clone())
        {
            entries.remove(&oldest);
        }
        entries.insert(
            key,
            PullRequestCacheEntry {
                fetched_at: Instant::now(),
                items: items.clone(),
            },
        );
        Some(items)
    }
}

/// Probe the two source-control providers currently supported by the UI.
/// GitHub authentication stays in the provider's own CLI; Bitbucket API
/// credentials are read from the process environment and never persisted.
pub async fn list_connections() -> Vec<SourceControlConnection> {
    let runner: Arc<dyn ProcessRunner> = Arc::new(SystemProcessRunner);
    vec![
        probe_connection(
            runner,
            "github",
            "GitHub",
            "gh",
            &["auth", "status", "--hostname", "github.com"],
            &["--version"],
            "gh auth login",
        )
        .await,
        probe_bitbucket_connection().await,
    ]
}

/// List pull requests for Git workspaces hosted on this device.
/// Provider failures are isolated per repository so one disconnected provider
/// does not hide results from the others.
pub async fn list_pull_requests(
    spaces: &[Space],
    device_id: &str,
    options: &PullRequestListOptions,
    cache: &PullRequestCache,
) -> Vec<PullRequestListItem> {
    let runner: Arc<dyn ProcessRunner> = Arc::new(SystemProcessRunner);
    let github = GitHubCli::with_runner(runner);
    let bitbucket = bitbucket_credentials();
    let bitbucket_client = bitbucket.as_ref().and_then(|_| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .ok()
    });
    let bitbucket_user = match (options.authored, &bitbucket, &bitbucket_client) {
        (true, Some((token, email)), Some(client)) => {
            cache.bitbucket_user(client, email, token).await
        }
        _ => None,
    };

    let mut pull_requests = Vec::new();
    for path in pull_request_paths(spaces, device_id) {
        let Some(remote_url) = git_remote_url(&path).await else {
            continue;
        };
        let Some(remote) = parse_git_remote(&remote_url) else {
            continue;
        };
        let repository = format!("{}/{}", remote.owner, remote.repository);
        let provider = match remote.host.as_str() {
            "github.com" => "github",
            "bitbucket.org" => "bitbucket",
            _ => "",
        };
        if options.host.as_deref().is_some_and(|host| host != provider)
            || options
                .project
                .as_deref()
                .is_some_and(|project| project != repository)
        {
            continue;
        }
        match remote.host.as_str() {
            "github.com" => {
                let key = PullRequestCacheKey::new(
                    "github",
                    &repository,
                    options.state,
                    options.authored,
                );
                let checkout_path = path.to_string_lossy().into_owned();
                let repository_for_fetch = repository.clone();
                let github = github.clone();
                if let Some(items) = cache
                    .get_or_fetch(key, options.refresh, move || async move {
                        github
                            .list_for_repository(
                                &checkout_path,
                                &repository_for_fetch,
                                options.state,
                                options.authored,
                            )
                            .await
                            .ok()
                            .map(|items| {
                                items
                                    .into_iter()
                                    .filter_map(|item| {
                                        pull_request_list_item(
                                            "github",
                                            &repository_for_fetch,
                                            item,
                                        )
                                    })
                                    .collect()
                            })
                    })
                    .await
                {
                    pull_requests.extend(items);
                }
            }
            "bitbucket.org" => {
                if let (Some((token, email)), Some(client)) = (&bitbucket, &bitbucket_client) {
                    let key = PullRequestCacheKey::new(
                        "bitbucket",
                        &repository,
                        options.state,
                        options.authored,
                    );
                    let workspace = remote.owner.clone();
                    let repository_name = remote.repository.clone();
                    let repository_for_fetch = repository.clone();
                    let email = email.clone();
                    let token = token.clone();
                    let current_user = bitbucket_user.clone();
                    if let Some(items) = cache
                        .get_or_fetch(key, options.refresh, move || async move {
                            list_bitbucket_pull_requests(
                                client,
                                &workspace,
                                &repository_name,
                                &email,
                                &token,
                                current_user.as_ref(),
                                options.state,
                                options.authored,
                            )
                            .await
                            .ok()
                            .map(|items| {
                                items
                                    .into_iter()
                                    .filter_map(|item| {
                                        bitbucket_pull_request_list_item(
                                            "bitbucket",
                                            &repository_for_fetch,
                                            item,
                                        )
                                    })
                                    .collect()
                            })
                        })
                        .await
                    {
                        pull_requests.extend(items);
                    }
                }
            }
            _ => {}
        }
    }
    pull_requests.sort_by(|left, right| {
        left.repository
            .cmp(&right.repository)
            .then_with(|| right.number.cmp(&left.number))
    });
    pull_requests
}

fn pull_request_paths(spaces: &[Space], device_id: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for space in spaces.iter().filter(|space| space.device_id == device_id) {
        let root = PathBuf::from(&space.path);
        if space.git_detected || root.join(".git").exists() {
            paths.push(root.clone());
        }

        // A project can be a containing folder rather than the checkout root
        // itself (for example, `reception-assistant/reception-assistant`).
        // The project picker intentionally allows those folders, so inspect
        // their direct children for Git checkouts before giving up on PRs.
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() && path.join(".git").exists() {
                    paths.push(path);
                }
            }
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

async fn git_remote_url(path: &Path) -> Option<String> {
    let remotes = tokio::process::Command::new("git")
        .args(["remote"])
        .current_dir(path)
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    if !remotes.status.success() {
        return None;
    }
    let remotes = String::from_utf8_lossy(&remotes.stdout);
    let remote = remotes
        .lines()
        .map(str::trim)
        .find(|remote| *remote == "origin")
        .or_else(|| {
            remotes
                .lines()
                .map(str::trim)
                .find(|remote| !remote.is_empty())
        })?;
    let push = tokio::process::Command::new("git")
        .args(["remote", "get-url", "--push", remote])
        .current_dir(path)
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    if push.status.success() {
        return Some(String::from_utf8_lossy(&push.stdout).trim().to_owned());
    }
    let fetch = tokio::process::Command::new("git")
        .args(["remote", "get-url", remote])
        .current_dir(path)
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    fetch
        .status
        .success()
        .then(|| String::from_utf8_lossy(&fetch.stdout).trim().to_owned())
}

/// Bitbucket does not have a supported local CLI for this integration. Use
/// Bitbucket's API-token basic-auth flow instead, with credentials supplied by
/// the process environment and never persisted by the app.
async fn probe_bitbucket_connection() -> SourceControlConnection {
    let credentials = match bitbucket_credentials() {
        Some((token, email)) => (token, email),
        _ => {
            return SourceControlConnection {
                provider: "bitbucket".into(),
                name: "Bitbucket".into(),
                connected: false,
                account: None,
                cli_version: None,
                detail: "Set BITBUCKET_API_TOKEN and BITBUCKET_EMAIL in the app environment".into(),
            };
        }
    };

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(client) => client,
        Err(_) => return bitbucket_connection_error("Could not initialize Bitbucket API client"),
    };
    let response = match client
        .get("https://api.bitbucket.org/2.0/user")
        .basic_auth(credentials.1, Some(credentials.0))
        .send()
        .await
    {
        Ok(response) => response,
        Err(_) => return bitbucket_connection_error("Could not reach the Bitbucket API"),
    };
    if !response.status().is_success() {
        return bitbucket_connection_error("Bitbucket rejected the API credentials");
    }
    let account = response
        .json::<BitbucketUser>()
        .await
        .ok()
        .and_then(|user| user.display_name.or(user.nickname).or(user.username));
    SourceControlConnection {
        provider: "bitbucket".into(),
        name: "Bitbucket".into(),
        connected: true,
        account,
        cli_version: None,
        detail: "Authenticated via Bitbucket API token".into(),
    }
}

fn bitbucket_connection_error(detail: &str) -> SourceControlConnection {
    SourceControlConnection {
        provider: "bitbucket".into(),
        name: "Bitbucket".into(),
        connected: false,
        account: None,
        cli_version: None,
        detail: detail.into(),
    }
}

#[derive(Clone, Debug, Deserialize)]
struct BitbucketUser {
    uuid: Option<String>,
    display_name: Option<String>,
    nickname: Option<String>,
    username: Option<String>,
}

fn bitbucket_credentials() -> Option<(String, String)> {
    let token = std::env::var("BITBUCKET_API_TOKEN")
        .ok()
        .filter(|value| !value.trim().is_empty())?;
    let email = std::env::var("BITBUCKET_EMAIL")
        .ok()
        .filter(|value| !value.trim().is_empty())?;
    Some((token, email))
}

async fn bitbucket_user(
    client: &reqwest::Client,
    email: &str,
    token: &str,
) -> Option<BitbucketUser> {
    client
        .get("https://api.bitbucket.org/2.0/user")
        .basic_auth(email, Some(token))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()
}

async fn probe_connection(
    runner: Arc<dyn ProcessRunner>,
    provider: &str,
    name: &str,
    program: &str,
    auth_args: &[&str],
    version_args: &[&str],
    login_command: &str,
) -> SourceControlConnection {
    let cwd = PathBuf::from(".");
    let env = if program == "gh" {
        vec![("GH_PROMPT_DISABLED".into(), "1".into())]
    } else {
        Vec::new()
    };
    let auth = runner
        .run(ProcessRequest {
            program: program.into(),
            args: auth_args.iter().map(|arg| (*arg).into()).collect(),
            cwd: cwd.clone(),
            env: env.clone(),
            timeout: Duration::from_secs(8),
            output_limit: 64 * 1024,
        })
        .await;
    let connected = auth.as_ref().is_ok_and(|output| output.success);
    let combined = auth
        .as_ref()
        .map(|output| {
            format!(
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        })
        .unwrap_or_default();
    let account = connected.then(|| account_from_status(&combined)).flatten();
    let cli_version = runner
        .run(ProcessRequest {
            program: program.into(),
            args: version_args.iter().map(|arg| (*arg).into()).collect(),
            cwd,
            env,
            timeout: Duration::from_secs(4),
            output_limit: 16 * 1024,
        })
        .await
        .ok()
        .filter(|output| output.success)
        .and_then(|output| {
            let text = String::from_utf8_lossy(&output.stdout);
            text.lines()
                .next()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
        });
    SourceControlConnection {
        provider: provider.into(),
        name: name.into(),
        connected,
        account,
        cli_version,
        detail: if connected {
            "Connected on this device".into()
        } else {
            format!("Not connected — run {login_command}")
        },
    }
}

fn account_from_status(status: &str) -> Option<String> {
    let lower = status.to_ascii_lowercase();
    let marker = " account ";
    let start = lower.find(marker)? + marker.len();
    let account = status[start..]
        .split_whitespace()
        .next()?
        .trim_matches(['\'', '"', '`', '(', ')', ',']);
    (!account.is_empty()).then(|| account.to_owned())
}

/// Repository identity extracted from a Git remote URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRemote {
    pub host: String,
    pub owner: String,
    pub repository: String,
}

/// Normalized branch context used to query a change request provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchHeadContext {
    pub local_branch: String,
    pub upstream_ref: Option<String>,
    pub remote_name: Option<String>,
    pub remote_url: Option<String>,
    pub host: Option<String>,
    pub repository: Option<String>,
    pub owner: Option<String>,
    pub head_branch: String,
    pub head_selectors: Vec<String>,
}

/// Git metadata needed by a provider to resolve a branch's change request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutSourceContext {
    pub checkout_root: PathBuf,
    pub branch: BranchHeadContext,
    pub default_branch: Option<String>,
}

/// A provider lookup paired with the exact Git context that produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct ChangeRequestResolution {
    pub source: CheckoutSourceContext,
    pub change_request: Option<ChangeRequestSummary>,
}

/// Safe, provider-facing failure classes. These variants never contain command output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ChangeRequestError {
    #[error("Git repository context is unavailable")]
    RepositoryUnavailable,
    #[error("GitHub is unavailable for this repository")]
    UnsupportedRepository,
    #[error("GitHub CLI is not installed")]
    CliUnavailable,
    #[error("GitHub CLI is not authenticated for this host")]
    Authentication,
    #[error("GitHub API rate limit reached")]
    RateLimited,
    #[error("GitHub request timed out")]
    Timeout,
    #[error("GitHub returned an invalid response")]
    Decode,
    #[error("GitHub request failed")]
    CommandFailed,
}

/// Provider boundary shared by the checkout badge and future source-control views.
#[async_trait]
pub trait ChangeRequestProvider: Send + Sync {
    fn provider(&self) -> &'static str;

    async fn find_for_branch(
        &self,
        source: &CheckoutSourceContext,
    ) -> Result<Option<ChangeRequestSummary>, ChangeRequestError>;
}

/// Checkout inspection plus provider resolution, injectable for cache/service tests.
#[async_trait]
pub trait CheckoutChangeRequestLookup: Send + Sync {
    async fn inspect_checkout(
        &self,
        cwd: &Path,
    ) -> Result<CheckoutSourceContext, ChangeRequestError>;

    async fn resolve_github_source(
        &self,
        source: &CheckoutSourceContext,
    ) -> Result<Option<ChangeRequestSummary>, ChangeRequestError>;
}

/// Host-side resolver. All subprocesses run on the device that owns `cwd`.
#[derive(Clone)]
pub struct ChangeRequestResolver {
    inspector: GitCheckoutInspector,
    github: GitHubCli,
}

impl ChangeRequestResolver {
    pub fn new() -> Self {
        let runner: Arc<dyn ProcessRunner> = Arc::new(SystemProcessRunner);
        Self {
            inspector: GitCheckoutInspector::new(runner.clone()),
            github: GitHubCli::with_runner(runner),
        }
    }

    pub async fn resolve_github(
        &self,
        cwd: &Path,
    ) -> Result<ChangeRequestResolution, ChangeRequestError> {
        let source = self.inspect_checkout(cwd).await?;
        let change_request = self.resolve_github_source(&source).await?;
        Ok(ChangeRequestResolution {
            source,
            change_request,
        })
    }

    /// Inspect Git separately so callers can key caches before a provider request.
    pub async fn inspect_checkout(
        &self,
        cwd: &Path,
    ) -> Result<CheckoutSourceContext, ChangeRequestError> {
        self.inspector.inspect(cwd).await
    }

    /// Resolve an already-inspected checkout without repeating Git subprocesses.
    pub async fn resolve_github_source(
        &self,
        source: &CheckoutSourceContext,
    ) -> Result<Option<ChangeRequestSummary>, ChangeRequestError> {
        if source.branch.host.is_none()
            || source.branch.owner.is_none()
            || source.branch.repository.is_none()
        {
            return Err(ChangeRequestError::UnsupportedRepository);
        }
        self.github.find_for_branch(source).await
    }
}

impl Default for ChangeRequestResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CheckoutChangeRequestLookup for ChangeRequestResolver {
    async fn inspect_checkout(
        &self,
        cwd: &Path,
    ) -> Result<CheckoutSourceContext, ChangeRequestError> {
        ChangeRequestResolver::inspect_checkout(self, cwd).await
    }

    async fn resolve_github_source(
        &self,
        source: &CheckoutSourceContext,
    ) -> Result<Option<ChangeRequestSummary>, ChangeRequestError> {
        ChangeRequestResolver::resolve_github_source(self, source).await
    }
}

/// GitHub provider backed by the host's existing `gh` installation and auth.
#[derive(Clone)]
pub struct GitHubCli {
    runner: Arc<dyn ProcessRunner>,
}

impl GitHubCli {
    pub fn new() -> Self {
        Self::with_runner(Arc::new(SystemProcessRunner))
    }

    fn with_runner(runner: Arc<dyn ProcessRunner>) -> Self {
        Self { runner }
    }

    async fn list_for_selector(
        &self,
        source: &CheckoutSourceContext,
        selector: &str,
    ) -> Result<Vec<GhPullRequest>, ChangeRequestError> {
        let request = ProcessRequest {
            program: "gh".into(),
            args: vec![
                "pr".into(),
                "list".into(),
                "--head".into(),
                selector.into(),
                "--state".into(),
                "all".into(),
                "--limit".into(),
                GITHUB_RESULT_LIMIT.into(),
                "--json".into(),
                GITHUB_JSON_FIELDS.into(),
            ],
            cwd: source.checkout_root.clone(),
            env: vec![("GH_PROMPT_DISABLED".into(), "1".into())],
            timeout: GITHUB_TIMEOUT,
            output_limit: GITHUB_OUTPUT_LIMIT,
        };
        let output = self.runner.run(request).await.map_err(classify_run_error)?;
        if !output.success {
            return Err(classify_github_failure(&output.stderr));
        }
        if output.stdout_truncated {
            return Err(ChangeRequestError::Decode);
        }
        serde_json::from_slice(&output.stdout).map_err(|_| ChangeRequestError::Decode)
    }

    async fn list_for_repository(
        &self,
        checkout_path: &str,
        repository: &str,
        state: PullRequestFetchState,
        authored: bool,
    ) -> Result<Vec<GhPullRequest>, ChangeRequestError> {
        let mut args = vec![
            "pr".into(),
            "list".into(),
            "--repo".into(),
            repository.into(),
            "--state".into(),
            state.github_arg().into(),
            "--limit".into(),
            PULL_REQUEST_RESULT_LIMIT.into(),
        ];
        if authored {
            args.extend(["--author".into(), "@me".into()]);
        }
        args.extend(["--json".into(), GITHUB_LIST_JSON_FIELDS.into()]);
        let output = self
            .runner
            .run(ProcessRequest {
                program: "gh".into(),
                args,
                cwd: PathBuf::from(checkout_path),
                env: vec![("GH_PROMPT_DISABLED".into(), "1".into())],
                timeout: GITHUB_TIMEOUT,
                output_limit: GITHUB_OUTPUT_LIMIT,
            })
            .await
            .map_err(classify_run_error)?;
        if !output.success {
            return Err(classify_github_failure(&output.stderr));
        }
        if output.stdout_truncated {
            return Err(ChangeRequestError::Decode);
        }
        serde_json::from_slice(&output.stdout).map_err(|_| ChangeRequestError::Decode)
    }

    /// Resolve the remote's default branch through the provider API.
    ///
    /// This deliberately never uses Git remote transport (`git ls-remote`)
    /// against the watched checkout, which would honor repository-controlled
    /// transport and credential configuration. `gh` queries the
    /// already-sanitized host/owner/repository identity with its own
    /// credentials. Failures degrade to "unknown", matching the pre-existing
    /// behavior when a default branch cannot be determined.
    async fn default_branch(&self, source: &CheckoutSourceContext) -> Option<String> {
        let host = source.branch.host.as_deref()?;
        let owner = source.branch.owner.as_deref()?;
        let repository = source.branch.repository.as_deref()?;
        let request = ProcessRequest {
            program: "gh".into(),
            args: vec![
                "repo".into(),
                "view".into(),
                format!("{host}/{owner}/{repository}"),
                "--json".into(),
                "defaultBranchRef".into(),
            ],
            cwd: source.checkout_root.clone(),
            env: vec![("GH_PROMPT_DISABLED".into(), "1".into())],
            timeout: GITHUB_TIMEOUT,
            output_limit: GITHUB_OUTPUT_LIMIT,
        };
        let output = self.runner.run(request).await.ok()?;
        if !output.success || output.stdout_truncated {
            return None;
        }
        let view: GhRepoView = serde_json::from_slice(&output.stdout).ok()?;
        view.default_branch_ref
            .map(|reference| reference.name)
            .filter(|name| !name.is_empty())
    }
}

impl Default for GitHubCli {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ChangeRequestProvider for GitHubCli {
    fn provider(&self) -> &'static str {
        "github"
    }

    async fn find_for_branch(
        &self,
        source: &CheckoutSourceContext,
    ) -> Result<Option<ChangeRequestSummary>, ChangeRequestError> {
        if source.branch.host.is_none()
            || source.branch.owner.is_none()
            || source.branch.repository.is_none()
        {
            return Err(ChangeRequestError::UnsupportedRepository);
        }
        if source.branch.head_selectors.is_empty() {
            return Ok(None);
        }

        for selector in &source.branch.head_selectors {
            let candidates = self.list_for_selector(source, selector).await?;
            if candidates.is_empty() {
                continue;
            }
            let default_branch = match source.default_branch.clone() {
                Some(branch) => Some(branch),
                None if needs_default_branch(source, &candidates) => {
                    self.default_branch(source).await
                }
                None => None,
            };
            return select_pull_request(
                self.provider(),
                source,
                default_branch.as_deref(),
                candidates,
            );
        }
        Ok(None)
    }
}

impl BranchHeadContext {
    /// Build provider head selectors from already-inspected Git branch metadata.
    pub fn resolve(
        local_branch: impl Into<String>,
        upstream_ref: Option<&str>,
        remote_name: Option<&str>,
        remote_url: Option<&str>,
    ) -> Self {
        let local_branch = local_branch.into();
        let upstream_ref = non_empty(upstream_ref).map(str::to_owned);
        let remote_name = non_empty(remote_name)
            .map(str::to_owned)
            .or_else(|| upstream_ref.as_deref().and_then(remote_from_upstream));
        let remote_url = non_empty(remote_url).map(str::to_owned);
        let parsed_remote = remote_url.as_deref().and_then(parse_git_remote);
        let upstream_branch = upstream_ref
            .as_deref()
            .and_then(|upstream| branch_from_upstream(upstream, remote_name.as_deref()));
        let head_branch = upstream_branch.unwrap_or(&local_branch).to_owned();

        let mut head_selectors = Vec::new();
        if let Some(remote) = parsed_remote.as_ref() {
            push_unique(
                &mut head_selectors,
                format!("{}:{head_branch}", remote.owner),
            );
        }
        if upstream_ref.is_some() {
            push_unique(&mut head_selectors, head_branch.clone());
        }
        if upstream_ref.is_none() || local_branch == head_branch {
            push_unique(&mut head_selectors, local_branch.clone());
        }

        Self {
            local_branch,
            upstream_ref,
            remote_name,
            remote_url,
            host: parsed_remote.as_ref().map(|remote| remote.host.clone()),
            repository: parsed_remote
                .as_ref()
                .map(|remote| remote.repository.clone()),
            owner: parsed_remote.map(|remote| remote.owner),
            head_branch,
            head_selectors,
        }
    }
}

/// Parse the SSH scp-like, SSH URL, and HTTP(S) forms commonly used by GitHub.
///
/// The host is intentionally not restricted to `github.com`: GitHub Enterprise
/// installations use arbitrary hostnames and are resolved later by the provider.
pub fn parse_git_remote(remote_url: &str) -> Option<GitRemote> {
    let remote_url = remote_url.trim();
    if remote_url.is_empty() || remote_url.chars().any(char::is_whitespace) {
        return None;
    }

    let (host, path) = if let Some((scheme, remainder)) = remote_url.split_once("://") {
        if !matches!(
            scheme.to_ascii_lowercase().as_str(),
            "ssh" | "http" | "https"
        ) {
            return None;
        }
        let (authority, path) = remainder.split_once('/')?;
        (host_from_authority(authority)?, path)
    } else {
        let (authority, path) = remote_url.split_once(':')?;
        if authority.contains('/') || path.starts_with('/') {
            return None;
        }
        (host_from_authority(authority)?, path)
    };

    let path = path.trim_matches('/');
    let mut segments = path.split('/');
    let owner = segments.next()?;
    let repository = segments.next()?;
    if segments.next().is_some() || owner.is_empty() || repository.is_empty() {
        return None;
    }
    let repository = repository.strip_suffix(".git").unwrap_or(repository);
    if repository.is_empty() {
        return None;
    }

    Some(GitRemote {
        host: host.to_ascii_lowercase(),
        owner: owner.to_owned(),
        repository: repository.to_owned(),
    })
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn host_from_authority(authority: &str) -> Option<&str> {
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = host.split_once(':').map_or(host, |(host, _)| host);
    (!host.is_empty()).then_some(host)
}

fn remote_from_upstream(upstream: &str) -> Option<String> {
    normalized_upstream(upstream)
        .split_once('/')
        .map(|(remote, _)| remote.to_owned())
}

fn branch_from_upstream<'a>(upstream: &'a str, remote_name: Option<&str>) -> Option<&'a str> {
    let upstream = normalized_upstream(upstream);
    if let Some(remote_name) = remote_name {
        let prefix = format!("{remote_name}/");
        if let Some(branch) = upstream.strip_prefix(&prefix) {
            return (!branch.is_empty()).then_some(branch);
        }
    }
    upstream
        .split_once('/')
        .and_then(|(_, branch)| (!branch.is_empty()).then_some(branch))
}

fn normalized_upstream(upstream: &str) -> &str {
    upstream.strip_prefix("refs/remotes/").unwrap_or(upstream)
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
    }
}

#[derive(Clone)]
struct GitCheckoutInspector {
    runner: Arc<dyn ProcessRunner>,
}

impl GitCheckoutInspector {
    fn new(runner: Arc<dyn ProcessRunner>) -> Self {
        Self { runner }
    }

    async fn inspect(&self, cwd: &Path) -> Result<CheckoutSourceContext, ChangeRequestError> {
        let checkout_root_value = self
            .git_required(cwd, &["rev-parse", "--show-toplevel"])
            .await?;
        if checkout_root_value.is_empty() {
            return Err(ChangeRequestError::RepositoryUnavailable);
        }
        let checkout_root = PathBuf::from(checkout_root_value);
        let local_branch = self
            .git_required(&checkout_root, &["branch", "--show-current"])
            .await?;
        if local_branch.is_empty() || local_branch == "HEAD" {
            return Err(ChangeRequestError::RepositoryUnavailable);
        }

        let upstream_ref = self
            .git_optional(
                &checkout_root,
                &[
                    "rev-parse",
                    "--abbrev-ref",
                    "--symbolic-full-name",
                    "@{upstream}",
                ],
            )
            .await;
        let branch_remote_key = format!("branch.{local_branch}.remote");
        let mut remote_name = self
            .git_optional(&checkout_root, &["config", "--get", &branch_remote_key])
            .await
            .filter(|remote| remote != ".");
        if remote_name.is_none() {
            remote_name = upstream_ref.as_deref().and_then(remote_from_upstream);
        }
        if remote_name.is_none() {
            remote_name = self
                .git_optional(&checkout_root, &["config", "--get", "remote.pushDefault"])
                .await;
        }
        if remote_name.is_none()
            && let Some(remotes) = self.git_optional(&checkout_root, &["remote"]).await
        {
            let remotes: Vec<_> = remotes
                .lines()
                .filter(|remote| !remote.is_empty())
                .collect();
            // `origin` is Git's conventional default destination when no
            // branch/upstream or push-default configuration is present. It is
            // still the useful PR source for a local branch in a fork +
            // upstream checkout.
            remote_name = remotes
                .iter()
                .find(|remote| **remote == "origin")
                .map(|remote| (*remote).to_owned())
                .or_else(|| (remotes.len() == 1).then(|| remotes[0].to_owned()));
        }

        let remote_url = if let Some(remote) = remote_name.as_deref() {
            self.git_optional(&checkout_root, &["remote", "get-url", "--push", remote])
                .await
        } else {
            None
        };
        let remote_url = if remote_url.is_none() {
            if let Some(remote) = remote_name.as_deref() {
                self.git_optional(&checkout_root, &["remote", "get-url", remote])
                    .await
            } else {
                None
            }
        } else {
            remote_url
        };

        let branch = BranchHeadContext::resolve(
            local_branch,
            upstream_ref.as_deref(),
            remote_name.as_deref(),
            remote_url.as_deref(),
        );
        // The default branch is read from local refs only. `refs/remotes/
        // <remote>/HEAD` is a fetch-time cache and may be absent; the provider
        // resolves the remote's default branch itself when needed. Never fall
        // back to `git ls-remote` here: it executes repository-controlled
        // transport and credential configuration (for example
        // `core.sshCommand`), which would let any watched checkout run
        // arbitrary commands on the host.
        let default_branch = if let Some(remote) = branch.remote_name.as_deref() {
            let remote_head = format!("refs/remotes/{remote}/HEAD");
            self.git_optional(
                &checkout_root,
                &["symbolic-ref", "--quiet", "--short", &remote_head],
            )
            .await
            .and_then(|reference| branch_from_upstream(&reference, Some(remote)).map(str::to_owned))
        } else {
            None
        };

        Ok(CheckoutSourceContext {
            checkout_root,
            branch,
            default_branch,
        })
    }

    async fn git_required(&self, cwd: &Path, args: &[&str]) -> Result<String, ChangeRequestError> {
        self.git(cwd, args)
            .await
            .ok_or(ChangeRequestError::RepositoryUnavailable)
    }

    async fn git_optional(&self, cwd: &Path, args: &[&str]) -> Option<String> {
        self.git(cwd, args).await
    }

    async fn git(&self, cwd: &Path, args: &[&str]) -> Option<String> {
        let output = self
            .runner
            .run(ProcessRequest {
                program: "git".into(),
                args: args.iter().map(|arg| (*arg).to_owned()).collect(),
                cwd: cwd.to_owned(),
                env: Vec::new(),
                timeout: GIT_TIMEOUT,
                output_limit: GIT_OUTPUT_LIMIT,
            })
            .await
            .ok()?;
        if !output.success || output.stdout_truncated {
            return None;
        }
        String::from_utf8(output.stdout)
            .ok()
            .map(|value| value.trim().to_owned())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhRepoView {
    default_branch_ref: Option<GhBranchRef>,
}

#[derive(Debug, Deserialize)]
struct GhBranchRef {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhPullRequest {
    number: u64,
    title: String,
    url: String,
    state: GhPullRequestState,
    base_ref_name: String,
    head_ref_name: String,
    updated_at: DateTime<Utc>,
    is_cross_repository: bool,
    head_repository_owner: GhRepositoryOwner,
    #[serde(default)]
    author: Option<GhPullRequestAuthor>,
    #[serde(default)]
    is_draft: bool,
    #[serde(default)]
    review_decision: Option<String>,
    #[serde(default)]
    status_check_rollup: Vec<GhCheckStatus>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    additions: u64,
    #[serde(default)]
    deletions: u64,
    #[serde(default)]
    changed_files: u64,
}

#[derive(Debug, Deserialize)]
struct GhCheckStatus {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum GhPullRequestState {
    Open,
    Closed,
    Merged,
}

impl From<GhPullRequestState> for ChangeRequestState {
    fn from(state: GhPullRequestState) -> Self {
        match state {
            GhPullRequestState::Open => Self::Open,
            GhPullRequestState::Closed => Self::Closed,
            GhPullRequestState::Merged => Self::Merged,
        }
    }
}

#[derive(Debug, Deserialize)]
struct GhRepositoryOwner {
    login: String,
}

#[derive(Debug, Deserialize)]
struct GhPullRequestAuthor {
    login: String,
}

#[derive(Debug, Deserialize)]
struct BitbucketPullRequestsResponse {
    values: Vec<BitbucketPullRequest>,
}

#[derive(Debug, Deserialize)]
struct BitbucketPullRequest {
    id: u64,
    title: String,
    state: String,
    links: BitbucketPullRequestLinks,
    source: BitbucketBranchContainer,
    destination: BitbucketBranchContainer,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    author: Option<BitbucketUser>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    diffstat: Option<BitbucketDiffstat>,
}

#[derive(Debug, Default, Deserialize)]
struct BitbucketDiffstat {
    #[serde(default)]
    lines_added: u64,
    #[serde(default)]
    lines_removed: u64,
    #[serde(default)]
    files_changed: u64,
}

#[derive(Debug, Deserialize)]
struct BitbucketPullRequestLinks {
    html: BitbucketHref,
}

#[derive(Debug, Deserialize)]
struct BitbucketHref {
    href: String,
}

#[derive(Debug, Deserialize)]
struct BitbucketBranchContainer {
    branch: Option<BitbucketBranch>,
}

#[derive(Debug, Deserialize)]
struct BitbucketBranch {
    name: String,
}

async fn list_bitbucket_pull_requests(
    client: &reqwest::Client,
    workspace: &str,
    repository: &str,
    email: &str,
    token: &str,
    current_user: Option<&BitbucketUser>,
    state: PullRequestFetchState,
    authored: bool,
) -> Result<Vec<BitbucketPullRequest>, ()> {
    let url =
        format!("https://api.bitbucket.org/2.0/repositories/{workspace}/{repository}/pullrequests");
    let mut query: Vec<(&str, String)> = state
        .bitbucket_states()
        .iter()
        .map(|state| ("state", (*state).to_owned()))
        .collect();
    query.push(("pagelen", PULL_REQUEST_RESULT_LIMIT.to_owned()));
    if authored {
        let Some(uuid) = current_user.and_then(|user| user.uuid.as_deref()) else {
            return Ok(Vec::new());
        };
        query.push(("q", format!("author.uuid=\"{uuid}\"")));
    }
    let response = client
        .get(url)
        .query(&query)
        .basic_auth(email, Some(token))
        .send()
        .await
        .map_err(|_| ())?
        .error_for_status()
        .map_err(|_| ())?
        .json::<BitbucketPullRequestsResponse>()
        .await
        .map_err(|_| ())?;
    let mut values = response.values;
    if authored {
        let Some(uuid) = current_user.and_then(|user| user.uuid.as_deref()) else {
            return Ok(Vec::new());
        };
        values.retain(|item| {
            item.author
                .as_ref()
                .and_then(|author| author.uuid.as_deref())
                .is_some_and(|author_uuid| author_uuid == uuid)
        });
    }
    Ok(values)
}

fn pull_request_list_item(
    provider: &str,
    repository: &str,
    pull_request: GhPullRequest,
) -> Option<PullRequestListItem> {
    let url = reqwest::Url::parse(pull_request.url.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    Some(PullRequestListItem {
        provider: provider.into(),
        repository: repository.into(),
        number: pull_request.number,
        title: pull_request.title,
        url: url.to_string(),
        state: pull_request.state.into(),
        base_ref: pull_request.base_ref_name,
        head_ref: pull_request.head_ref_name,
        draft: pull_request.is_draft,
        review: github_review_state(pull_request.review_decision.as_deref()),
        checks: github_checks_state(&pull_request.status_check_rollup),
        author: pull_request.author.map(|author| author.login),
        description: pull_request.body,
        additions: pull_request.additions,
        deletions: pull_request.deletions,
        changed_files: pull_request.changed_files,
    })
}

fn bitbucket_pull_request_list_item(
    provider: &str,
    repository: &str,
    pull_request: BitbucketPullRequest,
) -> Option<PullRequestListItem> {
    let url = reqwest::Url::parse(pull_request.links.html.href.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    let base_ref = pull_request.destination.branch?.name;
    let head_ref = pull_request.source.branch?.name;
    if pull_request.title.trim().is_empty()
        || base_ref.trim().is_empty()
        || head_ref.trim().is_empty()
    {
        return None;
    }
    let state = match pull_request.state.to_ascii_uppercase().as_str() {
        "OPEN" => ChangeRequestState::Open,
        "MERGED" => ChangeRequestState::Merged,
        _ => ChangeRequestState::Closed,
    };
    let diffstat = pull_request.diffstat.unwrap_or_default();
    Some(PullRequestListItem {
        provider: provider.into(),
        repository: repository.into(),
        number: pull_request.id,
        title: pull_request.title,
        url: url.to_string(),
        state,
        base_ref,
        head_ref,
        draft: pull_request.draft,
        review: PullRequestReviewState::None,
        checks: PullRequestChecksState::None,
        author: pull_request
            .author
            .and_then(|author| author.display_name.or(author.nickname).or(author.username)),
        description: pull_request.description,
        additions: diffstat.lines_added,
        deletions: diffstat.lines_removed,
        changed_files: diffstat.files_changed,
    })
}

fn github_review_state(decision: Option<&str>) -> PullRequestReviewState {
    match decision.unwrap_or_default().to_ascii_uppercase().as_str() {
        "APPROVED" => PullRequestReviewState::Approved,
        "CHANGES_REQUESTED" => PullRequestReviewState::ChangesRequested,
        "REVIEW_REQUIRED" => PullRequestReviewState::Required,
        _ => PullRequestReviewState::None,
    }
}

fn github_checks_state(checks: &[GhCheckStatus]) -> PullRequestChecksState {
    if checks.is_empty() {
        return PullRequestChecksState::None;
    }
    let mut pending = false;
    let mut failing = false;
    for check in checks {
        let conclusion = check
            .conclusion
            .as_deref()
            .unwrap_or_default()
            .to_ascii_uppercase();
        if matches!(
            conclusion.as_str(),
            "FAILURE" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED" | "STARTUP_FAILURE"
        ) {
            failing = true;
        } else if check
            .status
            .as_deref()
            .is_none_or(|status| status != "COMPLETED")
        {
            pending = true;
        }
    }
    if failing {
        PullRequestChecksState::Failing
    } else if pending {
        PullRequestChecksState::Pending
    } else {
        PullRequestChecksState::Passing
    }
}

/// The default branch only matters to suppress historical terminal pull
/// requests on the default branch itself; skip the extra provider request
/// unless a terminal candidate could actually be suppressed.
fn needs_default_branch(source: &CheckoutSourceContext, candidates: &[GhPullRequest]) -> bool {
    candidates.iter().any(|candidate| {
        candidate.head_ref_name == source.branch.head_branch
            && !matches!(candidate.state, GhPullRequestState::Open)
    })
}

fn select_pull_request(
    provider: &str,
    source: &CheckoutSourceContext,
    default_branch: Option<&str>,
    candidates: Vec<GhPullRequest>,
) -> Result<Option<ChangeRequestSummary>, ChangeRequestError> {
    let expected_owner = source.branch.owner.as_deref();
    let on_default_branch = default_branch == Some(&source.branch.head_branch);
    let mut matching: Vec<GhPullRequest> = candidates
        .into_iter()
        .filter(|candidate| candidate.head_ref_name == source.branch.head_branch)
        .filter(|candidate| {
            expected_owner.is_some_and(|owner| {
                candidate
                    .head_repository_owner
                    .login
                    .eq_ignore_ascii_case(owner)
            }) || (expected_owner.is_none() && !candidate.is_cross_repository)
        })
        .filter(|candidate| {
            !on_default_branch || matches!(candidate.state, GhPullRequestState::Open)
        })
        .collect();

    matching.sort_by(|left, right| {
        let left_open = matches!(left.state, GhPullRequestState::Open);
        let right_open = matches!(right.state, GhPullRequestState::Open);
        right_open
            .cmp(&left_open)
            .then_with(|| right.updated_at.cmp(&left.updated_at))
            .then_with(|| right.number.cmp(&left.number))
    });

    matching
        .into_iter()
        .next()
        .map(|pull_request| to_summary(provider, pull_request))
        .transpose()
}

fn to_summary(
    provider: &str,
    pull_request: GhPullRequest,
) -> Result<ChangeRequestSummary, ChangeRequestError> {
    if pull_request.number == 0
        || pull_request.title.trim().is_empty()
        || pull_request.base_ref_name.trim().is_empty()
        || pull_request.head_ref_name.trim().is_empty()
    {
        return Err(ChangeRequestError::Decode);
    }
    let url =
        reqwest::Url::parse(pull_request.url.trim()).map_err(|_| ChangeRequestError::Decode)?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(ChangeRequestError::Decode);
    }

    Ok(ChangeRequestSummary {
        provider: provider.to_owned(),
        number: pull_request.number,
        title: pull_request.title,
        url: url.to_string(),
        state: pull_request.state.into(),
        base_ref: pull_request.base_ref_name,
        head_ref: pull_request.head_ref_name,
    })
}

fn classify_github_failure(stderr: &[u8]) -> ChangeRequestError {
    let stderr = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    if stderr.contains("rate limit") || stderr.contains("secondary rate") {
        ChangeRequestError::RateLimited
    } else if stderr.contains("not logged")
        || stderr.contains("authentication")
        || stderr.contains("authenticate")
        || stderr.contains("gh auth login")
        || stderr.contains("bad credentials")
    {
        ChangeRequestError::Authentication
    } else {
        ChangeRequestError::CommandFailed
    }
}

fn classify_run_error(error: ProcessRunError) -> ChangeRequestError {
    match error {
        ProcessRunError::Spawn(io::ErrorKind::NotFound) => ChangeRequestError::CliUnavailable,
        ProcessRunError::Timeout => ChangeRequestError::Timeout,
        ProcessRunError::Spawn(_) | ProcessRunError::Io => ChangeRequestError::CommandFailed,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcessRequest {
    program: String,
    args: Vec<String>,
    cwd: PathBuf,
    env: Vec<(String, String)>,
    timeout: Duration,
    output_limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcessOutput {
    success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessRunError {
    Spawn(io::ErrorKind),
    Timeout,
    Io,
}

#[async_trait]
trait ProcessRunner: Send + Sync {
    async fn run(&self, request: ProcessRequest) -> Result<ProcessOutput, ProcessRunError>;
}

struct SystemProcessRunner;

#[async_trait]
impl ProcessRunner for SystemProcessRunner {
    async fn run(&self, request: ProcessRequest) -> Result<ProcessOutput, ProcessRunError> {
        let mut command = tokio::process::Command::new(&request.program);
        if request.program == "gh" {
            zeron_harness::compose_login_shell_path(&mut command);
        }
        command
            .args(&request.args)
            .current_dir(&request.cwd)
            .envs(request.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|error| ProcessRunError::Spawn(error.kind()))?;
        let stdout = child.stdout.take().ok_or(ProcessRunError::Io)?;
        let stderr = child.stderr.take().ok_or(ProcessRunError::Io)?;
        let completed = tokio::time::timeout(request.timeout, async {
            tokio::try_join!(
                child.wait(),
                read_capped(stdout, request.output_limit),
                read_capped(stderr, request.output_limit),
            )
        })
        .await;

        let (status, (stdout, stdout_truncated), (stderr, _stderr_truncated)) = match completed {
            Ok(Ok(output)) => output,
            Ok(Err(_)) => return Err(ProcessRunError::Io),
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(ProcessRunError::Timeout);
            }
        };
        Ok(ProcessOutput {
            success: status.success(),
            stdout,
            stderr,
            stdout_truncated,
        })
    }
}

async fn read_capped(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> io::Result<(Vec<u8>, bool)> {
    let mut output = Vec::with_capacity(limit.min(16 * 1024));
    let mut buffer = [0_u8; 8 * 1024];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(output.len());
        let keep = read.min(remaining);
        output.extend_from_slice(&buffer[..keep]);
        truncated |= keep < read;
    }
    Ok((output, truncated))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    use tempfile::tempdir;

    use super::*;

    #[derive(Default)]
    struct FakeProcessRunner {
        requests: Mutex<Vec<ProcessRequest>>,
        responses: Mutex<VecDeque<Result<ProcessOutput, ProcessRunError>>>,
    }

    impl FakeProcessRunner {
        fn with_responses(
            responses: impl IntoIterator<Item = Result<ProcessOutput, ProcessRunError>>,
        ) -> Arc<Self> {
            Arc::new(Self {
                requests: Mutex::new(Vec::new()),
                responses: Mutex::new(responses.into_iter().collect()),
            })
        }

        fn requests(&self) -> Vec<ProcessRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ProcessRunner for FakeProcessRunner {
        async fn run(&self, request: ProcessRequest) -> Result<ProcessOutput, ProcessRunError> {
            self.requests.lock().unwrap().push(request);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("missing fake process response")
        }
    }

    struct ExecutableGhRunner {
        executable: PathBuf,
    }

    #[async_trait]
    impl ProcessRunner for ExecutableGhRunner {
        async fn run(&self, mut request: ProcessRequest) -> Result<ProcessOutput, ProcessRunError> {
            if request.program == "gh" {
                request.program = self.executable.to_string_lossy().into_owned();
            }
            SystemProcessRunner.run(request).await
        }
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git fixture command starts");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn command_success(stdout: impl Into<Vec<u8>>) -> Result<ProcessOutput, ProcessRunError> {
        Ok(ProcessOutput {
            success: true,
            stdout: stdout.into(),
            stderr: Vec::new(),
            stdout_truncated: false,
        })
    }

    fn command_failure(stderr: &str) -> Result<ProcessOutput, ProcessRunError> {
        Ok(ProcessOutput {
            success: false,
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
            stdout_truncated: false,
        })
    }

    fn source(branch: &str, owner: &str, default_branch: Option<&str>) -> CheckoutSourceContext {
        CheckoutSourceContext {
            checkout_root: PathBuf::from("/checkout"),
            branch: BranchHeadContext::resolve(
                branch,
                Some(&format!("origin/{branch}")),
                Some("origin"),
                Some(&format!("https://github.com/{owner}/zeron.git")),
            ),
            default_branch: default_branch.map(str::to_owned),
        }
    }

    #[test]
    fn pull_request_paths_use_local_git_spaces() {
        let spaces = vec![zeron_proto::Space {
            id: "space-1".into(),
            device_id: "device-1".into(),
            path: "/workspace/project".into(),
            name: None,
            git_detected: true,
            git_checked_at: None,
            checkout_id: None,
            created_at: Utc::now(),
        }];

        assert_eq!(
            pull_request_paths(&spaces, "device-1"),
            vec![PathBuf::from("/workspace/project")]
        );
    }

    #[test]
    fn pull_request_paths_find_a_git_checkout_nested_in_a_project_folder() {
        let root = tempdir().expect("temporary project folder");
        let checkout = root.path().join("reception-assistant");
        std::fs::create_dir_all(&checkout).expect("nested checkout directory");
        run_git(&checkout, &["init", "--quiet"]);

        let spaces = vec![zeron_proto::Space {
            id: "space-1".into(),
            device_id: "device-1".into(),
            path: root.path().display().to_string(),
            name: Some("reception-assistant".into()),
            git_detected: false,
            git_checked_at: None,
            checkout_id: None,
            created_at: Utc::now(),
        }];

        assert_eq!(
            pull_request_paths(&spaces, "device-1"),
            vec![checkout],
            "PR discovery should inspect the checkout nested in a project folder"
        );
    }

    fn pull_request(
        number: u64,
        state: &str,
        owner: &str,
        branch: &str,
        updated_at: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "number": number,
            "title": format!("Pull request {number}"),
            "url": format!("https://github.com/acme/zeron/pull/{number}"),
            "state": state,
            "baseRefName": "main",
            "headRefName": branch,
            "updatedAt": updated_at,
            "isCrossRepository": owner != "acme",
            "headRepositoryOwner": { "login": owner }
        })
    }

    async fn resolve_with(
        source: &CheckoutSourceContext,
        response: Result<ProcessOutput, ProcessRunError>,
    ) -> (
        Result<Option<ChangeRequestSummary>, ChangeRequestError>,
        Arc<FakeProcessRunner>,
    ) {
        let runner = FakeProcessRunner::with_responses([response]);
        let github = GitHubCli::with_runner(runner.clone());
        let result = github.find_for_branch(source).await;
        (result, runner)
    }

    #[tokio::test]
    async fn github_cli_uses_exact_arguments_and_checkout_cwd() {
        let source = source("feature/status", "acme", Some("main"));
        let json = serde_json::to_vec(&vec![pull_request(
            90,
            "OPEN",
            "acme",
            "feature/status",
            "2026-08-15T12:00:00Z",
        )])
        .unwrap();
        let (result, runner) = resolve_with(&source, command_success(json)).await;

        let summary = result.unwrap().unwrap();
        assert_eq!(summary.number, 90);
        assert_eq!(summary.state, ChangeRequestState::Open);
        assert_eq!(summary.provider, "github");
        let requests = runner.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].program, "gh");
        assert_eq!(requests[0].cwd, Path::new("/checkout"));
        assert_eq!(
            requests[0].args,
            [
                "pr",
                "list",
                "--head",
                "acme:feature/status",
                "--state",
                "all",
                "--limit",
                "20",
                "--json",
                GITHUB_JSON_FIELDS,
            ]
        );
        assert_eq!(requests[0].env, [("GH_PROMPT_DISABLED".into(), "1".into())]);
        assert_eq!(requests[0].timeout, GITHUB_TIMEOUT);
        assert_eq!(requests[0].output_limit, GITHUB_OUTPUT_LIMIT);
    }

    #[tokio::test]
    async fn github_repository_listing_uses_requested_state() {
        let runner = FakeProcessRunner::with_responses([command_success(b"[]".to_vec())]);
        let github = GitHubCli::with_runner(runner.clone());

        github
            .list_for_repository(
                "/checkout",
                "acme/zeron",
                PullRequestFetchState::Open,
                false,
            )
            .await
            .unwrap();

        let requests = runner.requests();
        assert_eq!(
            requests[0].args,
            [
                "pr",
                "list",
                "--repo",
                "acme/zeron",
                "--state",
                "open",
                "--limit",
                PULL_REQUEST_RESULT_LIMIT,
                "--json",
                GITHUB_LIST_JSON_FIELDS,
            ]
        );
    }

    #[tokio::test]
    async fn pull_request_cache_reuses_fresh_results() {
        let cache = PullRequestCache::default();
        let key =
            PullRequestCacheKey::new("github", "acme/zeron", PullRequestFetchState::Open, false);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let first = cache
            .get_or_fetch(key.clone(), false, {
                let calls = calls.clone();
                move || async move {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Some(Vec::new())
                }
            })
            .await;
        let second = cache
            .get_or_fetch(key, false, {
                let calls = calls.clone();
                move || async move {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Some(Vec::new())
                }
            })
            .await;

        assert_eq!(first, Some(Vec::new()));
        assert_eq!(second, Some(Vec::new()));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        let refreshed = cache
            .get_or_fetch(
                PullRequestCacheKey::new(
                    "github",
                    "acme/zeron",
                    PullRequestFetchState::Open,
                    false,
                ),
                true,
                {
                    let calls = calls.clone();
                    move || async move {
                        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Some(Vec::new())
                    }
                },
            )
            .await;
        assert_eq!(refreshed, Some(Vec::new()));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn repository_listing_requests_all_fields_required_by_decoder() {
        for field in [
            "updatedAt",
            "isCrossRepository",
            "headRepositoryOwner",
            "isDraft",
            "reviewDecision",
            "statusCheckRollup",
        ] {
            assert!(
                GITHUB_LIST_JSON_FIELDS
                    .split(',')
                    .any(|candidate| candidate == field),
                "repository listing is missing {field}"
            );
        }
    }

    #[tokio::test]
    async fn real_git_checkout_resolves_through_host_fake_gh() {
        let temp = tempfile::tempdir().expect("fixture tempdir");
        let checkout = temp.path().join("checkout");
        std::fs::create_dir_all(&checkout).expect("checkout directory");
        run_git(&checkout, &["init", "-q", "-b", "main"]);
        run_git(&checkout, &["checkout", "-q", "-b", "feature/status"]);
        run_git(
            &checkout,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/acme/zeron.git",
            ],
        );
        assert_eq!(
            git_remote_url(&checkout).await.as_deref(),
            Some("https://github.com/acme/zeron.git")
        );

        let fake_gh = temp.path().join("gh");
        std::fs::write(
            &fake_gh,
            r##"#!/bin/sh
if [ "$GH_PROMPT_DISABLED" != "1" ]; then
  echo "interactive auth was not disabled" >&2
  exit 2
fi
printf '%s\n' '[{"number":90,"title":"Host-resolved pull request","url":"https://github.com/acme/zeron/pull/90","state":"OPEN","baseRefName":"main","headRefName":"feature/status","updatedAt":"2026-08-15T12:00:00Z","isCrossRepository":false,"headRepositoryOwner":{"login":"acme"}}]'
"##,
        )
        .expect("write fake gh");
        let mut permissions = std::fs::metadata(&fake_gh)
            .expect("fake gh metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, permissions).expect("make fake gh executable");

        let runner: Arc<dyn ProcessRunner> = Arc::new(ExecutableGhRunner {
            executable: fake_gh,
        });
        let resolver = ChangeRequestResolver {
            inspector: GitCheckoutInspector::new(runner.clone()),
            github: GitHubCli::with_runner(runner),
        };

        let resolution = resolver
            .resolve_github(&checkout)
            .await
            .expect("host resolves fake GitHub pull request");

        assert_eq!(
            std::fs::canonicalize(&resolution.source.checkout_root).expect("resolved root"),
            std::fs::canonicalize(&checkout).expect("fixture root")
        );
        assert_eq!(resolution.source.branch.local_branch, "feature/status");
        assert_eq!(resolution.source.branch.owner.as_deref(), Some("acme"));
        let pull_request = resolution.change_request.expect("pull request");
        assert_eq!(pull_request.number, 90);
        assert_eq!(pull_request.state, ChangeRequestState::Open);
        assert_eq!(pull_request.head_ref, "feature/status");
    }

    #[tokio::test]
    async fn git_inspector_uses_origin_without_upstream_in_a_multi_remote_checkout() {
        let temp = tempfile::tempdir().expect("fixture tempdir");
        let checkout = temp.path().join("checkout");
        std::fs::create_dir_all(&checkout).expect("checkout directory");
        run_git(&checkout, &["init", "-q", "-b", "main"]);
        run_git(&checkout, &["checkout", "-q", "-b", "feature/no-upstream"]);
        run_git(
            &checkout,
            &[
                "remote",
                "add",
                "origin",
                "git@github.com:contributor/zeron.git",
            ],
        );
        run_git(
            &checkout,
            &[
                "remote",
                "add",
                "upstream",
                "https://github.com/acme/zeron.git",
            ],
        );

        let source = ChangeRequestResolver::new()
            .inspect_checkout(&checkout)
            .await
            .expect("inspect checkout without branch upstream");

        assert_eq!(source.branch.upstream_ref, None);
        assert_eq!(source.branch.remote_name.as_deref(), Some("origin"));
        assert_eq!(
            source.branch.remote_url.as_deref(),
            Some("git@github.com:contributor/zeron.git")
        );
        assert_eq!(source.branch.owner.as_deref(), Some("contributor"));
        assert_eq!(
            source.branch.head_selectors,
            ["contributor:feature/no-upstream", "feature/no-upstream"]
        );
    }

    #[tokio::test]
    async fn git_inspector_never_runs_remote_transport_for_an_absent_default_branch() {
        let temp = tempfile::tempdir().expect("fixture tempdir");
        let checkout = temp.path().join("checkout");
        std::fs::create_dir_all(&checkout).expect("checkout directory");
        run_git(&checkout, &["init", "-q", "-b", "main"]);
        run_git(
            &checkout,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.com",
                "commit",
                "--allow-empty",
                "-qm",
                "fixture",
            ],
        );
        run_git(
            &checkout,
            &["remote", "add", "origin", "git@github.com:acme/zeron.git"],
        );
        // A repository-controlled transport command: any Git subcommand that
        // touches the remote (such as the former `ls-remote` default-branch
        // fallback) would execute it and create the marker.
        let marker = temp.path().join("transport-ran");
        run_git(
            &checkout,
            &[
                "config",
                "core.sshCommand",
                &format!("sh -c 'touch {}; exit 1'", marker.display()),
            ],
        );

        let source = ChangeRequestResolver::new()
            .inspect_checkout(&checkout)
            .await
            .expect("inspect checkout without a cached remote HEAD");

        assert_eq!(source.branch.remote_name.as_deref(), Some("origin"));
        assert_eq!(
            source.default_branch, None,
            "an unfetched default branch stays unknown until the provider resolves it"
        );
        assert!(
            !marker.exists(),
            "checkout inspection must never invoke Git remote transport"
        );
    }

    #[tokio::test]
    async fn merged_pull_request_is_returned_when_no_open_exists() {
        let source = source("feature/status", "acme", Some("main"));
        let json = serde_json::to_vec(&vec![pull_request(
            91,
            "MERGED",
            "acme",
            "feature/status",
            "2026-08-15T12:00:00Z",
        )])
        .unwrap();

        let (result, _) = resolve_with(&source, command_success(json)).await;
        let summary = result.unwrap().unwrap();
        assert_eq!(summary.number, 91);
        assert_eq!(summary.state, ChangeRequestState::Merged);
    }

    #[tokio::test]
    async fn closed_pull_request_is_returned_when_no_open_exists() {
        let source = source("feature/status", "acme", Some("main"));
        let json = serde_json::to_vec(&vec![pull_request(
            92,
            "CLOSED",
            "acme",
            "feature/status",
            "2026-08-15T12:00:00Z",
        )])
        .unwrap();

        let (result, _) = resolve_with(&source, command_success(json)).await;
        assert_eq!(result.unwrap().unwrap().state, ChangeRequestState::Closed);
    }

    #[tokio::test]
    async fn open_pull_request_wins_over_newer_terminal_candidates() {
        let source = source("feature/status", "acme", Some("main"));
        let json = serde_json::to_vec(&vec![
            pull_request(
                93,
                "MERGED",
                "acme",
                "feature/status",
                "2026-08-15T14:00:00Z",
            ),
            pull_request(94, "OPEN", "acme", "feature/status", "2026-08-15T10:00:00Z"),
            pull_request(
                95,
                "CLOSED",
                "acme",
                "feature/status",
                "2026-08-15T15:00:00Z",
            ),
        ])
        .unwrap();

        let (result, _) = resolve_with(&source, command_success(json)).await;
        assert_eq!(result.unwrap().unwrap().number, 94);
    }

    #[tokio::test]
    async fn newest_terminal_pull_request_wins_without_an_open_candidate() {
        let source = source("feature/status", "acme", Some("main"));
        let json = serde_json::to_vec(&vec![
            pull_request(
                96,
                "MERGED",
                "acme",
                "feature/status",
                "2026-08-15T12:00:00Z",
            ),
            pull_request(
                97,
                "CLOSED",
                "acme",
                "feature/status",
                "2026-08-15T13:00:00Z",
            ),
        ])
        .unwrap();

        let (result, _) = resolve_with(&source, command_success(json)).await;
        assert_eq!(result.unwrap().unwrap().number, 97);
    }

    #[tokio::test]
    async fn empty_specific_lookup_uses_the_next_safe_selector() {
        let source = source("feature/status", "acme", Some("main"));
        let json = serde_json::to_vec(&vec![pull_request(
            100,
            "OPEN",
            "acme",
            "feature/status",
            "2026-08-15T12:00:00Z",
        )])
        .unwrap();
        let runner = FakeProcessRunner::with_responses([
            command_success(b"[]".to_vec()),
            command_success(json),
        ]);
        let github = GitHubCli::with_runner(runner.clone());

        let result = github.find_for_branch(&source).await.unwrap().unwrap();

        assert_eq!(result.number, 100);
        let requests = runner.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].args[3], "acme:feature/status");
        assert_eq!(requests[1].args[3], "feature/status");
    }

    #[tokio::test]
    async fn pull_request_from_another_fork_is_rejected() {
        let source = source("feature/status", "contributor", Some("main"));
        let json = serde_json::to_vec(&vec![pull_request(
            98,
            "OPEN",
            "someone-else",
            "feature/status",
            "2026-08-15T12:00:00Z",
        )])
        .unwrap();

        let (result, _) = resolve_with(&source, command_success(json)).await;
        assert_eq!(result.unwrap(), None);
    }

    #[tokio::test]
    async fn partial_or_invalid_json_is_classified_as_decode() {
        let source = source("feature/status", "acme", Some("main"));
        for json in [br#"[{"number":90}]"#.to_vec(), b"not-json".to_vec()] {
            let (result, _) = resolve_with(&source, command_success(json)).await;
            assert_eq!(result.unwrap_err(), ChangeRequestError::Decode);
        }
    }

    #[tokio::test]
    async fn truncated_json_is_classified_as_decode() {
        let source = source("feature/status", "acme", Some("main"));
        let runner = FakeProcessRunner::with_responses([Ok(ProcessOutput {
            success: true,
            stdout: b"[]".to_vec(),
            stderr: Vec::new(),
            stdout_truncated: true,
        })]);
        let github = GitHubCli::with_runner(runner);

        let result = github.find_for_branch(&source).await;

        assert_eq!(result.unwrap_err(), ChangeRequestError::Decode);
    }

    #[tokio::test]
    async fn missing_github_executable_is_classified() {
        let source = source("feature/status", "acme", Some("main"));
        let (result, _) = resolve_with(
            &source,
            Err(ProcessRunError::Spawn(io::ErrorKind::NotFound)),
        )
        .await;
        assert_eq!(result.unwrap_err(), ChangeRequestError::CliUnavailable);
    }

    #[tokio::test]
    async fn authentication_failure_is_classified_without_exposing_stderr() {
        let source = source("feature/status", "acme", Some("main"));
        let secret_stderr = "not logged into github.example.com; token secret-value";
        let (result, _) = resolve_with(&source, command_failure(secret_stderr)).await;
        let error = result.unwrap_err();
        assert_eq!(error, ChangeRequestError::Authentication);
        assert!(!error.to_string().contains("secret-value"));
    }

    #[tokio::test]
    async fn rate_limit_failure_is_classified() {
        let source = source("feature/status", "acme", Some("main"));
        let (result, _) =
            resolve_with(&source, command_failure("GraphQL: API rate limit exceeded")).await;
        assert_eq!(result.unwrap_err(), ChangeRequestError::RateLimited);
    }

    #[tokio::test]
    async fn generic_github_failure_is_classified_without_exposing_stderr() {
        let source = source("feature/status", "acme", Some("main"));
        let (result, _) = resolve_with(
            &source,
            command_failure("provider failed with sensitive details"),
        )
        .await;
        let error = result.unwrap_err();
        assert_eq!(error, ChangeRequestError::CommandFailed);
        assert!(!error.to_string().contains("sensitive details"));
    }

    #[tokio::test]
    async fn timeout_is_classified() {
        let source = source("feature/status", "acme", Some("main"));
        let (result, _) = resolve_with(&source, Err(ProcessRunError::Timeout)).await;
        assert_eq!(result.unwrap_err(), ChangeRequestError::Timeout);
    }

    #[tokio::test]
    async fn terminal_pull_request_is_suppressed_on_default_branch() {
        let source = source("main", "acme", Some("main"));
        let json = serde_json::to_vec(&vec![pull_request(
            99,
            "MERGED",
            "acme",
            "main",
            "2026-08-15T12:00:00Z",
        )])
        .unwrap();

        let (result, _) = resolve_with(&source, command_success(json)).await;
        assert_eq!(result.unwrap(), None);
    }

    #[tokio::test]
    async fn unknown_default_branch_resolves_through_the_provider_before_suppression() {
        let source = source("main", "acme", None);
        let list = serde_json::to_vec(&vec![pull_request(
            99,
            "MERGED",
            "acme",
            "main",
            "2026-08-15T12:00:00Z",
        )])
        .unwrap();
        let runner = FakeProcessRunner::with_responses([
            command_success(list),
            command_success(br#"{"defaultBranchRef":{"name":"main"}}"#.to_vec()),
        ]);
        let github = GitHubCli::with_runner(runner.clone());

        let result = github.find_for_branch(&source).await;

        assert_eq!(result.unwrap(), None, "historical PR on main is suppressed");
        let requests = runner.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].program, "gh");
        assert_eq!(
            requests[1].args,
            [
                "repo",
                "view",
                "github.com/acme/zeron",
                "--json",
                "defaultBranchRef"
            ]
        );
        assert_eq!(requests[1].env, [("GH_PROMPT_DISABLED".into(), "1".into())]);
    }

    #[tokio::test]
    async fn open_candidates_skip_the_provider_default_branch_lookup() {
        let source = source("feature/status", "acme", None);
        let json = serde_json::to_vec(&vec![pull_request(
            90,
            "OPEN",
            "acme",
            "feature/status",
            "2026-08-15T12:00:00Z",
        )])
        .unwrap();

        let (result, runner) = resolve_with(&source, command_success(json)).await;

        assert_eq!(result.unwrap().unwrap().number, 90);
        assert_eq!(runner.requests().len(), 1);
    }

    #[tokio::test]
    async fn provider_default_branch_failure_keeps_the_terminal_pull_request() {
        let source = source("main", "acme", None);
        let list = serde_json::to_vec(&vec![pull_request(
            99,
            "MERGED",
            "acme",
            "main",
            "2026-08-15T12:00:00Z",
        )])
        .unwrap();
        let runner = FakeProcessRunner::with_responses([
            command_success(list),
            command_failure("could not resolve repository"),
        ]);
        let github = GitHubCli::with_runner(runner);

        let result = github.find_for_branch(&source).await;

        assert_eq!(result.unwrap().unwrap().number, 99);
    }

    #[tokio::test]
    async fn git_inspector_reads_branch_upstream_remote_and_default_branch() {
        let runner = FakeProcessRunner::with_responses([
            command_success("/checkout\n"),
            command_success("feature/status\n"),
            command_success("fork/published-status\n"),
            command_success("fork\n"),
            command_success("git@github.com:contributor/zeron.git\n"),
            command_success("fork/main\n"),
        ]);
        let inspector = GitCheckoutInspector::new(runner.clone());

        let source = inspector.inspect(Path::new("/nested/path")).await.unwrap();

        assert_eq!(source.checkout_root, Path::new("/checkout"));
        assert_eq!(source.branch.local_branch, "feature/status");
        assert_eq!(source.branch.head_branch, "published-status");
        assert_eq!(source.branch.owner.as_deref(), Some("contributor"));
        assert_eq!(source.default_branch.as_deref(), Some("main"));
        let requests = runner.requests();
        assert_eq!(requests[0].cwd, Path::new("/nested/path"));
        assert_eq!(requests[0].args, ["rev-parse", "--show-toplevel"]);
        assert_eq!(requests[4].args, ["remote", "get-url", "--push", "fork"]);
    }

    #[test]
    fn parses_supported_git_remote_forms() {
        for (url, expected) in [
            (
                "git@github.com:owner/repo.git",
                GitRemote {
                    host: "github.com".into(),
                    owner: "owner".into(),
                    repository: "repo".into(),
                },
            ),
            (
                "ssh://git@github.com/owner/repo.git",
                GitRemote {
                    host: "github.com".into(),
                    owner: "owner".into(),
                    repository: "repo".into(),
                },
            ),
            (
                "https://github.com/owner/repo",
                GitRemote {
                    host: "github.com".into(),
                    owner: "owner".into(),
                    repository: "repo".into(),
                },
            ),
            (
                "https://user@github.example.com/owner/repo.git/",
                GitRemote {
                    host: "github.example.com".into(),
                    owner: "owner".into(),
                    repository: "repo".into(),
                },
            ),
        ] {
            assert_eq!(parse_git_remote(url), Some(expected), "remote: {url}");
        }
    }

    #[test]
    fn rejects_unsupported_or_malformed_remotes() {
        for url in [
            "file:///tmp/repo",
            "/tmp/repo",
            "git://github.com/owner/repo.git",
            "https://github.com/owner",
            "https://github.com/owner/repo/extra",
            "",
        ] {
            assert_eq!(parse_git_remote(url), None, "remote: {url}");
        }
    }

    #[test]
    fn fork_remote_owner_selector_has_priority() {
        let context = BranchHeadContext::resolve(
            "local-name",
            Some("fork/published-name"),
            Some("fork"),
            Some("git@github.com:contributor/zeron.git"),
        );

        assert_eq!(context.host.as_deref(), Some("github.com"));
        assert_eq!(context.owner.as_deref(), Some("contributor"));
        assert_eq!(context.repository.as_deref(), Some("zeron"));
        assert_eq!(context.head_branch, "published-name");
        assert_eq!(
            context.head_selectors,
            ["contributor:published-name", "published-name"]
        );
    }

    #[test]
    fn branch_without_upstream_keeps_safe_local_fallback() {
        let context = BranchHeadContext::resolve(
            "feature/local",
            None,
            Some("origin"),
            Some("https://github.com/acme/zeron.git"),
        );

        assert_eq!(context.head_branch, "feature/local");
        assert_eq!(
            context.head_selectors,
            ["acme:feature/local", "feature/local"]
        );
    }

    #[test]
    fn matching_local_and_upstream_branches_are_deduplicated() {
        let context = BranchHeadContext::resolve(
            "feature/shared",
            Some("refs/remotes/origin/feature/shared"),
            Some("origin"),
            Some("https://github.com/acme/zeron"),
        );

        assert_eq!(context.remote_name.as_deref(), Some("origin"));
        assert_eq!(
            context.head_selectors,
            ["acme:feature/shared", "feature/shared"]
        );
    }
}
