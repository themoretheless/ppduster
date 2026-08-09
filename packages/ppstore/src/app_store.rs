//! Read-only Mac App Store catalog and installed-app inspection.
//!
//! This module deliberately does not install, update, or remove applications.
//! Network requests are made by spawning Apple's system `curl` directly, never
//! through a shell. Installed applications are identified by their App Store
//! receipt and inspected with the macOS metadata tools.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::path::Component;
use std::path::{Path, PathBuf};
use std::process::Command;
use walkdir::WalkDir;

const SEARCH_ENDPOINT: &str = "https://itunes.apple.com/search";
const LOOKUP_ENDPOINT: &str = "https://itunes.apple.com/lookup";
const MAX_RESULTS: usize = 200;
// Keep lookup URLs comfortably below common proxy limits while reducing the
// number of calls against Apple's approximately 20 requests/minute limit.
const LOOKUP_BATCH_SIZE: usize = 50;
const CURL_RESPONSE_LIMIT: &str = "16777216";

/// An application returned by Apple's public Search or Lookup API.
///
/// The aliases let this type deserialize the API's camelCase response while
/// keeping ppstore's serialized reports in snake_case.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogApp {
    #[serde(alias = "trackId")]
    pub adam_id: u64,
    #[serde(default, alias = "bundleId")]
    pub bundle_id: String,
    #[serde(default, alias = "trackName")]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default, alias = "minimumOsVersion")]
    pub minimum_macos_version: Option<String>,
    #[serde(default, alias = "sellerName")]
    pub seller_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, alias = "releaseNotes")]
    pub release_notes: Option<String>,
    #[serde(default, alias = "currentVersionReleaseDate")]
    pub release_date: Option<String>,
    #[serde(default, alias = "trackViewUrl")]
    pub store_url: Option<String>,
    #[serde(default, alias = "artworkUrl512")]
    pub artwork_url: Option<String>,
    #[serde(default)]
    pub price: Option<f64>,
    #[serde(default, alias = "formattedPrice")]
    pub formatted_price: Option<String>,
    #[serde(default)]
    pub currency: Option<String>,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default, alias = "fileSizeBytes")]
    pub file_size_bytes: Option<String>,
}

/// A locally installed application carrying a Mac App Store receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledApp {
    pub path: PathBuf,
    pub receipt_path: PathBuf,
    pub name: String,
    pub bundle_id: String,
    /// User-visible version, falling back to the build version when needed.
    pub version: String,
    /// `CFBundleVersion`, when present.
    pub build_version: Option<String>,
    /// Spotlight's `kMDItemAppStoreAdamID`, when available.
    pub adam_id: Option<u64>,
}

