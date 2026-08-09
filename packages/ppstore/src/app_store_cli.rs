//! CLI-facing orchestration for Mac App Store catalog and installer operations.
//!
//! The catalog module owns discovery and inventory, while the installer module
//! only asks Apple's services to enqueue a download.  This module joins those
//! pieces into batch reports: every requested application gets its own result,
//! and optional waiting verifies the installed receipt/version by rescanning.

use crate::app_store::{
    self, CatalogApp, InstalledApp, InstalledReport, SearchReport, UpdateCandidate, UpdateReport,
};
use crate::app_store_installer::{self, InstallerBackendStatus, QueueStatus, StoreOperation};
use crate::OutputFormat;
use anyhow::{bail, Result};
use serde::Serialize;
use std::collections::HashSet;
use std::env;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};
use tabled::{
    settings::{object::Columns, Modify, Style, Width},
    Table, Tabled,
};

const COUNTRY_ENV: &str = "PPSTORE_COUNTRY";
const APPLE_LOCALE_ENV: &str = "AppleLocale";
const DEFAULT_COUNTRY: &str = "US";
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const MAX_QUEUE_REQUEST_TIMEOUT: Duration = Duration::from_secs(90);

/// Resolve the App Store storefront in CLI precedence order.
///
/// An explicit `--country` wins, followed by
/// `PPSTORE_COUNTRY`, the region embedded in `AppleLocale`, and
/// finally `US`. Explicit/configured country codes are validated instead of
/// silently falling through to another storefront.
pub fn resolve_country(explicit: Option<&str>) -> Result<String> {
    if let Some(country) = explicit {
        return normalize_country(country);
    }

    match env::var(COUNTRY_ENV) {
        Ok(country) if !country.trim().is_empty() => return normalize_country(&country),
        Ok(_) | Err(env::VarError::NotPresent) => {}
        Err(env::VarError::NotUnicode(_)) => {
            bail!("{COUNTRY_ENV} contains non-Unicode data")
        }
    }

    match env::var(APPLE_LOCALE_ENV) {
        Ok(locale) => {
            if let Some(country) = country_from_apple_locale(&locale) {
                return Ok(country);
            }
        }
        Err(env::VarError::NotPresent) => {}
        Err(env::VarError::NotUnicode(_)) => {
            // AppleLocale is only a hint. An unreadable locale must not make
            // catalog access fail when the documented US fallback is safe.
        }
    }

    if let Some(locale) = defaults_apple_locale() {
        if let Some(country) = country_from_apple_locale(&locale) {
            return Ok(country);
        }
    }

    Ok(DEFAULT_COUNTRY.to_owned())
}

/// Extract a two-letter region from common Apple locale representations.
///
/// Examples include `en_US`, `en-US`, `zh_Hans_CN`, and
/// `en_US.UTF-8@calendar=gregorian`. A language-only locale has no country.
pub fn country_from_apple_locale(locale: &str) -> Option<String> {
    let locale = locale
        .trim()
        .trim_matches(|character| character == '\'' || character == '"');
    if let Some(country) = locale_region_override(locale) {
        return Some(country);
    }
    let base = locale.split(['@', '.']).next().unwrap_or_default().trim();
    let components: Vec<&str> = base
        .split(['_', '-'])
        .filter(|component| !component.is_empty())
        .collect();
    if components.len() < 2 {
        return None;
    }

    components
        .iter()
        .skip(1)
        .rev()
        .find(|component| {
            component.len() == 2 && component.bytes().all(|byte| byte.is_ascii_alphabetic())
        })
        .map(|country| country.to_ascii_uppercase())
}

fn locale_region_override(locale: &str) -> Option<String> {
    let modifiers = locale.split_once('@')?.1;
    for modifier in modifiers.split([';', ',']) {
        let Some((key, value)) = modifier.trim().split_once('=') else {
            continue;
        };
        if !key.trim().eq_ignore_ascii_case("rg") {
            continue;
        }
        let value = value.trim();
        let bytes = value.as_bytes();
        if bytes.len() == 6
            && bytes[..2].iter().all(u8::is_ascii_alphabetic)
            && value[2..].eq_ignore_ascii_case("zzzz")
        {
            return Some(value[..2].to_ascii_uppercase());
        }
    }
    None
}

fn defaults_apple_locale() -> Option<String> {
    let output = Command::new("/usr/bin/defaults")
        .args(["read", "-g", "AppleLocale"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let locale = String::from_utf8(output.stdout).ok()?;
    (!locale.trim().is_empty()).then_some(locale)
}

fn normalize_country(country: &str) -> Result<String> {
    let country = country.trim();
    if country.len() != 2 || !country.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        bail!("country must be a two-letter ISO 3166-1 code");
    }
    Ok(country.to_ascii_uppercase())
}

/// Print a catalog search as one JSON document or a human-readable table.
pub fn print_search(report: &SearchReport, output: OutputFormat) -> Result<()> {
    match output {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(report)?),
        OutputFormat::Table => {
            #[derive(Tabled)]
            struct Row {
                id: u64,
                name: String,
                version: String,
                price: String,
                min_macos: String,
                bundle_id: String,
            }

            let rows = report
                .apps
                .iter()
                .map(|app| Row {
                    id: app.adam_id,
                    name: app.name.clone(),
                    version: app.version.clone(),
                    price: catalog_price(app),
                    min_macos: app
                        .minimum_macos_version
                        .clone()
                        .unwrap_or_else(|| "-".into()),
                    bundle_id: app.bundle_id.clone(),
                })
                .collect::<Vec<_>>();
            print_rows(rows, Some(1), 42);
            println!(
                "{} result(s) for {:?} in the {} storefront",
                report.result_count, report.query, report.country
            );
        }
    }
    Ok(())
}

