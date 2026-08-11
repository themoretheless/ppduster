//! Read-only discovery of repositories available to the authenticated GitHub user.
//!
//! Authentication is delegated entirely to GitHub CLI. This module never reads,
//! accepts, or persists a GitHub token.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

const GH_FALLBACK_PATHS: &[&str] = &["/opt/homebrew/bin/gh", "/usr/local/bin/gh"];
const GH_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const GH_STDOUT_LIMIT: usize = 8 * 1024 * 1024;
const GH_STDERR_LIMIT: usize = 64 * 1024;
const GH_POLL_INTERVAL: Duration = Duration::from_millis(10);
const GH_DISPLAY_ERROR_LIMIT: usize = 640;
const GH_LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const GH_AUTH_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const GH_AUTH_PROBE_POLL_INTERVAL: Duration = Duration::from_secs(1);
const GH_AUTH_PROBE_OUTPUT_LIMIT: usize = 64 * 1024;
const GH_DEVICE_FLOW_URL: &str = "https://github.com/login/device";

const ACCESSIBLE_REPOSITORIES_QUERY: &str = r#"
query($endCursor: String) {
  viewer {
    login
    repositories(
      first: 100
      after: $endCursor
      affiliations: [OWNER, COLLABORATOR, ORGANIZATION_MEMBER]
    ) {
      nodes {
        id
        name
        nameWithOwner
        url
        sshUrl
        isPrivate
        isArchived
        defaultBranchRef {
          name
        }
        mainBranch: ref(qualifiedName: "refs/heads/main") {
          name
        }
        owner {
          login
          ... on User {
            name
          }
          ... on Organization {
            name
          }
        }
      }
      pageInfo {
        hasNextPage
        endCursor
      }
    }
  }
}
"#;

// Keep the probe structurally aligned with repository discovery while limiting
// it to one node. A successful OAuth status alone does not prove organization
// SSO or GraphQL field access.
const REPOSITORY_ACCESS_PROBE_QUERY: &str = r#"
query {
  viewer {
    login
    repositories(
      first: 1
      affiliations: [OWNER, COLLABORATOR, ORGANIZATION_MEMBER]
    ) {
      nodes {
        id
        name
        nameWithOwner
        url
        sshUrl
        isPrivate
        isArchived
        defaultBranchRef {
          name
        }
        mainBranch: ref(qualifiedName: "refs/heads/main") {
          name
        }
        owner {
          login
          ... on User {
            name
          }
          ... on Organization {
            name
          }
        }
      }
    }
  }
}
"#;

/// Repository metadata needed by the scenario repository picker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubRepository {
    pub id: String,
    pub name: String,
    pub name_with_owner: String,
    pub url: String,
    pub ssh_url: String,
    pub is_private: bool,
    pub is_archived: bool,
    pub default_branch: Option<String>,
    /// The `main` branch when it exists. Scenario repository sync targets this
    /// branch to match the clone-or-update contract.
    pub main_branch: Option<String>,
    /// Stable GitHub owner login used in repository paths.
    pub owner: String,
    /// Optional human-readable user or organization name.
    pub owner_name: Option<String>,
}

/// Authenticated GitHub account together with every repository visible to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubAccountRepositories {
    pub login: String,
    pub repositories: Vec<GithubRepository>,
}

/// List repositories visible to the user authenticated through GitHub CLI.
///
/// The call is noninteractive and asks `gh api graphql` to paginate all
/// repositories owned by the viewer or available through collaboration and
/// organization membership. GitHub CLI remains solely responsible for its
/// credentials; this function never reads or stores a token.
pub fn list_accessible_repositories() -> Result<Vec<GithubRepository>> {
    Ok(get_account_repositories()?.repositories)
}

/// Return the authenticated account login and its accessible repositories.
pub fn get_account_repositories() -> Result<GithubAccountRepositories> {
    get_account_repositories_inner().map_err(|error| {
        anyhow::anyhow!(sanitize_command_detail(
            &format!("{error:#}"),
            GH_DISPLAY_ERROR_LIMIT,
        ))
    })
}

/// Authenticate GitHub CLI through its browser/device flow.
///
/// `gh` owns the OAuth exchange and credential storage. The one-time code is
/// copied to the clipboard before the browser opens, so the desktop UI never
/// needs to read or display credentials.
pub fn login_via_web() -> Result<()> {
    login_via_web_with_device_flow_ready(|| {})
}

/// A thread-safe, idempotent cancellation signal for GitHub authentication.
///
/// Clone this value before moving it into a background login task. Calling
/// [`cancel`](Self::cancel) on any clone asks the task to stop its owned GitHub
/// CLI process tree and reap the direct child before returning.
#[derive(Clone, Debug, Default)]
pub struct GithubLoginCancellation {
    cancelled: Arc<AtomicBool>,
}

impl GithubLoginCancellation {
    /// Create a cancellation signal in its initial, non-cancelled state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation. Repeated calls are harmless.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Return whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// Terminal result of a cancellation-aware GitHub login attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubLoginOutcome {
    /// Repository access was already available or authentication completed.
    Authenticated,
    /// The caller requested cancellation before success was observed.
    Cancelled,
}

/// Authenticate GitHub CLI and report when its browser/device flow is ready.
///
/// GitHub CLI prints the fixed device-flow URL instead of opening it when its
/// standard streams are not attached to a terminal. `on_ready` fires once,
/// after that exact trusted URL appears on stderr, so a GUI caller can open the
/// browser without accepting an arbitrary URL from command output.
pub fn login_via_web_with_device_flow_ready(
    on_ready: impl FnOnce() + Send + 'static,
) -> Result<()> {
    let outcome = login_via_web_with_device_flow_ready_and_cancellation(
        on_ready,
        GithubLoginCancellation::new(),
    )?;
    debug_assert_eq!(outcome, GithubLoginOutcome::Authenticated);
    Ok(())
}

/// Authenticate GitHub CLI with a caller-controlled cancellation signal.
///
/// Cancellation is returned as [`GithubLoginOutcome::Cancelled`], not as an
/// authentication error. Once this function has observed successful repository
/// access or a successful login process, that success takes precedence over a
/// concurrent cancellation request.
pub fn login_via_web_with_device_flow_ready_and_cancellation(
    on_ready: impl FnOnce() + Send + 'static,
    cancellation: GithubLoginCancellation,
) -> Result<GithubLoginOutcome> {
    if cancellation.is_cancelled() {
        return Ok(GithubLoginOutcome::Cancelled);
    }
    let gh = discover_gh().ok_or_else(|| {
        anyhow::anyhow!(
            "GitHub CLI (`gh`) was not found in PATH, /opt/homebrew/bin, or /usr/local/bin. Install it and try again."
        )
    })?;
    login_via_web_with_gh_and_device_flow_ready_and_cancellation(&gh, on_ready, cancellation)
}

#[cfg(test)]
fn login_via_web_with_gh_and_device_flow_ready(
    gh: &Path,
    on_ready: impl FnOnce() + Send + 'static,
) -> Result<()> {
    let outcome = login_via_web_with_gh_and_device_flow_ready_and_cancellation(
        gh,
        on_ready,
        GithubLoginCancellation::new(),
    )?;
    debug_assert_eq!(outcome, GithubLoginOutcome::Authenticated);
    Ok(())
}

fn login_via_web_with_gh_and_device_flow_ready_and_cancellation(
    gh: &Path,
    on_ready: impl FnOnce() + Send + 'static,
    cancellation: GithubLoginCancellation,
) -> Result<GithubLoginOutcome> {
    // Avoid starting a second device flow only after exercising the same
    // repository GraphQL fields that the caller needs. OAuth status/scopes do
    // not by themselves prove organization SSO access. An unavailable
    // preflight is not an authentication failure: start the flow and keep
    // probing so a separately completed Terminal login can still satisfy it.
    match github_repository_access_is_ready_with_cancellation(gh, &cancellation) {
        Ok(CancellationAware::Completed(true)) => {
            return Ok(GithubLoginOutcome::Authenticated);
        }
        Ok(CancellationAware::Cancelled) => return Ok(GithubLoginOutcome::Cancelled),
        Ok(CancellationAware::Completed(false)) | Err(_) => {}
    }
    if cancellation.is_cancelled() {
        return Ok(GithubLoginOutcome::Cancelled);
    }
    let outcome = run_login_via_web_command_with_device_flow_ready_and_cancellation(
        gh,
        CommandLimits {
            timeout: GH_LOGIN_TIMEOUT,
            stdout_bytes: GH_STDERR_LIMIT,
            stderr_bytes: GH_STDERR_LIMIT,
        },
        on_ready,
        cancellation,
    )?;
    if outcome.cancelled {
        return Ok(GithubLoginOutcome::Cancelled);
    }
    if outcome.externally_satisfied {
        return Ok(GithubLoginOutcome::Authenticated);
    }
    let output = outcome.output;
    if output.stdout_truncated || output.stderr_truncated {
        bail!("GitHub authorization produced too much diagnostic output");
    }
    if !output.status.success() {
        let detail = classify_gh_failure(output.status, &output.stderr);
        bail!("GitHub authorization failed: {detail}");
    }
    Ok(GithubLoginOutcome::Authenticated)
}

#[cfg(test)]
fn run_login_via_web_command(gh: &Path, limits: CommandLimits) -> Result<BoundedOutput> {
    Ok(run_login_via_web_command_with_auth_probe(
        gh,
        limits,
        GH_AUTH_PROBE_POLL_INTERVAL,
        true,
        || {},
        || Ok(false),
    )?
    .output)
}

fn run_login_via_web_command_with_device_flow_ready_and_cancellation(
    gh: &Path,
    limits: CommandLimits,
    on_ready: impl FnOnce() + Send + 'static,
    cancellation: GithubLoginCancellation,
) -> Result<LoginCommandOutcome> {
    let gh_for_probe = gh.to_path_buf();
    let cancellation_for_probe = cancellation.clone();
    run_login_via_web_command_with_auth_probe_and_cancellation(
        gh,
        limits,
        GH_AUTH_PROBE_POLL_INTERVAL,
        true,
        on_ready,
        cancellation,
        move || match github_repository_access_is_ready_with_cancellation(
            &gh_for_probe,
            &cancellation_for_probe,
        )? {
            CancellationAware::Completed(ready) => Ok(ready),
            CancellationAware::Cancelled => Ok(false),
        },
    )
}

#[cfg(test)]
fn run_login_via_web_command_with_auth_probe(
    gh: &Path,
    limits: CommandLimits,
    auth_poll_interval: Duration,
    allow_external_satisfaction: bool,
    on_ready: impl FnOnce() + Send + 'static,
    auth_probe: impl FnMut() -> Result<bool> + Send + 'static,
) -> Result<LoginCommandOutcome> {
    run_login_via_web_command_with_auth_probe_and_cancellation(
        gh,
        limits,
        auth_poll_interval,
        allow_external_satisfaction,
        on_ready,
        GithubLoginCancellation::new(),
        auth_probe,
    )
}