/// A newer catalog version paired with the corresponding installed app.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdateCandidate {
    pub installed: InstalledApp,
    pub available: CatalogApp,
    pub compatible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum LookupIdentifier {
    AdamId(u64),
    BundleId(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchReport {
    pub query: String,
    pub country: String,
    pub limit: usize,
    pub result_count: usize,
    pub apps: Vec<CatalogApp>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LookupReport {
    pub identifier: LookupIdentifier,
    pub country: String,
    pub result_count: usize,
    pub apps: Vec<CatalogApp>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledReport {
    pub scanned_roots: Vec<PathBuf>,
    pub apps: Vec<InstalledApp>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdateReport {
    pub current_macos_version: String,
    pub checked_count: usize,
    pub updates: Vec<UpdateCandidate>,
    pub incompatible_updates: Vec<UpdateCandidate>,
    pub unmatched: Vec<InstalledApp>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct CatalogEnvelope {
    #[serde(default, alias = "resultCount")]
    _result_count: usize,
    #[serde(default)]
    results: Vec<CatalogApp>,
}

/// Reject an operation that requires Apple-provided macOS command-line tools.
pub fn ensure_macos() -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("Mac App Store operations are supported only on macOS");
    }
    Ok(())
}

/// Parse the common JSON envelope returned by Apple's Search and Lookup APIs.
///
/// This is intentionally pure so callers can test fixtures without making a
/// network request.
pub fn parse_catalog_response(input: impl AsRef<[u8]>) -> Result<Vec<CatalogApp>> {
    let envelope: CatalogEnvelope =
        serde_json::from_slice(input.as_ref()).context("parse Apple catalog JSON response")?;
    Ok(envelope.results)
}

/// Search the Mac App Store catalog.
pub fn search(query: &str, country: &str, limit: usize) -> Result<SearchReport> {
    ensure_macos()?;
    let query = query.trim();
    if query.is_empty() {
        bail!("search query must not be empty");
    }
    if query.chars().count() > 512 {
        bail!("search query must not exceed 512 characters");
    }
    if !(1..=MAX_RESULTS).contains(&limit) {
        bail!("search limit must be between 1 and {MAX_RESULTS}");
    }
    let country = normalize_country(country)?;
    let limit_string = limit.to_string();
    let body = curl_get(
        SEARCH_ENDPOINT,
        &[
            ("term", query),
            ("country", country.as_str()),
            ("media", "software"),
            ("entity", "desktopSoftware"),
            ("limit", limit_string.as_str()),
        ],
    )?;
    let apps = parse_catalog_response(&body)?;

    Ok(SearchReport {
        query: query.to_owned(),
        country,
        limit,
        result_count: apps.len(),
        apps,
    })
}

/// Look up a Mac App Store app by its numeric Adam ID or bundle identifier.
pub fn lookup(identifier: LookupIdentifier, country: &str) -> Result<LookupReport> {
    ensure_macos()?;
    let country = normalize_country(country)?;
    let (key, value) = match &identifier {
        LookupIdentifier::AdamId(id) => {
            if *id == 0 {
                bail!("Adam ID must be greater than zero");
            }
            ("id", id.to_string())
        }
        LookupIdentifier::BundleId(bundle_id) => {
            let bundle_id = bundle_id.trim();
            if bundle_id.is_empty() {
                bail!("bundle identifier must not be empty");
            }
            if bundle_id.chars().count() > 255 {
                bail!("bundle identifier must not exceed 255 characters");
            }
            ("bundleId", bundle_id.to_owned())
        }
    };
    let body = curl_get(
        LOOKUP_ENDPOINT,
        &[
            (key, value.as_str()),
            ("country", country.as_str()),
            ("media", "software"),
            ("entity", "desktopSoftware"),
        ],
    )?;
    let apps = parse_catalog_response(&body)?;

    Ok(LookupReport {
        identifier,
        country,
        result_count: apps.len(),
        apps,
    })
}

pub fn lookup_by_adam_id(adam_id: u64, country: &str) -> Result<LookupReport> {
    lookup(LookupIdentifier::AdamId(adam_id), country)
}

/// Look up multiple numeric Adam IDs in bounded batches.
///
/// The returned catalog records can be matched by `adam_id`; Apple may omit
/// unknown or storefront-unavailable IDs and does not guarantee input order.
pub fn lookup_by_adam_ids(adam_ids: &[u64], country: &str) -> Result<Vec<CatalogApp>> {
    ensure_macos()?;
    let country = normalize_country(country)?;
    let mut seen = HashSet::with_capacity(adam_ids.len());
    let ids = adam_ids
        .iter()
        .copied()
        .filter(|adam_id| seen.insert(*adam_id))
        .collect::<Vec<_>>();
    if ids.contains(&0) {
        bail!("Adam ID must be greater than zero");
    }

    let mut apps = Vec::new();
    for ids in ids.chunks(LOOKUP_BATCH_SIZE) {
        let value = ids.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
        let body = curl_get(
            LOOKUP_ENDPOINT,
            &[
                ("id", value.as_str()),
                ("country", country.as_str()),
                ("media", "software"),
                ("entity", "desktopSoftware"),
            ],
        )?;
        apps.extend(parse_catalog_response(&body)?);
    }
    Ok(apps)
}

pub fn lookup_by_bundle_id(bundle_id: &str, country: &str) -> Result<LookupReport> {
    lookup(
        LookupIdentifier::BundleId(bundle_id.trim().to_owned()),
        country,
    )
}

/// The standard roots checked by [`scan_installed`].
pub fn default_scan_roots() -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from("/Applications")];
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join("Applications"));
        if cfg!(target_os = "macos") {
            if let Some(external_root) = preferred_volume_applications_root(&home) {
                roots.push(external_root);
            }
        }
    }
    roots
}

