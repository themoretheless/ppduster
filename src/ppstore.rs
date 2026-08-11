//! Safe adapter for the standalone `ppstore` executable.
//!
//! `ppduster` deliberately does not implement App Store installation here.
//! This module resolves a separately installed `ppstore`, launches it without
//! a shell, and accepts only the versioned JSON mutation protocol.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const PPSTORE_OVERRIDE_ENV: &str = "PPDUSTER_PPSTORE_PATH";
const PPSTORE_EXECUTABLE: &str = "ppstore";
const SUPPORTED_VERSION_PREFIX: &str = "ppstore 0.1.";
const MUTATION_PROTOCOL_VERSION: u32 = 1;
const PPSTORE_REQUEST_TIMEOUT_SECS: u64 = 30;
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const CHILD_PROCESS_TIMEOUT: Duration = Duration::from_secs(120);
const CAPTURE_LIMIT_BYTES: usize = 1024 * 1024;
const ERROR_DETAIL_CHARS: usize = 512;
const POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallOperation {
    Install,
    Get,
}

impl InstallOperation {
    fn protocol_name(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Get => "get",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    Applied(String),
    AlreadySatisfied(String),
}

/// Resolve the external `ppstore` executable using a deterministic trust order.
///
/// An explicit `PPDUSTER_PPSTORE_PATH` must be absolute and valid; an invalid
/// override is a hard error and never silently falls through to another binary.
pub fn resolve_executable() -> Result<PathBuf> {
    let explicit = env::var_os(PPSTORE_OVERRIDE_ENV);
    let current_executable = env::current_exe().ok();
    let home = dirs::home_dir();
    let fixed = [
        PathBuf::from("/usr/local/bin/ppstore"),
        PathBuf::from("/opt/homebrew/bin/ppstore"),
    ];
    resolve_executable_in(
        explicit.as_deref(),
        current_executable.as_deref(),
        home.as_deref(),
        env::var_os("PATH").as_deref(),
        &fixed,
    )
}

/// Run a user-facing `ppstore` command with inherited stdout/stderr.
///
/// Arguments are passed directly to the resolved executable. No shell is
/// involved, stdin is closed, and dynamic-loader injection variables are
/// removed from the child environment.
pub fn run_passthrough(args: &[OsString]) -> Result<ExitStatus> {
    let executable = resolve_executable()?;
    probe_compatible_version(&executable)?;
    let mut command = ppstore_command(&executable);
    command.args(args);
    command
        .status()
        .with_context(|| format!("run ppstore at {}", safe_path_label(&executable)))
}

/// Install or obtain one App Store application through `ppstore` protocol v1.
pub fn install(app_id: u64, get: bool, country: Option<&str>) -> Result<InstallOutcome> {
    if app_id == 0 {
        bail!("App Store application ID must be greater than zero");
    }
    let country = country.map(normalize_country).transpose()?;
    let operation = if get {
        InstallOperation::Get
    } else {
        InstallOperation::Install
    };
    let executable = resolve_executable()?;
    probe_compatible_version(&executable)?;
    install_with_executable(
        &executable,
        app_id,
        operation,
        country.as_deref(),
        CHILD_PROCESS_TIMEOUT,
    )
}

fn probe_compatible_version(executable: &Path) -> Result<()> {
    let mut command = ppstore_command(executable);
    command
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = run_bounded(command, VERSION_PROBE_TIMEOUT)
        .with_context(|| format!("probe ppstore at {}", safe_path_label(executable)))?;
    if !output.status.success() {
        let detail = sanitize_detail(
            &String::from_utf8_lossy(&output.stderr.bytes),
            ERROR_DETAIL_CHARS,
        );
        bail!(
            "ppstore --version exited with {}{}",
            status_label(output.status),
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        );
    }
    if output.stdout.truncated {
        bail!("ppstore --version returned an oversized response");
    }
    let version = std::str::from_utf8(&output.stdout.bytes)
        .context("ppstore --version returned non-UTF-8 output")?;
    validate_version(version)
}

fn validate_version(version: &str) -> Result<()> {
    let version = version.trim();
    let suffix = version.strip_prefix(SUPPORTED_VERSION_PREFIX);
    if !suffix.is_some_and(|suffix| {
        suffix
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_digit())
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'.')
    }) {
        bail!(
            "unsupported ppstore version {:?}; expected ppstore 0.1.x",
            sanitize_detail(version, 80)
        );
    }
    Ok(())
}

