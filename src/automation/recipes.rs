//! Shared GitHub scenario recipe for the desktop composer and MCP clients.
use super::*;
use std::collections::BTreeMap;

fn context_reference_expression(field: &FieldRef) -> ExpressionV1 {
    ExpressionV1::Ref {
        reference: ReferenceV1::Context {
            field: field.clone(),
        },
    }
}

fn github_background_clone_requirement(loop_id: &str) -> StepCondition {
    let equals_false = |field: &str| ExpressionV1::Compare {
        operator: ComparisonOperator::Equal,
        left: Box::new(context_reference_expression(
            &FieldRef::loop_item(loop_id).field(field),
        )),
        right: Box::new(ExpressionV1::Literal {
            value: ExpressionValue::Bool(false),
        }),
    };
    let branch = FieldRef::loop_item(loop_id).field("default_branch");
    StepCondition::Expression {
        rule: ExpressionV1::All {
            expressions: vec![
                equals_false("private"),
                equals_false("archived"),
                ExpressionV1::Exists {
                    reference: ReferenceV1::Context {
                        field: branch.clone(),
                    },
                },
                ExpressionV1::Not {
                    expression: Box::new(ExpressionV1::IsNull {
                        expression: Box::new(context_reference_expression(&branch)),
                    }),
                },
            ],
        },
        policy: RuleOutcomePolicy::default(),
    }
}

pub fn github_repository_task(
    ordinal: usize,
    selected_repositories: Vec<GithubRepositoryInput>,
    destination_root: &str,
) -> Result<Task, String> {
    let destination_root = destination_root.trim();
    if destination_root.is_empty()
        || std::path::Path::new(destination_root)
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err("укажите папку репозиториев без компонентов '..'".into());
    }
    let destination_prefix = format!("{}/", destination_root.trim_end_matches('/'));
    let mut select_step =
        default_step(ActionKind::GithubPreviewRepositories, "select-repositories")
            .expect("GitHub repository snapshot is a graph action");
    select_step.action = Action::GithubPreviewRepositories {
        selected_repositories,
    };
    let loop_id = "repositories";
    let mut clone_step = default_step(ActionKind::GitCloneIfMissing, "clone-repository")
        .expect("Git clone-if-missing is a graph action");
    clone_step.require = Some(github_background_clone_requirement(loop_id));

    let mut fetch_step = default_step(ActionKind::GitFetch, "fetch-repository")
        .expect("Git fetch is a graph action");
    if let Action::GitFetch { branch, .. } = &mut fetch_step.action {
        *branch = "*".into();
    }

    let graph = WorkflowGraph {
        entries: vec![select_step.id.clone()],
        variables: BTreeMap::from([(
            "selected_repositories".into(),
            ScenarioVariable::new(FieldRef::step("select-repositories").field("repositories")),
        )]),
        nodes: vec![
            GraphNode::Action(Box::new(ActionNode {
                step: select_step,
                bindings: BTreeMap::new(),
            })),
            GraphNode::ForEach(ForEachNode {
                id: loop_id.into(),
                collection: Binding::field(FieldRef::scenario().field("selected_repositories")),
                item_alias: "repository".into(),
                index_alias: Some("repository_index".into()),
                concurrency: 1,
                on_error: LoopFailurePolicy::Stop,
                body: Box::new(WorkflowGraph {
                    entries: vec!["clone-repository".into()],
                    nodes: vec![
                        GraphNode::Action(Box::new(ActionNode {
                            step: clone_step,
                            bindings: BTreeMap::from([
                                (
                                    "repo".into(),
                                    Binding::field(FieldRef::loop_item(loop_id).field("https_url")),
                                ),
                                (
                                    "dest".into(),
                                    Binding::interpolated([
                                        TemplatePart::literal(&destination_prefix),
                                        TemplatePart::field(
                                            FieldRef::loop_item(loop_id).field("full_name"),
                                        ),
                                    ]),
                                ),
                                (
                                    "branch".into(),
                                    Binding::field(
                                        FieldRef::loop_item(loop_id).field("default_branch"),
                                    ),
                                ),
                            ]),
                        })),
                        GraphNode::Action(Box::new(ActionNode {
                            step: fetch_step,
                            bindings: BTreeMap::from([
                                (
                                    "repo".into(),
                                    Binding::field(FieldRef::loop_item(loop_id).field("https_url")),
                                ),
                                (
                                    "dest".into(),
                                    Binding::interpolated([
                                        TemplatePart::literal(&destination_prefix),
                                        TemplatePart::field(
                                            FieldRef::loop_item(loop_id).field("full_name"),
                                        ),
                                    ]),
                                ),
                            ]),
                        })),
                    ],
                    edges: vec![GraphEdge::new(
                        "clone-repository",
                        EdgePort::Success,
                        "fetch-repository",
                    )],
                    ..WorkflowGraph::default()
                }),
            }),
        ],
        edges: vec![GraphEdge::new(
            "select-repositories",
            EdgePort::Success,
            "repositories",
        )],
        ..WorkflowGraph::default()
    };
    graph.validate().map_err(|errors| format!("{errors:?}"))?;
    let task = Task {
        id: format!("github-repositories-{ordinal}"),
        name: "GitHub: сохранить выбор, клонировать или fetch".into(),
        description: "Загрузить репозитории только в инспекторе настройки, сохранить выбранные публичные значения в блоке GitHub и при запуске без повторного discovery-запроса клонировать отсутствующие репозитории, а для существующих выполнить fetch всех веток без изменения рабочей копии. Папка по умолчанию: $HOME/Developer/<owner>/<repository>.".into(),
        platform: crate::rules::Platform::Macos,
        trust: TrustRequirement::ExternalAllowed,
        scenarios: Vec::new(),
        resolved_scenarios: Vec::new(),
        graph: Some(graph),
        steps: Vec::new(),
    };
    task.validate()?;
    Ok(task)
}
