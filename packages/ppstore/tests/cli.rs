use std::process::Command;

fn ppstore() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ppstore"))
}

#[test]
fn top_level_help_exposes_standalone_commands() {
    let output = ppstore().arg("--help").output().expect("run ppstore");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 help");
    for command in ["search", "list", "outdated", "install", "upgrade", "doctor"] {
        assert!(
            stdout.contains(command),
            "missing command {command}: {stdout}"
        );
    }
    assert!(stdout.contains("--output"));
}

#[test]
fn installed_and_update_aliases_are_accepted() {
    for alias in ["installed", "update"] {
        let output = ppstore()
            .args([alias, "--help"])
            .output()
            .expect("run alias help");
        assert!(
            output.status.success(),
            "alias {alias} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn catalog_fixture_is_parseable_without_network_access() {
    let fixture = br#"{
        "resultCount": 1,
        "results": [{
            "trackId": 497799835,
            "bundleId": "com.apple.dt.Xcode",
            "trackName": "Xcode",
            "version": "26.0"
        }]
    }"#;
    let apps = ppstore::app_store::parse_catalog_response(fixture).expect("parse fixture");
    assert_eq!(apps.len(), 1);
    assert_eq!(apps[0].adam_id, 497_799_835);
    assert_eq!(apps[0].bundle_id, "com.apple.dt.Xcode");
}
