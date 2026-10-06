use ppduster::data_graph::{DataDocument, DataNode, DataPosition};
use serde_json::{json, Value};
fn execute(document: &DataDocument) -> Value {
    serde_json::from_str(&ppduster::data_pipeline::process_request(
        &document.graph.compile().unwrap().to_string(),
    ))
    .unwrap()
}
#[test]
fn branched_filter_projection_round_trip_preserves_results() {
    let mut doc = DataDocument::default();
    doc.graph.nodes.push(DataNode{id:"filter".into(),kind:"transform.filter".into(),version:1,position:DataPosition{x:300.,y:260.},config:json!({"title":"Только активные","expression":{"kind":"condition","field":"active","operator":"equal","value":"true"}})});
    doc.graph.nodes.push(DataNode {
        id: "active-output".into(),
        kind: "sink.json".into(),
        version: 1,
        position: DataPosition { x: 600., y: 260. },
        config: json!({"title":"Активные"}),
    });
    doc.graph.connect("source", "filter").unwrap();
    doc.graph.connect("filter", "active-output").unwrap();
    let before = execute(&doc);
    assert_eq!(before["ok"], true);
    assert_eq!(before["sink_outputs"]["output"], "name\nAlice\nBob");
    let active: Value =
        serde_json::from_str(before["sink_outputs"]["active-output"].as_str().unwrap()).unwrap();
    assert_eq!(active.as_array().unwrap().len(), 1);
    assert_eq!(active[0]["name"], "Alice");
    let decoded = DataDocument::decode(&doc.encode().unwrap()).unwrap();
    assert_eq!(execute(&decoded), before);
}
#[test]
fn rejected_cycle_does_not_replace_existing_connection() {
    let mut doc = DataDocument::default();
    doc.graph.nodes.push(DataNode {
        id: "filter".into(),
        kind: "transform.filter".into(),
        version: 1,
        position: DataPosition { x: 0., y: 0. },
        config: json!({"conditions":[]}),
    });
    doc.graph.connect("fields", "filter").unwrap();
    let before = serde_json::to_value(&doc.graph).unwrap();
    assert!(doc.graph.connect("filter", "fields").is_err());
    assert_eq!(serde_json::to_value(&doc.graph).unwrap(), before);
}
#[test]
fn imported_list_template_join_executes_end_to_end() {
    let doc=DataDocument::decode(&json!({"format":"peregon-pipeline","version":2,"savedAt":"2026-10-01T00:00:00Z","metadata":{"name":"Values"},"view":{"x":0,"y":0,"zoom":1},"graph":{"version":2,"id":"values","name":"Values","revision":0,"nodes":[
        {"id":"list","type":"source.list","position":{"x":0,"y":0},"config":{"text":"a, b, a"}},
        {"id":"template","type":"transform.template","position":{"x":240,"y":0},"config":{"valueTemplate":"<{value}>"}},
        {"id":"join","type":"sink.join","position":{"x":480,"y":0},"config":{"delimiter":"\n","unique":true}}
    ],"connections":[
        {"id":"a","from":{"nodeId":"list","port":"values"},"to":{"nodeId":"template","port":"values"}},
        {"id":"b","from":{"nodeId":"template","port":"values"},"to":{"nodeId":"join","port":"values"}}
    ]}}).to_string()).unwrap();
    let result = execute(&doc);
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["sink_outputs"]["join"], "<a>\n<b>");
}
#[test]
fn missing_inputs_and_incompatible_types_are_rejected() {
    let mut doc = DataDocument::default();
    doc.graph.connections.clear();
    assert!(doc.graph.compile().is_err());
    doc.graph.nodes[0].kind = "source.list".into();
    assert!(doc.graph.connect("source", "fields").is_err());
}