fn install_with_executable(
    executable: &Path,
    app_id: u64,
    operation: InstallOperation,
    country: Option<&str>,
    process_timeout: Duration,
) -> Result<InstallOutcome> {
    let args = install_arguments(app_id, operation, country);
    let mut command = ppstore_command(executable);
    command
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = run_bounded(command, process_timeout).with_context(|| {
        format!(
            "run ppstore {} for application {}",
            operation.protocol_name(),
            app_id
        )
    })?;

    if output.stdout.truncated {
        bail!(
            "ppstore returned more than {} bytes on stdout; refusing a truncated protocol response",
            CAPTURE_LIMIT_BYTES
        );
    }

    if !output.status.success() {
        let detail = mutation_failure_detail(&output.stdout.bytes, &output.stderr.bytes, app_id);
        bail!(
            "ppstore {} for application {} exited with {}: {}",
            operation.protocol_name(),
            app_id,
            status_label(output.status),
            detail
        );
    }

    let report: MutationReportV1 =
        serde_json::from_slice(&output.stdout.bytes).map_err(|error| {
            let detail = sanitize_detail(&String::from_utf8_lossy(&output.stdout.bytes), 240);
            anyhow!(
                "parse ppstore mutation protocol v1: {}; stdout: {}",
                error,
                if detail.is_empty() {
                    "<empty>"
                } else {
                    &detail
                }
            )
        })?;
    validate_mutation_report(report, app_id, operation)
}

fn install_arguments(
    app_id: u64,
    operation: InstallOperation,
    country: Option<&str>,
) -> Vec<OsString> {
    let mut args = vec![
        "--output".into(),
        "json".into(),
        "install".into(),
        app_id.to_string().into(),
    ];
    if let Some(country) = country {
        args.push("--country".into());
        args.push(country.into());
    }
    if operation == InstallOperation::Get {
        args.push("--get".into());
    }
    args.extend([
        OsString::from("--yes"),
        OsString::from("--no-wait"),
        OsString::from("--timeout"),
        OsString::from(PPSTORE_REQUEST_TIMEOUT_SECS.to_string()),
    ]);
    args
}

fn resolve_executable_in(
    explicit: Option<&OsStr>,
    current_executable: Option<&Path>,
    home: Option<&Path>,
    path: Option<&OsStr>,
    fixed_candidates: &[PathBuf],
) -> Result<PathBuf> {
    if let Some(explicit) = explicit {
        let candidate = Path::new(explicit);
        if !candidate.is_absolute() {
            bail!(
                "{} must contain an absolute executable path, got {}",
                PPSTORE_OVERRIDE_ENV,
                safe_path_label(candidate)
            );
        }
        return validate_executable(candidate).with_context(|| {
            format!(
                "invalid explicit ppstore executable from {}: {}",
                PPSTORE_OVERRIDE_ENV,
                safe_path_label(candidate)
            )
        });
    }

    let mut candidates = Vec::new();
    if let Some(parent) = current_executable.and_then(Path::parent) {
        candidates.push(parent.join(ppstore_executable_name()));
    }
    candidates.extend(fixed_candidates.iter().cloned());
    if let Some(home) = home {
        candidates.push(home.join(".cargo/bin").join(ppstore_executable_name()));
    }
    if let Some(path) = path {
        candidates.extend(
            env::split_paths(path)
                .filter(|directory| directory.is_absolute())
                .map(|directory| directory.join(ppstore_executable_name())),
        );
    }

    let mut seen = HashSet::new();
    let mut rejected = Vec::new();
    for candidate in candidates {
        if !seen.insert(candidate.clone()) || !candidate.exists() {
            continue;
        }
        match validate_executable(&candidate) {
            Ok(executable) => return Ok(executable),
            Err(error) => rejected.push(format!(
                "{} ({})",
                safe_path_label(&candidate),
                sanitize_detail(&error.to_string(), 160)
            )),
        }
    }

    let rejected = if rejected.is_empty() {
        String::new()
    } else {
        format!(" Rejected candidates: {}.", rejected.join(", "))
    };
    bail!(
        "ppstore was not found beside ppduster, in /usr/local/bin, /opt/homebrew/bin, ~/.cargo/bin, or an absolute PATH entry. Set {} to a trusted absolute path.{}",
        PPSTORE_OVERRIDE_ENV,
        rejected
    )
}

