use std::collections::BTreeMap;
use std::fs;

use ppduster::automation::{
    run_task, Action, ActionNode, AuthPolicy, Binding, ElevationPolicy, ForEachNode, GraphNode,
    InspectPathAction, LoopFailurePolicy, RunOptions, Step, StepStatus, Task, TrustRequirement,
    WorkflowGraph, TASK_FORMAT_VERSION, WORKFLOW_GRAPH_VERSION,
};
use ppduster::rules::Platform;

fn inspect_step(id: &str, path: &str) -> Step {
    Step {
        id: id.into(),
        name: format!("Inspect {path}"),
        bindings: BTreeMap::new(),
        auth: AuthPolicy::None,
        check: None,
        dangerous: false,
        allow_elevation: ElevationPolicy::Forbidden,
        when: None,
        require: None,
        action: Action::InspectPath(InspectPathAction {
            path: path.into(),
            recursive_size: false,
            sha256: false,
            expect: None,
        }),
    }
}

fn action_node(step: Step) -> GraphNode {
    GraphNode::Action(Box::new(ActionNode {
        step,
        bindings: BTreeMap::new(),
    }))
}

fn one_action_graph(version: u32, step: Step) -> WorkflowGraph {
    WorkflowGraph {
        version,
        entries: vec![step.id.clone()],
        nodes: vec![action_node(step)],
        ..WorkflowGraph::default()
    }
}

fn canonical_task(graph: WorkflowGraph) -> Task {
    Task {
        id: "canonical-v3".into(),
        name: "Canonical graph task".into(),
        description: "A graph-only task used at the public API boundary.".into(),
        platform: Platform::Any,
        trust: TrustRequirement::ExternalAllowed,
        scenarios: Vec::new(),
        resolved_scenarios: Vec::new(),
        steps: Vec::new(),
        graph: Some(graph),
    }
}

fn graph_task_document(format_version: u32, graph: &WorkflowGraph) -> String {
    serde_yaml::to_string(&serde_json::json!({
        "format_version": format_version,
        "id": "graph-document",
        "name": "Graph document",
        "description": "Serialized explicitly to exercise the import boundary.",
        "platform": "any",
        "trust": "external-allowed",
        "workflow_graph": serde_json::to_value(graph).unwrap(),
    }))
    .unwrap()
}

#[test]
fn legacy_steps_yaml_deserializes_to_a_graph_only_v3_task() {
    let yaml = r#"
id: legacy-steps
name: Legacy steps
description: Imported from the v1 linear representation.
platform: any
trust: external-allowed
steps:
  - id: inspect
    name: Inspect a known directory
    type: inspect-path
    path: /tmp
"#;

    let task: Task = serde_yaml::from_str(yaml).unwrap();

    assert!(task.steps.is_empty(), "legacy steps must be import-only");
    assert!(task.scenarios.is_empty());
    let graph = task.workflow_graph().unwrap();
    assert_eq!(graph.version, WORKFLOW_GRAPH_VERSION);
    assert_eq!(graph.entries, ["inspect"]);
    assert_eq!(graph.nodes.len(), 1);
    assert_eq!(graph.nodes[0].id(), "inspect");
}

#[test]
fn version_two_graphs_are_migrated_recursively() {
    let body = one_action_graph(2, inspect_step("inspect-item", "/tmp"));
    let graph = WorkflowGraph {
        version: 2,
        entries: vec!["items".into()],
        nodes: vec![GraphNode::ForEach(ForEachNode {
            id: "items".into(),
            collection: Binding::literal(serde_json::json!(["first", "second"])),
            item_alias: "item".into(),
            index_alias: Some("index".into()),
            concurrency: 1,
            on_error: LoopFailurePolicy::Stop,
            body: Box::new(body),
        })],
        ..WorkflowGraph::default()
    };
    let document = graph_task_document(2, &graph);

    let task: Task = serde_yaml::from_str(&document).unwrap();
    let migrated = task.workflow_graph().unwrap();

    assert_eq!(migrated.version, WORKFLOW_GRAPH_VERSION);
    let GraphNode::ForEach(loop_node) = &migrated.nodes[0] else {
        panic!("expected a for-each root")
    };
    assert_eq!(loop_node.body.version, WORKFLOW_GRAPH_VERSION);
    assert_eq!(loop_node.body.nodes[0].id(), "inspect-item");
}

#[test]
fn serialization_emits_only_the_v3_task_and_workflow_graph_keys() {
    let legacy = r#"
id: legacy-to-serialize
name: Legacy to serialize
description: Verifies that legacy syntax is never written back out.
platform: any
trust: external-allowed
steps:
  - id: inspect
    type: inspect-path
    path: /tmp
"#;
    let task: Task = serde_yaml::from_str(legacy).unwrap();

    let serialized = serde_yaml::to_string(&task).unwrap();
    let value: serde_yaml::Value = serde_yaml::from_str(&serialized).unwrap();
    let root = value.as_mapping().unwrap();

    assert_eq!(
        root.get(serde_yaml::Value::from("format_version"))
            .and_then(serde_yaml::Value::as_u64),
        Some(u64::from(TASK_FORMAT_VERSION))
    );
    assert!(root.contains_key(serde_yaml::Value::from("workflow_graph")));
    assert!(!root.contains_key(serde_yaml::Value::from("steps")));
    assert!(!root.contains_key(serde_yaml::Value::from("graph")));
    assert!(serialized.contains("format_version: 3"));
    assert!(serialized.contains("workflow_graph:"));
}