/// Print installed receipt-bearing applications.
pub fn print_installed(report: &InstalledReport, output: OutputFormat) -> Result<()> {
    match output {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(report)?),
        OutputFormat::Table => {
            print_installed_table(&report.apps);
            println!(
                "{} App Store app(s) across {} scan root(s)",
                report.apps.len(),
                report.scanned_roots.len()
            );
            print_warnings(&report.warnings);
        }
    }
    Ok(())
}

/// Print compatible and incompatible available updates.
pub fn print_updates(report: &UpdateReport, output: OutputFormat) -> Result<()> {
    match output {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(report)?),
        OutputFormat::Table => {
            #[derive(Tabled)]
            struct Row {
                id: u64,
                name: String,
                installed: String,
                available: String,
                compatible: bool,
                bundle_id: String,
            }

            let rows = report
                .updates
                .iter()
                .chain(&report.incompatible_updates)
                .map(|candidate| Row {
                    id: candidate.available.adam_id,
                    name: candidate.installed.name.clone(),
                    installed: candidate.installed.version.clone(),
                    available: candidate.available.version.clone(),
                    compatible: candidate.compatible,
                    bundle_id: candidate.installed.bundle_id.clone(),
                })
                .collect::<Vec<_>>();
            print_rows(rows, Some(1), 42);
            println!(
                "checked {} app(s) on macOS {}; {} compatible update(s), {} incompatible, {} unmatched",
                report.checked_count,
                report.current_macos_version,
                report.updates.len(),
                report.incompatible_updates.len(),
                report.unmatched.len()
            );
            print_warnings(&report.warnings);
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MutationStatus {
    /// Validation passed, but `--apply` was not supplied.
    Planned,
    /// Apple's download service accepted the request.
    Queued,
    /// The request was submitted, but its final state still needs a rescan.
    Pending,
    /// An upgrade-all item was intentionally left unchanged.
    Skipped,
    /// A receipt and the expected installed version were observed.
    Completed,
    /// Install/get was unnecessary because the app is already present.
    AlreadyInstalled,
    /// Update was unnecessary because the installed version is current.
    Current,
    /// The catalog release requires a newer macOS version.
    Incompatible,
    /// `get` was rejected because the item is paid or its price is unknown.
    PriceRequired,
    /// An update was requested for an application not found locally.
    NotInstalled,
    /// The requested application was not found in the storefront.
    NotFound,
    /// Lookup, inventory, or queueing failed.
    Failed,
}

impl MutationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Queued => "queued",
            Self::Pending => "pending",
            Self::Skipped => "skipped",
            Self::Completed => "completed",
            Self::AlreadyInstalled => "already-installed",
            Self::Current => "current",
            Self::Incompatible => "incompatible",
            Self::PriceRequired => "price-required",
            Self::NotInstalled => "not-installed",
            Self::NotFound => "not-found",
            Self::Failed => "failed",
        }
    }

    pub fn is_failure(self) -> bool {
        matches!(
            self,
            Self::Incompatible
                | Self::PriceRequired
                | Self::NotInstalled
                | Self::NotFound
                | Self::Failed
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MutationResult {
    pub adam_id: u64,
    pub name: Option<String>,
    pub bundle_id: Option<String>,
    pub installed_version: Option<String>,
    pub target_version: Option<String>,
    pub status: MutationStatus,
    pub downloads_queued: Option<usize>,
    pub message: String,
}

/// Current machine-readable contract version for install and upgrade reports.
pub const MUTATION_PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize)]
pub struct MutationReport {
    /// Version of the JSON mutation-report contract.
    pub protocol_version: u32,
    pub country: String,
    pub operation: StoreOperation,
    pub apply: bool,
    pub wait: bool,
    pub timeout_millis: u64,
    pub requested_count: usize,
    pub results: Vec<MutationResult>,
    pub warnings: Vec<String>,
    /// Errors that could not be assigned to an item (notably `upgrade all`
    /// preflight failures). Item-specific failures stay in `results`.
    pub errors: Vec<String>,
}

impl MutationReport {
    fn new(
        country: &str,
        operation: StoreOperation,
        apply: bool,
        wait: bool,
        timeout: Duration,
        requested_count: usize,
    ) -> Self {
        Self {
            protocol_version: MUTATION_PROTOCOL_VERSION,
            country: country.to_owned(),
            operation,
            apply,
            wait,
            timeout_millis: timeout.as_millis().min(u64::MAX as u128) as u64,
            requested_count,
            results: Vec::new(),
            warnings: Vec::new(),
            errors: Vec::new(),
        }
    }