fn validate_executable(candidate: &Path) -> Result<PathBuf> {
    let canonical = candidate
        .canonicalize()
        .with_context(|| format!("resolve {}", safe_path_label(candidate)))?;
    let metadata = fs::metadata(&canonical)
        .with_context(|| format!("inspect {}", safe_path_label(&canonical)))?;
    if !metadata.is_file() {
        bail!("resolved path is not a regular file");
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode();
        if mode & 0o111 == 0 {
            bail!("resolved file is not executable");
        }
        if mode & 0o022 != 0 {
            bail!("resolved executable is writable by group or other users");
        }
    }

    Ok(canonical)
}

fn ppstore_executable_name() -> &'static str {
    if cfg!(windows) {
        "ppstore.exe"
    } else {
        PPSTORE_EXECUTABLE
    }
}

fn ppstore_command(executable: &Path) -> Command {
    let mut command = Command::new(executable);
    command.stdin(Stdio::null());
    sanitize_dynamic_loader_environment(&mut command);
    command
}

fn sanitize_dynamic_loader_environment(command: &mut Command) {
    sanitize_dynamic_loader_environment_from(command, env::vars_os().map(|(key, _)| key));
}

fn sanitize_dynamic_loader_environment_from(
    command: &mut Command,
    keys: impl IntoIterator<Item = OsString>,
) {
    for key in keys {
        if is_dynamic_loader_injection_key(&key) {
            command.env_remove(key);
        }
    }
}

fn is_dynamic_loader_injection_key(key: &OsStr) -> bool {
    let Some(key) = key.to_str() else {
        return false;
    };
    let upper = key.to_ascii_uppercase();
    upper.starts_with("DYLD_")
        || matches!(
            upper.as_str(),
            "LD_PRELOAD" | "LD_LIBRARY_PATH" | "LD_AUDIT"
        )
}

fn normalize_country(country: &str) -> Result<String> {
    let country = country.trim();
    if country.len() != 2 || !country.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        bail!("country must be a two-letter ISO 3166-1 code");
    }
    Ok(country.to_ascii_uppercase())
}

#[derive(Debug, Deserialize)]
struct MutationReportV1 {
    protocol_version: u32,
    operation: String,
    apply: bool,
    wait: bool,
    timeout_millis: u64,
    requested_count: usize,
    results: Vec<MutationResultV1>,
    errors: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct MutationResultV1 {
    adam_id: u64,
    status: String,
    message: String,
}

fn validate_mutation_report(
    mut report: MutationReportV1,
    expected_app_id: u64,
    expected_operation: InstallOperation,
) -> Result<InstallOutcome> {
    if report.protocol_version != MUTATION_PROTOCOL_VERSION {
        bail!(
            "unsupported ppstore mutation protocol {}; expected {}",
            report.protocol_version,
            MUTATION_PROTOCOL_VERSION
        );
    }
    if report.operation != expected_operation.protocol_name() {
        bail!(
            "ppstore reported operation {:?}; expected {:?}",
            sanitize_detail(&report.operation, 40),
            expected_operation.protocol_name()
        );
    }
    if !report.apply {
        bail!("ppstore protocol response says apply=false after an applied request");
    }
    if report.wait {
        bail!("ppstore protocol response says wait=true after a --no-wait request");
    }
    let expected_timeout = PPSTORE_REQUEST_TIMEOUT_SECS * 1000;
    if report.timeout_millis != expected_timeout {
        bail!(
            "ppstore reported timeout_millis={}; expected {}",
            report.timeout_millis,
            expected_timeout
        );
    }
    if report.requested_count != 1 || report.results.len() != 1 {
        bail!(
            "ppstore protocol must contain exactly one requested result; requested_count={}, results={}",
            report.requested_count,
            report.results.len()
        );
    }
    if !report.errors.is_empty() {
        let detail = sanitize_detail(&report.errors.join("; "), ERROR_DETAIL_CHARS);
        bail!("ppstore reported batch errors: {detail}");
    }

    let result = report.results.pop().expect("length checked above");
    if result.adam_id != expected_app_id {
        bail!(
            "ppstore returned application ID {}; expected {}",
            result.adam_id,
            expected_app_id
        );
    }
    let message = sanitize_detail(&result.message, ERROR_DETAIL_CHARS);
    match result.status.as_str() {
        "queued" | "pending" | "completed" => Ok(InstallOutcome::Applied(message)),
        "already-installed" => Ok(InstallOutcome::AlreadySatisfied(message)),
        other => bail!(
            "ppstore returned non-success install status {:?}: {}",
            sanitize_detail(other, 80),
            if message.is_empty() {
                "no detail"
            } else {
                &message
            }
        ),
    }
}

struct BoundedOutput {
    status: ExitStatus,
    stdout: CapturedStream,
    stderr: CapturedStream,
}

struct CapturedStream {
    bytes: Vec<u8>,
    truncated: bool,
}

fn run_bounded(mut command: Command, timeout: Duration) -> Result<BoundedOutput> {
    let mut child = command.spawn().context("start ppstore process")?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("ppstore stdout was not piped"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("ppstore stderr was not piped"))?;
    let stdout_reader = thread::spawn(move || read_bounded(stdout, CAPTURE_LIMIT_BYTES));
    let stderr_reader = thread::spawn(move || read_bounded(stderr, CAPTURE_LIMIT_BYTES));

    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().context("poll ppstore process")? {
            break status;
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            let _ = join_reader(stdout_reader, "stdout");
            let stderr = join_reader(stderr_reader, "stderr").unwrap_or(CapturedStream {
                bytes: Vec::new(),
                truncated: false,
            });
            let detail = sanitize_detail(&String::from_utf8_lossy(&stderr.bytes), 240);
            bail!(
                "ppstore process exceeded {} ms{}",
                timeout.as_millis(),
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                }
            );
        }
        thread::sleep(POLL_INTERVAL.min(timeout.saturating_sub(started.elapsed())));
    };

    Ok(BoundedOutput {
        status,
        stdout: join_reader(stdout_reader, "stdout")?,
        stderr: join_reader(stderr_reader, "stderr")?,
    })
}

