use ppduster::audit;
use std::fs;

#[test]
fn appends_and_reads_events_from_jsonl_file() {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("audit.log");

    audit::append_event(&log_path, "scan", "completed", Some("categories=caches")).unwrap();
    audit::append_event(&log_path, "clean", "failed", Some("permission denied")).unwrap();

    let entries = audit::read_events(&log_path).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].action, "scan");
    assert_eq!(entries[0].outcome, "completed");
    assert_eq!(entries[0].detail.as_deref(), Some("categories=caches"));
    assert_eq!(entries[1].action, "clean");
    assert_eq!(entries[1].outcome, "failed");
    assert_eq!(entries[1].detail.as_deref(), Some("permission denied"));

    fs::remove_file(log_path).unwrap();
}

#[test]
fn missing_log_returns_empty_entries() {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("missing.log");

    let entries = audit::read_events(&log_path).unwrap();
    assert!(entries.is_empty());
}
