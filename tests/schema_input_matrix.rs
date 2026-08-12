use std::collections::BTreeMap;

use ppduster::automation::{
    block_definition, default_step, materialize_step, validate_literal_binding, ActionKind,
    Binding, BindingError, BindingLimits, ContextStore, ContextType, FieldSchema, Sensitivity,
    Step,
};
use serde_json::{json, Value};

fn executable_action_kinds() -> impl Iterator<Item = ActionKind> {
    ActionKind::ALL
        .into_iter()
        .filter(|kind| kind.is_graph_action())
}

fn input_leaves(kind: ActionKind) -> Vec<(String, FieldSchema)> {
    fn visit(
        value_type: &ContextType,
        prefix: &str,
        inherited_required: bool,
        inherited_sensitivity: Sensitivity,
        output: &mut Vec<(String, FieldSchema)>,
    ) {
        let ContextType::Object { schema } = value_type else {
            return;
        };
        for (name, field) in &schema.fields {
            let target = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}.{name}")
            };
            let required = inherited_required && field.required;
            let sensitivity = inherited_sensitivity.combine(field.sensitivity);
            match &field.value_type {
                ContextType::Object { schema }
                    if !schema.fields.is_empty()
                        && ((field.required && !field.nullable)
                            || schema.fields.values().all(|child| !child.required)) =>
                {
                    visit(&field.value_type, &target, required, sensitivity, output)
                }
                _ => output.push((
                    target,
                    FieldSchema {
                        value_type: field.value_type.clone(),
                        required,
                        nullable: field.nullable,
                        description: field.description.clone(),
                        sensitivity,
                        allowed_values: field.allowed_values.clone(),
                    },
                )),
            }
        }
    }

    let definition = block_definition(kind);
    let mut leaves = Vec::new();
    visit(
        &ContextType::object(definition.input_schema),
        "",
        true,
        Sensitivity::Public,
        &mut leaves,
    );
    leaves
}

fn independently_literal_bindable_leaves(kind: ActionKind) -> Vec<(String, FieldSchema)> {
    // The selector consumes the whole live GitHub context through one exact
    // structural binding. Partial/literal bindings would bypass its producer
    // and account-pin invariants, so they intentionally do not participate in
    // the generic literal-input matrix.
    if kind == ActionKind::GithubSelectRepositories {
        Vec::new()
    } else {
        input_leaves(kind)
    }
}

fn materialize_literals(
    kind: ActionKind,
    suffix: &str,
    literals: impl IntoIterator<Item = (String, Value)>,
) -> Result<Step, BindingError> {
    let mut step = default_step(kind, format!("{}-{suffix}", kind.id())).unwrap();
    let bindings = literals
        .into_iter()
        .map(|(target, value)| (target, Binding::literal(value)))
        .collect::<BTreeMap<_, _>>();

    // Shell mode is the only enum input whose non-default value changes the
    // step's dangerous-operation contract.
    if kind == ActionKind::RunCommand && bindings.get("shell") == Some(&Binding::literal("allow")) {
        step.dangerous = true;
    }

    materialize_step(
        &step,
        &bindings,
        &ContextStore::default(),
        BindingLimits::default(),
    )
}

fn assert_round_trip(step: Step, kind: ActionKind, case: &str) {
    assert_eq!(step.action.kind(), kind, "action kind changed for {case}");
    step.validate()
        .unwrap_or_else(|error| panic!("materialized step {case} must validate: {error}"));
    let encoded = serde_json::to_value(&step).unwrap();
    let decoded: Step = serde_json::from_value(encoded).unwrap();
    assert_eq!(decoded.action.kind(), kind, "serde changed {case}");
    decoded
        .validate()
        .unwrap_or_else(|error| panic!("round-tripped step {case} must validate: {error}"));
}