fn read_bounded(mut reader: impl Read, limit: usize) -> io::Result<CapturedStream> {
    let mut bytes = Vec::with_capacity(limit.min(8192));
    let mut truncated = false;
    let mut buffer = [0_u8; 8192];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len());
        let retained = remaining.min(count);
        bytes.extend_from_slice(&buffer[..retained]);
        truncated |= retained < count;
    }
    Ok(CapturedStream { bytes, truncated })
}

fn join_reader(
    reader: thread::JoinHandle<io::Result<CapturedStream>>,
    stream: &str,
) -> Result<CapturedStream> {
    reader
        .join()
        .map_err(|_| anyhow!("ppstore {stream} reader panicked"))?
        .with_context(|| format!("read ppstore {stream}"))
}

fn mutation_failure_detail(stdout: &[u8], stderr: &[u8], expected_app_id: u64) -> String {
    if let Ok(report) = serde_json::from_slice::<MutationReportV1>(stdout) {
        if let Some(result) = report
            .results
            .iter()
            .find(|result| result.adam_id == expected_app_id)
        {
            let detail = sanitize_detail(&result.message, ERROR_DETAIL_CHARS);
            if !detail.is_empty() {
                return detail;
            }
        }
        let detail = sanitize_detail(&report.errors.join("; "), ERROR_DETAIL_CHARS);
        if !detail.is_empty() {
            return detail;
        }
    }
    let detail = sanitize_detail(&String::from_utf8_lossy(stderr), ERROR_DETAIL_CHARS);
    if !detail.is_empty() {
        return detail;
    }
    let detail = sanitize_detail(&String::from_utf8_lossy(stdout), ERROR_DETAIL_CHARS);
    if detail.is_empty() {
        "no error detail".into()
    } else {
        detail
    }
}

fn status_label(status: ExitStatus) -> String {
    status
        .code()
        .map(|code| format!("exit code {code}"))
        .unwrap_or_else(|| "signal termination".into())
}

fn sanitize_detail(raw: &str, max_chars: usize) -> String {
    let mut plain = String::with_capacity(raw.len().min(max_chars));
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
            plain.push(' ');
        } else {
            plain.push(character);
        }
    }
    let normalized = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut characters = normalized.chars();
    let mut shortened = characters.by_ref().take(max_chars).collect::<String>();
    if characters.next().is_some() && max_chars > 0 {
        shortened.pop();
        shortened.push('…');
    }
    shortened
}

