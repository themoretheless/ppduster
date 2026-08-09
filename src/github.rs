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
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

const GH_FALLBACK_PATHS: &[&str] = &["/opt/homebrew/bin/gh", "/usr/local/bin/gh"];

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
    let gh = discover_gh().ok_or_else(|| {
        anyhow::anyhow!(
            "GitHub CLI (`gh`) was not found in PATH, /opt/homebrew/bin, or /usr/local/bin. Install it and authenticate with `gh auth login`."
        )
    })?;

    let output = Command::new(&gh)
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
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("run GitHub CLI at {}", gh.display()))?;

    if !output.status.success() {
        let detail = classify_gh_failure(output.status, &output.stderr);
        bail!(
            "GitHub repository discovery failed: {detail}. Check `gh auth status`; authenticate or refresh access with `gh auth login`."
        );
    }

    parse_accessible_repositories(&output.stdout)
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
    let mut in_escape_sequence = false;
    for character in raw.chars() {
        if in_escape_sequence {
            if character.is_ascii_alphabetic() {
                in_escape_sequence = false;
            }
            continue;
        }
        if character == '\u{1b}' {
            in_escape_sequence = true;
        } else if character.is_control() {
            without_controls.push(' ');
        } else {
            without_controls.push(character);
        }
    }

    let redacted = without_controls
        .split_whitespace()
        .map(|part| {
            if ["ghp_", "github_pat_", "gho_", "ghu_", "ghs_", "ghr_"]
                .iter()
                .any(|prefix| part.contains(prefix))
            {
                "[redacted]"
            } else {
                part
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
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
            let candidate = directory.join(gh_executable_name());
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }

    fallback_paths
        .iter()
        .find(|candidate| is_executable_file(candidate))
        .cloned()
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
        assert!(detail.ends_with('…'));
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
            Some(path_gh)
        );
        assert_eq!(
            discover_gh_in(Some(OsStr::new("")), std::slice::from_ref(&fallback_gh)),
            Some(fallback_gh)
        );
    }

    #[test]
    fn command_discovery_ignores_missing_candidates() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing-gh");

        assert_eq!(discover_gh_in(None, &[missing]), None);
    }

    fn make_executable(path: &Path) {
        fs::write(path, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions).unwrap();
        }
    }
}