fn manual_literal(kind: ActionKind, target: &str) -> Option<Value> {
    Some(match (kind, target) {
        (ActionKind::GithubListRepositories, _) => return None,
        (ActionKind::CreateDirectory, "path") => json!("/tmp/ppduster-matrix-created"),
        (ActionKind::InspectPath, "path") => json!("/tmp/ppduster-matrix-inspect"),
        (ActionKind::InspectPath, "recursive_size" | "sha256") => json!(true),
        (ActionKind::InspectPath, "expect") => json!({
            "exists": true,
            "kind": "file",
            "empty": false,
            "min_size_bytes": 1,
            "max_size_bytes": 2,
            "modified_at_or_after": "2026-01-01T00:00:00Z",
            "modified_at_or_before": "2026-01-02T00:00:00Z",
            "sha256": "a".repeat(64),
        }),
        (ActionKind::InspectPath, "expect.exists") => json!(true),
        (ActionKind::InspectPath, "expect.kind") => json!("file"),
        (ActionKind::InspectPath, "expect.empty") => json!(false),
        (ActionKind::InspectPath, "expect.min_size_bytes") => json!(1),
        (ActionKind::InspectPath, "expect.max_size_bytes") => json!(2),
        (ActionKind::InspectPath, "expect.modified_at_or_after") => {
            json!("2026-01-01T00:00:00Z")
        }
        (ActionKind::InspectPath, "expect.modified_at_or_before") => {
            json!("2026-01-02T00:00:00Z")
        }
        (ActionKind::InspectPath, "expect.sha256") => json!("a".repeat(64)),
        (ActionKind::CopyPath, "src") => json!("/tmp/ppduster-matrix-source"),
        (ActionKind::CopyPath, "dest") => json!("/tmp/ppduster-matrix-destination"),
        (ActionKind::WriteFile, "path") => json!("/tmp/ppduster-matrix.txt"),
        (ActionKind::WriteFile, "content") => json!("matrix content"),
        (ActionKind::WriteFile, "on_conflict") => json!("replace"),
        (ActionKind::RemovePath, "path") => json!("/tmp/ppduster-matrix-remove"),
        (
            ActionKind::GitClone
            | ActionKind::GitInspect
            | ActionKind::GitCloneIfMissing
            | ActionKind::GitFetch
            | ActionKind::GitFastForward,
            "repo",
        ) => json!("https://github.com/example/matrix.git"),
        (
            ActionKind::GitClone
            | ActionKind::GitInspect
            | ActionKind::GitCloneIfMissing
            | ActionKind::GitFetch
            | ActionKind::GitFastForward,
            "dest",
        ) => json!("/tmp/ppduster-matrix-repository"),
        (
            ActionKind::GitClone
            | ActionKind::GitCloneIfMissing
            | ActionKind::GitFetch
            | ActionKind::GitFastForward,
            "branch",
        ) => json!("main"),
        (ActionKind::BrewInstall, "package") => json!("ripgrep"),
        (ActionKind::BrewInstall, "cask") => json!(false),
        (ActionKind::RunCommand, "program") => json!("true"),
        (ActionKind::RunCommand, "args") => json!(["--version"]),
        (ActionKind::RunCommand, "cwd") => json!("/tmp"),
        (ActionKind::RunCommand, "env") => json!({ "LANG": "C" }),
        (ActionKind::RunCommand, "shell") => json!("forbidden"),
        (ActionKind::RunScript, "interpreter") => json!("sh"),
        (ActionKind::RunScript, "script") => json!("/tmp/ppduster-matrix.sh"),
        (ActionKind::RunScript, "args") => json!(["--version"]),
        (ActionKind::RunScript, "cwd") => json!("/tmp"),
        (ActionKind::RunScript, "env") => json!({ "LANG": "C" }),
        (ActionKind::RunScript, "success_exit_codes") => json!([0, 1]),
        (ActionKind::ConfigurePackageRegistryFiles, "secrets.profile") => json!("matrix"),
        (ActionKind::ConfigurePackageRegistryFiles, "secrets.username_env") => {
            json!("PPDUSTER_MATRIX_USERNAME")
        }
        (ActionKind::ConfigurePackageRegistryFiles, "secrets.token_env") => {
            json!("PPDUSTER_MATRIX_TOKEN")
        }
        (ActionKind::ConfigurePackageRegistryFiles, "npm.scope") => json!("@matrix"),
        (ActionKind::ConfigurePackageRegistryFiles, "npm.registry") => {
            json!("https://registry.example.com/matrix/npm/")
        }
        (ActionKind::ConfigurePackageRegistryFiles, "nuget.public_source_name") => {
            json!("public-matrix")
        }
        (ActionKind::ConfigurePackageRegistryFiles, "nuget.public_source") => {
            json!("https://api.nuget.org/v3/index.json")
        }
        (ActionKind::ConfigurePackageRegistryFiles, "nuget.source_name") => {
            json!("private-matrix")
        }
        (ActionKind::ConfigurePackageRegistryFiles, "nuget.source") => {
            json!("https://registry.example.com/matrix/nuget/v3/index.json")
        }
        (ActionKind::ConfigurePackageRegistryFiles, "nuget.package_patterns") => {
            json!(["Matrix.*"])
        }
        (ActionKind::DownloadFile, "url") => json!("https://example.com/matrix.zip"),
        (ActionKind::DownloadFile, "dest") => json!("/tmp/ppduster-matrix.zip"),
        (ActionKind::DownloadFile, "checksum.sha256") => json!("b".repeat(64)),
        (ActionKind::ExtractArchive, "src") => json!("/tmp/ppduster-matrix.zip"),
        (ActionKind::ExtractArchive, "dest") => json!("/tmp/ppduster-matrix-extracted"),
        (ActionKind::ExtractArchive, "format") => json!("zip"),
        (ActionKind::ExtractArchive, "max_unpacked_bytes") => json!(1024),
        (ActionKind::InstallDmg, "dmg") => json!("/tmp/ppduster-matrix.dmg"),
        (ActionKind::InstallDmg, "app_name") => json!("Matrix.app"),
        (ActionKind::InstallDmg, "target") => json!("/Applications"),
        (ActionKind::InstallDmg, "identity") => json!({
            "bundle_identifier": "com.example.Matrix",
            "team_identifier": "MATRIXTEAM",
            "version": "1.0.0",
        }),
        (ActionKind::InstallDmg, "identity.bundle_identifier") => {
            json!("com.example.Matrix")
        }
        (ActionKind::InstallDmg, "identity.team_identifier") => json!("MATRIXTEAM"),
        (ActionKind::InstallDmg, "identity.version") => json!("1.0.0"),
        (ActionKind::InstallPkg, "pkg") => json!("/tmp/ppduster-matrix.pkg"),
        (ActionKind::InstallPkg, "target") => json!("/"),
        (ActionKind::MacosRequirements, "minimum_version") => json!("14.0"),
        (ActionKind::MacosRequirements, "require_rosetta_on_apple_silicon") => json!(true),
        (ActionKind::AppStoreInstall, "app_id") => json!(42),
        (ActionKind::AppStoreInstall, "operation") => json!("get"),
        (ActionKind::BambuStudioRelease, "channel") => json!("beta"),
        (ActionKind::ActivateLicense, "provider") => json!("light-burn"),
        (ActionKind::ActivateLicense, "method") => json!("vendor-ui"),
        _ => return None,
    })
}