    pub fn failed_count(&self) -> usize {
        self.errors.len()
            + self
                .results
                .iter()
                .filter(|result| result.status.is_failure())
                .count()
    }

    pub fn has_failures(&self) -> bool {
        self.failed_count() != 0
    }
}

/// Resolve, validate, and optionally enqueue install/get/update requests.
///
/// This function never aborts a batch because one item fails. Calling it with
/// `apply == false` performs catalog, compatibility, price, and inventory
/// validation without touching App Store state.
pub fn install_apps(
    ids: &[u64],
    country: &str,
    operation: StoreOperation,
    apply: bool,
    wait: bool,
    timeout: Duration,
) -> MutationReport {
    let ids = stable_unique_ids(ids);
    let mut report = MutationReport::new(country, operation, apply, wait, timeout, ids.len());
    let normalized_country = match normalize_country(country) {
        Ok(country) => country,
        Err(error) => {
            fail_requested_ids(&mut report, &ids, format!("invalid storefront: {error:#}"));
            return report;
        }
    };
    report.country = normalized_country.clone();

    if timeout.is_zero() && apply {
        fail_requested_ids(
            &mut report,
            &ids,
            "timeout must be greater than zero when applying changes".into(),
        );
        return report;
    }

    let mut inventory = match app_store::scan_installed(&[]) {
        Ok(inventory) => inventory,
        Err(error) => {
            fail_requested_ids(
                &mut report,
                &ids,
                format!("cannot scan installed App Store apps: {error:#}"),
            );
            return report;
        }
    };
    report.warnings.append(&mut inventory.warnings);

    let current_macos = match app_store::current_macos_version() {
        Ok(version) => version,
        Err(error) => {
            fail_requested_ids(
                &mut report,
                &ids,
                format!("cannot determine the current macOS version: {error:#}"),
            );
            return report;
        }
    };
    let backend = apply.then(app_store_installer::backend_status);
    let lookup_ids = ids
        .iter()
        .copied()
        .filter(|adam_id| *adam_id != 0)
        .collect::<Vec<_>>();
    let catalog_apps = match app_store::lookup_by_adam_ids(&lookup_ids, &normalized_country) {
        Ok(apps) => apps,
        Err(error) => {
            let message = format!("catalog lookup failed: {error:#}");
            for &adam_id in &ids {
                report.results.push(if adam_id == 0 {
                    failure_result(0, "Adam ID must be greater than zero")
                } else {
                    failure_result(adam_id, message.clone())
                });
            }
            return report;
        }
    };

    for &adam_id in &ids {
        if adam_id == 0 {
            report
                .results
                .push(failure_result(0, "Adam ID must be greater than zero"));
            continue;
        }

        let Some(catalog) = catalog_apps
            .iter()
            .find(|app| app.adam_id == adam_id)
            .cloned()
        else {
            report.results.push(result_for_catalog_id(
                adam_id,
                MutationStatus::NotFound,
                "application was not found in this storefront".into(),
            ));
            continue;
        };

        let installed = find_installed(&inventory.apps, &catalog).cloned();
        let result = mutate_catalog_app(
            &catalog,
            installed.as_ref(),
            &current_macos,
            operation,
            MutationExecution {
                apply,
                wait,
                timeout,
                backend: backend.as_ref(),
            },
        );
        if result.status == MutationStatus::Completed {
            if let Ok(refreshed) = app_store::scan_installed(&[]) {
                inventory.apps = refreshed.apps;
                report.warnings.extend(refreshed.warnings);
            }
        }
        report.results.push(result);
    }

    report
}