#[test]
fn graph_only_v3_task_round_trips_without_a_legacy_projection() {
    let original = canonical_task(one_action_graph(
        WORKFLOW_GRAPH_VERSION,
        inspect_step("inspect", "/tmp"),
    ));

    let yaml = serde_yaml::to_string(&original).unwrap();
    let round_trip: Task = serde_yaml::from_str(&yaml).unwrap();

    assert!(round_trip.steps.is_empty());
    assert!(round_trip.scenarios.is_empty());
    assert_eq!(round_trip.id, original.id);
    assert_eq!(round_trip.name, original.name);
    assert_eq!(round_trip.description, original.description);
    assert_eq!(round_trip.platform, original.platform);
    assert_eq!(round_trip.trust, original.trust);
    assert_eq!(
        serde_json::to_value(round_trip.workflow_graph().unwrap()).unwrap(),
        serde_json::to_value(original.workflow_graph().unwrap()).unwrap()
    );
}

#[test]
fn run_task_executes_the_canonical_graph_directly() {
    let temp = tempfile::tempdir().unwrap();
    let inspected = temp.path().join("already-present");
    fs::create_dir(&inspected).unwrap();
    let task = canonical_task(one_action_graph(
        WORKFLOW_GRAPH_VERSION,
        inspect_step("inspect-canonical", &inspected.to_string_lossy()),
    ));

    assert!(task.steps.is_empty());
    let report = run_task(&task, &RunOptions::default()).unwrap();

    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.steps.len(), 1);
    assert_eq!(report.steps[0].step_id, "inspect-canonical");
    assert!(matches!(report.steps[0].status, StepStatus::Satisfied));
}

#[test]
fn unsupported_future_task_and_graph_versions_are_rejected() {
    let graph = one_action_graph(WORKFLOW_GRAPH_VERSION, inspect_step("inspect", "/tmp"));
    let future_task = graph_task_document(TASK_FORMAT_VERSION + 1, &graph);
    let task_error = serde_yaml::from_str::<Task>(&future_task)
        .unwrap_err()
        .to_string();
    assert!(task_error.contains("unsupported task format version"));

    let future_graph = WorkflowGraph {
        version: WORKFLOW_GRAPH_VERSION + 1,
        ..graph.clone()
    };
    let future_graph_document = graph_task_document(TASK_FORMAT_VERSION, &future_graph);
    let graph_error = serde_yaml::from_str::<Task>(&future_graph_document)
        .unwrap_err()
        .to_string();
    assert!(graph_error.contains("unsupported workflow graph version"));

    let future_body = WorkflowGraph {
        version: WORKFLOW_GRAPH_VERSION + 1,
        ..one_action_graph(
            WORKFLOW_GRAPH_VERSION,
            inspect_step("nested-inspect", "/tmp"),
        )
    };
    let graph_with_future_body = WorkflowGraph {
        version: WORKFLOW_GRAPH_VERSION,
        entries: vec!["items".into()],
        nodes: vec![GraphNode::ForEach(ForEachNode {
            id: "items".into(),
            collection: Binding::literal(serde_json::json!(["item"])),
            item_alias: "item".into(),
            index_alias: None,
            concurrency: 1,
            on_error: LoopFailurePolicy::Stop,
            body: Box::new(future_body),
        })],
        ..WorkflowGraph::default()
    };
    let nested_future_document = graph_task_document(TASK_FORMAT_VERSION, &graph_with_future_body);
    let nested_error = serde_yaml::from_str::<Task>(&nested_future_document)
        .unwrap_err()
        .to_string();
    assert!(nested_error.contains("unsupported workflow graph version"));
}

#[test]
fn format_v3_rejects_every_legacy_or_empty_executable_form() {
    let graph = one_action_graph(WORKFLOW_GRAPH_VERSION, inspect_step("inspect", "/tmp"));
    let metadata = serde_json::json!({
        "format_version": TASK_FORMAT_VERSION,
        "id": "strict-v3",
        "name": "Strict v3",
        "description": "Only the canonical workflow_graph key is valid.",
        "platform": "any",
        "trust": "external-allowed",
    });
    let mut documents = Vec::new();
    for executable in [
        serde_json::json!({ "steps": [serde_json::to_value(inspect_step("legacy", "/tmp")).unwrap()] }),
        serde_json::json!({ "scenarios": ["legacy-child"] }),
        serde_json::json!({ "graph": serde_json::to_value(&graph).unwrap() }),
        serde_json::json!({}),
    ] {
        let mut document = metadata.clone();
        document
            .as_object_mut()
            .unwrap()
            .extend(executable.as_object().unwrap().clone());
        documents.push(serde_yaml::to_string(&document).unwrap());
    }

    for document in documents {
        let error = serde_yaml::from_str::<Task>(&document)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("format version 3 requires exactly one workflow_graph"),
            "unexpected error: {error}"
        );
    }
}
