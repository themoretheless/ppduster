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

const ACCESSIBLE_REPOSITORIES_QUERY: &str = r#"
query($endCursor: String) {
  viewer {
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

/// List repositories visible to the user authenticated through GitHub CLI.
///
/// The call is noninteractive and asks `gh api graphql` to paginate all
/// repositories owned by the viewer or available through collaboration and
/// organization membership. GitHub CLI remains solely responsible for its
/// credentials; this function never reads or stores a token.
pub fn list_accessible_repositories() -> Result<Vec<GithubRepository>> {
    list_accessible_repositories_inner().map_err(|error| {
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
    let gh = discover_gh().ok_or_else(|| {
        anyhow::anyhow!(
            "GitHub CLI (`gh`) was not found in PATH, /opt/homebrew/bin, or /usr/local/bin. Install it and try again."
        )
    })?;
    let output = run_login_via_web_command(
        &gh,
        CommandLimits {
            timeout: GH_LOGIN_TIMEOUT,
            stdout_bytes: GH_STDERR_LIMIT,
            stderr_bytes: GH_STDERR_LIMIT,
        },
    )?;
    if output.stdout_truncated || output.stderr_truncated {
        bail!("GitHub authorization produced too much diagnostic output");
    }
    if !output.status.success() {
        let detail = classify_gh_failure(output.status, &output.stderr);
        bail!("GitHub authorization failed: {detail}");
    }
    Ok(())
}

fn run_login_via_web_command(gh: &Path, limits: CommandLimits) -> Result<BoundedOutput> {
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
    run_bounded_command(command, limits, "GitHub authorization")
}

fn list_accessible_repositories_inner() -> Result<Vec<GithubRepository>> {
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

    parse_accessible_repositories(&output.stdout)
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
struct BoundedBytes {
    bytes: Vec<u8>,
    truncated: bool,
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
    run_bounded_command(command, limits, "GitHub repository discovery")
}

fn run_bounded_command(
    mut command: Command,
    limits: CommandLimits,
    operation: &str,
) -> Result<BoundedOutput> {
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command
        .spawn()
        .with_context(|| format!("start {operation}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("GitHub CLI stdout was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("GitHub CLI stderr was not captured"))?;
    let stdout_reader = thread::spawn(move || read_bounded(stdout, limits.stdout_bytes));
    let stderr_reader = thread::spawn(move || read_bounded(stderr, limits.stderr_bytes));

    let started = Instant::now();
    let mut status = None;
    loop {
        if status.is_none() {
            status = match child.try_wait() {
                Ok(status) => status,
                Err(error) => {
                    terminate_process_tree(&mut child);
                    let _ = child.wait();
                    return Err(error).context("poll GitHub CLI process status");
                }
            };
        }
        if status.is_some() && stdout_reader.is_finished() && stderr_reader.is_finished() {
            break;
        }
        if started.elapsed() >= limits.timeout {
            terminate_process_tree(&mut child);
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

fn read_bounded(mut reader: impl Read, limit: usize) -> io::Result<BoundedBytes> {
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    let mut truncated = false;
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let available = limit.saturating_sub(bytes.len());
        let retained = available.min(read);
        bytes.extend_from_slice(&buffer[..retained]);
        truncated |= retained < read;
    }
    Ok(BoundedBytes { bytes, truncated })
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

fn terminate_process_tree(child: &mut Child) {
    #[cfg(unix)]
    // SAFETY: `child.id()` belongs to the live child that this function owns,
    // and `run_gh_command` placed that child in a fresh process group whose ID
    // equals its PID. No pointer or shared-memory invariants are involved.
    unsafe {
        // `run_gh_command` starts gh as the leader of a fresh process group. Kill
        // the group so descendants cannot keep stdout/stderr open after timeout.
        if libc::killpg(child.id() as libc::pid_t, libc::SIGKILL) != 0 {
            let _ = child.kill();
        }
    }
    #[cfg(not(unix))]
    let _ = child.kill();
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

fn parse_accessible_repositories(input: &[u8]) -> Result<Vec<GithubRepository>> {
    let pages: Vec<GraphqlPage> =
        serde_json::from_slice(input).context("parse paginated GitHub repository response")?;
    let mut repositories: Vec<GithubRepository> = Vec::new();
    let mut seen_ids = HashSet::new();

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
        for repository in data.viewer.repositories.nodes.into_iter().flatten() {
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
    Ok(repositories)
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

    const REPOSITORY_PAGES: &str = r#"
[
  {
    "data": {
      "viewer": {
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
        let repositories = parse_accessible_repositories(REPOSITORY_PAGES.as_bytes()).unwrap();

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
        let error = parse_accessible_repositories(
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
        let error = parse_accessible_repositories(response.to_string().as_bytes())
            .unwrap_err()
            .to_string();

        assert!(!error.contains("ghp_super-secret"));
        assert!(error.contains("[redacted]"));
        assert!(error.chars().count() < 500);
    }

    #[test]
    fn query_requests_all_accessible_affiliations_and_picker_fields() {
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
                timeout: Duration::from_secs(1),
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

    #[cfg(unix)]
    #[test]
    fn web_login_uses_explicit_non_token_browser_flow() {
        let temp = tempfile::tempdir().unwrap();
        let fake_gh = temp.path().join("gh");
        write_executable(&fake_gh, b"#!/bin/sh\nprintf '%s\\n' \"$@\"\n");

        let output = run_login_via_web_command(
            &fs::canonicalize(fake_gh).unwrap(),
            CommandLimits {
                timeout: Duration::from_secs(1),
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
}