/// Whether a preferred App Store volume name is exactly one safe path
/// component. Dots inside ordinary names are allowed; `.` and `..` components
/// and absolute or multi-component paths are not.
pub fn is_valid_volume_component(name: &str) -> bool {
    if name.is_empty() || name.contains('/') || name.contains('\0') {
        return false;
    }

    let path = Path::new(name);
    if path.is_absolute() {
        return false;
    }
    let mut components = path.components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

/// Scan the standard application roots plus any extra roots supplied by the
/// caller. Missing standard roots are harmless and are simply skipped.
pub fn scan_installed(extra_roots: &[PathBuf]) -> Result<InstalledReport> {
    let mut roots = default_scan_roots();
    roots.extend(extra_roots.iter().cloned());
    scan_installed_in_roots(&roots)
}

/// Scan exactly the supplied roots for application bundles with MAS receipts.
pub fn scan_installed_in_roots(roots: &[PathBuf]) -> Result<InstalledReport> {
    ensure_macos()?;
    let mut unique_roots = Vec::new();
    let mut seen_roots = HashSet::new();
    for root in roots {
        if seen_roots.insert(root.clone()) {
            unique_roots.push(root.clone());
        }
    }

    let mut apps = Vec::new();
    let mut warnings = Vec::new();
    let mut seen_apps = HashSet::new();

    for root in &unique_roots {
        if !root.exists() {
            continue;
        }
        if !root.is_dir() {
            warnings.push(format!("scan root is not a directory: {}", root.display()));
            continue;
        }

        let mut entries = WalkDir::new(root)
            .follow_links(false)
            .min_depth(1)
            .into_iter();
        while let Some(entry) = entries.next() {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    warnings.push(format!("cannot inspect {}: {error}", root.display()));
                    continue;
                }
            };
            if !entry.file_type().is_dir() || !is_app_bundle(entry.path()) {
                continue;
            }

            // Application bundles can contain helper .app bundles. They are not
            // independently installed MAS applications, so do not descend.
            entries.skip_current_dir();
            let app_path = entry.into_path();
            let receipt_path = app_path.join("Contents/_MASReceipt/receipt");
            if !is_nonempty_file(&receipt_path) || !seen_apps.insert(app_path.clone()) {
                continue;
            }

            match inspect_installed_app(&app_path) {
                Ok(app) => apps.push(app),
                Err(error) => warnings.push(format!("{}: {error:#}", app_path.display())),
            }
        }
    }

    apps.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.path.cmp(&right.path))
    });

    Ok(InstalledReport {
        scanned_roots: unique_roots,
        apps,
        warnings,
    })
}

/// Parse the JSON representation of an application's `Info.plist`.
///
/// `app_path` and `adam_id` are supplied separately because they come from the
/// filesystem and Spotlight, not from the plist itself.
pub fn parse_installed_app_plist(
    input: impl AsRef<[u8]>,
    app_path: &Path,
    adam_id: Option<u64>,
) -> Result<InstalledApp> {
    let value: Value = serde_json::from_slice(input.as_ref()).context("parse Info.plist JSON")?;
    let object = value
        .as_object()
        .context("Info.plist JSON root must be an object")?;

    let bundle_id = plist_string(object.get("CFBundleIdentifier"))
        .filter(|value| !value.trim().is_empty())
        .context("Info.plist has no CFBundleIdentifier")?;
    let build_version =
        plist_string(object.get("CFBundleVersion")).filter(|value| !value.trim().is_empty());
    let short_version = plist_string(object.get("CFBundleShortVersionString"))
        .filter(|value| !value.trim().is_empty());
    let version = short_version
        .or_else(|| build_version.clone())
        .context("Info.plist has no application version")?;
    let fallback_name = app_path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or(&bundle_id)
        .to_owned();
    let name = plist_string(object.get("CFBundleDisplayName"))
        .or_else(|| plist_string(object.get("CFBundleName")))
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(fallback_name);

    Ok(InstalledApp {
        path: app_path.to_path_buf(),
        receipt_path: app_path.join("Contents/_MASReceipt/receipt"),
        name,
        bundle_id,
        version,
        build_version,
        adam_id,
    })
}