/// Upgrade selected Adam IDs, or every available update when `selected` is
/// `None`.
pub fn upgrade_apps(
    selected: Option<&[u64]>,
    country: &str,
    apply: bool,
    wait: bool,
    timeout: Duration,
) -> MutationReport {
    let selected_storage = selected.map(stable_unique_ids);
    let selected = selected_storage.as_deref();
    let initial_count = selected.map_or(0, <[u64]>::len);
    let mut report = MutationReport::new(
        country,
        StoreOperation::Update,
        apply,
        wait,
        timeout,
        initial_count,
    );
    let normalized_country = match normalize_country(country) {
        Ok(country) => country,
        Err(error) => {
            record_upgrade_preflight_failure(
                &mut report,
                selected,
                format!("invalid storefront: {error:#}"),
            );
            return report;
        }
    };
    report.country = normalized_country.clone();

    if timeout.is_zero() && apply {
        record_upgrade_preflight_failure(
            &mut report,
            selected,
            "timeout must be greater than zero when applying changes".into(),
        );
        return report;
    }

    let mut inventory = match app_store::scan_installed(&[]) {
        Ok(inventory) => inventory,
        Err(error) => {
            record_upgrade_preflight_failure(
                &mut report,
                selected,
                format!("cannot scan installed App Store apps: {error:#}"),
            );
            return report;
        }
    };
    report.warnings.append(&mut inventory.warnings);

    let updates = match app_store::check_updates(&inventory.apps, &normalized_country) {
        Ok(updates) => updates,
        Err(error) => {
            record_upgrade_preflight_failure(
                &mut report,
                selected,
                format!("cannot check App Store updates: {error:#}"),
            );
            return report;
        }
    };
    report.warnings.extend(updates.warnings.clone());
    let backend = apply.then(app_store_installer::backend_status);

    match selected {
        Some(ids) => {
            for &adam_id in ids {
                let result = selected_upgrade_result(
                    adam_id,
                    &inventory.apps,
                    &updates,
                    apply,
                    wait,
                    timeout,
                    backend.as_ref(),
                );
                report.results.push(result);
            }
        }
        None => {
            for candidate in &updates.updates {
                report.results.push(queue_update_candidate(
                    candidate,
                    apply,
                    wait,
                    timeout,
                    backend.as_ref(),
                ));
            }
            for candidate in &updates.incompatible_updates {
                report.results.push(skipped_incompatible_result(candidate));
            }
            for installed in &updates.unmatched {
                report.results.push(skipped_unmatched_result(installed));
            }
            report.requested_count = report.results.len();
        }
    }

    report
}

fn selected_upgrade_result(
    adam_id: u64,
    installed: &[InstalledApp],
    updates: &UpdateReport,
    apply: bool,
    wait: bool,
    timeout: Duration,
    backend: Option<&InstallerBackendStatus>,
) -> MutationResult {
    if adam_id == 0 {
        return failure_result(0, "Adam ID must be greater than zero");
    }
    if let Some(candidate) = updates
        .updates
        .iter()
        .find(|candidate| candidate_matches_id(candidate, adam_id))
    {
        return queue_update_candidate(candidate, apply, wait, timeout, backend);
    }
    if let Some(candidate) = updates
        .incompatible_updates
        .iter()
        .find(|candidate| candidate_matches_id(candidate, adam_id))
    {
        return incompatible_result(candidate);
    }

    let Some(local) = installed.iter().find(|app| app.adam_id == Some(adam_id)) else {
        return MutationResult {
            adam_id,
            name: None,
            bundle_id: None,
            installed_version: None,
            target_version: None,
            status: MutationStatus::NotInstalled,
            downloads_queued: None,
            message: "application is not installed with an App Store receipt".into(),
        };
    };
    if updates
        .unmatched
        .iter()
        .any(|unmatched| same_installed_app(unmatched, local))
    {
        result_for_installed(
            local,
            MutationStatus::NotFound,
            "no matching catalog application was found".into(),
        )
    } else {
        result_for_installed(
            local,
            MutationStatus::Current,
            "installed version is current".into(),
        )
    }
}

#[derive(Clone, Copy)]
struct MutationExecution<'a> {
    apply: bool,
    wait: bool,
    timeout: Duration,
    backend: Option<&'a InstallerBackendStatus>,
}