#[test]
fn every_executable_default_step_materializes_and_round_trips() {
    let kinds = executable_action_kinds().collect::<Vec<_>>();
    assert_eq!(kinds.len(), 24, "update the default-input matrix");

    for kind in kinds {
        let materialized = materialize_literals(kind, "default", []).unwrap_or_else(|error| {
            panic!("default step for {} must materialize: {error}", kind.id())
        });
        assert_round_trip(materialized, kind, &format!("{} default", kind.id()));
    }
}

#[test]
fn every_independently_safe_schema_leaf_materializes_and_round_trips() {
    let mut leaf_count = 0;
    let mut safe_count = 0;

    for kind in executable_action_kinds() {
        for (target, field) in independently_literal_bindable_leaves(kind) {
            leaf_count += 1;
            let value = manual_literal(kind, &target).unwrap_or_else(|| {
                panic!("missing manual-literal fixture for {}.{target}", kind.id())
            });
            validate_literal_binding(&value, &field).unwrap_or_else(|error| {
                panic!(
                    "fixture for {}.{target} violates its schema: {error}",
                    kind.id()
                )
            });
            let result = materialize_literals(
                kind,
                &format!("leaf-{}", target.replace('.', "-")),
                [(target.clone(), value)],
            );

            safe_count += 1;
            let materialized = result.unwrap_or_else(|error| {
                panic!(
                    "manual leaf {}.{target} must materialize: {error}",
                    kind.id()
                )
            });
            assert_round_trip(materialized, kind, &format!("{}.{target}", kind.id()));
        }
    }

    assert_eq!(leaf_count, 75, "update fixtures when schemas change");
    assert_eq!(safe_count, 75);
}

#[test]
fn every_action_materializes_all_compatible_manual_fields_together() {
    let mut action_count = 0;
    let mut binding_count = 0;

    for kind in executable_action_kinds() {
        action_count += 1;
        let mut literals = Vec::new();
        for (target, field) in independently_literal_bindable_leaves(kind) {
            let value = manual_literal(kind, &target).unwrap_or_else(|| {
                panic!(
                    "missing grouped manual-literal fixture for {}.{target}",
                    kind.id()
                )
            });
            validate_literal_binding(&value, &field).unwrap_or_else(|error| {
                panic!(
                    "grouped fixture for {}.{target} violates its schema: {error}",
                    kind.id()
                )
            });
            binding_count += 1;
            literals.push((target, value));
        }
        let materialized =
            materialize_literals(kind, "all-manual", literals).unwrap_or_else(|error| {
                panic!(
                    "all compatible manual fields for {} must materialize together: {error}",
                    kind.id()
                )
            });
        assert_round_trip(
            materialized,
            kind,
            &format!("{} all compatible manual fields", kind.id()),
        );
    }

    assert_eq!(action_count, 24, "update the grouped action matrix");
    assert_eq!(binding_count, 75, "update grouped manual fixtures");
}

#[test]
fn github_selection_exposes_only_the_structural_context_as_bindable_input() {
    let definition = block_definition(ActionKind::GithubSelectRepositories);
    assert_eq!(
        definition
            .input_schema
            .fields
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["github"]
    );
    for static_policy_field in ["expected_account_login", "repository_ids"] {
        assert!(
            definition.input_schema.field(static_policy_field).is_none(),
            "static selector policy {static_policy_field} must not be bindable"
        );
    }
    assert!(independently_literal_bindable_leaves(ActionKind::GithubSelectRepositories).is_empty());
}