/// Parse the raw output from `mdls -raw -name kMDItemAppStoreAdamID`.
pub fn parse_mdls_adam_id(input: &str) -> Option<u64> {
    let value = input.trim();
    if value.is_empty() || value == "(null)" || value.eq_ignore_ascii_case("null") {
        return None;
    }
    value
        .trim_matches('"')
        .trim()
        .parse()
        .ok()
        .filter(|id| *id > 0)
}

/// Natural, numeric-aware comparison for application and macOS versions.
///
/// Numeric runs are compared without integer conversion, so unusually long
/// version components cannot overflow. Separators are ignored and trailing
/// zero components are insignificant (`1.0.0 == 1`).
pub fn compare_versions(left: &str, right: &str) -> Ordering {
    let mut left_tokens = version_tokens(left);
    let mut right_tokens = version_tokens(right);
    trim_trailing_zeroes(&mut left_tokens);
    trim_trailing_zeroes(&mut right_tokens);

    for (left, right) in left_tokens.iter().zip(&right_tokens) {
        let ordering = compare_version_tokens(left, right);
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left_tokens.len().cmp(&right_tokens.len())
}

/// Whether `current_version` satisfies a catalog minimum macOS version.
pub fn is_macos_compatible(current_version: &str, minimum_version: Option<&str>) -> bool {
    match minimum_version
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        None => true,
        Some(_) if current_version.trim().is_empty() => false,
        Some(minimum) => compare_versions(current_version, minimum) != Ordering::Less,
    }
}

/// Produce an update report from local and catalog data without I/O.
///
/// Matching prefers Adam IDs and falls back to bundle identifiers. A catalog
/// app is considered an update only when its version is naturally greater than
/// the installed version. Updates requiring a newer macOS are reported
/// separately rather than silently discarded.
pub fn outdated(
    installed: &[InstalledApp],
    catalog: &[CatalogApp],
    current_macos_version: &str,
) -> UpdateReport {
    let mut updates = Vec::new();
    let mut incompatible_updates = Vec::new();
    let mut unmatched = Vec::new();

    for local in installed {
        let available = find_catalog_match(local, catalog);
        let Some(available) = available else {
            unmatched.push(local.clone());
            continue;
        };
        if compare_versions(&available.version, &local.version) != Ordering::Greater {
            continue;
        }

        let compatible = is_macos_compatible(
            current_macos_version,
            available.minimum_macos_version.as_deref(),
        );
        let candidate = UpdateCandidate {
            installed: local.clone(),
            available: available.clone(),
            compatible,
        };
        if compatible {
            updates.push(candidate);
        } else {
            incompatible_updates.push(candidate);
        }
    }

    let sort_candidates = |left: &UpdateCandidate, right: &UpdateCandidate| {
        left.installed
            .name
            .to_lowercase()
            .cmp(&right.installed.name.to_lowercase())
            .then_with(|| left.installed.bundle_id.cmp(&right.installed.bundle_id))
    };
    updates.sort_by(sort_candidates);
    incompatible_updates.sort_by(sort_candidates);
    unmatched.sort_by(|left, right| left.bundle_id.cmp(&right.bundle_id));

    UpdateReport {
        current_macos_version: current_macos_version.to_owned(),
        checked_count: installed.len(),
        updates,
        incompatible_updates,
        unmatched,
        warnings: Vec::new(),
    }
}