fn run_login_via_web_command_with_auth_probe_and_cancellation(
    gh: &Path,
    limits: CommandLimits,
    auth_poll_interval: Duration,
    allow_external_satisfaction: bool,
    on_ready: impl FnOnce() + Send + 'static,
    cancellation: GithubLoginCancellation,
    auth_probe: impl FnMut() -> Result<bool> + Send + 'static,
) -> Result<LoginCommandOutcome> {
    if !gh.is_absolute() {
        bail!("refusing to run GitHub CLI from a non-absolute path");
    }
    let mut command = Command::new(gh);
    command
        .args([
            "auth",
            "login",
            "--hostname",
            "github.com",
            "--git-protocol",
            "https",
            "--web",
            "--clipboard",
        ])
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let cancellation_for_ready = cancellation.clone();
    run_login_command(
        command,
        limits,
        auth_poll_interval,
        allow_external_satisfaction,
        Some((
            GH_DEVICE_FLOW_URL.as_bytes(),
            Box::new(move || {
                if !cancellation_for_ready.is_cancelled() {
                    on_ready();
                }
            }),
        )),
        cancellation,
        auth_probe,
    )
}

fn get_account_repositories_inner() -> Result<GithubAccountRepositories> {
    let gh = discover_gh().ok_or_else(|| {
        anyhow::anyhow!(
            "GitHub CLI (`gh`) was not found in PATH, /opt/homebrew/bin, or /usr/local/bin. Install it and authenticate with `gh auth login`."
        )
    })?;

    let output = run_gh_command(
        &gh,
        CommandLimits {
            timeout: GH_COMMAND_TIMEOUT,
            stdout_bytes: GH_STDOUT_LIMIT,
            stderr_bytes: GH_STDERR_LIMIT,
        },
    )?;

    if output.stdout_truncated {
        bail!(
            "GitHub repository discovery returned more than {} MiB; narrow the authenticated account's repository access and try again",
            GH_STDOUT_LIMIT / (1024 * 1024)
        );
    }
    if output.stderr_truncated {
        bail!(
            "GitHub repository discovery exceeded the diagnostic-output limit; the excess output was discarded safely"
        );
    }

    if !output.status.success() {
        let detail = classify_gh_failure(output.status, &output.stderr);
        bail!(
            "GitHub repository discovery failed: {detail}. Check `gh auth status`; authenticate or refresh access with `gh auth login`."
        );
    }

    parse_account_repositories(&output.stdout)
}

#[derive(Debug, Clone, Copy)]
struct CommandLimits {
    timeout: Duration,
    stdout_bytes: usize,
    stderr_bytes: usize,
}

#[derive(Debug)]
struct BoundedOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

#[derive(Debug)]
struct LoginCommandOutcome {
    output: BoundedOutput,
    externally_satisfied: bool,
    cancelled: bool,
}

enum CancellationAware<T> {
    Completed(T),
    Cancelled,
}

#[derive(Debug)]
struct BoundedBytes {
    bytes: Vec<u8>,
    truncated: bool,
}

fn github_repository_access_is_ready_with_cancellation(
    gh: &Path,
    cancellation: &GithubLoginCancellation,
) -> Result<CancellationAware<bool>> {
    let output = match run_gh_repository_access_probe_command_with_cancellation(
        gh,
        CommandLimits {
            timeout: GH_AUTH_PROBE_TIMEOUT,
            stdout_bytes: GH_AUTH_PROBE_OUTPUT_LIMIT,
            stderr_bytes: GH_AUTH_PROBE_OUTPUT_LIMIT,
        },
        cancellation,
    )? {
        CancellationAware::Completed(output) => output,
        CancellationAware::Cancelled => return Ok(CancellationAware::Cancelled),
    };
    if output.stdout_truncated || output.stderr_truncated {
        bail!("GitHub repository access probe produced too much diagnostic output");
    }
    if !output.status.success() {
        bail!("GitHub repository access could not be verified");
    }
    parse_repository_access_probe(&output.stdout).map(CancellationAware::Completed)
}

fn parse_repository_access_probe(input: &[u8]) -> Result<bool> {
    let body = parse_included_response_body(input)?;
    let response: GraphqlPage =
        serde_json::from_slice(body).context("parse GitHub repository access probe")?;
    if !response.errors.is_empty() {
        bail!("GitHub repository access probe was rejected");
    }
    let data = response
        .data
        .ok_or_else(|| anyhow::anyhow!("GitHub repository access probe returned no data"))?;
    if data.viewer.login.trim().is_empty() {
        bail!("GitHub repository access probe returned an empty viewer login");
    }
    Ok(true)
}

fn parse_included_response_body(input: &[u8]) -> Result<&[u8]> {
    let mut line_start = 0;
    let mut line_number = 0;

    loop {
        let newline_offset = input[line_start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "GitHub repository access probe returned no HTTP header/body boundary"
                )
            })?;
        let line_end = line_start + newline_offset;
        let mut line = &input[line_start..line_end];
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        line_start = line_end + 1;

        if line_number == 0 {
            validate_included_http_status_line(line)?;
            line_number += 1;
            continue;
        }

        if line.is_empty() {
            return Ok(&input[line_start..]);
        }

        let Some(colon) = line.iter().position(|byte| *byte == b':') else {
            bail!("GitHub repository access probe returned a malformed HTTP header");
        };
        let name = &line[..colon];
        if name.is_empty() || !name.iter().all(|byte| is_http_header_name_byte(*byte)) {
            bail!("GitHub repository access probe returned a malformed HTTP header name");
        }

        if name.eq_ignore_ascii_case(b"x-github-sso") {
            let value = trim_ascii_whitespace(&line[colon + 1..]);
            let state =
                trim_ascii_whitespace(value.split(|byte| *byte == b';').next().unwrap_or_default());
            if state.eq_ignore_ascii_case(b"required")
                || state.eq_ignore_ascii_case(b"partial-results")
            {
                bail!("GitHub repository access probe requires organization SSO authorization");
            }
            // GitHub documents only `required` and `partial-results` for this
            // response header. Treat a new or malformed state as unavailable
            // rather than accidentally accepting an incomplete repository set.
            bail!("GitHub repository access probe returned an unrecognized SSO restriction");
        }

        line_number += 1;
    }
}

fn validate_included_http_status_line(line: &[u8]) -> Result<()> {
    let line = std::str::from_utf8(line)
        .context("GitHub repository access probe returned a non-UTF-8 HTTP status line")?;
    let mut fields = line.split_ascii_whitespace();
    let protocol = fields.next().unwrap_or_default();
    let status = fields.next().and_then(|field| field.parse::<u16>().ok());
    if !protocol.starts_with("HTTP/") || status.is_none() {
        bail!("GitHub repository access probe returned a malformed HTTP status line");
    }
    if !matches!(status, Some(200..=299)) {
        bail!("GitHub repository access probe returned an unsuccessful HTTP status");
    }
    Ok(())
}

