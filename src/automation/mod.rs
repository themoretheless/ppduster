pub mod binding;
pub mod block;
pub mod context;
pub mod expression;
pub mod graph;
pub mod loader;
mod package_registry;
pub mod package_secrets;
pub mod runner;
pub mod task;

pub use binding::{
    materialize_step, parse_binding_target, resolve_binding, validate_literal_binding,
    BindingError, BindingLimits, ResolvedBinding,
};
pub use block::{
    block_definition, block_definitions, block_policy_capabilities, default_action, default_step,
    definition_for_action, ActionKind, BlockDefinition, BlockPolicyCapabilities, PolicyRequirement,
};
pub use context::{
    AdditionalProperties, Binding, ContextLookupError, ContextOrigin, ContextPathSegment,
    ContextProvenance, ContextScope, ContextStore, ContextType, ContextTypeName, ContextValue,
    FieldRef, FieldSchema, ObjectSchema, ResolvedContextValue, ResolvedSchema, ResolvedSchemaOwned,
    SchemaValidationError, SchemaValidationErrorKind, SemanticFormat, Sensitivity, TemplatePart,
    CONTEXT_SCHEMA_VERSION,
};
pub use expression::{
    check_rule, check_value_expression, CheckedExpressionV1, CollectionQuantifier,
    ComparisonOperator, EvaluationError, EvaluationErrorKind, EvaluationIssue, EvaluationValue,
    ExpressionDiagnostic, ExpressionDiagnosticCode, ExpressionEvaluation, ExpressionLimits,
    ExpressionSchemaResolver, ExpressionType, ExpressionTypeKind, ExpressionV1, ExpressionValue,
    ExpressionValueResolver, ReferenceV1, RuleEvaluation, RuleExprV1, SchemaResolutionError,
    ValueExprV1,
};
pub use graph::{
    ActionNode, EdgeEndpoint, EdgePort, ForEachNode, GraphEdge, GraphExit, GraphNode,
    GraphValidationError, GraphValidationErrorKind, IfNode, JoinMode, JoinNode,
    LinearMigrationError, LoopFailurePolicy, SwitchCase, SwitchNode, WorkflowGraph,
    WorkflowGraphMigrationError, MIN_MIGRATABLE_WORKFLOW_GRAPH_VERSION, WORKFLOW_GRAPH_VERSION,
};
pub use loader::{PackTrust, TaskPack, TaskSource};
pub use runner::{
    context_store_from_reports, describe_step, run_task, ActionOutcome, ActionPlan,
    AutomationError, GithubAccountOutput, GithubContextOutput, GithubRepositoriesOutput,
    GithubRepositoryOutput, PathMetadataOutput, ProcessExitOutput, RunOptions, RunReport,
    StepLogEntry, StepOutput, StepReport, StepStatus, StructuredStepOutput,
};
pub use task::{
    Action, ActivateLicenseAction, AppBundleIdentity, AppStoreInstallAction, AppStoreOperation,
    ArchiveFormat, AuthPolicy, BambuStudioReleaseAction, Check, Checksum, CopyPathAction,
    CreateDirectoryAction, ElevationPolicy, EncryptedSecretsSpec, IndeterminatePolicy,
    InspectPathAction, LicenseMethod, LicenseProvider, NpmRegistryFileSpec, NugetRegistryFileSpec,
    PathExpectation, PathKind, ReleaseChannel, RemovePathAction, RuleOutcomePolicy,
    ScriptInterpreter, ShellMode, Step, StepCondition, Task, TaskFile, TaskMigrationError,
    TrustRequirement, WriteConflictPolicy, WriteFileAction, TASK_FORMAT_VERSION,
};