/// Return the current macOS product version reported by `sw_vers`.
pub fn current_macos_version() -> Result<String> {
    ensure_macos()?;
    let output = Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .output()
        .context("run /usr/bin/sw_vers")?;
    if !output.status.success() {
        bail!(
            "sw_vers failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let version = String::from_utf8(output.stdout)
        .context("sw_vers returned non-UTF-8 output")?
        .trim()
        .to_owned();
    if version.is_empty() {
        bail!("sw_vers returned an empty product version");
    }
    Ok(version)
}

/// Fetch catalog records for installed apps, then calculate available updates.
///
/// Adam IDs are looked up in batches; apps without Spotlight metadata fall
/// back to individual bundle-ID lookups.
pub fn check_updates(installed: &[InstalledApp], country: &str) -> Result<UpdateReport> {
    ensure_macos()?;
    let country = normalize_country(country)?;
    let current_version = current_macos_version()?;
    let mut catalog = Vec::new();
    let mut warnings = Vec::new();

    let adam_ids: Vec<u64> = installed.iter().filter_map(|app| app.adam_id).collect();
    catalog.extend(lookup_by_adam_ids(&adam_ids, &country)?);

    let mut queried_bundles = HashSet::new();
    let bundle_ids = installed
        .iter()
        .filter(|app| app.adam_id.is_none())
        .filter_map(|app| {
            queried_bundles
                .insert(app.bundle_id.clone())
                .then_some(app.bundle_id.as_str())
        })
        .collect::<Vec<_>>();
    for ids in bundle_ids.chunks(LOOKUP_BATCH_SIZE) {
        let value = ids.join(",");
        let body = curl_get(
            LOOKUP_ENDPOINT,
            &[
                ("bundleId", value.as_str()),
                ("country", country.as_str()),
                ("media", "software"),
                ("entity", "desktopSoftware"),
            ],
        )?;
        catalog.extend(parse_catalog_response(&body)?);
    }

    let mut report = outdated(installed, &catalog, &current_version);
    warnings.extend(report.unmatched.iter().map(|app| {
        format!(
            "no App Store catalog result for {} ({})",
            app.name, app.bundle_id
        )
    }));
    report.warnings = warnings;
    Ok(report)
}

fn curl_get(endpoint: &str, query: &[(&str, &str)]) -> Result<Vec<u8>> {
    let mut command = Command::new("/usr/bin/curl");
    command.args([
        "--fail",
        "--silent",
        "--show-error",
        "--location",
        "--proto",
        "=https",
        "--connect-timeout",
        "10",
        "--max-time",
        "30",
        "--max-filesize",
        CURL_RESPONSE_LIMIT,
        "--get",
        endpoint,
    ]);
    for (name, value) in query {
        command.arg("--data-urlencode");
        command.arg(format!("{name}={value}"));
    }

    let output = command
        .output()
        .with_context(|| format!("request Apple catalog endpoint {endpoint}"))?;
    if !output.status.success() {
        let status = output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "terminated by signal".to_owned());
        let detail = String::from_utf8_lossy(&output.stderr);
        bail!("Apple catalog request failed ({status}): {}", detail.trim());
    }
    if output.stdout.is_empty() {
        bail!("Apple catalog returned an empty response");
    }
    Ok(output.stdout)
}

fn normalize_country(country: &str) -> Result<String> {
    let country = country.trim();
    if country.len() != 2 || !country.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        bail!("country must be a two-letter ISO 3166-1 code");
    }
    Ok(country.to_ascii_uppercase())
}

fn is_app_bundle(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("app"))
}

fn is_nonempty_file(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
}

fn preferred_volume_applications_root(home: &Path) -> Option<PathBuf> {
    let preferences = home.join("Library/Preferences/com.apple.appstored.plist");
    let output = Command::new("/usr/libexec/PlistBuddy")
        .args(["-c", "Print :PreferredVolume:name"])
        .arg(preferences)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let name = std::str::from_utf8(&output.stdout)
        .ok()?
        .trim_end_matches(['\r', '\n']);
    if !is_valid_volume_component(name) {
        return None;
    }
    Some(Path::new("/Volumes").join(name).join("Applications"))
}