fn mutate_catalog_app(
    catalog: &CatalogApp,
    installed: Option<&InstalledApp>,
    current_macos: &str,
    operation: StoreOperation,
    execution: MutationExecution<'_>,
) -> MutationResult {
    if matches!(operation, StoreOperation::Install | StoreOperation::Get) && installed.is_some() {
        return result_for_catalog(
            catalog,
            installed,
            MutationStatus::AlreadyInstalled,
            "application is already installed with an App Store receipt".into(),
        );
    }

    if !app_store::is_macos_compatible(current_macos, catalog.minimum_macos_version.as_deref()) {
        return result_for_catalog(
            catalog,
            installed,
            MutationStatus::Incompatible,
            format!(
                "requires macOS {} (current {})",
                catalog
                    .minimum_macos_version
                    .as_deref()
                    .unwrap_or("unknown"),
                current_macos
            ),
        );
    }

    if operation == StoreOperation::Get {
        match catalog.price {
            Some(price) if price <= 0.0 => {}
            Some(price) => {
                return result_for_catalog(
                    catalog,
                    installed,
                    MutationStatus::PriceRequired,
                    format!(
                        "get is restricted to free apps; catalog price is {}",
                        catalog
                            .formatted_price
                            .clone()
                            .unwrap_or_else(|| price.to_string())
                    ),
                )
            }
            None => {
                return result_for_catalog(
                    catalog,
                    installed,
                    MutationStatus::PriceRequired,
                    "get is restricted to apps with an explicit zero catalog price".into(),
                )
            }
        }
    }

    match operation {
        StoreOperation::Update if installed.is_none() => {
            return result_for_catalog(
                catalog,
                None,
                MutationStatus::NotInstalled,
                "cannot update an application that is not installed".into(),
            )
        }
        StoreOperation::Update => {
            let local = installed.expect("update presence checked above");
            if app_store::compare_versions(&catalog.version, &local.version)
                != std::cmp::Ordering::Greater
            {
                return result_for_catalog(
                    catalog,
                    installed,
                    MutationStatus::Current,
                    "installed version is current".into(),
                );
            }
        }
        StoreOperation::Install | StoreOperation::Get => {}
    }

    if !execution.apply {
        return result_for_catalog(
            catalog,
            installed,
            MutationStatus::Planned,
            format!("would enqueue {}", operation.as_str()),
        );
    }
    let Some(backend) = execution.backend else {
        return result_for_catalog(
            catalog,
            installed,
            MutationStatus::Failed,
            "installer backend status was not checked".into(),
        );
    };
    if !backend.available {
        return result_for_catalog(
            catalog,
            installed,
            MutationStatus::Failed,
            format!("installer backend unavailable: {}", backend.detail),
        );
    }

    let queued = match app_store_installer::queue(
        catalog.adam_id,
        operation,
        execution.timeout.min(MAX_QUEUE_REQUEST_TIMEOUT),
    ) {
        Ok(queued) => queued,
        Err(error) => {
            return result_for_catalog(
                catalog,
                installed,
                MutationStatus::Failed,
                format!("App Store did not accept the request: {error:#}"),
            )
        }
    };
    let (submission_status, submission_detail, confirmed_downloads) = match &queued.status {
        QueueStatus::Queued => (
            MutationStatus::Queued,
            format!("{} download(s) queued", queued.downloads_queued),
            Some(queued.downloads_queued),
        ),
        QueueStatus::Pending { detail } => (MutationStatus::Pending, detail.clone(), None),
    };
    if !execution.wait {
        let mut result =
            result_for_catalog(catalog, installed, submission_status, submission_detail);
        result.downloads_queued = confirmed_downloads;
        return result;
    }

    match wait_for_receipt_and_version(catalog, installed, operation, execution.timeout) {
        Ok(observed) => {
            let mut result = result_for_catalog(
                catalog,
                Some(&observed),
                MutationStatus::Completed,
                format!("verified installed version {}", observed.version),
            );
            result.downloads_queued = confirmed_downloads;
            result
        }
        Err(VerificationError::TimedOut(message)) => {
            let detail = if submission_status == MutationStatus::Pending {
                format!("{submission_detail}; {message}")
            } else {
                message
            };
            let mut result =
                result_for_catalog(catalog, installed, MutationStatus::Pending, detail);
            result.downloads_queued = confirmed_downloads;
            result
        }
        Err(VerificationError::Failed(message)) => {
            let detail = format!(
                "request was submitted; final state is pending because verification failed: {message}"
            );
            let mut result =
                result_for_catalog(catalog, installed, MutationStatus::Pending, detail);
            result.downloads_queued = confirmed_downloads;
            result
        }
    }
}

fn queue_update_candidate(
    candidate: &UpdateCandidate,
    apply: bool,
    wait: bool,
    timeout: Duration,
    backend: Option<&InstallerBackendStatus>,
) -> MutationResult {
    // check_updates already performed the same compatibility calculation.
    if !candidate.compatible {
        return incompatible_result(candidate);
    }
    mutate_catalog_app(
        &candidate.available,
        Some(&candidate.installed),
        // A compatible candidate should remain compatible here. Passing its
        // own minimum avoids another sw_vers subprocess and preserves that
        // checked decision.
        candidate
            .available
            .minimum_macos_version
            .as_deref()
            .unwrap_or("0"),
        StoreOperation::Update,
        MutationExecution {
            apply,
            wait,
            timeout,
            backend,
        },
    )
}

fn incompatible_result(candidate: &UpdateCandidate) -> MutationResult {
    result_for_catalog(
        &candidate.available,
        Some(&candidate.installed),
        MutationStatus::Incompatible,
        format!(
            "update requires macOS {}",
            candidate
                .available
                .minimum_macos_version
                .as_deref()
                .unwrap_or("a newer release")
        ),
    )
}

fn skipped_incompatible_result(candidate: &UpdateCandidate) -> MutationResult {
    result_for_catalog(
        &candidate.available,
        Some(&candidate.installed),
        MutationStatus::Skipped,
        format!(
            "skipped because the update requires macOS {}",
            candidate
                .available
                .minimum_macos_version
                .as_deref()
                .unwrap_or("a newer release")
        ),
    )
}

fn skipped_unmatched_result(installed: &InstalledApp) -> MutationResult {
    result_for_installed(
        installed,
        MutationStatus::Skipped,
        "skipped because no matching catalog application was found".into(),
    )
}

enum VerificationError {
    TimedOut(String),
    Failed(String),
}