fn is_http_header_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn trim_ascii_whitespace(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

#[cfg(test)]
fn run_gh_repository_access_probe_command(
    gh: &Path,
    limits: CommandLimits,
) -> Result<BoundedOutput> {
    match run_gh_repository_access_probe_command_with_cancellation(
        gh,
        limits,
        &GithubLoginCancellation::new(),
    )? {
        CancellationAware::Completed(output) => Ok(output),
        CancellationAware::Cancelled => {
            unreachable!("a fresh cancellation signal cannot be cancelled")
        }
    }
}

fn run_gh_repository_access_probe_command_with_cancellation(
    gh: &Path,
    limits: CommandLimits,
    cancellation: &GithubLoginCancellation,
) -> Result<CancellationAware<BoundedOutput>> {
    if !gh.is_absolute() {
        bail!("refusing to run GitHub CLI from a non-absolute path");
    }

    let mut command = Command::new(gh);
    command
        .args(["api", "graphql", "--hostname", "github.com", "--include"])
        .arg("-f")
        .arg(format!("query={REPOSITORY_ACCESS_PROBE_QUERY}"))
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_bounded_command_with_cancellation(
        command,
        limits,
        "GitHub repository access probe",
        None,
        cancellation,
    )
}

fn run_gh_command(gh: &Path, limits: CommandLimits) -> Result<BoundedOutput> {
    if !gh.is_absolute() {
        bail!("refusing to run GitHub CLI from a non-absolute path");
    }

    let mut command = Command::new(gh);
    command
        .args([
            "api",
            "graphql",
            "--hostname",
            "github.com",
            "--paginate",
            "--slurp",
        ])
        .arg("-f")
        .arg(format!("query={ACCESSIBLE_REPOSITORIES_QUERY}"))
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_bounded_command(command, limits, "GitHub repository discovery", None)
}

type OnceMarkerCallback = (&'static [u8], Box<dyn FnOnce() + Send + 'static>);

struct BoundedCaptureState {
    output: BoundedBytes,
    marker_callback: Option<OnceMarkerCallback>,
    marker_window: Vec<u8>,
    limit: usize,
}

impl BoundedCaptureState {
    fn new(limit: usize, marker_callback: Option<OnceMarkerCallback>) -> Self {
        Self {
            output: BoundedBytes {
                bytes: Vec::with_capacity(limit.min(64 * 1024)),
                truncated: false,
            },
            marker_callback,
            marker_window: Vec::new(),
            limit,
        }
    }

    fn record(&mut self, chunk: &[u8]) {
        if let Some((marker, _)) = self.marker_callback.as_ref() {
            self.marker_window.extend_from_slice(chunk);
            if self
                .marker_window
                .windows(marker.len())
                .any(|candidate| candidate == *marker)
            {
                let (_, callback) = self
                    .marker_callback
                    .take()
                    .expect("marker callback was present while matching");
                callback();
                self.marker_window.clear();
            } else {
                let retained = marker.len().saturating_sub(1);
                if self.marker_window.len() > retained {
                    self.marker_window
                        .drain(..self.marker_window.len() - retained);
                }
            }
        }

        let available = self.limit.saturating_sub(self.output.bytes.len());
        let retained = available.min(chunk.len());
        self.output.bytes.extend_from_slice(&chunk[..retained]);
        self.output.truncated |= retained < chunk.len();
    }

    fn finish(self) -> BoundedBytes {
        self.output
    }
}

/// Captures a login stream in an unlinked regular temporary file. Reads at the
/// current EOF return immediately, unlike a pipe that can stay blocked forever
/// when a descendant inherits its write handle. The file has no discoverable
/// path while it may contain GitHub CLI authentication output.
struct PollableOutputCapture {
    _owner: fs::File,
    reader: fs::File,
    state: BoundedCaptureState,
}

impl PollableOutputCapture {
    fn new(
        limit: usize,
        marker_callback: Option<OnceMarkerCallback>,
        stream: &str,
    ) -> Result<(Self, fs::File)> {
        let temporary = tempfile::NamedTempFile::new()
            .with_context(|| format!("create temporary GitHub CLI {stream} capture"))?;
        Self::from_temporary(temporary, limit, marker_callback, stream)
    }

    fn from_temporary(
        temporary: tempfile::NamedTempFile,
        limit: usize,
        marker_callback: Option<OnceMarkerCallback>,
        stream: &str,
    ) -> Result<(Self, fs::File)> {
        let reader = temporary
            .reopen()
            .with_context(|| format!("open temporary GitHub CLI {stream} capture for reading"))?;
        let writer = temporary
            .reopen()
            .with_context(|| format!("open temporary GitHub CLI {stream} capture for writing"))?;
        // Reopen every handle first, then unlink the secure NamedTempFile. The
        // anonymous owner keeps the file alive only until this capture drops.
        let owner = temporary.into_file();
        Ok((
            Self {
                _owner: owner,
                reader,
                state: BoundedCaptureState::new(limit, marker_callback),
            },
            writer,
        ))
    }

    fn drain_available(&mut self, stream: &str) -> Result<()> {
        let mut buffer = [0_u8; 8192];
        loop {
            let read = self
                .reader
                .read(&mut buffer)
                .with_context(|| format!("read GitHub CLI {stream}"))?;
            if read == 0 {
                return Ok(());
            }
            self.state.record(&buffer[..read]);
            if self.state.output.truncated {
                return Ok(());
            }
        }
    }

    fn is_truncated(&self) -> bool {
        self.state.output.truncated
    }

    fn finish(self) -> BoundedBytes {
        self.state.finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthProbePurpose {
    Periodic,
    Final,
}

/// Runs at most one potentially slow GitHub capability probe away from the
/// login supervision loop. The worker is intentionally detachable: each real
/// probe has its own command timeout, while output quota enforcement and child
/// reaping must never wait for it.
struct AsyncAuthProbe {
    requests: Option<Sender<AuthProbePurpose>>,
    results: Receiver<(AuthProbePurpose, Result<bool>)>,
    in_flight: Option<AuthProbePurpose>,
    worker: Option<thread::JoinHandle<()>>,
}

impl AsyncAuthProbe {
    fn spawn(mut probe: impl FnMut() -> Result<bool> + Send + 'static) -> Result<AsyncAuthProbe> {
        let (request_sender, request_receiver) = mpsc::channel();
        let (result_sender, result_receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("github-auth-probe".into())
            .spawn(move || {
                while let Ok(purpose) = request_receiver.recv() {
                    let result = probe();
                    if result_sender.send((purpose, result)).is_err() {
                        break;
                    }
                }
            })
            .context("start GitHub authorization capability probe worker")?;
        Ok(Self {
            requests: Some(request_sender),
            results: result_receiver,
            in_flight: None,
            worker: Some(worker),
        })
    }

    fn start(&mut self, purpose: AuthProbePurpose) -> bool {
        debug_assert!(self.in_flight.is_none());
        let Some(requests) = &self.requests else {
            return false;
        };
        if requests.send(purpose).is_err() {
            self.requests = None;
            return false;
        }
        self.in_flight = Some(purpose);
        true
    }

    fn poll(&mut self) -> Option<(AuthProbePurpose, Result<bool>)> {
        match self.results.try_recv() {
            Ok((purpose, result)) => {
                debug_assert_eq!(self.in_flight, Some(purpose));
                self.in_flight = None;
                Some((purpose, result))
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => self.in_flight.take().map(|purpose| {
                (
                    purpose,
                    Err(anyhow::anyhow!(
                        "GitHub authorization capability probe worker stopped unexpectedly"
                    )),
                )
            }),
        }
    }

    fn is_in_flight(&self) -> bool {
        self.in_flight.is_some()
    }
}

impl Drop for AsyncAuthProbe {
    fn drop(&mut self) {
        self.requests.take();
        let Some(worker) = self.worker.take() else {
            return;
        };
        if worker.is_finished() {
            let _ = worker.join();
        }
        // Dropping an unfinished JoinHandle detaches the bounded probe. This is
        // what lets a quota violation terminate and reap the login immediately.
    }
}

fn run_login_command(
    mut command: Command,
    limits: CommandLimits,
    auth_poll_interval: Duration,
    allow_external_satisfaction: bool,
    stderr_marker_callback: Option<OnceMarkerCallback>,
    cancellation: GithubLoginCancellation,
    auth_probe: impl FnMut() -> Result<bool> + Send + 'static,
) -> Result<LoginCommandOutcome> {
    #[cfg(unix)]
    command.process_group(0);

    let (mut stdout_capture, stdout_writer) =
        PollableOutputCapture::new(limits.stdout_bytes, None, "stdout")?;
    let (mut stderr_capture, stderr_writer) =
        PollableOutputCapture::new(limits.stderr_bytes, stderr_marker_callback, "stderr")?;
    command
        .stdout(Stdio::from(stdout_writer))
        .stderr(Stdio::from(stderr_writer));
    let mut child = command.spawn().context("start GitHub authorization")?;
    let process_tree = ChildProcessTree::attach(&child);
    let mut auth_probes = match AsyncAuthProbe::spawn(auth_probe) {
        Ok(probes) => probes,
        Err(error) => {
            process_tree.terminate(&mut child);
            let _ = child.wait();
            return Err(error);
        }
    };

    let started = Instant::now();
    let mut next_auth_probe = started + auth_poll_interval;
    let mut status = None;
    let mut externally_satisfied = false;
    let mut cancelled = false;
    let mut final_auth_probe_completed = false;
    let mut timed_out = false;
    loop {
        if status.is_none() {
            status = match child.try_wait() {
                Ok(status) => status,
                Err(error) => {
                    process_tree.terminate(&mut child);
                    let _ = child.wait();
                    return Err(error).context("poll GitHub CLI process status");
                }
            };
        }

        // A process success already observable from the OS wins a concurrent
        // cancellation request. Otherwise cancellation takes priority over
        // marker callbacks, queued probe results, failures, and timeouts.
        if status.as_ref().is_some_and(ExitStatus::success) {
            break;
        }
        if cancellation.is_cancelled() {
            if status.is_none() {
                process_tree.terminate(&mut child);
                status = Some(
                    child
                        .wait()
                        .context("reap GitHub authorization after cancellation")?,
                );
            }
            cancelled = true;
            break;
        }

        if let Err(error) = stdout_capture
            .drain_available("stdout")
            .and_then(|()| stderr_capture.drain_available("stderr"))
        {
            if status.is_none() {
                process_tree.terminate(&mut child);
                let _ = child.wait();
            }
            return Err(error);
        }
        if stdout_capture.is_truncated() || stderr_capture.is_truncated() {
            // The regular-file capture prevents inherited output handles from
            // blocking cleanup. Stop the tree as soon as the retained bound is
            // crossed so those files cannot grow for the full login timeout.
            if status.is_none() {
                process_tree.terminate(&mut child);
                status = Some(
                    child
                        .wait()
                        .context("reap GitHub authorization after excess output")?,
                );
            }
            break;
        }

        if allow_external_satisfaction {
            if let Some((purpose, result)) = auth_probes.poll() {
                if matches!(result, Ok(true)) {
                    if cancellation.is_cancelled() {
                        if status.is_none() {
                            process_tree.terminate(&mut child);
                            status = Some(
                                child
                                    .wait()
                                    .context("reap GitHub authorization after cancellation")?,
                            );
                        }
                        cancelled = true;
                        break;
                    }
                    externally_satisfied = true;
                    if status.is_none() {
                        process_tree.terminate(&mut child);
                        status = Some(
                            child
                                .wait()
                                .context("reap GitHub authorization after external login")?,
                        );
                    }
                    break;
                }
                match purpose {
                    AuthProbePurpose::Periodic => {
                        next_auth_probe = Instant::now() + auth_poll_interval;
                    }
                    AuthProbePurpose::Final => final_auth_probe_completed = true,
                }
            }
        }

        if status.is_none() && started.elapsed() >= limits.timeout {
            // Enforce the login deadline before starting or awaiting the final
            // capability check. That check can still win the timeout race, but
            // a slow network request cannot leave the login tree running.
            process_tree.terminate(&mut child);
            status = Some(
                child
                    .wait()
                    .context("reap GitHub authorization after timeout")?,
            );
            timed_out = true;
        }

        let login_failed = status
            .as_ref()
            .is_some_and(|exit_status| !exit_status.success());
        if timed_out || login_failed {
            if !allow_external_satisfaction {
                if timed_out {
                    bail!(
                        "GitHub authorization exceeded its {} second time limit",
                        limits.timeout.as_secs_f32()
                    );
                }
                break;
            }

            // If a periodic check was already running when the child failed,
            // consume it first, then run a distinct final check. This preserves
            // the narrow race where a separately completed login becomes ready
            // just after the periodic request started.
            if !final_auth_probe_completed
                && !auth_probes.is_in_flight()
                && !auth_probes.start(AuthProbePurpose::Final)
            {
                final_auth_probe_completed = true;
            }
            if final_auth_probe_completed {
                if timed_out {
                    bail!(
                        "GitHub authorization exceeded its {} second time limit",
                        limits.timeout.as_secs_f32()
                    );
                }
                break;
            }
        } else if allow_external_satisfaction
            && !auth_probes.is_in_flight()
            && Instant::now() >= next_auth_probe
            && !auth_probes.start(AuthProbePurpose::Periodic)
        {
            next_auth_probe = Instant::now() + auth_poll_interval;
        }

        let sleep_for = if timed_out || status.is_some() {
            GH_POLL_INTERVAL
        } else {
            GH_POLL_INTERVAL.min(limits.timeout.saturating_sub(started.elapsed()))
        };
        thread::sleep(sleep_for);
    }

    // Pick up bytes written between the last poll and process termination. A
    // regular file reaches its current EOF immediately even if a descendant
    // still has the corresponding output handle open.
    if !cancelled && !stdout_capture.is_truncated() && !stderr_capture.is_truncated() {
        stdout_capture.drain_available("stdout")?;
        stderr_capture.drain_available("stderr")?;
    }
    let stdout = stdout_capture.finish();
    let stderr = stderr_capture.finish();
    Ok(LoginCommandOutcome {
        output: BoundedOutput {
            status: status.expect("the GitHub CLI process finished"),
            stdout: stdout.bytes,
            stderr: stderr.bytes,
            stdout_truncated: stdout.truncated,
            stderr_truncated: stderr.truncated,
        },
        externally_satisfied,
        cancelled,
    })
}

fn run_bounded_command(
    mut command: Command,
    limits: CommandLimits,
    operation: &str,
    stderr_marker_callback: Option<OnceMarkerCallback>,
) -> Result<BoundedOutput> {
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command
        .spawn()
        .with_context(|| format!("start {operation}"))?;
    let process_tree = ChildProcessTree::attach(&child);
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("GitHub CLI stdout was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("GitHub CLI stderr was not captured"))?;
    let stdout_reader = thread::spawn(move || read_bounded(stdout, limits.stdout_bytes));
    let stderr_reader = thread::spawn(move || {
        read_bounded_with_once_marker(stderr, limits.stderr_bytes, stderr_marker_callback)
    });

    let started = Instant::now();
    let mut status = None;
    loop {
        if status.is_none() {
            status = match child.try_wait() {
                Ok(status) => status,
                Err(error) => {
                    process_tree.terminate(&mut child);
                    let _ = child.wait();
                    return Err(error).context("poll GitHub CLI process status");
                }
            };
        }
        if status.is_some() && stdout_reader.is_finished() && stderr_reader.is_finished() {
            break;
        }
        if started.elapsed() >= limits.timeout {
            process_tree.terminate(&mut child);
            let _ = child.wait();
            bail!(
                "{operation} exceeded its {} second time limit",
                limits.timeout.as_secs_f32()
            );
        }
        thread::sleep(GH_POLL_INTERVAL.min(limits.timeout.saturating_sub(started.elapsed())));
    }

    let stdout = join_reader(stdout_reader, "stdout")?;
    let stderr = join_reader(stderr_reader, "stderr")?;
    Ok(BoundedOutput {
        status: status.expect("finished readers imply a finished GitHub CLI process"),
        stdout: stdout.bytes,
        stderr: stderr.bytes,
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
    })
}

fn run_bounded_command_with_cancellation(
    mut command: Command,
    limits: CommandLimits,
    operation: &str,
    stderr_marker_callback: Option<OnceMarkerCallback>,
    cancellation: &GithubLoginCancellation,
) -> Result<CancellationAware<BoundedOutput>> {
    #[cfg(unix)]
    command.process_group(0);

    let (mut stdout_capture, stdout_writer) =
        PollableOutputCapture::new(limits.stdout_bytes, None, "stdout")?;
    let (mut stderr_capture, stderr_writer) =
        PollableOutputCapture::new(limits.stderr_bytes, stderr_marker_callback, "stderr")?;
    command
        .stdout(Stdio::from(stdout_writer))
        .stderr(Stdio::from(stderr_writer));
    let mut child = command
        .spawn()
        .with_context(|| format!("start {operation}"))?;
    let process_tree = ChildProcessTree::attach(&child);

    let started = Instant::now();
    let mut status = None;
    loop {
        if status.is_none() {
            status = match child.try_wait() {
                Ok(status) => status,
                Err(error) => {
                    process_tree.terminate(&mut child);
                    let _ = child.wait();
                    return Err(error).context("poll GitHub CLI process status");
                }
            };
        }
        if cancellation.is_cancelled() {
            if status.is_none() {
                process_tree.terminate(&mut child);
                child
                    .wait()
                    .with_context(|| format!("reap {operation} after cancellation"))?;
            }
            return Ok(CancellationAware::Cancelled);
        }
        if let Err(error) = stdout_capture
            .drain_available("stdout")
            .and_then(|()| stderr_capture.drain_available("stderr"))
        {
            if status.is_none() {
                process_tree.terminate(&mut child);
                let _ = child.wait();
            }
            return Err(error);
        }
        if stdout_capture.is_truncated() || stderr_capture.is_truncated() {
            if status.is_none() {
                process_tree.terminate(&mut child);
                status = Some(
                    child
                        .wait()
                        .with_context(|| format!("reap {operation} after excess output"))?,
                );
            }
            break;
        }
        if status.is_some() {
            break;
        }
        if started.elapsed() >= limits.timeout {
            process_tree.terminate(&mut child);
            let _ = child.wait();
            bail!(
                "{operation} exceeded its {} second time limit",
                limits.timeout.as_secs_f32()
            );
        }
        thread::sleep(GH_POLL_INTERVAL.min(limits.timeout.saturating_sub(started.elapsed())));
    }

    if !stdout_capture.is_truncated() && !stderr_capture.is_truncated() {
        stdout_capture.drain_available("stdout")?;
        stderr_capture.drain_available("stderr")?;
    }
    let stdout = stdout_capture.finish();
    let stderr = stderr_capture.finish();
    Ok(CancellationAware::Completed(BoundedOutput {
        status: status.expect("the bounded GitHub CLI process finished"),
        stdout: stdout.bytes,
        stderr: stderr.bytes,
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
    }))
}

fn read_bounded(mut reader: impl Read, limit: usize) -> io::Result<BoundedBytes> {
    read_bounded_with_once_marker(&mut reader, limit, None)
}

fn read_bounded_with_once_marker(
    mut reader: impl Read,
    limit: usize,
    marker_callback: Option<OnceMarkerCallback>,
) -> io::Result<BoundedBytes> {
    let mut state = BoundedCaptureState::new(limit, marker_callback);
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        state.record(&buffer[..read]);
    }
    Ok(state.finish())
}

fn join_reader(
    reader: thread::JoinHandle<io::Result<BoundedBytes>>,
    stream: &str,
) -> Result<BoundedBytes> {
    reader
        .join()
        .map_err(|_| anyhow::anyhow!("GitHub CLI {stream} reader stopped unexpectedly"))?
        .with_context(|| format!("read GitHub CLI {stream}"))
}

struct ChildProcessTree {
    #[cfg(windows)]
    job: Option<WindowsJob>,
}

impl ChildProcessTree {
    fn attach(_child: &Child) -> Self {
        Self {
            #[cfg(windows)]
            job: WindowsJob::attach(_child),
        }
    }

    fn terminate(&self, child: &mut Child) {
        #[cfg(unix)]
        // SAFETY: `child.id()` belongs to the child this guard accompanies,
        // and both command runners place the child in a fresh process group
        // whose ID equals its PID. No pointer invariants are involved.
        unsafe {
            if libc::killpg(child.id() as libc::pid_t, libc::SIGKILL) != 0 {
                let _ = child.kill();
            }
        }

        #[cfg(windows)]
        {
            // A Job Object terminates descendants as well as the direct child.
            // Assignment can be rejected by a restrictive parent job; retain
            // the direct-child fallback in that case.
            if let Some(job) = &self.job {
                job.terminate();
            }
            let _ = child.kill();
        }

        #[cfg(not(any(unix, windows)))]
        let _ = child.kill();
    }
}

#[cfg(windows)]
struct WindowsJob(*mut std::ffi::c_void);

#[cfg(windows)]
impl WindowsJob {
    fn attach(child: &Child) -> Option<Self> {
        use std::os::windows::io::AsRawHandle;

        // SAFETY: a null security descriptor and name are documented inputs.
        // The returned handle is owned by `WindowsJob` and closed in `Drop`.
        let handle =
            unsafe { windows_job_ffi::create_job_object_w(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return None;
        }
        let job = Self(handle);
        // SAFETY: both handles are live for this call. `Child` keeps ownership
        // of its process handle; the Job Object only records membership.
        let attached = unsafe {
            windows_job_ffi::assign_process_to_job_object(handle, child.as_raw_handle().cast())
        };
        if attached == 0 {
            return None;
        }
        Some(job)
    }

    fn terminate(&self) {
        // SAFETY: the job handle remains live for the lifetime of `self`.
        unsafe {
            windows_job_ffi::terminate_job_object(self.0, 1);
        }
    }
}

#[cfg(windows)]
impl Drop for WindowsJob {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a unique owned handle returned by
        // `CreateJobObjectW` and has not otherwise been closed.
        unsafe {
            windows_job_ffi::close_handle(self.0);
        }
    }
}

#[cfg(windows)]
mod windows_job_ffi {
    use std::ffi::c_void;

    type Handle = *mut c_void;

    #[link(name = "kernel32")]
    extern "system" {
        #[link_name = "CreateJobObjectW"]
        pub(super) fn create_job_object_w(
            job_attributes: *const c_void,
            name: *const u16,
        ) -> Handle;
        #[link_name = "AssignProcessToJobObject"]
        pub(super) fn assign_process_to_job_object(job: Handle, process: Handle) -> i32;
        #[link_name = "TerminateJobObject"]
        pub(super) fn terminate_job_object(job: Handle, exit_code: u32) -> i32;
        #[link_name = "CloseHandle"]
        pub(super) fn close_handle(object: Handle) -> i32;
    }
}

fn classify_gh_failure(status: ExitStatus, stderr: &[u8]) -> String {
    let raw = String::from_utf8_lossy(stderr);
    let normalized = raw.to_lowercase();

    if normalized.contains("not logged into")
        || normalized.contains("authentication required")
        || normalized.contains("bad credentials")
        || normalized.contains("http 401")
    {
        return "GitHub CLI is not authenticated for github.com".into();
    }
    if normalized.contains("resource not accessible")
        || normalized.contains("insufficient scope")
        || normalized.contains("http 403")
    {
        return "the GitHub credentials do not grant repository-list access".into();
    }
    if normalized.contains("rate limit") {
        return "the GitHub API rate limit was exceeded".into();
    }
    if normalized.contains("could not resolve host")
        || normalized.contains("connection refused")
        || normalized.contains("network is unreachable")
        || normalized.contains("timed out")
    {
        return "github.com could not be reached".into();
    }

    let detail = sanitize_command_detail(&raw, 320);
    if detail.is_empty() {
        format!("GitHub CLI exited with status {status}")
    } else {
        format!("GitHub CLI reported: {detail}")
    }
}

fn sanitize_command_detail(raw: &str, max_chars: usize) -> String {
    let mut without_controls = String::with_capacity(raw.len().min(max_chars));
    let mut after_escape = false;
    let mut in_csi_sequence = false;
    for character in raw.chars() {
        if after_escape {
            after_escape = false;
            in_csi_sequence = character == '[';
            continue;
        }
        if in_csi_sequence {
            if ('@'..='~').contains(&character) {
                in_csi_sequence = false;
            }
            continue;
        }
        if character == '\u{1b}' {
            after_escape = true;
        } else if character.is_control() {
            without_controls.push(' ');
        } else {
            without_controls.push(character);
        }
    }

    let normalized = without_controls
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let normalized = redact_github_device_codes(&normalized);
    let mut lowercase = normalized.clone();
    lowercase.make_ascii_lowercase();
    let sensitive_offset = [
        "authorization",
        "bearer ",
        "access_token",
        "client_secret",
        "github_token",
        "gh_token",
        "token=",
        "token:",
        "password=",
        "password:",
        "ghp_",
        "github_pat_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
    ]
    .iter()
    .filter_map(|marker| lowercase.find(marker))
    .min();
    let redacted = sensitive_offset.map_or(normalized.clone(), |offset| {
        let prefix = normalized[..offset].trim_end();
        if prefix.is_empty() {
            "[redacted]".to_owned()
        } else {
            format!("{prefix} [redacted]")
        }
    });
    let mut characters = redacted.chars();
    let mut shortened = characters.by_ref().take(max_chars).collect::<String>();
    if characters.next().is_some() {
        if max_chars == 0 {
            return String::new();
        }
        shortened.pop();
        shortened.push('…');
    }
    shortened
}

fn redact_github_device_codes(detail: &str) -> String {
    const DEVICE_CODE_LENGTH: usize = 9;

    let bytes = detail.as_bytes();
    let mut redacted = String::with_capacity(detail.len());
    let mut copied_through = 0;
    let mut offset = 0;
    while offset + DEVICE_CODE_LENGTH <= bytes.len() {
        if is_github_device_code_at(bytes, offset) {
            redacted.push_str(&detail[copied_through..offset]);
            redacted.push_str("[redacted]");
            offset += DEVICE_CODE_LENGTH;
            copied_through = offset;
        } else {
            offset += 1;
        }
    }

    if copied_through == 0 {
        return detail.to_owned();
    }
    redacted.push_str(&detail[copied_through..]);
    redacted
}

fn is_github_device_code_at(bytes: &[u8], offset: usize) -> bool {
    const DEVICE_CODE_LENGTH: usize = 9;

    let Some(candidate) = bytes.get(offset..offset + DEVICE_CODE_LENGTH) else {
        return false;
    };
    // `gh` currently prints uppercase codes, but redact case-insensitively so
    // diagnostics remain safe if presentation or upstream formatting changes.
    let code_character = |byte: &u8| byte.is_ascii_alphanumeric();
    if candidate[4] != b'-'
        || !candidate[..4].iter().all(code_character)
        || !candidate[5..].iter().all(code_character)
    {
        return false;
    }

    let token_character = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'-';
    offset
        .checked_sub(1)
        .and_then(|before| bytes.get(before))
        .is_none_or(|before| !token_character(*before))
        && bytes
            .get(offset + DEVICE_CODE_LENGTH)
            .is_none_or(|after| !token_character(*after))
}

fn discover_gh() -> Option<PathBuf> {
    let fallback_paths = GH_FALLBACK_PATHS
        .iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    discover_gh_in(env::var_os("PATH").as_deref(), &fallback_paths)
}

fn discover_gh_in(path: Option<&OsStr>, fallback_paths: &[PathBuf]) -> Option<PathBuf> {
    if let Some(path) = path {
        for directory in env::split_paths(path) {
            if !directory.is_absolute() {
                continue;
            }
            let candidate = directory.join(gh_executable_name());
            if let Some(candidate) = canonical_executable(&candidate) {
                return Some(candidate);
            }
        }
    }

    fallback_paths
        .iter()
        .filter(|candidate| candidate.is_absolute())
        .find_map(|candidate| canonical_executable(candidate))
}

fn canonical_executable(path: &Path) -> Option<PathBuf> {
    let canonical = fs::canonicalize(path).ok()?;
    (canonical.is_absolute() && is_executable_file(&canonical)).then_some(canonical)
}

fn gh_executable_name() -> &'static str {
    if cfg!(windows) {
        "gh.exe"
    } else {
        "gh"
    }
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }

    #[cfg(not(unix))]
    {
        true
    }
}

fn parse_account_repositories(input: &[u8]) -> Result<GithubAccountRepositories> {
    let pages: Vec<GraphqlPage> =
        serde_json::from_slice(input).context("parse paginated GitHub repository response")?;
    let mut repositories: Vec<GithubRepository> = Vec::new();
    let mut seen_ids = HashSet::new();
    let mut account_login: Option<String> = None;

    for page in pages {
        if !page.errors.is_empty() {
            let messages = page
                .errors
                .into_iter()
                .map(|error| error.message)
                .collect::<Vec<_>>()
                .join("; ");
            let messages = sanitize_command_detail(&messages, 320);
            bail!(
                "GitHub API rejected repository discovery: {messages}. Check `gh auth status`; authenticate or refresh access with `gh auth login`."
            );
        }

        let data = page.data.ok_or_else(|| {
            anyhow::anyhow!(
                "GitHub API returned no repository data. Check `gh auth status`; authenticate or refresh access with `gh auth login`."
            )
        })?;
        let viewer = data.viewer;
        if viewer.login.trim().is_empty() {
            bail!("GitHub API returned an empty viewer login");
        }
        if let Some(login) = &account_login {
            if login != &viewer.login {
                bail!("GitHub API returned inconsistent viewer logins across pages");
            }
        } else {
            account_login = Some(viewer.login.clone());
        }
        for repository in viewer.repositories.nodes.into_iter().flatten() {
            if seen_ids.insert(repository.id.clone()) {
                repositories.push(repository.into());
            }
        }
    }

    repositories.sort_by(|left, right| {
        left.name_with_owner
            .to_lowercase()
            .cmp(&right.name_with_owner.to_lowercase())
            .then_with(|| left.name_with_owner.cmp(&right.name_with_owner))
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(GithubAccountRepositories {
        login: account_login.ok_or_else(|| anyhow::anyhow!("GitHub API returned no pages"))?,
        repositories,
    })
}

#[derive(Debug, Deserialize)]
struct GraphqlPage {
    data: Option<GraphqlData>,
    #[serde(default)]
    errors: Vec<GraphqlError>,
}

#[derive(Debug, Deserialize)]
struct GraphqlError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct GraphqlData {
    viewer: GraphqlViewer,
}

#[derive(Debug, Deserialize)]
struct GraphqlViewer {
    login: String,
    repositories: GraphqlRepositories,
}

#[derive(Debug, Deserialize)]
struct GraphqlRepositories {
    nodes: Vec<Option<GraphqlRepository>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphqlRepository {
    id: String,
    name: String,
    name_with_owner: String,
    url: String,
    ssh_url: String,
    is_private: bool,
    is_archived: bool,
    default_branch_ref: Option<GraphqlDefaultBranch>,
    main_branch: Option<GraphqlDefaultBranch>,
    owner: GraphqlOwner,
}

#[derive(Debug, Deserialize)]
struct GraphqlDefaultBranch {
    name: String,
}

#[derive(Debug, Deserialize)]
struct GraphqlOwner {
    login: String,
    name: Option<String>,
}

impl From<GraphqlRepository> for GithubRepository {
    fn from(repository: GraphqlRepository) -> Self {
        Self {
            id: repository.id,
            name: repository.name,
            name_with_owner: repository.name_with_owner,
            url: repository.url,
            ssh_url: repository.ssh_url,
            is_private: repository.is_private,
            is_archived: repository.is_archived,
            default_branch: repository.default_branch_ref.map(|branch| branch.name),
            main_branch: repository.main_branch.map(|branch| branch.name),
            owner: repository.owner.login,
            owner_name: repository.owner.name,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    };

    static LOGIN_PROCESS_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn pollable_output_capture_unlinks_backing_file_immediately() {
        use std::io::Write as _;

        let directory = tempfile::tempdir().unwrap();
        let temporary = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        let path = temporary.path().to_path_buf();
        let (mut capture, mut writer) =
            PollableOutputCapture::from_temporary(temporary, 64, None, "test").unwrap();

        assert!(!path.exists());
        writer.write_all(b"device-flow output").unwrap();
        capture.drain_available("test").unwrap();
        assert_eq!(capture.finish().bytes, b"device-flow output");
    }

    const REPOSITORY_PAGES: &str = r#"
[
  {
    "data": {
      "viewer": {
        "login": "octocat",
        "repositories": {
          "nodes": [
            {
              "id": "R_second",
              "name": "Zulu",
              "nameWithOwner": "zeta/Zulu",
              "url": "https://github.com/zeta/Zulu",
              "sshUrl": "git@github.com:zeta/Zulu.git",
              "isPrivate": true,
              "isArchived": false,
              "defaultBranchRef": { "name": "main" },
              "mainBranch": { "name": "main" },
              "owner": { "login": "zeta", "name": "Zeta Org" }
            },
            null,
            {
              "id": "R_first",
              "name": "alpha",
              "nameWithOwner": "Acme/alpha",
              "url": "https://github.com/Acme/alpha",
              "sshUrl": "git@github.com:Acme/alpha.git",
              "isPrivate": false,
              "isArchived": true,
              "defaultBranchRef": null,
              "mainBranch": null,
              "owner": { "login": "Acme", "name": null }
            }
          ],
          "pageInfo": { "hasNextPage": true, "endCursor": "next" }
        }
      }
    }
  },
  {
    "data": {
      "viewer": {
        "login": "octocat",
        "repositories": {
          "nodes": [
            {
              "id": "R_second",
              "name": "Zulu",
              "nameWithOwner": "zeta/Zulu",
              "url": "https://github.com/zeta/Zulu",
              "sshUrl": "git@github.com:zeta/Zulu.git",
              "isPrivate": true,
              "isArchived": false,
              "defaultBranchRef": { "name": "main" },
              "mainBranch": { "name": "main" },
              "owner": { "login": "zeta", "name": "Zeta Org" }
            }
          ],
          "pageInfo": { "hasNextPage": false, "endCursor": null }
        }
      }
    }
  }
]
"#;

    #[test]
    fn parses_flattens_deduplicates_and_sorts_repository_pages() {
        let account = parse_account_repositories(REPOSITORY_PAGES.as_bytes()).unwrap();
        let repositories = account.repositories;

        assert_eq!(account.login, "octocat");
        assert_eq!(repositories.len(), 2);
        assert_eq!(repositories[0].name_with_owner, "Acme/alpha");
        assert_eq!(repositories[0].name, "alpha");
        assert_eq!(repositories[0].owner, "Acme");
        assert_eq!(repositories[0].owner_name, None);
        assert_eq!(repositories[0].default_branch, None);
        assert_eq!(repositories[0].main_branch, None);
        assert!(repositories[0].is_archived);
        assert_eq!(repositories[1].name_with_owner, "zeta/Zulu");
        assert_eq!(repositories[1].default_branch.as_deref(), Some("main"));
        assert_eq!(repositories[1].main_branch.as_deref(), Some("main"));
        assert!(repositories[1].is_private);
    }

    #[test]
    fn graphql_errors_include_an_authentication_recovery_hint() {
        let error = parse_account_repositories(
            br#"[{"data":null,"errors":[{"message":"Resource not accessible"}]}]"#,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("Resource not accessible"));
        assert!(error.contains("gh auth login"));
    }

    #[test]
    fn graphql_errors_are_redacted_and_bounded() {
        let message = format!("failed token=ghp_super-secret {}", "x".repeat(1_000));
        let response = serde_json::json!([{
            "data": null,
            "errors": [{ "message": message }]
        }]);
        let error = parse_account_repositories(response.to_string().as_bytes())
            .unwrap_err()
            .to_string();

        assert!(!error.contains("ghp_super-secret"));
        assert!(error.contains("[redacted]"));
        assert!(error.chars().count() < 500);
    }

    #[test]
    fn query_requests_all_accessible_affiliations_and_picker_fields() {
        assert!(ACCESSIBLE_REPOSITORIES_QUERY.contains("viewer {\n    login"));
        for expected in [
            "OWNER",
            "COLLABORATOR",
            "ORGANIZATION_MEMBER",
            "id",
            "nameWithOwner",
            "url",
            "sshUrl",
            "isPrivate",
            "isArchived",
            "defaultBranchRef",
            "mainBranch",
            "owner",
        ] {
            assert!(ACCESSIBLE_REPOSITORIES_QUERY.contains(expected));
        }
    }

    #[test]
    fn command_error_detail_is_plain_redacted_and_bounded() {
        let raw = format!(
            "\u{1b}[31mrequest failed\u{1b}[0m token=ghp_secret {}",
            "x".repeat(500)
        );
        let detail = sanitize_command_detail(&raw, 80);

        assert!(!detail.contains('\u{1b}'));
        assert!(!detail.contains("ghp_secret"));
        assert!(detail.contains("[redacted]"));
        assert!(detail.chars().count() <= 80);

        let bounded = sanitize_command_detail(&"x".repeat(500), 80);
        assert_eq!(bounded.chars().count(), 80);
        assert!(bounded.ends_with('…'));

        let osc = sanitize_command_detail("\u{1b}]0;token=ghp_osc-secret\u{7}", 80);
        assert!(!osc.contains("ghp_osc-secret"));
        assert!(osc.contains("[redacted]"));
    }

    #[test]
    fn command_error_detail_redacts_github_device_codes_and_keeps_context() {
        let detail = sanitize_command_detail(
            "Copy your one-time code: \u{1b}[1mAB1D-E2GH\u{1b}[0m. OAuth exchange failed; retry ABCD-1234.",
            320,
        );

        assert!(!detail.contains("AB1D-E2GH"));
        assert!(!detail.contains("ABCD-1234"));
        assert_eq!(detail.matches("[redacted]").count(), 2);
        assert!(detail.contains("Copy your one-time code:"));
        assert!(detail.contains("OAuth exchange failed; retry"));
    }

    #[test]
    fn command_error_detail_only_redacts_complete_device_code_tokens() {
        let detail = sanitize_command_detail(
            "lowercase abcd-1234; short ABC-1234; embedded XABCD-1234Y",
            320,
        );

        assert_eq!(
            detail,
            "lowercase [redacted]; short ABC-1234; embedded XABCD-1234Y"
        );
    }

    #[cfg(unix)]
    #[test]
    fn fallback_gh_failure_diagnostic_never_exposes_device_code() {
        use std::os::unix::process::ExitStatusExt as _;

        let detail = classify_gh_failure(
            ExitStatus::from_raw(17 << 8),
            b"Copy code WXYZ-7890. OAuth exchange failed unexpectedly.",
        );

        assert!(!detail.contains("WXYZ-7890"));
        assert!(detail.contains("[redacted]"));
        assert!(detail.contains("OAuth exchange failed unexpectedly"));
    }

    #[test]
    fn command_discovery_prefers_path_then_uses_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let path_directory = temp.path().join("path-bin");
        let fallback_directory = temp.path().join("fallback-bin");
        fs::create_dir_all(&path_directory).unwrap();
        fs::create_dir_all(&fallback_directory).unwrap();
        let path_gh = path_directory.join(gh_executable_name());
        let fallback_gh = fallback_directory.join(gh_executable_name());
        make_executable(&path_gh);
        make_executable(&fallback_gh);
        let search_path = env::join_paths([&path_directory]).unwrap();

        assert_eq!(
            discover_gh_in(Some(&search_path), std::slice::from_ref(&fallback_gh)),
            Some(fs::canonicalize(path_gh).unwrap())
        );
        assert_eq!(
            discover_gh_in(Some(OsStr::new("")), std::slice::from_ref(&fallback_gh)),
            Some(fs::canonicalize(fallback_gh).unwrap())
        );
    }

    #[test]
    fn command_discovery_skips_empty_and_relative_path_entries() {
        let temp = tempfile::tempdir().unwrap();
        let absolute_directory = temp.path().join("absolute-bin");
        fs::create_dir_all(&absolute_directory).unwrap();
        let absolute_gh = absolute_directory.join(gh_executable_name());
        make_executable(&absolute_gh);
        let search_path = env::join_paths([
            Path::new(""),
            Path::new("relative-bin"),
            absolute_directory.as_path(),
        ])
        .unwrap();

        assert_eq!(
            discover_gh_in(Some(&search_path), &[]),
            Some(fs::canonicalize(absolute_gh).unwrap())
        );

        let relative_fallback = PathBuf::from("relative-bin").join(gh_executable_name());
        assert_eq!(discover_gh_in(None, &[relative_fallback]), None);
    }

    #[test]
    fn command_discovery_ignores_missing_candidates() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing-gh");

        assert_eq!(discover_gh_in(None, &[missing]), None);
    }

    #[cfg(unix)]
    #[test]
    fn command_runtime_is_bounded_and_timed_out_process_is_reaped() {
        let temp = tempfile::tempdir().unwrap();
        let fake_gh = temp.path().join("gh");
        write_executable(&fake_gh, b"#!/bin/sh\nsleep 5\n");
        let started = Instant::now();

        let error = run_gh_command(
            &fs::canonicalize(fake_gh).unwrap(),
            CommandLimits {
                timeout: Duration::from_millis(50),
                stdout_bytes: 64,
                stderr_bytes: 64,
            },
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("time limit"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn command_timeout_includes_pipes_inherited_by_orphaned_descendants() {
        let temp = tempfile::tempdir().unwrap();
        let fake_gh = temp.path().join("gh");
        write_executable(&fake_gh, b"#!/bin/sh\nsleep 5 &\nprintf '[]'\n");
        let started = Instant::now();

        let error = run_gh_command(
            &fs::canonicalize(fake_gh).unwrap(),
            CommandLimits {
                timeout: Duration::from_millis(50),
                stdout_bytes: 64,
                stderr_bytes: 64,
            },
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("time limit"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn command_output_is_drained_but_retained_memory_is_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let fake_gh = temp.path().join("gh");
        write_executable(
            &fake_gh,
            b"#!/bin/sh\nprintf '0123456789'\nprintf 'abcdefghij' >&2\n",
        );

        let output = run_gh_command(
            &fs::canonicalize(fake_gh).unwrap(),
            CommandLimits {
                // This case verifies bounded retention, not timeout behavior.
                // Leave headroom for process scheduling in the parallel suite.
                timeout: Duration::from_secs(10),
                stdout_bytes: 4,
                stderr_bytes: 5,
            },
        )
        .unwrap();

        assert!(output.status.success());
        assert_eq!(output.stdout, b"0123");
        assert_eq!(output.stderr, b"abcde");
        assert!(output.stdout_truncated);
        assert!(output.stderr_truncated);
    }

    #[test]
    fn command_runner_refuses_relative_executable_path() {
        let error = run_gh_command(
            Path::new("gh"),
            CommandLimits {
                timeout: Duration::from_secs(1),
                stdout_bytes: 1,
                stderr_bytes: 1,
            },
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("non-absolute"));
    }

    #[test]
    fn fragmented_repeated_device_flow_marker_fires_callback_once() {
        let callback_count = Arc::new(AtomicUsize::new(0));
        let callback_count_for_reader = Arc::clone(&callback_count);
        let reader = FragmentedReader::new([
            b"diagnostic before https://github.com/login/".as_slice(),
            b"device diagnostic between ".as_slice(),
            b"https://github.com/login/device diagnostic after".as_slice(),
        ]);

        let output = read_bounded_with_once_marker(
            reader,
            1024,
            Some((
                GH_DEVICE_FLOW_URL.as_bytes(),
                Box::new(move || {
                    callback_count_for_reader.fetch_add(1, Ordering::SeqCst);
                }),
            )),
        )
        .unwrap();

        assert_eq!(callback_count.load(Ordering::SeqCst), 1);
        assert!(!output.truncated);
    }

    #[test]
    fn missing_device_flow_marker_does_not_fire_callback() {
        let callback_count = Arc::new(AtomicUsize::new(0));
        let callback_count_for_reader = Arc::clone(&callback_count);
        let reader = FragmentedReader::new([
            b"Open this URL: https://example.com/login/device".as_slice(),
            b" and keep waiting".as_slice(),
        ]);

        let output = read_bounded_with_once_marker(
            reader,
            1024,
            Some((
                GH_DEVICE_FLOW_URL.as_bytes(),
                Box::new(move || {
                    callback_count_for_reader.fetch_add(1, Ordering::SeqCst);
                }),
            )),
        )
        .unwrap();

        assert_eq!(callback_count.load(Ordering::SeqCst), 0);
        assert!(!output.truncated);
    }

    #[test]
    fn repository_access_probe_accepts_the_required_graphql_shape() {
        let valid = included_probe_response(
            &["Content-Type: application/json"],
            r#"{
            "data": {
                "viewer": {
                    "login": "octocat",
                    "repositories": {"nodes": []}
                }
            }
        }"#,
        );

        assert!(parse_repository_access_probe(&valid).unwrap());
    }

    #[test]
    fn repository_access_probe_fails_closed_for_errors_or_missing_identity() {
        for invalid_body in [
            r#"{"data":null,"errors":[{"message":"Resource protected by organization SAML enforcement"}]}"#,
            r#"{"data":null}"#,
            r#"{"data":{"viewer":{"login":"","repositories":{"nodes":[]}}}}"#,
        ] {
            let invalid = included_probe_response(&[], invalid_body);
            assert!(parse_repository_access_probe(&invalid).is_err());
        }
        let invalid = included_probe_response(&[], r#"{"data":"not-an-object"}"#);
        assert!(parse_repository_access_probe(&invalid).is_err());
    }

    #[test]
    fn repository_access_probe_rejects_github_sso_restriction_headers() {
        let valid_body = r#"{"data":{"viewer":{"login":"octocat","repositories":{"nodes":[]}}}}"#;
        for header in [
            "X-GitHub-SSO: partial-results; organizations=21955855,20582480",
            "x-github-sso:\tREQUIRED ; url=https://github.com/orgs/example/sso",
            "X-GITHUB-SSO: future-restriction",
        ] {
            let response = included_probe_response(&[header], valid_body);
            assert!(parse_repository_access_probe(&response).is_err());
        }
    }

    #[test]
    fn repository_access_probe_parses_lf_headers_without_scanning_the_json_body() {
        let response = b"HTTP/2 200\ncontent-type: application/json\n\n{\"extensions\":{\"note\":\"X-GitHub-SSO: partial-results\"},\"data\":{\"viewer\":{\"login\":\"octocat\",\"repositories\":{\"nodes\":[]}}}}";

        assert!(parse_repository_access_probe(response).unwrap());
    }

    #[test]
    fn repository_access_probe_requires_well_formed_included_http_headers() {
        for malformed in [
            br#"{"data":{"viewer":{"login":"octocat","repositories":{"nodes":[]}}}}"#.as_slice(),
            b"HTTP/2 200\r\nMalformed header\r\n\r\n{}".as_slice(),
            b"not-http 200\r\n\r\n{}".as_slice(),
            b"HTTP/2 403 Forbidden\r\nContent-Type: application/json\r\n\r\n{}".as_slice(),
        ] {
            assert!(parse_repository_access_probe(malformed).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn repository_access_probe_uses_bounded_graphql_without_requesting_a_token() {
        let temp = tempfile::tempdir().unwrap();
        let fake_gh = temp.path().join("gh");
        write_executable(&fake_gh, b"#!/bin/sh\nprintf '%s\\n' \"$@\"\n");

        let output = run_gh_repository_access_probe_command(
            &fs::canonicalize(fake_gh).unwrap(),
            CommandLimits {
                timeout: Duration::from_secs(10),
                stdout_bytes: 16 * 1024,
                stderr_bytes: 1024,
            },
        )
        .unwrap();
        let arguments = String::from_utf8(output.stdout).unwrap();

        assert!(
            arguments.starts_with("api\ngraphql\n--hostname\ngithub.com\n--include\n-f\nquery=")
        );
        assert!(arguments.contains("repositories("));
        assert!(arguments.contains("first: 1"));
        assert!(arguments.contains("ORGANIZATION_MEMBER"));
        assert!(!arguments.contains("--show-token"));
        assert!(!arguments.contains("auth token"));
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_interrupts_and_reaps_a_blocked_preflight_probe() {
        let _process_test_guard = LOGIN_PROCESS_TEST_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let fake_gh = temp.path().join("gh");
        let probe_pid_file = temp.path().join("probe.pid");
        let login_marker = temp.path().join("login-started");
        let detached_ready = temp.path().join("detached-ready");
        let test_binary = env::current_exe().unwrap();
        write_executable(
            &fake_gh,
            format!(
                "#!/bin/sh\nif [ \"$1\" = api ]; then\n  PPDUSTER_GITHUB_TEST_DETACHED_READY={ready} {test_binary} --exact github::tests::detached_pipe_holder_helper --ignored &\n  helper_pid=$!\n  printf '%s %s' \"$$\" \"$helper_pid\" > {pid_file}\n  wait\nfi\nprintf started > {login_marker}\nsleep 30\n",
                ready = shell_quote_for_test(&detached_ready),
                test_binary = shell_quote_for_test(&test_binary),
                pid_file = shell_quote_for_test(&probe_pid_file),
                login_marker = shell_quote_for_test(&login_marker),
            )
            .as_bytes(),
        );

        let canonical_gh = fs::canonicalize(fake_gh).unwrap();
        let cancellation = GithubLoginCancellation::new();
        let cancellation_for_login = cancellation.clone();
        let login = thread::spawn(move || {
            login_via_web_with_gh_and_device_flow_ready_and_cancellation(
                &canonical_gh,
                || {},
                cancellation_for_login,
            )
        });

        wait_for_test_path(&detached_ready, Duration::from_secs(5));
        let cancelled_at = Instant::now();
        cancellation.cancel();
        let outcome = login.join().unwrap().unwrap();
        assert_eq!(outcome, GithubLoginOutcome::Cancelled);
        assert!(cancelled_at.elapsed() < Duration::from_secs(2));
        assert!(!login_marker.exists());

        let pids = fs::read_to_string(probe_pid_file).unwrap();
        let mut pids = pids
            .split_ascii_whitespace()
            .map(|pid| pid.parse::<libc::pid_t>().unwrap());
        let probe_pid = pids.next().unwrap();
        let detached_pid = pids.next().unwrap();
        assert_process_gone(probe_pid, Duration::from_secs(2));
        // This helper deliberately escaped the probe process group. Its
        // inherited output handle must not leave a pipe-reader thread behind
        // or delay cancellation; stop it explicitly after that assertion.
        unsafe {
            libc::kill(detached_pid, libc::SIGKILL);
        }
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_stops_login_while_capability_probe_is_blocked_and_reaps_tree() {
        let _process_test_guard = LOGIN_PROCESS_TEST_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let fake_gh = temp.path().join("gh");
        let pid_file = temp.path().join("login.pids");
        write_executable(
            &fake_gh,
            format!(
                "#!/bin/sh\nsleep 30 &\ndescendant=$!\nprintf '%s %s' \"$$\" \"$descendant\" > {}\nwait\n",
                shell_quote_for_test(&pid_file),
            )
            .as_bytes(),
        );

        let cancellation = GithubLoginCancellation::new();
        let cancellation_for_thread = cancellation.clone();
        let pid_file_for_probe = pid_file.clone();
        let (probe_started_sender, probe_started_receiver) = mpsc::channel();
        let (release_probe_sender, release_probe_receiver) = mpsc::channel();
        let canceller = thread::spawn(move || {
            probe_started_receiver
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
            let cancelled_at = Instant::now();
            cancellation_for_thread.cancel();
            cancelled_at
        });

        let outcome = run_login_via_web_command_with_auth_probe_and_cancellation(
            &fs::canonicalize(fake_gh).unwrap(),
            CommandLimits {
                timeout: Duration::from_secs(10),
                stdout_bytes: 1024,
                stderr_bytes: 1024,
            },
            Duration::ZERO,
            true,
            || {},
            cancellation,
            move || {
                wait_for_test_path(&pid_file_for_probe, Duration::from_secs(5));
                let _ = probe_started_sender.send(());
                let _ = release_probe_receiver.recv_timeout(Duration::from_secs(5));
                Ok(true)
            },
        )
        .unwrap();

        let cancelled_at = canceller.join().unwrap();
        assert!(outcome.cancelled);
        assert!(!outcome.externally_satisfied);
        assert!(cancelled_at.elapsed() < Duration::from_secs(2));
        let pids = fs::read_to_string(pid_file).unwrap();
        for pid in pids
            .split_ascii_whitespace()
            .map(|pid| pid.parse::<libc::pid_t>().unwrap())
        {
            assert_process_gone(pid, Duration::from_secs(2));
        }

        // Let the intentionally detached test probe worker finish promptly.
        let _ = release_probe_sender.send(());
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_interrupts_a_final_repository_probe_after_login_failure() {
        let _process_test_guard = LOGIN_PROCESS_TEST_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let fake_gh = temp.path().join("gh");
        let login_pid_file = temp.path().join("login.pid");
        let probe_pid_file = temp.path().join("probe.pid");
        write_executable(
            &fake_gh,
            format!(
                "#!/bin/sh\nif [ \"$1\" = auth ]; then\n  printf '%s' \"$$\" > {login_pid}\n  exit 17\nfi\nprintf '%s' \"$$\" > {probe_pid}\nsleep 30\n",
                login_pid = shell_quote_for_test(&login_pid_file),
                probe_pid = shell_quote_for_test(&probe_pid_file),
            )
            .as_bytes(),
        );

        let cancellation = GithubLoginCancellation::new();
        let cancellation_for_login = cancellation.clone();
        let canonical_gh = fs::canonicalize(fake_gh).unwrap();
        let login = thread::spawn(move || {
            run_login_via_web_command_with_device_flow_ready_and_cancellation(
                &canonical_gh,
                CommandLimits {
                    timeout: Duration::from_secs(10),
                    stdout_bytes: 1024,
                    stderr_bytes: 1024,
                },
                || {},
                cancellation_for_login,
            )
        });

        wait_for_test_path(&probe_pid_file, Duration::from_secs(5));
        let cancelled_at = Instant::now();
        cancellation.cancel();
        let outcome = login.join().unwrap().unwrap();

        assert!(outcome.cancelled);
        assert!(!outcome.externally_satisfied);
        assert!(cancelled_at.elapsed() < Duration::from_secs(2));
        let login_pid = fs::read_to_string(login_pid_file)
            .unwrap()
            .parse::<libc::pid_t>()
            .unwrap();
        let probe_pid = fs::read_to_string(probe_pid_file)
            .unwrap()
            .parse::<libc::pid_t>()
            .unwrap();
        assert_process_gone(login_pid, Duration::from_secs(2));
        assert_process_gone(probe_pid, Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_wins_over_a_queued_external_auth_success() {
        let _process_test_guard = LOGIN_PROCESS_TEST_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let fake_gh = temp.path().join("gh");
        write_executable(&fake_gh, b"#!/bin/sh\nsleep 30\n");
        let cancellation = GithubLoginCancellation::new();
        let cancellation_for_probe = cancellation.clone();

        let outcome = run_login_via_web_command_with_auth_probe_and_cancellation(
            &fs::canonicalize(fake_gh).unwrap(),
            CommandLimits {
                timeout: Duration::from_secs(10),
                stdout_bytes: 1024,
                stderr_bytes: 1024,
            },
            Duration::ZERO,
            true,
            || {},
            cancellation,
            move || {
                cancellation_for_probe.cancel();
                Ok(true)
            },
        )
        .unwrap();

        assert!(outcome.cancelled);
        assert!(!outcome.externally_satisfied);
    }

    #[cfg(unix)]
    #[test]
    fn observed_process_success_wins_over_callback_cancellation() {
        let _process_test_guard = LOGIN_PROCESS_TEST_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let fake_gh = temp.path().join("gh");
        let release_file = temp.path().join("release-login");
        write_executable(
            &fake_gh,
            format!(
                "#!/bin/sh\nprintf '%s\\n' '{}' >&2\nwhile [ ! -e {} ]; do sleep 0.01; done\nexit 0\n",
                GH_DEVICE_FLOW_URL,
                shell_quote_for_test(&release_file),
            )
            .as_bytes(),
        );
        let cancellation = GithubLoginCancellation::new();
        let cancellation_for_callback = cancellation.clone();

        let outcome = run_login_via_web_command_with_auth_probe_and_cancellation(
            &fs::canonicalize(fake_gh).unwrap(),
            CommandLimits {
                timeout: Duration::from_secs(10),
                stdout_bytes: 1024,
                stderr_bytes: 1024,
            },
            Duration::from_secs(1),
            false,
            move || {
                cancellation_for_callback.cancel();
                fs::write(&release_file, b"ready").unwrap();
                // Keep the callback on the supervisor thread until the child
                // has made its successful exit observable.
                thread::sleep(Duration::from_millis(100));
            },
            cancellation,
            || Ok(false),
        )
        .unwrap();

        assert!(!outcome.cancelled);
        assert!(outcome.output.status.success());
    }

    #[cfg(unix)]
    #[test]
    fn external_authentication_stops_and_reaps_the_waiting_login_process_group() {
        let _process_test_guard = LOGIN_PROCESS_TEST_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let fake_gh = temp.path().join("gh");
        let pid_file = temp.path().join("login.pid");
        write_executable(
            &fake_gh,
            format!(
                "#!/bin/sh\nsleep 5 &\nprintf '%s\\n' \"$$\" > '{}'\nwait\n",
                pid_file.display()
            )
            .as_bytes(),
        );
        let canonical_gh = fs::canonicalize(fake_gh).unwrap();
        let external_ready_at = Arc::new(Mutex::new(None));
        let external_ready_at_for_probe = Arc::clone(&external_ready_at);
        let pid_file_for_probe = pid_file.clone();

        let outcome = run_login_via_web_command_with_auth_probe(
            &canonical_gh,
            CommandLimits {
                timeout: Duration::from_secs(10),
                stdout_bytes: 1024,
                stderr_bytes: 1024,
            },
            Duration::from_millis(10),
            true,
            || {},
            move || {
                let ready = pid_file_for_probe.exists();
                let mut ready_at = external_ready_at_for_probe.lock().unwrap();
                if ready && ready_at.is_none() {
                    *ready_at = Some(Instant::now());
                }
                Ok(ready)
            },
        )
        .unwrap();

        assert!(outcome.externally_satisfied);
        assert!(!outcome.output.status.success());
        assert!(
            external_ready_at
                .lock()
                .unwrap()
                .expect("the external login probe reported readiness")
                .elapsed()
                < Duration::from_secs(2)
        );
        let pid = fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse::<libc::pid_t>()
            .unwrap();
        // A killed but unreaped direct child remains addressable as a zombie.
        // ESRCH here verifies that the runner waited for it before returning.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    }

    #[cfg(unix)]
    #[test]
    fn login_output_limit_stops_and_reaps_a_spamming_process_promptly() {
        let _process_test_guard = LOGIN_PROCESS_TEST_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let fake_gh = temp.path().join("gh");
        let pid_file = temp.path().join("login.pid");
        write_executable(
            &fake_gh,
            format!(
                "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\nwhile :; do printf '0123456789abcdef'; done\n",
                pid_file.display()
            )
            .as_bytes(),
        );
        let started = Instant::now();

        let outcome = run_login_via_web_command_with_auth_probe(
            &fs::canonicalize(fake_gh).unwrap(),
            CommandLimits {
                timeout: Duration::from_secs(10),
                stdout_bytes: 1024,
                stderr_bytes: 1024,
            },
            Duration::from_secs(1),
            true,
            || {},
            || Ok(false),
        )
        .unwrap();

        assert!(outcome.output.stdout_truncated);
        assert!(!outcome.externally_satisfied);
        assert!(started.elapsed() < Duration::from_secs(5));
        let pid = fs::read_to_string(pid_file)
            .unwrap()
            .parse::<libc::pid_t>()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    }

    #[cfg(unix)]
    #[test]
    fn slow_auth_probe_does_not_delay_login_output_quota_enforcement() {
        let _process_test_guard = LOGIN_PROCESS_TEST_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let fake_gh = temp.path().join("gh");
        let pid_file = temp.path().join("login.pid");
        let start_spam_file = temp.path().join("start-spam");
        write_executable(
            &fake_gh,
            format!(
                "#!/bin/sh\nprintf '%s' \"$$\" > '{pid}'\nwhile [ ! -e '{start_spam}' ]; do sleep 0.01; done\nwhile :; do printf '0123456789abcdef'; done\n",
                pid = pid_file.display(),
                start_spam = start_spam_file.display(),
            )
            .as_bytes(),
        );

        let (release_probe, wait_for_release) = mpsc::channel();
        let probe_finished = Arc::new(AtomicBool::new(false));
        let probe_finished_for_worker = Arc::clone(&probe_finished);
        let outcome = run_login_via_web_command_with_auth_probe(
            &fs::canonicalize(fake_gh).unwrap(),
            CommandLimits {
                timeout: Duration::from_secs(10),
                stdout_bytes: 1024,
                stderr_bytes: 1024,
            },
            Duration::ZERO,
            true,
            || {},
            move || {
                fs::write(&start_spam_file, b"ready").unwrap();
                let _ = wait_for_release.recv_timeout(Duration::from_secs(5));
                probe_finished_for_worker.store(true, Ordering::SeqCst);
                Ok(false)
            },
        )
        .unwrap();

        assert!(outcome.output.stdout_truncated);
        assert!(!outcome.externally_satisfied);
        assert!(
            !probe_finished.load(Ordering::SeqCst),
            "the login runner waited for a blocked capability probe"
        );
        let pid = fs::read_to_string(pid_file)
            .unwrap()
            .parse::<libc::pid_t>()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);

        release_probe.send(()).unwrap();
        let release_deadline = Instant::now() + Duration::from_secs(1);
        while !probe_finished.load(Ordering::SeqCst) && Instant::now() < release_deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(probe_finished.load(Ordering::SeqCst));
    }

    #[cfg(unix)]
    #[test]
    fn external_authentication_does_not_wait_for_detached_descendant_output_handles() {
        let _process_test_guard = LOGIN_PROCESS_TEST_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let fake_gh = temp.path().join("gh");
        let ready_file = temp.path().join("detached-ready");
        let pid_file = temp.path().join("detached.pid");
        let test_binary = env::current_exe().unwrap();
        write_executable(
            &fake_gh,
            format!(
                "#!/bin/sh\nPPDUSTER_GITHUB_TEST_DETACHED_READY={} {} --exact github::tests::detached_pipe_holder_helper --ignored &\nhelper_pid=$!\nprintf '%s' \"$helper_pid\" > {}\nwhile [ ! -f {} ]; do\n  if ! kill -0 \"$helper_pid\" 2>/dev/null; then exit 31; fi\n  sleep 0.01\ndone\nwait\n",
                shell_quote_for_test(&ready_file),
                shell_quote_for_test(&test_binary),
                shell_quote_for_test(&pid_file),
                shell_quote_for_test(&ready_file),
            )
            .as_bytes(),
        );

        let mut command = Command::new(fs::canonicalize(fake_gh).unwrap());
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let started = Instant::now();
        let ready_file_for_probe = ready_file.clone();
        let auth_probe = move || Ok(ready_file_for_probe.exists());
        let result = run_login_command(
            command,
            CommandLimits {
                timeout: Duration::from_secs(10),
                stdout_bytes: 1024,
                stderr_bytes: 1024,
            },
            Duration::from_millis(10),
            true,
            None,
            GithubLoginCancellation::new(),
            auth_probe,
        );

        let detached_pid = fs::read_to_string(pid_file)
            .unwrap()
            .parse::<libc::pid_t>()
            .unwrap();
        // The helper deliberately escaped the login process group. Stop it
        // after proving that an inherited output handle cannot block return.
        unsafe {
            libc::kill(detached_pid, libc::SIGKILL);
        }

        let outcome = result.unwrap();
        assert!(outcome.externally_satisfied);
        // The helper stays alive for ten seconds; generous parallel-suite
        // scheduling headroom still distinguishes prompt non-pipe cleanup.
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "subprocess helper for the detached-pipe cleanup regression test"]
    fn detached_pipe_holder_helper() {
        let Some(ready_file) = env::var_os("PPDUSTER_GITHUB_TEST_DETACHED_READY") else {
            return;
        };
        assert_ne!(unsafe { libc::setsid() }, -1);
        fs::write(ready_file, b"ready").unwrap();
        thread::sleep(Duration::from_secs(10));
    }

    #[cfg(unix)]
    #[test]
    fn final_external_auth_probe_wins_race_with_login_failure() {
        let temp = tempfile::tempdir().unwrap();
        let fake_gh = temp.path().join("gh");
        write_executable(&fake_gh, b"#!/bin/sh\nexit 17\n");
        let probe_count = Arc::new(AtomicUsize::new(0));
        let probe_count_for_worker = Arc::clone(&probe_count);

        let outcome = run_login_via_web_command_with_auth_probe(
            &fs::canonicalize(fake_gh).unwrap(),
            CommandLimits {
                timeout: Duration::from_secs(10),
                stdout_bytes: 1024,
                stderr_bytes: 1024,
            },
            Duration::from_secs(60),
            true,
            || {},
            move || {
                probe_count_for_worker.fetch_add(1, Ordering::SeqCst);
                Ok(true)
            },
        )
        .unwrap();

        assert!(outcome.externally_satisfied);
        assert!(!outcome.output.status.success());
        assert_eq!(probe_count.load(Ordering::SeqCst), 1);
    }

    #[cfg(unix)]
    #[test]
    fn verified_repository_access_skips_redundant_login() {
        let temp = tempfile::tempdir().unwrap();
        let fake_gh = temp.path().join("gh");
        let login_marker = temp.path().join("login-started");
        write_executable(
            &fake_gh,
            format!(
                "#!/bin/sh\nif [ \"$1\" = api ] && [ \"$2\" = graphql ]; then\n  printf '%s\\n' 'HTTP/2.0 200 OK' 'Content-Type: application/json' '' '{{\"data\":{{\"viewer\":{{\"login\":\"octocat\",\"repositories\":{{\"nodes\":[]}}}}}}}}'\n  exit 0\nfi\nprintf started > '{}'\nexit 23\n",
                login_marker.display()
            )
            .as_bytes(),
        );

        login_via_web_with_gh_and_device_flow_ready(&fs::canonicalize(fake_gh).unwrap(), || {})
            .unwrap();

        assert!(!login_marker.exists());
    }

    #[cfg(unix)]
    #[test]
    fn rejected_repository_capability_starts_login() {
        let temp = tempfile::tempdir().unwrap();
        let fake_gh = temp.path().join("gh");
        let login_marker = temp.path().join("login-started");
        write_executable(
            &fake_gh,
            format!(
                "#!/bin/sh\nif [ \"$1\" = api ] && [ \"$2\" = graphql ]; then\n  printf '%s\\n' 'HTTP/2.0 200 OK' 'Content-Type: application/json' '' '{{\"data\":null,\"errors\":[{{\"message\":\"Resource protected by organization SAML enforcement\"}}]}}'\n  exit 0\nfi\nprintf started > '{}'\nexit 23\n",
                login_marker.display()
            )
            .as_bytes(),
        );

        let error =
            login_via_web_with_gh_and_device_flow_ready(&fs::canonicalize(fake_gh).unwrap(), || {})
                .unwrap_err()
                .to_string();

        assert!(login_marker.exists());
        assert!(error.contains("GitHub authorization failed"));
    }

    #[cfg(unix)]
    #[test]
    fn preflight_probe_error_does_not_disable_later_external_completion() {
        let temp = tempfile::tempdir().unwrap();
        let fake_gh = temp.path().join("gh");
        let probe_marker = temp.path().join("access-probed");
        let login_marker = temp.path().join("login-started");
        write_executable(
            &fake_gh,
            format!(
                "#!/bin/sh\nif [ \"$1\" = api ] && [ \"$2\" = graphql ]; then\n  if [ ! -e '{probe_marker}' ]; then\n    printf probed > '{probe_marker}'\n    printf '%s\\n' 'HTTP/2.0 200 OK' 'Content-Type: application/json' '' '{{\"data\":'\n  else\n    printf '%s\\n' 'HTTP/2.0 200 OK' 'Content-Type: application/json' '' '{{\"data\":{{\"viewer\":{{\"login\":\"octocat\",\"repositories\":{{\"nodes\":[]}}}}}}}}'\n  fi\n  exit 0\nfi\nprintf started > '{login_marker}'\nsleep 5\nexit 23\n",
                probe_marker = probe_marker.display(),
                login_marker = login_marker.display(),
            )
            .as_bytes(),
        );
        login_via_web_with_gh_and_device_flow_ready(&fs::canonicalize(fake_gh).unwrap(), || {})
            .unwrap();

        assert!(login_marker.exists());
    }

    #[cfg(unix)]
    #[test]
    fn web_login_uses_explicit_non_token_browser_flow() {
        let temp = tempfile::tempdir().unwrap();
        let fake_gh = temp.path().join("gh");
        write_executable(&fake_gh, b"#!/bin/sh\nprintf '%s\\n' \"$@\"\n");

        let output = run_login_via_web_command(
            &fs::canonicalize(fake_gh).unwrap(),
            CommandLimits {
                // This case verifies exact arguments, not timeout behavior.
                // Leave headroom for process scheduling in the parallel suite.
                timeout: Duration::from_secs(10),
                stdout_bytes: 1024,
                stderr_bytes: 1024,
            },
        )
        .unwrap();
        let arguments = String::from_utf8(output.stdout).unwrap();

        assert_eq!(
            arguments.lines().collect::<Vec<_>>(),
            [
                "auth",
                "login",
                "--hostname",
                "github.com",
                "--git-protocol",
                "https",
                "--web",
                "--clipboard",
            ]
        );
        assert!(!arguments.contains("--with-token"));
    }

    fn included_probe_response(headers: &[&str], body: &str) -> Vec<u8> {
        let mut response = b"HTTP/2.0 200 OK\r\n".to_vec();
        for header in headers {
            response.extend_from_slice(header.as_bytes());
            response.extend_from_slice(b"\r\n");
        }
        response.extend_from_slice(b"\r\n");
        response.extend_from_slice(body.as_bytes());
        response
    }

    fn make_executable(path: &Path) {
        write_executable(path, b"#!/bin/sh\n");
    }

    fn write_executable(path: &Path, contents: &[u8]) {
        fs::write(path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions).unwrap();
        }
    }

    #[cfg(unix)]
    fn shell_quote_for_test(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
    }

    #[cfg(unix)]
    fn wait_for_test_path(path: &Path, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while !path.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(path.exists(), "timed out waiting for {}", path.display());
    }

    #[cfg(unix)]
    fn assert_process_gone(pid: libc::pid_t, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while unsafe { libc::kill(pid, 0) } == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "process {pid} was still addressable"
        );
    }

    struct FragmentedReader {
        fragments: VecDeque<Vec<u8>>,
        offset: usize,
    }

    impl FragmentedReader {
        fn new<const N: usize>(fragments: [&[u8]; N]) -> Self {
            Self {
                fragments: fragments
                    .into_iter()
                    .map(|fragment| fragment.to_vec())
                    .collect(),
                offset: 0,
            }
        }
    }

    impl Read for FragmentedReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            loop {
                let Some(fragment) = self.fragments.front() else {
                    return Ok(0);
                };
                if self.offset == fragment.len() {
                    self.fragments.pop_front();
                    self.offset = 0;
                    continue;
                }

                let read = buffer.len().min(fragment.len() - self.offset);
                buffer[..read].copy_from_slice(&fragment[self.offset..self.offset + read]);
                self.offset += read;
                return Ok(read);
            }
        }
    }
}