fn inspect_installed_app(app_path: &Path) -> Result<InstalledApp> {
    let info_plist = app_path.join("Contents/Info.plist");
    if !info_plist.is_file() {
        bail!("missing Contents/Info.plist");
    }

    // A single conversion exposes every required plist key as JSON.
    let output = Command::new("/usr/bin/plutil")
        .args(["-convert", "json", "-o", "-"])
        .arg(&info_plist)
        .output()
        .with_context(|| format!("run plutil for {}", info_plist.display()))?;
    if !output.status.success() {
        bail!(
            "plutil failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let adam_id = read_adam_id(app_path);
    parse_installed_app_plist(&output.stdout, app_path, adam_id)
}

fn read_adam_id(app_path: &Path) -> Option<u64> {
    let output = Command::new("/usr/bin/mdls")
        .args(["-raw", "-name", "kMDItemAppStoreAdamID"])
        .arg(app_path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_mdls_adam_id(&String::from_utf8_lossy(&output.stdout))
}

fn plist_string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn find_catalog_match<'a>(
    installed: &InstalledApp,
    catalog: &'a [CatalogApp],
) -> Option<&'a CatalogApp> {
    installed
        .adam_id
        .and_then(|id| catalog.iter().find(|app| app.adam_id == id))
        .or_else(|| {
            catalog.iter().find(|app| {
                !installed.bundle_id.is_empty()
                    && app.bundle_id.eq_ignore_ascii_case(&installed.bundle_id)
            })
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VersionToken {
    Number(String),
    Text(String),
}

fn version_tokens(version: &str) -> Vec<VersionToken> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut current_is_number = None;

    let flush = |tokens: &mut Vec<VersionToken>, current: &mut String, kind: Option<bool>| {
        if current.is_empty() {
            return;
        }
        if kind == Some(true) {
            tokens.push(VersionToken::Number(std::mem::take(current)));
        } else {
            tokens.push(VersionToken::Text(std::mem::take(current).to_lowercase()));
        }
    };

    for character in version.chars() {
        let is_number = character.is_ascii_digit();
        let is_text = character.is_alphanumeric() && !is_number;
        if !is_number && !is_text {
            flush(&mut tokens, &mut current, current_is_number);
            current_is_number = None;
            continue;
        }

        if current_is_number.is_some_and(|kind| kind != is_number) {
            flush(&mut tokens, &mut current, current_is_number);
        }
        current_is_number = Some(is_number);
        current.push(character);
    }
    flush(&mut tokens, &mut current, current_is_number);
    tokens
}

fn trim_trailing_zeroes(tokens: &mut Vec<VersionToken>) {
    while matches!(tokens.last(), Some(VersionToken::Number(number)) if number.bytes().all(|byte| byte == b'0'))
    {
        tokens.pop();
    }
}

fn compare_version_tokens(left: &VersionToken, right: &VersionToken) -> Ordering {
    match (left, right) {
        (VersionToken::Number(left), VersionToken::Number(right)) => {
            compare_numeric_strings(left, right)
        }
        (VersionToken::Text(left), VersionToken::Text(right)) => left.cmp(right),
        (VersionToken::Number(_), VersionToken::Text(_)) => Ordering::Greater,
        (VersionToken::Text(_), VersionToken::Number(_)) => Ordering::Less,
    }
}

fn compare_numeric_strings(left: &str, right: &str) -> Ordering {
    let left = left.trim_start_matches('0');
    let right = right.trim_start_matches('0');
    let left = if left.is_empty() { "0" } else { left };
    let right = if right.is_empty() { "0" } else { right };
    left.len().cmp(&right.len()).then_with(|| left.cmp(right))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed(version: &str, adam_id: Option<u64>) -> InstalledApp {
        InstalledApp {
            path: PathBuf::from("/Applications/Example.app"),
            receipt_path: PathBuf::from("/Applications/Example.app/Contents/_MASReceipt/receipt"),
            name: "Example".to_owned(),
            bundle_id: "com.example.app".to_owned(),
            version: version.to_owned(),
            build_version: Some("100".to_owned()),
            adam_id,
        }
    }

    fn catalog(version: &str, minimum: Option<&str>) -> CatalogApp {
        CatalogApp {
            adam_id: 42,
            bundle_id: "com.example.app".to_owned(),
            name: "Example".to_owned(),
            version: version.to_owned(),
            minimum_macos_version: minimum.map(str::to_owned),
            seller_name: None,
            description: None,
            release_notes: None,
            release_date: None,
            store_url: None,
            artwork_url: None,
            price: None,
            formatted_price: None,
            currency: None,
            genres: Vec::new(),
            file_size_bytes: None,
        }
    }

    #[test]
    fn parses_apple_catalog_aliases() {
        let json = r#"{
            "resultCount": 1,
            "results": [{
                "trackId": 497799835,
                "bundleId": "com.apple.dt.Xcode",
                "trackName": "Xcode",
                "version": "16.4",
                "minimumOsVersion": "15.2",
                "sellerName": "Apple",
                "trackViewUrl": "https://apps.apple.com/app/id497799835",
                "artworkUrl512": "https://example.invalid/icon.png",
                "fileSizeBytes": "123456"
            }]
        }"#;

        let apps = parse_catalog_response(json).unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].adam_id, 497_799_835);
        assert_eq!(apps[0].bundle_id, "com.apple.dt.Xcode");
        assert_eq!(apps[0].minimum_macos_version.as_deref(), Some("15.2"));
        assert_eq!(apps[0].file_size_bytes.as_deref(), Some("123456"));
    }

    #[test]
    fn parses_installed_plist_and_numeric_build_version() {
        let json = r#"{
            "CFBundleIdentifier": "com.example.app",
            "CFBundleDisplayName": "Example App",
            "CFBundleShortVersionString": "2.7.1",
            "CFBundleVersion": 2710
        }"#;
        let app_path = Path::new("/Applications/Example.app");

        let app = parse_installed_app_plist(json, app_path, Some(42)).unwrap();
        assert_eq!(app.name, "Example App");
        assert_eq!(app.bundle_id, "com.example.app");
        assert_eq!(app.version, "2.7.1");
        assert_eq!(app.build_version.as_deref(), Some("2710"));
        assert_eq!(app.adam_id, Some(42));
    }

    #[test]
    fn parses_mdls_adam_id_best_effort() {
        assert_eq!(parse_mdls_adam_id("497799835\n"), Some(497_799_835));
        assert_eq!(parse_mdls_adam_id("\"497799835\""), Some(497_799_835));
        assert_eq!(parse_mdls_adam_id("(null)"), None);
        assert_eq!(parse_mdls_adam_id("not-a-number"), None);
        assert_eq!(parse_mdls_adam_id("0"), None);
    }

    #[test]
    fn compares_versions_naturally_without_overflow() {
        assert_eq!(compare_versions("1.10", "1.9"), Ordering::Greater);
        assert_eq!(compare_versions("2.0.0", "2"), Ordering::Equal);
        assert_eq!(compare_versions("1.0.1", "1.0"), Ordering::Greater);
        assert_eq!(compare_versions("001.02", "1.2"), Ordering::Equal);
        assert_eq!(
            compare_versions("1.999999999999999999999999", "1.10"),
            Ordering::Greater
        );
    }

    #[test]
    fn checks_macos_minimum_version() {
        assert!(is_macos_compatible("15.1", Some("14.6")));
        assert!(is_macos_compatible("14.6.0", Some("14.6")));
        assert!(!is_macos_compatible("13.6", Some("14.0")));
        assert!(is_macos_compatible("13.6", None));
    }

    #[test]
    fn outdated_separates_incompatible_updates() {
        let local = vec![installed("1.9", Some(42))];

        let report = outdated(&local, &[catalog("1.10", Some("14.0"))], "14.5");
        assert_eq!(report.updates.len(), 1);
        assert!(report.incompatible_updates.is_empty());
        assert!(report.updates[0].compatible);

        let report = outdated(&local, &[catalog("2.0", Some("15.0"))], "14.5");
        assert!(report.updates.is_empty());
        assert_eq!(report.incompatible_updates.len(), 1);
        assert!(!report.incompatible_updates[0].compatible);
    }

    #[test]
    fn outdated_falls_back_to_bundle_id() {
        let local = vec![installed("1.0", None)];
        let report = outdated(&local, &[catalog("1.1", None)], "14.0");
        assert_eq!(report.updates.len(), 1);
        assert!(report.unmatched.is_empty());
    }

    #[test]
    fn invalid_catalog_json_is_an_error() {
        assert!(parse_catalog_response("not json").is_err());
    }

    #[test]
    fn validates_external_volume_as_one_normal_component() {
        assert!(is_valid_volume_component("External Apps"));
        assert!(is_valid_volume_component("Apps.Store"));
        assert!(!is_valid_volume_component(""));
        assert!(!is_valid_volume_component("."));
        assert!(!is_valid_volume_component(".."));
        assert!(!is_valid_volume_component("/Volumes/External"));
        assert!(!is_valid_volume_component("External/Other"));
        assert!(!is_valid_volume_component("External\0Other"));
    }
}