fn wait_for_receipt_and_version(
    catalog: &CatalogApp,
    previous: Option<&InstalledApp>,
    operation: StoreOperation,
    timeout: Duration,
) -> std::result::Result<InstalledApp, VerificationError> {
    let started = Instant::now();
    loop {
        let inventory = app_store::scan_installed(&[]).map_err(|error| {
            VerificationError::Failed(format!(
                "queued request, but receipt verification failed: {error:#}"
            ))
        })?;
        if let Some(observed) = find_installed(&inventory.apps, catalog) {
            let version_verified = match operation {
                StoreOperation::Install | StoreOperation::Get => {
                    !observed.version.trim().is_empty()
                }
                StoreOperation::Update => {
                    app_store::compare_versions(&observed.version, &catalog.version)
                        != std::cmp::Ordering::Less
                        || previous.is_some_and(|previous| {
                            app_store::compare_versions(&observed.version, &previous.version)
                                == std::cmp::Ordering::Greater
                        })
                }
            };
            if version_verified {
                return Ok(observed.clone());
            }
        }

        let elapsed = started.elapsed();
        if elapsed >= timeout {
            return Err(VerificationError::TimedOut(format!(
                "request was queued, but receipt/version {} was not observed within {} ms",
                catalog.version,
                timeout.as_millis()
            )));
        }
        thread::sleep(POLL_INTERVAL.min(timeout - elapsed));
    }
}

fn find_installed<'a>(
    installed: &'a [InstalledApp],
    catalog: &CatalogApp,
) -> Option<&'a InstalledApp> {
    installed.iter().find(|local| {
        local.adam_id == Some(catalog.adam_id)
            || (!catalog.bundle_id.is_empty() && local.bundle_id == catalog.bundle_id)
    })
}

fn candidate_matches_id(candidate: &UpdateCandidate, adam_id: u64) -> bool {
    candidate.available.adam_id == adam_id || candidate.installed.adam_id == Some(adam_id)
}

fn same_installed_app(left: &InstalledApp, right: &InstalledApp) -> bool {
    left.path == right.path || left.bundle_id == right.bundle_id
}

fn result_for_catalog(
    catalog: &CatalogApp,
    installed: Option<&InstalledApp>,
    status: MutationStatus,
    message: String,
) -> MutationResult {
    MutationResult {
        adam_id: catalog.adam_id,
        name: nonempty(&catalog.name),
        bundle_id: nonempty(&catalog.bundle_id),
        installed_version: installed.map(|app| app.version.clone()),
        target_version: nonempty(&catalog.version),
        status,
        downloads_queued: None,
        message,
    }
}

fn result_for_catalog_id(adam_id: u64, status: MutationStatus, message: String) -> MutationResult {
    MutationResult {
        adam_id,
        name: None,
        bundle_id: None,
        installed_version: None,
        target_version: None,
        status,
        downloads_queued: None,
        message,
    }
}

fn result_for_installed(
    installed: &InstalledApp,
    status: MutationStatus,
    message: String,
) -> MutationResult {
    MutationResult {
        adam_id: installed.adam_id.unwrap_or(0),
        name: nonempty(&installed.name),
        bundle_id: nonempty(&installed.bundle_id),
        installed_version: nonempty(&installed.version),
        target_version: None,
        status,
        downloads_queued: None,
        message,
    }
}

fn failure_result(adam_id: u64, message: impl Into<String>) -> MutationResult {
    result_for_catalog_id(adam_id, MutationStatus::Failed, message.into())
}

fn fail_requested_ids(report: &mut MutationReport, ids: &[u64], message: String) {
    if ids.is_empty() {
        report.errors.push(message);
    } else {
        report.results.extend(
            ids.iter()
                .map(|&adam_id| failure_result(adam_id, message.clone())),
        );
    }
}

fn record_upgrade_preflight_failure(
    report: &mut MutationReport,
    selected: Option<&[u64]>,
    message: String,
) {
    match selected {
        Some(ids) if !ids.is_empty() => fail_requested_ids(report, ids, message),
        Some(_) | None => report.errors.push(message),
    }
}

fn nonempty(value: &str) -> Option<String> {
    (!value.trim().is_empty()).then(|| value.to_owned())
}

fn stable_unique_ids(ids: &[u64]) -> Vec<u64> {
    let mut seen = HashSet::with_capacity(ids.len());
    ids.iter()
        .copied()
        .filter(|adam_id| seen.insert(*adam_id))
        .collect()
}

fn catalog_price(app: &CatalogApp) -> String {
    app.formatted_price
        .clone()
        .or_else(|| app.price.map(|price| price.to_string()))
        .unwrap_or_else(|| "-".into())
}

