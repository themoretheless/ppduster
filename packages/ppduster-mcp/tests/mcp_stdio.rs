use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use tempfile::TempDir;

const MODERN_PROTOCOL: &str = "2026-07-28";

struct McpProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    finished: bool,
}

impl McpProcess {
    fn start(output_dir: &TempDir) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_ppduster-mcp"))
            .arg("--output-dir")
            .arg(output_dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin: Some(stdin),
            stdout,
            finished: false,
        }
    }

    fn request(&mut self, request: Value) -> Value {
        let stdin = self.stdin.as_mut().unwrap();
        serde_json::to_writer(&mut *stdin, &request).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();

        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        assert!(
            !line.is_empty(),
            "MCP server closed stdout without a response"
        );
        serde_json::from_str(&line).unwrap()
    }

    fn notify(&mut self, notification: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        serde_json::to_writer(&mut *stdin, &notification).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    fn shutdown(mut self) {
        self.stdin.take();
        let status = self.child.wait().unwrap();
        self.finished = true;
        assert!(status.success(), "MCP server exited with {status}");
    }
}

impl Drop for McpProcess {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.stdin.take();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn request_meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": MODERN_PROTOCOL,
        "io.modelcontextprotocol/clientInfo": {
            "name": "ppduster-mcp-tests",
            "version": "0.1.0"
        },
        "io.modelcontextprotocol/clientCapabilities": {}
    })
}

fn scheme() -> Value {
    json!({
        "id": "mcp-generated",
        "name": "MCP generated project",
        "description": "Protocol integration fixture.",
        "scenarios": [{
            "id": "create-workspace",
            "name": "Create workspace",
            "description": "Create a workspace directory after plan review.",
            "platform": "any",
            "steps": [{
                "id": "create-directory",
                "name": "Create workspace directory",
                "type": "create-directory",
                "path": "$HOME/Developer"
            }]
        }]
    })
}

#[test]
fn modern_stdio_discovery_lists_tools_and_creates_a_scheme() {
    let output = TempDir::new().unwrap();
    let mut server = McpProcess::start(&output);

    let discovery = server.request(json!({
        "jsonrpc": "2.0",
        "id": "discover",
        "method": "server/discover",
        "params": { "_meta": request_meta() }
    }));
    assert_eq!(discovery["result"]["resultType"], "complete");
    assert!(discovery["result"]["supportedVersions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|version| version == MODERN_PROTOCOL));
    assert!(discovery["result"]["supportedVersions"]
        .as_array()
        .unwrap()
        .iter()
        .all(|version| version != "2025-03-26"));
    assert!(discovery["result"]["capabilities"]["tools"].is_object());

    let tools = server.request(json!({
        "jsonrpc": "2.0",
        "id": "tools",
        "method": "tools/list",
        "params": { "_meta": request_meta() }
    }));
    let names = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, ["create_scheme", "list_blocks", "validate_scheme"]);
    let validate_tool = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "validate_scheme")
        .unwrap();
    let steps_schema = find_property_schema(&validate_tool["inputSchema"], "steps").unwrap();
    assert_eq!(steps_schema["items"]["type"], "object");
    let required = steps_schema["items"]["required"].as_array().unwrap();
    assert!(required.iter().any(|field| field == "id"));
    assert!(required.iter().any(|field| field == "name"));
    assert!(required.iter().any(|field| field == "type"));

    let validation = server.request(json!({
        "jsonrpc": "2.0",
        "id": "validate",
        "method": "tools/call",
        "params": {
            "name": "validate_scheme",
            "arguments": { "scheme": scheme() },
            "_meta": request_meta()
        }
    }));
    assert_eq!(validation["result"]["resultType"], "complete");
    assert_eq!(validation["result"]["structuredContent"]["valid"], true);
    assert_ne!(validation["result"]["isError"], true);

    let creation = server.request(json!({
        "jsonrpc": "2.0",
        "id": "create",
        "method": "tools/call",
        "params": {
            "name": "create_scheme",
            "arguments": {
                "scheme": scheme(),
                "output_path": "from-mcp.ppduster.yaml"
            },
            "_meta": request_meta()
        }
    }));
    assert_eq!(creation["result"]["resultType"], "complete");
    assert_eq!(creation["result"]["structuredContent"]["created"], true);
    assert!(output.path().join("from-mcp.ppduster.yaml").is_file());

    server.shutdown();
}

fn find_property_schema<'a>(value: &'a Value, property: &str) -> Option<&'a Value> {
    if let Some(schema) = value
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|properties| properties.get(property))
    {
        return Some(schema);
    }
    match value {
        Value::Array(values) => values
            .iter()
            .find_map(|value| find_property_schema(value, property)),
        Value::Object(values) => values
            .values()
            .find_map(|value| find_property_schema(value, property)),
        _ => None,
    }
}

#[test]
fn legacy_initialize_clients_can_list_and_call_tools() {
    let output = TempDir::new().unwrap();
    let mut server = McpProcess::start(&output);

    let initialization = server.request(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {
                "name": "ppduster-mcp-legacy-tests",
                "version": "0.1.0"
            }
        }
    }));
    assert_eq!(initialization["result"]["protocolVersion"], "2025-11-25");
    assert!(initialization["result"]["capabilities"]["tools"].is_object());
    server.notify(json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    }));

    let tools = server.request(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    }));
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 3);

    let catalog = server.request(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "list_blocks",
            "arguments": {
                "kind": "create-directory"
            }
        }
    }));
    assert_ne!(catalog["result"]["isError"], true);
    assert_eq!(
        catalog["result"]["structuredContent"]["blocks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    server.shutdown();
}
