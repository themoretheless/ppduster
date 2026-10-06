//! Peregon-compatible data graphs for the shared ppduster workspace.
use anyhow::{bail, ensure, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DataDocument {
    pub format: String,
    pub version: u32,
    #[serde(rename = "savedAt")]
    pub saved_at: String,
    pub metadata: Value,
    pub view: Value,
    pub graph: DataGraph,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DataGraph {
    pub version: u32,
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub revision: u64,
    pub nodes: Vec<DataNode>,
    pub connections: Vec<DataConnection>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DataNode {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default = "node_version")]
    pub version: u32,
    pub position: DataPosition,
    pub config: Value,
}
fn node_version() -> u32 {
    1
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct DataPosition {
    pub x: f32,
    pub y: f32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DataEndpoint {
    #[serde(rename = "nodeId")]
    pub node_id: String,
    pub port: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DataConnection {
    pub id: String,
    pub from: DataEndpoint,
    pub to: DataEndpoint,
}
impl DataNode {
    pub fn title(&self) -> &str {
        self.config["title"].as_str().unwrap_or(&self.kind)
    }
    pub fn is_source(&self) -> bool {
        self.kind.starts_with("source.")
    }
    pub fn is_sink(&self) -> bool {
        self.kind.starts_with("sink.")
    }
    pub fn input_port(&self) -> &str {
        if self.kind == "transform.template"
            || self.kind == "sink.join"
            || self.kind == "sink.template"
        {
            "values"
        } else {
            "records"
        }
    }
    pub fn output_port(&self) -> &str {
        if self.is_sink() {
            "text"
        } else if self.kind == "source.list" || self.kind == "transform.template" {
            "values"
        } else if self.kind == "transform.filter" {
            "matched"
        } else {
            "records"
        }
    }
}
impl Default for DataDocument {
    fn default() -> Self {
        Self {
            format: "peregon-pipeline".into(),
            version: 2,
            saved_at: chrono::Utc::now().to_rfc3339(),
            metadata: json!({"name":"Новый поток данных"}),
            view: json!({"x":0,"y":0,"zoom":1}),
            graph: DataGraph {
                version: 2,
                id: "ppduster-data".into(),
                name: "Новый поток данных".into(),
                revision: 0,
                nodes: vec![
                    DataNode {
                        id: "source".into(),
                        kind: "source.json".into(),
                        version: 1,
                        position: DataPosition { x: 40., y: 80. },
                        config: json!({"title":"Исходные данные","text":"[{\"name\":\"Alice\",\"active\":true},{\"name\":\"Bob\",\"active\":false}]","arrayPath":""}),
                    },
                    DataNode {
                        id: "fields".into(),
                        kind: "transform.project".into(),
                        version: 1,
                        position: DataPosition { x: 310., y: 80. },
                        config: json!({"title":"Выбрать поля","fields":["name"]}),
                    },
                    DataNode {
                        id: "output".into(),
                        kind: "sink.csv".into(),
                        version: 1,
                        position: DataPosition { x: 580., y: 80. },
                        config: json!({"title":"Результат CSV","includeHeader":true,"delimiter":","}),
                    },
                ],
                connections: vec![
                    edge("source", "records", "fields", "records"),
                    edge("fields", "records", "output", "records"),
                ],
            },
        }
    }
}
pub fn edge(from: &str, output: &str, to: &str, input: &str) -> DataConnection {
    DataConnection {
        id: format!("{from}-{to}"),
        from: DataEndpoint {
            node_id: from.into(),
            port: output.into(),
        },
        to: DataEndpoint {
            node_id: to.into(),
            port: input.into(),
        },
    }
}
fn config_value(config: &Value, keys: &[&str], fallback: Value) -> Value {
    keys.iter()
        .find_map(|key| config.get(*key).filter(|v| !v.is_null()).cloned())
        .unwrap_or(fallback)
}
impl DataDocument {
    pub fn decode(source: &str) -> Result<Self> {
        ensure!(source.len() <= 10 * 1024 * 1024, "Файл превышает 10 МБ");
        let mut doc: Self = serde_json::from_str(source)?;
        if doc.graph.name.is_empty() {
            doc.graph.name = doc.metadata["name"]
                .as_str()
                .unwrap_or("Поток данных")
                .to_owned();
        }
        ensure!(
            doc.format == "peregon-pipeline" && doc.version == 2 && doc.graph.version == 2,
            "Неподдерживаемый формат потока данных"
        );
        doc.graph.validate_structure()?;
        Ok(doc)
    }
    pub fn encode(&self) -> Result<String> {
        self.graph.validate_structure()?;
        let mut doc = self.clone();
        doc.saved_at = chrono::Utc::now().to_rfc3339();
        doc.metadata["name"] = json!(doc.graph.name);
        Ok(serde_json::to_string_pretty(&doc)?)
    }
}
impl DataGraph {
    pub fn validate_structure(&self) -> Result<()> {
        ensure!(
            self.nodes.len() <= 1000 && self.connections.len() <= 5000,
            "Слишком большой граф"
        );
        let mut ids = BTreeSet::new();
        for node in &self.nodes {
            ensure!(
                !node.id.is_empty() && ids.insert(&node.id),
                "Повторный или пустой ID блока"
            );
            ensure!(
                node.version == 1 && node.config.is_object(),
                "Неподдерживаемая версия или настройки блока {}",
                node.id
            );
            ensure!(
                node.position.x.is_finite() && node.position.y.is_finite(),
                "Некорректная позиция блока"
            );
            ensure!(
                matches!(
                    node.kind.as_str(),
                    "source.json"
                        | "source.csv"
                        | "source.list"
                        | "transform.filter"
                        | "transform.project"
                        | "transform.template"
                        | "sink.flat"
                        | "sink.template"
                        | "sink.join"
                        | "sink.json"
                        | "sink.csv"
                        | "sink.xml"
                        | "sink.sql"
                ),
                "Неизвестный блок {}",
                node.kind
            );
        }
        let mut edge_ids = BTreeSet::new();
        let mut targets = BTreeSet::new();
        for connection in &self.connections {
            ensure!(
                edge_ids.insert(&connection.id) && targets.insert(&connection.to.node_id),
                "Повторная связь или несколько входов блока"
            );
            let from = self
                .nodes
                .iter()
                .find(|n| n.id == connection.from.node_id)
                .ok_or_else(|| anyhow::anyhow!("Источник связи отсутствует"))?;
            let to = self
                .nodes
                .iter()
                .find(|n| n.id == connection.to.node_id)
                .ok_or_else(|| anyhow::anyhow!("Получатель связи отсутствует"))?;
            ensure!(
                !from.is_sink() && !to.is_source(),
                "Недопустимое направление связи"
            );
            ensure!(
                connection.from.port == from.output_port() && connection.to.port == to.input_port(),
                "Недопустимые порты связи"
            );
            let output_kind = if from.output_port() == "matched" {
                "records"
            } else {
                from.output_port()
            };
            ensure!(
                output_kind == to.input_port(),
                "Нельзя соединить записи и список значений"
            );
        }
        let mut ready = BTreeSet::new();
        while ready.len() < self.nodes.len() {
            let previous = ready.len();
            for node in &self.nodes {
                if self
                    .connections
                    .iter()
                    .filter(|e| e.to.node_id == node.id)
                    .all(|e| ready.contains(&e.from.node_id))
                {
                    ready.insert(node.id.clone());
                }
            }
            ensure!(previous != ready.len(), "Граф содержит цикл");
        }
        Ok(())
    }
    pub fn compile(&self) -> Result<Value> {
        self.validate_structure()?;
        ensure!(!self.nodes.is_empty(), "Добавьте источник данных");
        let mut steps = vec![];
        let mut ready = BTreeSet::new();
        while ready.len() < self.nodes.len() {
            for node in &self.nodes {
                if ready.contains(&node.id) {
                    continue;
                }
                let incoming = self.connections.iter().find(|e| e.to.node_id == node.id);
                if incoming.is_some_and(|e| !ready.contains(&e.from.node_id)) {
                    continue;
                }
                let c = &node.config;
                let val = |keys: &[&str], fallback: Value| config_value(c, keys, fallback);
                let mut step = json!({"node_id":node.id});
                if node.is_source() {
                    step["node_type"] = json!("source");
                    step["config"] = json!({
                        "data":val(&["data","json","text"],json!("")),"format":node.kind.trim_start_matches("source."),
                        "path":val(&["path","arrayPath"],json!("")),"csv_delimiter":val(&["csv_delimiter","delimiter"],json!(","))
                    });
                } else {
                    let Some(incoming) = incoming else {
                        bail!("Подключите вход блока «{}»", node.title());
                    };
                    step["input"] =
                        json!({"node_id":incoming.from.node_id,"port":incoming.from.port});
                    if node.kind == "transform.filter" {
                        step["node_type"] = json!("filter");
                        step["config"] = json!({"filters":val(&["filters","conditions"],json!([])),"filter_mode":val(&["filter_mode","mode"],json!("all"))});
                        if let Some(expression) = c.get("expression").filter(|v| !v.is_null()) {
                            step["config"]["expression"] = expression.clone();
                        }
                    } else if node.kind == "transform.project" {
                        step["node_type"] = json!("project");
                        step["config"] =
                            json!({"fields":val(&["fields","selectedFields"],json!([]))});
                    } else {
                        let template = node.kind == "transform.template";
                        step["node_type"] = json!(if template { "template" } else { "sink" });
                        step["config"] = json!({
                            "format":val(&["format","outputFormat"],json!(if node.kind=="sink.join" {"flat"} else {node.kind.trim_start_matches("sink.")})),
                            "delimiter":val(&["delimiter"],json!(", ")),"skip_empty":val(&["skip_empty","skipEmpty"],json!(true)),"unique":val(&["unique"],json!(false)),
                            "fields":val(&["fields"],json!([])),"csv_delimiter":val(&["csv_delimiter","csvDelimiter","delimiter"],json!(",")),
                            "csv_include_header":val(&["csv_include_header","csvIncludeHeader","includeHeader"],json!(true)),"csv_quote_all":val(&["csv_quote_all","csvQuoteAll","quoteAll"],json!(false)),
                            "xml_root":val(&["xml_root","xmlRoot","root"],json!("rows")),"xml_row":val(&["xml_row","xmlRow","row"],json!("row")),"table_name":val(&["table_name","tableName","table"],json!("result")),
                            "value_template":val(&["value_template","valueTemplate","template"],json!("{value}")),"strip_outer_quotes":val(&["strip_outer_quotes","stripOuterQuotes"],json!(true))
                        });
                    }
                }
                ready.insert(node.id.clone());
                steps.push(step);
            }
        }
        Ok(json!({"action":"execute_plan","plan":{"version":1,"preview_limit":50,"steps":steps}}))
    }
    /// Change one input atomically: a rejected connection preserves existing wiring.
    pub fn connect(&mut self, from: &str, to: &str) -> Result<()> {
        let source = self
            .nodes
            .iter()
            .find(|n| n.id == from)
            .ok_or_else(|| anyhow::anyhow!("Блок отсутствует"))?;
        let target = self
            .nodes
            .iter()
            .find(|n| n.id == to)
            .ok_or_else(|| anyhow::anyhow!("Блок отсутствует"))?;
        let connection = edge(from, source.output_port(), to, target.input_port());
        let mut next = self.clone();
        next.connections.retain(|e| e.to.node_id != to);
        next.connections.push(connection);
        next.validate_structure()?;
        next.revision += 1;
        *self = next;
        Ok(())
    }
}