/// Print a mutation batch as exactly one JSON document or one table/report.
pub fn print_mutation(report: &MutationReport, output: OutputFormat) -> Result<()> {
    match output {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(report)?),
        OutputFormat::Table => {
            #[derive(Tabled)]
            struct Row {
                id: u64,
                name: String,
                installed: String,
                target: String,
                status: String,
                message: String,
            }

            let rows = report
                .results
                .iter()
                .map(|result| Row {
                    id: result.adam_id,
                    name: result.name.clone().unwrap_or_else(|| "-".into()),
                    installed: result
                        .installed_version
                        .clone()
                        .unwrap_or_else(|| "-".into()),
                    target: result.target_version.clone().unwrap_or_else(|| "-".into()),
                    status: result.status.as_str().into(),
                    message: result.message.clone(),
                })
                .collect::<Vec<_>>();
            print_rows(rows, Some(5), 60);
            println!(
                "operation={} storefront={} apply={} wait={}; {} requested, {} failed",
                report.operation.as_str(),
                report.country,
                report.apply,
                report.wait,
                report.requested_count,
                report.failed_count()
            );
            print_warnings(&report.warnings);
            if !report.errors.is_empty() {
                println!("Errors:");
                for error in &report.errors {
                    println!("  - {error}");
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub platform: String,
    pub backend: InstallerBackendStatus,
    pub inventory: Option<InstalledReport>,
    pub errors: Vec<String>,
}

impl DoctorReport {
    pub fn is_healthy(&self) -> bool {
        self.backend.available && self.inventory.is_some() && self.errors.is_empty()
    }
}

/// Inspect the native backend and local App Store receipt inventory without
/// changing App Store state.
pub fn doctor(extra_roots: &[PathBuf]) -> DoctorReport {
    let backend = app_store_installer::backend_status();
    let (inventory, errors) = match app_store::scan_installed(extra_roots) {
        Ok(inventory) => (Some(inventory), Vec::new()),
        Err(error) => (
            None,
            vec![format!("cannot scan installed App Store apps: {error:#}")],
        ),
    };
    DoctorReport {
        platform: env::consts::OS.to_owned(),
        backend,
        inventory,
        errors,
    }
}

pub fn print_doctor(report: &DoctorReport, output: OutputFormat) -> Result<()> {
    match output {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(report)?),
        OutputFormat::Table => {
            #[derive(Tabled)]
            struct StatusRow {
                platform: String,
                backend: String,
                available: bool,
                installed: usize,
                detail: String,
            }
            let row = StatusRow {
                platform: report.platform.clone(),
                backend: report.backend.backend.into(),
                available: report.backend.available,
                installed: report
                    .inventory
                    .as_ref()
                    .map_or(0, |inventory| inventory.apps.len()),
                detail: report.backend.detail.clone(),
            };
            print_rows(vec![row], Some(4), 72);
            if let Some(inventory) = &report.inventory {
                print_installed_table(&inventory.apps);
                print_warnings(&inventory.warnings);
            }
            if !report.errors.is_empty() {
                println!("Errors:");
                for error in &report.errors {
                    println!("  - {error}");
                }
            }
        }
    }
    Ok(())
}

fn print_installed_table(apps: &[InstalledApp]) {
    #[derive(Tabled)]
    struct Row {
        id: String,
        name: String,
        version: String,
        bundle_id: String,
        path: String,
    }
    let rows = apps
        .iter()
        .map(|app| Row {
            id: app
                .adam_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "-".into()),
            name: app.name.clone(),
            version: app.version.clone(),
            bundle_id: app.bundle_id.clone(),
            path: app.path.display().to_string(),
        })
        .collect::<Vec<_>>();
    print_rows(rows, Some(4), 64);
}

fn print_rows<T: Tabled>(rows: Vec<T>, wrap_column: Option<usize>, width: usize) {
    if rows.is_empty() {
        println!("No matching applications.");
        return;
    }
    let mut table = Table::new(rows);
    table.with(Style::rounded());
    if let Some(column) = wrap_column {
        table.with(Modify::new(Columns::single(column)).with(Width::wrap(width).keep_words(true)));
    }
    println!("{table}");
}

fn print_warnings(warnings: &[String]) {
    if warnings.is_empty() {
        return;
    }
    println!("Warnings:");
    for warning in warnings {
        println!("  - {warning}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog_for_idempotency(price: Option<f64>, minimum_macos: Option<&str>) -> CatalogApp {
        CatalogApp {
            adam_id: 123_456,
            bundle_id: "example.already-installed".into(),
            name: "Already Installed".into(),
            version: "2.0".into(),
            minimum_macos_version: minimum_macos.map(str::to_owned),
            seller_name: None,
            description: None,
            release_notes: None,
            release_date: None,
            store_url: None,
            artwork_url: None,
            price,
            formatted_price: price.map(|value| format!("${value:.2}")),
            currency: Some("USD".into()),
            genres: Vec::new(),
            file_size_bytes: None,
        }
    }

    fn installed_for_idempotency() -> InstalledApp {
        InstalledApp {
            path: PathBuf::from("/Applications/Already Installed.app"),
            receipt_path: PathBuf::from(
                "/Applications/Already Installed.app/Contents/_MASReceipt/receipt",
            ),
            name: "Already Installed".into(),
            bundle_id: "example.already-installed".into(),
            version: "1.0".into(),
            build_version: Some("1".into()),
            adam_id: Some(123_456),
        }
    }

    fn applying_without_backend() -> MutationExecution<'static> {
        MutationExecution {
            apply: true,
            wait: true,
            timeout: Duration::from_secs(30),
            backend: None,
        }
    }

    #[test]
    fn apple_locale_country_parsing_handles_common_variants() {
        assert_eq!(country_from_apple_locale("en_US"), Some("US".into()));
        assert_eq!(country_from_apple_locale("ru-RU.UTF-8"), Some("RU".into()));
        assert_eq!(
            country_from_apple_locale("zh_Hans_CN@calendar=gregorian"),
            Some("CN".into())
        );
        assert_eq!(country_from_apple_locale("'pt_BR'"), Some("BR".into()));
        assert_eq!(
            country_from_apple_locale("en_US@rg=ruzzzz"),
            Some("RU".into())
        );
        assert_eq!(
            country_from_apple_locale("en_US@calendar=gregorian;rg=gbZZZZ"),
            Some("GB".into())
        );
    }

    #[test]
    fn apple_locale_country_parsing_rejects_language_only_or_malformed_values() {
        assert_eq!(country_from_apple_locale("en"), None);
        assert_eq!(country_from_apple_locale("C"), None);
        assert_eq!(country_from_apple_locale("en_419"), None);
        assert_eq!(country_from_apple_locale(""), None);
    }

    #[test]
    fn mutation_status_labels_and_failure_classification_are_stable() {
        assert_eq!(
            MutationStatus::AlreadyInstalled.as_str(),
            "already-installed"
        );
        assert_eq!(MutationStatus::Pending.as_str(), "pending");
        assert_eq!(MutationStatus::Skipped.as_str(), "skipped");
        assert!(!MutationStatus::Planned.is_failure());
        assert!(!MutationStatus::Queued.is_failure());
        assert!(!MutationStatus::Pending.is_failure());
        assert!(!MutationStatus::Skipped.is_failure());
        assert!(!MutationStatus::Completed.is_failure());
        assert!(!MutationStatus::AlreadyInstalled.is_failure());
        assert!(!MutationStatus::Current.is_failure());
        assert!(MutationStatus::Incompatible.is_failure());
        assert!(MutationStatus::PriceRequired.is_failure());
        assert!(MutationStatus::NotInstalled.is_failure());
        assert!(MutationStatus::NotFound.is_failure());
        assert!(MutationStatus::Failed.is_failure());
    }

    #[test]
    fn mutation_reports_serialize_stable_protocol_version() {
        for (operation, operation_name) in [
            (StoreOperation::Install, "install"),
            (StoreOperation::Update, "update"),
        ] {
            let report =
                MutationReport::new("US", operation, false, true, Duration::from_secs(30), 2);
            let encoded = serde_json::to_string(&report).expect("serialize mutation report");
            assert!(
                encoded.starts_with(r#"{"protocol_version":1,"country":"US","operation":"#),
                "protocol_version must be the first serialized mutation-report field: {encoded}"
            );

            let value: serde_json::Value =
                serde_json::from_str(&encoded).expect("parse mutation report JSON");
            assert_eq!(
                value["protocol_version"],
                serde_json::json!(MUTATION_PROTOCOL_VERSION)
            );
            assert_eq!(value["operation"], serde_json::json!(operation_name));
            assert_eq!(value["apply"], serde_json::json!(false));
            assert_eq!(value["requested_count"], serde_json::json!(2));
            assert!(value["results"].as_array().is_some_and(Vec::is_empty));
        }
    }

    #[test]
    fn get_of_installed_paid_app_is_idempotent_before_price_check() {
        let catalog = catalog_for_idempotency(Some(19.99), Some("1.0"));
        let installed = installed_for_idempotency();

        let result = mutate_catalog_app(
            &catalog,
            Some(&installed),
            "26.0",
            StoreOperation::Get,
            applying_without_backend(),
        );

        assert_eq!(result.status, MutationStatus::AlreadyInstalled);
        assert_eq!(result.installed_version.as_deref(), Some("1.0"));
        assert!(result.message.contains("already installed"));
    }

    #[test]
    fn install_of_installed_app_is_idempotent_before_compatibility_check() {
        let catalog = catalog_for_idempotency(Some(0.0), Some("99.0"));
        let installed = installed_for_idempotency();

        let result = mutate_catalog_app(
            &catalog,
            Some(&installed),
            "26.0",
            StoreOperation::Install,
            applying_without_backend(),
        );

        assert_eq!(result.status, MutationStatus::AlreadyInstalled);
        assert_ne!(result.status, MutationStatus::Incompatible);
        assert!(result.message.contains("already installed"));
    }

    #[test]
    fn upgrade_all_unmatched_apps_are_skipped_without_failing_the_batch() {
        let installed = InstalledApp {
            path: PathBuf::from("/Applications/Unavailable.app"),
            receipt_path: PathBuf::from(
                "/Applications/Unavailable.app/Contents/_MASReceipt/receipt",
            ),
            name: "Unavailable".into(),
            bundle_id: "example.unavailable".into(),
            version: "1.0".into(),
            build_version: Some("1".into()),
            adam_id: None,
        };
        let result = skipped_unmatched_result(&installed);
        let mut report = MutationReport::new(
            "US",
            StoreOperation::Update,
            true,
            false,
            Duration::from_secs(30),
            1,
        );
        report.results.push(result);

        assert_eq!(report.results[0].status, MutationStatus::Skipped);
        assert!(!report.has_failures());
    }
}
