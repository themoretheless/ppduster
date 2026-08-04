use crate::rules::expand_path_template;
use std::path::{Component, Path, PathBuf};

/// Paths and path prefixes that must never be deleted, even if a rule matches.
pub fn never_touch_patterns() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = dirs::home_dir() {
        for rel in [
            "Documents",
            "Desktop",
            "Downloads",
            "Pictures",
            "Movies",
            "Music",
            "Photos Library.photoslibrary",
            ".ssh",
            ".gnupg",
            ".aws",
            ".config/gcloud",
            "Library/Keychains",
            "Library/Mail",
            "Library/Messages",
            "Library/Calendars",
            "Library/Mobile Documents",
            "Library/CloudStorage",
            "Library/Containers/com.apple.mail",
        ] {
            out.push(home.join(rel));
        }
    }
    // System trees (prefix-matched). Bare "/" / "C:\\" are handled separately
    // so they do not mark every absolute path as forbidden.
    for p in [
        "/System",
        "/bin",
        "/sbin",
        "/usr",
        "/etc",
        "/dev",
        "/proc",
        "/sys",
        "/boot",
        "/lib",
        "/lib64",
        "/Applications",
        "/Library",
        "/private/var/db",
        "/private/etc",
        "C:\\Windows",
        "C:\\Program Files",
        "C:\\Program Files (x86)",
    ] {
        out.push(PathBuf::from(p));
    }
    // Expanded sensitive templates
    for t in [
        "$HOME/.ssh",
        "$HOME/.gnupg",
        "$HOME/Library/Keychains",
        "%USERPROFILE%\\Documents",
        "%USERPROFILE%\\Desktop",
    ] {
        if let Some(p) = expand_path_template(t) {
            out.push(p);
        }
    }
    out
}

/// Return true if `path` is forbidden to delete.
pub fn is_never_touch(path: &Path) -> bool {
    let Ok(canon) = normalize(path) else {
        // If we cannot normalize, be conservative.
        return true;
    };

    // Exact root-like paths only (do not treat "/" as a prefix of every path).
    if is_filesystem_root(&canon) {
        return true;
    }

    for forbidden in never_touch_patterns() {
        // Skip bare filesystem roots in the prefix list; handled above.
        if is_filesystem_root(&forbidden) {
            continue;
        }
        let Ok(f) = normalize(&forbidden) else {
            continue;
        };
        if is_filesystem_root(&f) {
            continue;
        }
        // Exact match or strict child of a protected path.
        if canon == f || canon.starts_with(&f) {
            return true;
        }
    }

    // Block deleting the home directory itself
    if let Some(home) = dirs::home_dir() {
        if let Ok(h) = normalize(&home) {
            if canon == h {
                return true;
            }
        }
    }

    false
}

/// A candidate root from a rule must not itself be a never-touch path.
pub fn is_safe_rule_root(path: &Path) -> bool {
    if is_never_touch(path) {
        return false;
    }
    // Refuse scanning pure system roots as rule roots
    if is_filesystem_root(path) {
        return false;
    }
    true
}

/// Ensure a discovered file stays under its rule root (symlink escape guard).
pub fn stays_under_root(root: &Path, candidate: &Path) -> bool {
    let Ok(r) = normalize(root) else {
        return false;
    };
    let Ok(c) = normalize(candidate) else {
        return false;
    };
    c.starts_with(&r)
}

pub fn normalize(path: &Path) -> std::io::Result<PathBuf> {
    // Prefer canonicalize when path exists; otherwise lexical normalize.
    if path.exists() {
        return path.canonicalize();
    }
    Ok(lexical_normalize(path))
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn is_filesystem_root(path: &Path) -> bool {
    let s = path.to_string_lossy();
    s == "/"
        || s == "C:\\"
        || s == "C:/"
        || path.parent().is_none()
        || (path.components().count() == 1 && path.has_root())
}

/// Age check: true if file is old enough to clean (or age filter disabled).
pub fn is_old_enough(path: &Path, min_age_days: u64) -> bool {
    if min_age_days == 0 {
        return true;
    }
    let Ok(meta) = path.metadata() else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return true; // if unknown, allow under other filters
    };
    let Ok(elapsed) = modified.elapsed() else {
        // future mtime
        return false;
    };
    elapsed.as_secs() >= min_age_days.saturating_mul(24 * 3600)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_documents_blocked() {
        if let Some(home) = dirs::home_dir() {
            assert!(is_never_touch(&home.join("Documents")));
            assert!(is_never_touch(&home.join("Documents/file.txt")));
            assert!(is_never_touch(&home));
        }
    }

    #[test]
    fn cache_not_blocked() {
        if let Some(home) = dirs::home_dir() {
            // User caches are not in never-touch list
            assert!(!is_never_touch(&home.join("Library/Caches/foo")));
        }
    }
}