#[test]
fn every_canonical_serde_enum_literal_materializes() {
    let cases: [(ActionKind, &str, &[&str]); 9] = [
        (
            ActionKind::InspectPath,
            "expect.kind",
            &["file", "directory", "symlink", "other"],
        ),
        (ActionKind::WriteFile, "on_conflict", &["fail", "replace"]),
        (ActionKind::RunCommand, "shell", &["forbidden", "allow"]),
        (
            ActionKind::RunScript,
            "interpreter",
            &["sh", "bash", "powershell"],
        ),
        (
            ActionKind::ExtractArchive,
            "format",
            &["auto", "zip", "tar", "tar-gz", "tar-bz2", "tar-xz"],
        ),
        (
            ActionKind::AppStoreInstall,
            "operation",
            &["install", "get"],
        ),
        (
            ActionKind::BambuStudioRelease,
            "channel",
            &["release", "beta"],
        ),
        (ActionKind::ActivateLicense, "provider", &["light-burn"]),
        (ActionKind::ActivateLicense, "method", &["vendor-ui"]),
    ];

    let mut literal_count = 0;
    for (kind, target, values) in cases {
        let segments = target
            .split('.')
            .map(ppduster::automation::ContextPathSegment::field)
            .collect::<Vec<_>>();
        let resolved = block_definition(kind)
            .input_schema
            .resolve_input_target_owned(&segments)
            .unwrap_or_else(|| panic!("missing enum schema leaf {}.{target}", kind.id()));
        let field = FieldSchema {
            value_type: resolved.value_type,
            required: resolved.required,
            nullable: resolved.nullable,
            description: None,
            sensitivity: resolved.sensitivity,
            allowed_values: resolved.allowed_values,
        };
        for value in values {
            literal_count += 1;
            let literal = json!(value);
            validate_literal_binding(&literal, &field).unwrap_or_else(|error| {
                panic!(
                    "enum literal {}.{target}={value} violates schema: {error}",
                    kind.id()
                )
            });
            let materialized = materialize_literals(
                kind,
                &format!("enum-{}-{value}", target.replace('.', "-")),
                [(target.into(), literal)],
            )
            .unwrap_or_else(|error| {
                panic!(
                    "enum literal {}.{target}={value} must materialize: {error}",
                    kind.id()
                )
            });
            assert_round_trip(
                materialized,
                kind,
                &format!("{}.{target}={value}", kind.id()),
            );
        }
    }

    assert_eq!(literal_count, 23, "update the serde-enum matrix");
}

#[test]
fn every_invalid_serde_enum_literal_is_rejected_before_materialization() {
    let cases: [(ActionKind, &str, &str); 9] = [
        (ActionKind::InspectPath, "expect.kind", "socket"),
        (ActionKind::WriteFile, "on_conflict", "overwrite"),
        (ActionKind::RunCommand, "shell", "bash"),
        (ActionKind::RunScript, "interpreter", "python"),
        (ActionKind::ExtractArchive, "format", "rar"),
        (ActionKind::AppStoreInstall, "operation", "update"),
        (ActionKind::BambuStudioRelease, "channel", "nightly"),
        (ActionKind::ActivateLicense, "provider", "other"),
        (ActionKind::ActivateLicense, "method", "command-line"),
    ];

    let mut rejected_count = 0;
    for (kind, target, value) in cases {
        let segments = target
            .split('.')
            .map(ppduster::automation::ContextPathSegment::field)
            .collect::<Vec<_>>();
        let resolved = block_definition(kind)
            .input_schema
            .resolve_input_target_owned(&segments)
            .unwrap_or_else(|| panic!("missing enum schema leaf {}.{target}", kind.id()));
        let field = FieldSchema {
            value_type: resolved.value_type,
            required: resolved.required,
            nullable: resolved.nullable,
            description: None,
            sensitivity: resolved.sensitivity,
            allowed_values: resolved.allowed_values,
        };
        let literal = json!(value);

        assert!(
            validate_literal_binding(&literal, &field).is_err(),
            "invalid enum literal {}.{target}={value} passed editor validation",
            kind.id()
        );

        let result = materialize_literals(
            kind,
            &format!("invalid-enum-{}", target.replace('.', "-")),
            [(target.into(), literal)],
        );
        assert!(
            matches!(result, Err(BindingError::InvalidValue { .. })),
            "invalid enum literal {}.{target}={value} was not rejected by materialization: {result:?}",
            kind.id()
        );
        rejected_count += 1;
    }

    assert_eq!(rejected_count, 9, "update the invalid serde-enum matrix");
}