fn safe_path_label(path: &Path) -> String {
    sanitize_detail(&path.to_string_lossy(), 240)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn report_json(
        app_id: u64,
        operation: &str,
        status: &str,
        apply: bool,
        result_count: usize,
    ) -> Vec<u8> {
        let results = (0..result_count)
            .map(|index| {
                serde_json::json!({
                    "adam_id": app_id + index as u64,
                    "status": status,
                    "message": format!("status {status}")
                })
            })
            .collect::<Vec<_>>();
        serde_json::to_vec(&serde_json::json!({
            "protocol_version": 1,
            "country": "US",
            "operation": operation,
            "apply": apply,
            "wait": false,
            "timeout_millis": 30000,
            "requested_count": result_count,
            "results": results,
            "warnings": [],
            "errors": []
        }))
        .unwrap()
    }

    fn parse_report(bytes: &[u8]) -> MutationReportV1 {
        serde_json::from_slice(bytes).unwrap()
    }

    #[test]
    fn install_arguments_are_exact_and_get_is_explicit() {
        let install = install_arguments(497_799_835, InstallOperation::Install, Some("US"));
        assert_eq!(
            install,
            [
                "--output",
                "json",
                "install",
                "497799835",
                "--country",
                "US",
                "--yes",
                "--no-wait",
                "--timeout",
                "30"
            ]
            .map(OsString::from)
        );

        let get = install_arguments(640_199_958, InstallOperation::Get, None);
        assert_eq!(
            get,
            [
                "--output",
                "json",
                "install",
                "640199958",
                "--get",
                "--yes",
                "--no-wait",
                "--timeout",
                "30"
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn accepted_protocol_statuses_map_to_closed_outcomes() {
        for status in ["queued", "pending", "completed"] {
            let report = parse_report(&report_json(42, "install", status, true, 1));
            let outcome = validate_mutation_report(report, 42, InstallOperation::Install).unwrap();
            assert_eq!(outcome, InstallOutcome::Applied(format!("status {status}")));
        }

        let report = parse_report(&report_json(42, "install", "already-installed", true, 1));
        let outcome = validate_mutation_report(report, 42, InstallOperation::Install).unwrap();
        assert_eq!(
            outcome,
            InstallOutcome::AlreadySatisfied("status already-installed".into())
        );
    }

    #[test]
    fn every_other_status_fails_closed() {
        for status in [
            "planned",
            "skipped",
            "current",
            "incompatible",
            "price-required",
            "not-installed",
            "not-found",
            "failed",
            "future-success-like-status",
        ] {
            let report = parse_report(&report_json(42, "install", status, true, 1));
            let error = validate_mutation_report(report, 42, InstallOperation::Install)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("non-success install status"),
                "{status}: {error}"
            );
        }
    }

    #[test]
    fn protocol_shape_mismatches_fail_closed() {
        let cases = [
            ("protocol_version", serde_json::json!(2)),
            ("operation", serde_json::json!("get")),
            ("apply", serde_json::json!(false)),
            ("wait", serde_json::json!(true)),
            ("timeout_millis", serde_json::json!(29_999)),
            ("requested_count", serde_json::json!(2)),
        ];
        for (field, replacement) in cases {
            let mut value: serde_json::Value =
                serde_json::from_slice(&report_json(42, "install", "queued", true, 1)).unwrap();
            value[field] = replacement;
            let report: MutationReportV1 = serde_json::from_value(value).unwrap();
            assert!(
                validate_mutation_report(report, 42, InstallOperation::Install).is_err(),
                "field {field} must be enforced"
            );
        }

        let report = parse_report(&report_json(42, "install", "queued", true, 2));
        assert!(validate_mutation_report(report, 42, InstallOperation::Install).is_err());

        let report = parse_report(&report_json(41, "install", "queued", true, 1));
        assert!(validate_mutation_report(report, 42, InstallOperation::Install).is_err());

        let mut value: serde_json::Value =
            serde_json::from_slice(&report_json(42, "install", "queued", true, 1)).unwrap();
        value["errors"] = serde_json::json!(["batch failed"]);
        let report: MutationReportV1 = serde_json::from_value(value).unwrap();
        assert!(validate_mutation_report(report, 42, InstallOperation::Install).is_err());
    }

    #[test]
    fn country_is_normalized_before_process_launch() {
        assert_eq!(normalize_country(" ru ").unwrap(), "RU");
        for invalid in ["", "R", "RUS", "1A", "ру"] {
            assert!(normalize_country(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn compatibility_probe_accepts_only_ppstore_zero_one() {
        for supported in ["ppstore 0.1.0", "ppstore 0.1.23\n"] {
            validate_version(supported).unwrap();
        }
        for unsupported in [
            "",
            "ppstore 0.1.",
            "ppstore 0.2.0",
            "ppstore 1.0.0",
            "other 0.1.0",
            "ppstore 0.1.0 unexpected",
        ] {
            assert!(
                validate_version(unsupported).is_err(),
                "accepted {unsupported:?}"
            );
        }
    }

    #[test]
    fn dynamic_loader_environment_keys_are_removed_case_insensitively() {
        let mut command = Command::new("ignored");
        sanitize_dynamic_loader_environment_from(
            &mut command,
            [
                "DYLD_INSERT_LIBRARIES",
                "dyld_library_path",
                "LD_PRELOAD",
                "ld_library_path",
                "LD_AUDIT",
                "PATH",
            ]
            .map(OsString::from),
        );
        let removed = command
            .get_envs()
            .filter_map(|(key, value)| value.is_none().then_some(key.to_owned()))
            .collect::<HashSet<_>>();
        for key in [
            "DYLD_INSERT_LIBRARIES",
            "dyld_library_path",
            "LD_PRELOAD",
            "ld_library_path",
            "LD_AUDIT",
        ] {
            assert!(removed.contains(OsStr::new(key)), "did not remove {key}");
        }
        assert!(!removed.contains(OsStr::new("PATH")));
    }

    #[test]
    fn bounded_reader_drains_but_retains_only_the_limit() {
        let capture = read_bounded(Cursor::new(vec![b'x'; 4096]), 100).unwrap();
        assert_eq!(capture.bytes.len(), 100);
        assert!(capture.truncated);
        let capture = read_bounded(Cursor::new(b"short"), 100).unwrap();
        assert_eq!(capture.bytes, b"short");
        assert!(!capture.truncated);
    }

    #[test]
    fn error_details_are_plain_single_line_and_bounded() {
        let detail = sanitize_detail(
            &format!("\u{1b}[31mfailed\u{1b}[0m\n{}", "x".repeat(1000)),
            80,
        );
        assert!(!detail.contains('\u{1b}'));
        assert!(!detail.contains('\n'));
        assert!(detail.chars().count() <= 80);
        assert!(detail.ends_with('…'));
    }

    #[cfg(unix)]
    fn make_executable(path: &Path, body: &str, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn explicit_override_is_absolute_validated_and_never_falls_through() {
        let temp = tempfile::tempdir().unwrap();
        let fallback = temp.path().join("fallback");
        make_executable(&fallback, "#!/bin/sh\nexit 0\n", 0o755);

        let error = resolve_executable_in(
            Some(OsStr::new("relative/ppstore")),
            None,
            None,
            None,
            std::slice::from_ref(&fallback),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("absolute executable path"));

        let missing = temp.path().join("missing");
        assert!(
            resolve_executable_in(Some(missing.as_os_str()), None, None, None, &[fallback])
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolver_uses_sibling_fixed_cargo_then_only_absolute_path_entries() {
        let temp = tempfile::tempdir().unwrap();
        let current_dir = temp.path().join("current");
        let fixed_dir = temp.path().join("fixed");
        let home = temp.path().join("home");
        let cargo_dir = home.join(".cargo/bin");
        let path_dir = temp.path().join("path");
        for directory in [&current_dir, &fixed_dir, &cargo_dir, &path_dir] {
            fs::create_dir_all(directory).unwrap();
        }
        let current_exe = current_dir.join("ppduster");
        make_executable(&current_exe, "#!/bin/sh\n", 0o755);
        let sibling = current_dir.join(ppstore_executable_name());
        let fixed = fixed_dir.join(ppstore_executable_name());
        let cargo = cargo_dir.join(ppstore_executable_name());
        let path_ppstore = path_dir.join(ppstore_executable_name());
        for candidate in [&sibling, &fixed, &cargo, &path_ppstore] {
            make_executable(candidate, "#!/bin/sh\n", 0o755);
        }
        let absolute_path = env::join_paths([&path_dir]).unwrap();

        assert_eq!(
            resolve_executable_in(
                None,
                Some(&current_exe),
                Some(&home),
                Some(&absolute_path),
                std::slice::from_ref(&fixed),
            )
            .unwrap(),
            sibling.canonicalize().unwrap()
        );
        fs::remove_file(&sibling).unwrap();
        assert_eq!(
            resolve_executable_in(
                None,
                Some(&current_exe),
                Some(&home),
                Some(&absolute_path),
                std::slice::from_ref(&fixed),
            )
            .unwrap(),
            fixed.canonicalize().unwrap()
        );
        fs::remove_file(&fixed).unwrap();
        assert_eq!(
            resolve_executable_in(
                None,
                Some(&current_exe),
                Some(&home),
                Some(&absolute_path),
                &[],
            )
            .unwrap(),
            cargo.canonicalize().unwrap()
        );
        fs::remove_file(&cargo).unwrap();
        assert_eq!(
            resolve_executable_in(
                None,
                Some(&current_exe),
                Some(&home),
                Some(&absolute_path),
                &[],
            )
            .unwrap(),
            path_ppstore.canonicalize().unwrap()
        );

        fs::remove_file(&path_ppstore).unwrap();
        let relative_dir = PathBuf::from("relative-bin");
        let mixed_path = env::join_paths([relative_dir, path_dir.clone()]).unwrap();
        assert!(
            resolve_executable_in(None, Some(&current_exe), None, Some(&mixed_path), &[]).is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolver_canonicalizes_symlinks_and_rejects_unsafe_modes() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("real-ppstore");
        make_executable(&target, "#!/bin/sh\n", 0o755);
        let link = temp.path().join("ppstore-link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(
            validate_executable(&link).unwrap(),
            target.canonicalize().unwrap()
        );

        make_executable(&target, "#!/bin/sh\n", 0o775);
        assert!(validate_executable(&target)
            .unwrap_err()
            .to_string()
            .contains("writable by group"));
        make_executable(&target, "#!/bin/sh\n", 0o644);
        assert!(validate_executable(&target)
            .unwrap_err()
            .to_string()
            .contains("not executable"));
        fs::remove_file(&target).unwrap();
        fs::create_dir(&target).unwrap();
        assert!(validate_executable(&target)
            .unwrap_err()
            .to_string()
            .contains("not a regular file"));
    }

    #[cfg(unix)]
    fn shell_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }

    #[cfg(unix)]
    fn write_fake_ppstore(path: &Path, json: &[u8], exit_code: i32, args_file: Option<&Path>) {
        let encoded = shell_quote(std::str::from_utf8(json).unwrap());
        let record_args = args_file.map_or_else(String::new, |args_file| {
            format!(
                "printf '%s\\n' \"$@\" > {}\n",
                shell_quote(&args_file.to_string_lossy())
            )
        });
        let script = format!("#!/bin/sh\n{record_args}printf '%s' {encoded}\nexit {exit_code}\n");
        make_executable(path, &script, 0o755);
    }

    #[cfg(unix)]
    #[test]
    fn fake_executable_receives_exact_arguments_and_never_a_shell_command() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("ppstore");
        let args_file = temp.path().join("args");
        write_fake_ppstore(
            &executable,
            &report_json(42, "get", "queued", true, 1),
            0,
            Some(&args_file),
        );

        let outcome = install_with_executable(
            &executable,
            42,
            InstallOperation::Get,
            Some("RU"),
            // This case verifies the process contract, not timeout behavior.
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(outcome, InstallOutcome::Applied("status queued".into()));

        let args = fs::read_to_string(args_file).unwrap();
        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            [
                "--output",
                "json",
                "install",
                "42",
                "--country",
                "RU",
                "--get",
                "--yes",
                "--no-wait",
                "--timeout",
                "30"
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn fake_nonzero_exit_is_sanitized_and_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("ppstore");
        make_executable(
            &executable,
            "#!/bin/sh\nprintf '\\033[31mfailed\\033[0m\\n' >&2\nexit 7\n",
            0o755,
        );
        let error = install_with_executable(
            &executable,
            42,
            InstallOperation::Install,
            None,
            // This case verifies error sanitization, not timeout behavior.
            Duration::from_secs(10),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("exit code 7"));
        assert!(error.contains("failed"));
        assert!(!error.contains('\u{1b}'));
    }

    #[cfg(unix)]
    #[test]
    fn fake_process_timeout_is_bounded_and_killed() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("ppstore");
        make_executable(&executable, "#!/bin/sh\nexec /bin/sleep 5\n", 0o755);
        let started = Instant::now();
        let error = install_with_executable(
            &executable,
            42,
            InstallOperation::Install,
            None,
            Duration::from_millis(80),
        )
        .unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains("exceeded"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
