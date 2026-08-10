# Контексты, правила и исполняемый граф ppduster: доказательный обзор

Дата среза: **2026-08-11**. Документ разделяет три разных уровня доказательности:

1. **ландшафтная выборка `n=100`** — проверено, что это 100 уникальных публичных GitHub-репозиториев, релевантных пяти заранее заданным стратам;
2. **углублённая матрица `n=12`** — по этим двенадцати системам дополнительно просмотрены первичные репозитории и официальная документация по контекстам, типам, expressions, управлению потоком, ошибкам, секретам и ограничениям;
3. **mapping на ppduster** — инженерные решения, выведенные из матрицы и текущего кода проекта.

Это **не** утверждение, что все 100 репозиториев изучены с одинаковой глубиной. Частоты возможностей ниже относятся только к явно обозначенной матрице `n=12`; список `n=100` показывает широту пространства решений и не используется как статистическая выборка экосистемы.

## 1. Методика и воспроизводимость

### 1.1. Дизайн выборки

Выборка целевая, стратифицированная, неслучайная: ровно пять страт по 20 репозиториев.

| Страта | Что включалось | Зачем это ppduster |
|---|---|---|
| A. Low-code и integration automation | визуальные конструкторы, node-based flows, интеграционные платформы | UX выбора контекста, структура блоков, вложенные циклы и ветви |
| B. Durable workflow engines | оркестраторы с replay, состояниями, DAG/structured workflow | исполняемый Graph IR, состояния, retry/error semantics |
| C. CI/CD и GitOps | workflow YAML, job/step contexts, матрицы, dependencies | сериализуемые правила, secrets, limits, совместимость YAML |
| D. Data/ML pipelines | dataflow DAG, typed outputs, dynamic mapping, reduce | provenance, schema, fan-out/fan-in, большие результаты |
| E. Expressions, rules и graph tooling | безопасные DSL, policy engines, редакторы графов | закрытый AST, type checking, bounded evaluation, UI graph model |

Критерии включения:

- публичный первичный репозиторий реализации, спецификации или основной библиотеки;
- прямая связь хотя бы с двумя из тем: dataflow/context, workflow graph, expressions/rules, fan-out/fan-in, schema/typing, orchestration;
- канонический GitHub URL после redirects отвечает HTTP `200` на дату среза;
- после канонизации URL не дублирует уже включённый репозиторий.

Порядок внутри страты — порядок инженерной релевантности для ppduster, а не рейтинг качества или популярности. Stars, forks и лицензии не использовались как веса.

### 1.2. Discovery и HTTP/API-проверка

Кандидаты собирались из официальных страниц проектов, их GitHub-организаций и перекрёстных ссылок официальной документации. Затем каждый root URL проверялся публичным HTTP GET с redirects:

```sh
curl -L -sS -o /dev/null -w '%{http_code} %{url_effective}\n' \
  'https://github.com/<OWNER>/<REPOSITORY>'
```

Канонические redirects были приняты в выборку. Например, старые адреса Cadence, Dagu и Serverless Workflow были заменены на текущие canonical roots. Два разных старых продукта, перенаправлявшихся в один `harness/harness`, были исключены как дубликаты и заменены GoCD и Jenkins X.

Аутентифицированный GitHub Search/GraphQL API не использовался: локальная GitHub CLI-сессия на момент исследования не имела рабочего токена. Поэтому документ не заявляет полноту, ranking by stars или анализ текущих GitHub metadata. Для воспроизведения проверки root URL достаточно публичного HTTP метода выше.

Проверка количества repository roots в этом документе:

```sh
rg -o 'https://github\.com/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+' \
  docs/context-rules-research.md | sort -u | wc -l
```

Ожидаемый результат: `100`. В каждой из пяти таблиц ниже ровно 20 строк.

### 1.3. Глубокая выборка `n=12`

Для матрицы взяты архитектурно разные системы: n8n, Node-RED, Activepieces, Windmill, Temporal, Argo Workflows, GitHub Actions, Kestra, Prefect, Dagster, Airflow и StackStorm/Orquesta. По ним использованы только первичные repository roots из списка и официальные docs. Матрица кодирует наблюдаемые механизмы; пустоты и неоднозначности не заполнялись предположениями.

## 2. Полный список: ровно 100 уникальных GitHub-репозиториев

### Страта A — low-code, visual workflow и integration automation (`1–20`)

| № | Репозиторий | Подкатегория |
|---:|---|---|
| 1 | [n8n-io/n8n](https://github.com/n8n-io/n8n) | visual integration workflows |
| 2 | [node-red/node-red](https://github.com/node-red/node-red) | message-flow editor/runtime |
| 3 | [activepieces/activepieces](https://github.com/activepieces/activepieces) | no-code automation |
| 4 | [Windmill-Labs/windmill](https://github.com/Windmill-Labs/windmill) | script/flow automation |
| 5 | [huginn/huginn](https://github.com/huginn/huginn) | agent-based automation |
| 6 | [kestra-io/kestra](https://github.com/kestra-io/kestra) | declarative flow platform |
| 7 | [StackStorm/st2](https://github.com/StackStorm/st2) | event-driven automation |
| 8 | [Automatisch/automatisch](https://github.com/Automatisch/automatisch) | integration automation |
| 9 | [PipedreamHQ/pipedream](https://github.com/PipedreamHQ/pipedream) | integration components/workflows |
| 10 | [triggerdotdev/trigger.dev](https://github.com/triggerdotdev/trigger.dev) | background jobs/workflows |
| 11 | [ToolJet/ToolJet](https://github.com/ToolJet/ToolJet) | low-code apps and workflows |
| 12 | [Budibase/budibase](https://github.com/Budibase/budibase) | low-code apps/automation |
| 13 | [appsmithorg/appsmith](https://github.com/appsmithorg/appsmith) | low-code actions/data bindings |
| 14 | [nocobase/nocobase](https://github.com/nocobase/nocobase) | plugin-based workflow UI |
| 15 | [directus/directus](https://github.com/directus/directus) | data platform with flows |
| 16 | [FlowiseAI/Flowise](https://github.com/FlowiseAI/Flowise) | visual AI flows |
| 17 | [langflow-ai/langflow](https://github.com/langflow-ai/langflow) | typed visual AI components |
| 18 | [bytedance/flowgram.ai](https://github.com/bytedance/flowgram.ai) | workflow editor framework |
| 19 | [TIBCOSoftware/flogo](https://github.com/TIBCOSoftware/flogo) | event-driven low-code engine |
| 20 | [lowdefy/lowdefy](https://github.com/lowdefy/lowdefy) | declarative low-code bindings |

### Страта B — durable workflow engines и general orchestrators (`21–40`)

| № | Репозиторий | Подкатегория |
|---:|---|---|
| 21 | [temporalio/temporal](https://github.com/temporalio/temporal) | durable workflow service |
| 22 | [cadence-workflow/cadence](https://github.com/cadence-workflow/cadence) | durable workflow service |
| 23 | [Netflix/conductor](https://github.com/Netflix/conductor) | microservice orchestration |
| 24 | [argoproj/argo-workflows](https://github.com/argoproj/argo-workflows) | Kubernetes workflows/DAGs |
| 25 | [camunda/camunda](https://github.com/camunda/camunda) | BPMN/process orchestration |
| 26 | [restatedev/restate](https://github.com/restatedev/restate) | durable execution |
| 27 | [inngest/inngest](https://github.com/inngest/inngest) | event-driven durable functions |
| 28 | [hatchet-dev/hatchet](https://github.com/hatchet-dev/hatchet) | distributed task orchestration |
| 29 | [dbos-inc/dbos-transact-py](https://github.com/dbos-inc/dbos-transact-py) | durable Python workflows |
| 30 | [microsoft/durabletask-dotnet](https://github.com/microsoft/durabletask-dotnet) | durable task framework |
| 31 | [apache/dolphinscheduler](https://github.com/apache/dolphinscheduler) | distributed workflow scheduler |
| 32 | [azkaban/azkaban](https://github.com/azkaban/azkaban) | batch workflow scheduler |
| 33 | [apache/oozie](https://github.com/apache/oozie) | Hadoop workflow scheduler |
| 34 | [dagucloud/dagu](https://github.com/dagucloud/dagu) | local YAML DAG runner |
| 35 | [open-workflow-specification/specification](https://github.com/open-workflow-specification/specification) | portable workflow specification |
| 36 | [automatiko-io/automatiko-engine](https://github.com/automatiko-io/automatiko-engine) | workflow automation engine |
| 37 | [argoproj/argo-events](https://github.com/argoproj/argo-events) | event dependency workflows |
| 38 | [knative/eventing](https://github.com/knative/eventing) | event graph/runtime |
| 39 | [tektoncd/pipeline](https://github.com/tektoncd/pipeline) | Kubernetes task pipelines |
| 40 | [brigadecore/brigade](https://github.com/brigadecore/brigade) | event-driven scripting pipelines |

### Страта C — CI/CD, build pipelines и GitOps (`41–60`)

| № | Репозиторий | Подкатегория |
|---:|---|---|
| 41 | [actions/runner](https://github.com/actions/runner) | GitHub Actions execution runtime |
| 42 | [actions/languageservices](https://github.com/actions/languageservices) | workflow schema/expression tooling |
| 43 | [jenkinsci/jenkins](https://github.com/jenkinsci/jenkins) | CI orchestration |
| 44 | [gitlabhq/gitlabhq](https://github.com/gitlabhq/gitlabhq) | CI/CD platform mirror |
| 45 | [concourse/concourse](https://github.com/concourse/concourse) | pipeline engine |
| 46 | [woodpecker-ci/woodpecker](https://github.com/woodpecker-ci/woodpecker) | container CI pipelines |
| 47 | [go-gitea/gitea](https://github.com/go-gitea/gitea) | Git service with Actions-compatible CI |
| 48 | [nektos/act](https://github.com/nektos/act) | local GitHub Actions runner |
| 49 | [dagger/dagger](https://github.com/dagger/dagger) | typed programmable pipelines |
| 50 | [earthly/earthly](https://github.com/earthly/earthly) | reproducible build pipelines |
| 51 | [gocd/gocd](https://github.com/gocd/gocd) | deployment pipelines |
| 52 | [jenkins-x/jx](https://github.com/jenkins-x/jx) | Kubernetes CI/CD/GitOps |
| 53 | [argoproj/argo-cd](https://github.com/argoproj/argo-cd) | declarative GitOps delivery |
| 54 | [fluxcd/flux2](https://github.com/fluxcd/flux2) | GitOps reconciliation graph |
| 55 | [spinnaker/spinnaker](https://github.com/spinnaker/spinnaker) | continuous delivery orchestration |
| 56 | [buildkite/agent](https://github.com/buildkite/agent) | pipeline agent/runtime |
| 57 | [travis-ci/travis-build](https://github.com/travis-ci/travis-build) | CI build compiler/runtime |
| 58 | [werf/werf](https://github.com/werf/werf) | CI/CD delivery tooling |
| 59 | [garden-io/garden](https://github.com/garden-io/garden) | development/deploy action graph |
| 60 | [screwdriver-cd/screwdriver](https://github.com/screwdriver-cd/screwdriver) | continuous delivery platform |

### Страта D — data engineering, scientific и ML pipelines (`61–80`)

| № | Репозиторий | Подкатегория |
|---:|---|---|
| 61 | [apache/airflow](https://github.com/apache/airflow) | data workflow scheduler |
| 62 | [PrefectHQ/prefect](https://github.com/PrefectHQ/prefect) | Python workflow orchestration |
| 63 | [dagster-io/dagster](https://github.com/dagster-io/dagster) | typed data orchestration |
| 64 | [spotify/luigi](https://github.com/spotify/luigi) | Python dependency pipelines |
| 65 | [mage-ai/mage-ai](https://github.com/mage-ai/mage-ai) | visual data pipelines |
| 66 | [kubeflow/pipelines](https://github.com/kubeflow/pipelines) | ML pipeline IR/runtime |
| 67 | [apache/beam](https://github.com/apache/beam) | portable dataflow model |
| 68 | [apache/nifi](https://github.com/apache/nifi) | visual dataflow |
| 69 | [apache/hop](https://github.com/apache/hop) | visual data integration workflows |
| 70 | [kedro-org/kedro](https://github.com/kedro-org/kedro) | data pipeline framework |
| 71 | [nextflow-io/nextflow](https://github.com/nextflow-io/nextflow) | scientific dataflow workflows |
| 72 | [snakemake/snakemake](https://github.com/snakemake/snakemake) | scientific workflow engine |
| 73 | [dbt-labs/dbt-core](https://github.com/dbt-labs/dbt-core) | SQL model dependency graph |
| 74 | [meltano/meltano](https://github.com/meltano/meltano) | data integration orchestration |
| 75 | [airbytehq/airbyte](https://github.com/airbytehq/airbyte) | connector/data sync workflows |
| 76 | [apache/seatunnel](https://github.com/apache/seatunnel) | data integration engine |
| 77 | [apache/flink](https://github.com/apache/flink) | streaming/batch dataflow runtime |
| 78 | [Netflix/metaflow](https://github.com/Netflix/metaflow) | data science workflows |
| 79 | [zenml-io/zenml](https://github.com/zenml-io/zenml) | ML pipeline orchestration |
| 80 | [mlflow/mlflow](https://github.com/mlflow/mlflow) | ML workflows and model lifecycle |

### Страта E — expression/rule engines и graph tooling (`81–100`)

| № | Репозиторий | Подкатегория |
|---:|---|---|
| 81 | [cel-expr/cel-spec](https://github.com/cel-expr/cel-spec) | safe expression language specification |
| 82 | [cel-expr/cel-go](https://github.com/cel-expr/cel-go) | CEL Go checker/evaluator |
| 83 | [cel-expr/cel-java](https://github.com/cel-expr/cel-java) | CEL Java checker/evaluator |
| 84 | [microsoft/Power-Fx](https://github.com/microsoft/Power-Fx) | low-code formula language |
| 85 | [expr-lang/expr](https://github.com/expr-lang/expr) | typed Go expression engine |
| 86 | [jmespath/jmespath.py](https://github.com/jmespath/jmespath.py) | JSON query expressions, Python |
| 87 | [jmespath/jmespath.js](https://github.com/jmespath/jmespath.js) | JSON query expressions, JavaScript |
| 88 | [jsonata-js/jsonata](https://github.com/jsonata-js/jsonata) | JSON query/transformation language |
| 89 | [JSONPath-Plus/JSONPath](https://github.com/JSONPath-Plus/JSONPath) | structural JSON path/query |
| 90 | [open-policy-agent/opa](https://github.com/open-policy-agent/opa) | policy engine/Rego |
| 91 | [open-policy-agent/conftest](https://github.com/open-policy-agent/conftest) | policy checks over structured config |
| 92 | [hashicorp/go-bexpr](https://github.com/hashicorp/go-bexpr) | bounded Boolean expressions |
| 93 | [google/starlark-go](https://github.com/google/starlark-go) | deterministic embedded language |
| 94 | [bazelbuild/starlark](https://github.com/bazelbuild/starlark) | Starlark specification/design |
| 95 | [rhaiscript/rhai](https://github.com/rhaiscript/rhai) | embeddable scripting engine |
| 96 | [gorules/zen](https://github.com/gorules/zen) | rules/decision engine |
| 97 | [CacheControl/json-rules-engine](https://github.com/CacheControl/json-rules-engine) | JSON rules AST |
| 98 | [microsoft/RulesEngine](https://github.com/microsoft/RulesEngine) | JSON-configured rules engine |
| 99 | [bpmn-io/bpmn-js](https://github.com/bpmn-io/bpmn-js) | process graph modeler |
| 100 | [xyflow/xyflow](https://github.com/xyflow/xyflow) | node/edge editor toolkit |

## 3. Глубокая матрица `n=12`: dataflow, типы и control flow

Обозначения класса типизации:

- **D** — основной data plane динамический; schema/metadata может помогать редактору, но не доказывает форму каждого output;
- **S** — декларативный контракт вокруг динамических runtime-значений;
- **H** — типы и control flow в основном задаёт host language/SDK.

| Система | Context/dataflow | Типизация | Expressions/rules | Loop/fan-out | Branch/join |
|---|---|---|---|---|---|
| [n8n](https://github.com/n8n-io/n8n) | Узлы передают массивы items; ссылки идут на output узла, а `pairedItem` сохраняет происхождение элемента | **D**: JSON items; node property metadata и sample data помогают UI | `{{ ... }}` и expression editor, JS-like операции | Большинство узлов автоматически обрабатывает каждый item; есть Loop/feedback patterns | IF/Switch и Merge; после разветвления item linking критичен |
| [Node-RED](https://github.com/node-red/node-red) | Mutable JS-object `msg`, плюс node/flow/global context; `msg.parts` несёт sequence provenance | **D**: произвольные JS values | JSONata в Change/Switch и JavaScript в Function | Split превращает array/object в sequence | Switch маршрутизирует, Join собирает sequence; feedback wire создаёт цикл |
| [Activepieces](https://github.com/activepieces/activepieces) | Дочерние шаги видят outputs родителей через `{{step_slug.path}}`; checkpoint хранит вложенные loop/router runs | **S**: TypeScript Property schema для inputs, output остаётся JSON/sample-driven | Структурные router conditions плюс template refs | `LOOP_ON_ITEMS` содержит дочерние шаги | Router содержит ordered branches и fallback; выход из контейнера — структурная конвергенция |
| [Windmill](https://github.com/Windmill-Labs/windmill) | `flow_input`, `previous_result`, `results.<id>`, flow env и loop item/index | **S**: JSON schema выводится из script signatures; resource types отдельны | Bounded flow expressions исполняются QuickJS/Deno; predicates — Boolean JS expressions | For-loop владеет вложенным flow, имеет sequential/parallel и parallelism | Branch-one выбирает первую truthy ветвь; branch-all запускает несколько и ждёт их результаты |
| [Temporal](https://github.com/temporalio/temporal) | Значения передаются как args/returns/futures; durable Event History обеспечивает replay | **H**: SDK types и payload converters | Условия и вычисления — детерминированный host code | Обычные циклы host language; Activity/Child Workflow futures | `if/switch` и ожидание futures/promises в workflow code, без отдельного визуального Join node |
| [Argo Workflows](https://github.com/argoproj/argo-workflows) | Parameters/artifacts, task outputs и workflow variables | **S**: Kubernetes CRD структурирован; `Parameter` преимущественно string, `withItems` schemaless | simple tags и expression tags; `when`/`depends` | `withItems`, `withParam`, `withSequence` создают expanded nodes | Steps/DAG dependencies; fan-in задаётся dependencies/enhanced depends |
| [GitHub Actions](https://github.com/actions/runner), [language services](https://github.com/actions/languageservices) | Именованные namespaces `github`, `steps`, `needs`, `matrix`, `inputs`, `vars`, `secrets` | **S**: workflow schema и documented context types; expression values динамические | Закрытый `${{ ... }}` DSL с операторами/functions и ограничениями доступности contexts | Job matrix создаёт fan-out; `max-parallel`/`fail-fast` управляют исполнением | `if` выбирает шаг/job; `needs` формирует dependency fan-in |
| [Kestra](https://github.com/kestra-io/kestra) | `inputs`, `vars`, `outputs.<task>`, execution/taskrun context; loop outputs индексируются iteration key | **S**: plugin/task properties и outputs имеют machine-readable contracts | Pebble expressions, functions, `??`, `if`, comparisons | `ForEach` содержит tasks; iteration value доступно через `taskrun.value` | If/Switch/Parallel — структурные flowable tasks; sibling output visibility зависит от scope |
| [Prefect](https://github.com/PrefectHQ/prefect) | Python return values и `PrefectFuture`; data dependency выводится из передачи future downstream | **H**: annotations/Pydantic валидируют flow inputs, результаты — произвольные serializable Python values | Host Python | `.map()` и `.submit()` создают task runs/futures | Host `if/loop`; явный `wait_for` и future resolution формируют fan-in |
| [Dagster](https://github.com/dagster-io/dagster) | Op outputs/assets формируют edges; IO managers материализуют значения между steps | **H**: Python annotations и Dagster types на inputs/outputs | Host Python плюс typed config | `DynamicOut.map(...).collect()` — явный dynamic map/reduce | Graph edges и `collect` задают convergence; условие обычно выражается host code/optional output |
| [Airflow](https://github.com/apache/airflow) | XCom адресуется `dag_id/task_id/key`; TaskFlow автоматически передаёт return values | **D**: XCom — arbitrary serializable value; operator fields/templates дают metadata | Python + Jinja templates | Dynamic Task Mapping расширяет task по list/dict/XCom | Branch operators выбирают пути; trigger rules задают семантику fan-in/skipped/upstream_failed |
| [StackStorm](https://github.com/StackStorm/st2) | Context dictionary: input → vars → task publish → output; branches получают локальные копии | **S**: workflow/action JSON Schema, context runtime-dynamic | YAQL и Jinja инспектируются до исполнения | `with items` с ограничением concurrency | Transitions образуют graph; `join` сливает ветви и их contexts |

Первичные материалы к таблице:

- n8n: [UI data mapping](https://docs.n8n.io/build/work-with-data/reference-data/use-the-ui-mapper), [item processing and loops](https://docs.n8n.io/build/flow-logic/loop), [credentials](https://docs.n8n.io/integrations/builtin/credentials).
- Node-RED: [messages, sequences, Split/Join](https://nodered.org/docs/user-guide/messages), [context scopes/storage](https://nodered.org/docs/user-guide/context), [credential fields](https://nodered.org/docs/creating-nodes/credentials).
- Activepieces: [passing data](https://www.activepieces.com/docs/flows/passing-data), [durable execution/checkpoints](https://www.activepieces.com/docs/install/architecture/durable-execution), [limits](https://www.activepieces.com/docs/install/reference/limits).
- Windmill: [flow architecture/data exchange](https://www.windmill.dev/docs/flows/architecture), [for loops](https://www.windmill.dev/docs/flows/flow_loops), [branches](https://www.windmill.dev/docs/flows/flow_branches), [variables and secrets](https://www.windmill.dev/docs/core_concepts/variables_and_secrets).
- Temporal: [Workflow Definition](https://docs.temporal.io/workflow-definition), [Workflow Execution](https://docs.temporal.io/workflow-execution), [error handling](https://docs.temporal.io/develop/go/best-practices/error-handling), [payload encryption](https://docs.temporal.io/production-deployment/data-encryption).
- Argo Workflows: [field reference](https://argo-workflows.readthedocs.io/en/latest/fields/), [loops](https://argo-workflows.readthedocs.io/en/latest/walk-through/loops/), [conditionals](https://argo-workflows.readthedocs.io/en/latest/walk-through/conditionals/), [output parameters](https://argo-workflows.readthedocs.io/en/latest/walk-through/output-parameters/), [secrets](https://argo-workflows.readthedocs.io/en/latest/walk-through/secrets/).
- GitHub Actions: [contexts](https://docs.github.com/en/actions/reference/workflows-and-actions/contexts), [workflow syntax](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax), [matrix](https://docs.github.com/en/actions/how-tos/write-workflows/choose-what-workflows-do/run-job-variations), [secrets](https://docs.github.com/en/actions/reference/security/secrets).
- Kestra: [outputs and loop scoping](https://kestra.io/docs/workflow-components/outputs), [Pebble syntax](https://kestra.io/docs/expressions/syntax), [concurrency](https://kestra.io/docs/workflow-components/concurrency), [secrets](https://kestra.io/docs/concepts/secret).
- Prefect: [tasks, futures and mapping](https://docs.prefect.io/v3/concepts/tasks), [states](https://docs.prefect.io/v3/concepts/states), [typed blocks/secrets](https://docs.prefect.io/v3/concepts/blocks), [results](https://docs.prefect.io/v3/advanced/results).
- Dagster: [graphs](https://docs.dagster.io/guides/build/ops/graphs), [dynamic graphs](https://docs.dagster.io/guides/build/ops/dynamic-graphs), [Dagster types](https://docs.dagster.io/api/dagster/types), [environment variables and secrets](https://docs.dagster.io/guides/operate/configuration/using-environment-variables-and-secrets).
- Airflow: [architecture/data exchange](https://airflow.apache.org/docs/apache-airflow/stable/core-concepts/overview.html), [XCom](https://airflow.apache.org/docs/apache-airflow/stable/core-concepts/xcoms.html), [dynamic task mapping](https://airflow.apache.org/docs/apache-airflow/stable/authoring-and-scheduling/dynamic-task-mapping.html), [secrets backends](https://airflow.apache.org/docs/apache-airflow/stable/security/secrets/secrets-backend/index.html).
- StackStorm: [runtime context and branch merge](https://docs.stackstorm.com/orquesta/context.html), [workflow language, with-items, join and failure](https://docs.stackstorm.com/orquesta/languages/orquesta.html), [inspection](https://docs.stackstorm.com/orquesta/start.html), [encrypted datastore](https://docs.stackstorm.com/datastore.html).

## 4. Глубокая матрица `n=12`: missing/null/error, secrets и limits

| Система | Missing и null | Failure/error model | Secrets boundary | Limits/guardrails |
|---|---|---|---|---|
| n8n | `null` — JSON value; неправильная item provenance или отсутствующий paired item даёт mapping/linking errors | node error settings, retry/continue, error workflow | credentials отделены от item data и шифруются instance key | batching рекомендуется для больших наборов; универсальный переносимый loop cap не является контрактом workflow |
| Node-RED | JS различает `undefined` и `null`; Function, вернувшая `null`, не отправляет message | Catch/Status nodes и `node.error`; ошибка может идти отдельной wire | credential properties отделены от обычной config/message | runtime/context storage настраиваемы; Split/Join должны управлять размером sequences |
| Activepieces | path resolution зависит от реально сохранённого output; отсутствующий output ломает downstream binding | retry/continue-on-failure; checkpoint хранит status и error каждого nested step | connections/secret inputs не пишутся открыто в run checkpoint | опубликованы timeout, worker concurrency, file и run-log size limits; большие outputs offload-ятся |
| Windmill | JS semantics для `null`/`undefined`; flow inputs имеют required/nullable schema | retry, skip failure, error handlers; branch/loop status видим по iteration | encrypted variables, ACL, audit и log masking | loop parallelism задаётся явно; recursive variable resolution и payload paths ограничены |
| Temporal | nullability задаёт SDK type; отсутствующий history/event — не «пустая строка» | Activity Failure/Timeout/Cancellation и retry policy; replay требует deterministic code | нет универсального `secrets` graph namespace; секреты держат в worker/config, sensitive payload защищает codec | Workflow/Activity timeouts, retry policy и Event History задают операционные пределы |
| Argo | template/parameter reference должна разрешиться; parameters преимущественно строки, nullable semantics не унифицирована с artifacts | node phases, `continueOn`, retry strategy, exit handlers | Kubernetes Secret/secretKeyRef, не обычный workflow output | workflow/template `parallelism`, Kubernetes quotas, offloading node status |
| GitHub Actions | документированный nonexistent property вычисляется в empty string — опасное смешение missing и value | `success/failure/cancelled/always`, `continue-on-error`, job result через `needs` | отдельный `secrets` context, ограничения доступности и log redaction | matrix/concurrency/timeouts и platform Actions limits |
| Kestra | Pebble `??` задаёт fallback; required/nullable определяются plugin/input schema | task/execution states, retry, allow-failure/warning patterns | `secret()` и pluggable secret managers | flow concurrency и `ForEach.concurrencyLimit`; docs отдельно предупреждают о стоимости больших execution contexts |
| Prefect | Python `None`; отсутствие результата отличается от state без persisted result | rich states: Failed, Crashed, TimedOut, Cancelled; retries/hooks | Secret block/`SecretStr`, block schema шифрует и скрывает поле | task timeout, global/tag concurrency, result storage policies |
| Dagster | Python `None`/Optional и Dagster type checks; отсутствие dynamic output меняет graph expansion | Failure/RetryRequested, run/step events | `EnvVar`/resources и внешний secret manager, не общий output namespace | executor/run queue concurrency и resource limits |
| Airflow | XCom может содержать `None`; task states различают skipped, failed, upstream_failed | retries/callbacks; trigger rules определяют допустимые upstream states | Connections/Variables через secrets backend с определённым search order | Pools, max active tasks/runs, task mapping limits и timeouts |
| StackStorm | `null` можно задать явно; reference-before-assignment ловится inspection | Orquesta fail-fast, transition remediation, `continue/noop/fail`, retry | encrypted key-value datastore и secret action parameters | `with items.concurrency`, action/workflow policies; join ждёт установленное число branches |

### 4.1. Количественные результаты — только для `n=12`

- **12/12** имеют явный механизм передачи run-local данных downstream: message/items, именованные context namespaces, task outputs/futures или branch-local context.
- **9/12** используют отдельный expression/template DSL в декларативном слое: n8n, Node-RED, Activepieces, Windmill, Argo, GitHub Actions, Kestra, Airflow и StackStorm. Temporal, Prefect и Dagster в основном используют host language.
- По основному data plane матрица делится на **3 D / 6 S / 3 H**: три преимущественно динамических payload-системы, шесть декларативно-схемных и три host-typed.
- **11/12** имеют platform-level fan-out/mapping primitive; исключение в этой классификации — Temporal, где обычный цикл является детерминированным workflow code, хотя runtime сохраняет его durable semantics.
- **9/12** имеют именованный или структурно выделенный fan-in/barrier: Merge, Join, branch-all completion, DAG dependencies, `needs`, nested flow completion, `collect` либо trigger rules. В Activepieces конвергенция неявна в Router container, а Temporal и Prefect используют ожидание host-language futures.
- **10/12** отделяют secret storage/reference от обычного output context. Temporal и Dagster обычно оставляют хранилище секретов внешнему worker/resource/config layer, поэтому не кодировались как самостоятельный workflow secret namespace.
- **12/12** различают успешное и ошибочное terminal state; конкретные missing/null semantics при этом несовместимы между системами. GitHub Actions, например, превращает missing property в empty string, тогда как StackStorm может отклонить reference-before-assignment ещё при inspection.

Эти числа нельзя экстраполировать на все 100 проектов: глубокие 12 выбраны для максимального архитектурного разнообразия, а не случайно.

## 5. Наблюдаемые устойчивые паттерны

### 5.1. Context — не один глобальный JSON

Сильные системы различают несколько пространств: scenario/workflow input, output конкретного producer, loop item/index, run metadata, variables/resources и secrets. Даже динамические n8n и Node-RED добавляют provenance (`pairedItem`, `msg.parts`), потому что один JSON path недостаточен после fan-out и join.

Следствие: строка `github.repositories[0].https_url` без producer identity и scope недостаточна. Она ломается при rename, nested loop, нескольких GitHub blocks и branch convergence.

### 5.2. Schema полезна даже при динамическом runtime

Ни одна из матричных категорий не требует «всё должно быть статически типизировано». Практичный компромисс повторяется: inputs имеют контракт, outputs имеют declared schema или sample/inferred shape, runtime сохраняет реальное JSON value, а UI показывает совместимые поля.

Ключевой дополнительный слой — semantic format. `string(url)`, `string(git-url)`, `string(path)` имеют одинаковое JSON-представление, но разную пригодность для входов. Именно format позволяет корректно предложить `https_url` для `repo`, а не любой string.

### 5.3. Expressions должны быть закрыты и ограничены

В декларативных системах правила редко исполняются как произвольный shell. Используются AST/DSL с известными operators/functions, контролем доступных contexts и limits. CEL особенно полезен как ориентир: parse/check/evaluate разделены, типы известны до исполнения, evaluator не получает ambient I/O.

Произвольный JavaScript удобен, но переносит в condition evaluator проблемы capability security, nondeterminism, unbounded CPU/memory и трудной диагностики. Для ppduster безопаснее закрытый сериализованный AST; textual syntax, если появится, должна лишь компилироваться в тот же AST.

### 5.4. Loop — scope и nested graph, а не «специальный clone action»

Повторяемая модель: fan-out владеет body/subflow, создаёт item/index scope и агрегирует статусы/результаты на своей границе. Это позволяет поместить внутрь любой action, condition или nested loop. Специализированный `ForEachGitCloneIfMissing` годится как v1 compatibility adapter, но не как конечный IR.

### 5.5. Branch merge обязан иметь определённую семантику

Простого визуального схождения линий недостаточно. Необходимо определить:

- какие upstream states удовлетворяют Join;
- что происходит с невыбранной ветвью;
- как объединяются contexts;
- кто выигрывает при одинаковых keys;
- видимы ли branch-local values downstream.

StackStorm показывает риск last-writer-wins при merge одинаковых names. Более безопасное правило для ppduster: Join сначала control-only; branch-local data не протекает наружу без явного typed export/collect/merge.

### 5.6. Missing, null, error и skipped — четыре разные вещи

Смешение missing с empty string удобно для короткого YAML, но опасно для destructive automation. Для ppduster безопаснее:

- `missing` — путь/producer не дал значение;
- `null` — значение присутствует и явно равно null;
- `error` — expression/action не может вычислиться;
- `skipped` — node намеренно не исполнялся из-за control flow.

Ни одно из них не должно неявно превращаться в `false` или `""` без явно выбранной policy.

### 5.7. Secrets — taint, а не просто скрытый input

Разделение secret storage недостаточно: derived template из secret также секретен. Нужна propagation sensitivity, запрет secret → public field/output, redaction в plan/report/log и provenance без plaintext. Это прямо согласуется с Windmill, GitHub Actions, Prefect и StackStorm.

### 5.8. Limits — часть wire contract

Fan-out, nested graph, regex и context output создают DoS/операционные риски. Limits должны быть воспроизводимыми: глубина/число AST nodes, regex size, path segments, template parts/rendered bytes, loop iterations, expanded node instances, concurrency и retained output bytes.

## 6. Выбранные решения для ppduster

Ниже нормативные решения, а не утверждения о поведении внешних систем.

### 6.1. Единый versioned schema registry

Каждый block kind должен иметь один authoritative `BlockDefinition`:

```text
BlockDefinition {
  action_kind,
  schema_version,
  input_schema,
  output_schema,
  read_only,
  may_use_secrets
}
```

Registry одновременно используют loader/validator, runner, UI context picker и отчёты. Отдельные hardcoded UI-списки полей запрещены. Schema включает:

- primitive/object/array;
- required отдельно от nullable;
- `additional_fields` policy;
- semantic formats (`git-url`, `url`, `path`, `directory-path`, `git-ref`, `secret-ref`);
- sensitivity (`public`, `internal`, `confidential`, `secret`);
- стабильный versioned schema ID.

Runtime output обязан валидироваться против declared output schema до публикации в context store.

### 6.2. Structural `FieldRef` и `Binding`

Ссылка хранит producer и структурный path, а не строковый шаблон:

```text
FieldRef {
  scope: scenario | step(step_id) | loop-item(step_id),
  segments: [field(name) | index(n)]
}

Binding =
  literal(JSON)
  | field(FieldRef)
  | interpolated([literal | field])
```

Binding target — JSON Pointer/structural input path consumer block. Проверка до выполнения должна доказать:

- producer/schema существует;
- producer видим в scope и доминирует consumer;
- source type assignable target type;
- nullable/required совместимы;
- semantic format совместим;
- secret taint разрешён target field;
- template состоит только из bounded scalar parts.

Legacy `{{repository.https_url}}` читается только schema-v1 migration layer и преобразуется в structural parts; новые v2 файлы его не создают.

### 6.3. Closed AST с CEL-like semantics

Минимальный AST:

```text
literal, ref,
all, any, not,
exists, is-null, is-empty,
compare(eq/ne/lt/le/gt/ge),
contains, starts-with, ends-with, matches,
in,
quantifier(any/all/none, collection, local, predicate)
```

Семантика:

- root condition обязан иметь Boolean type;
- отсутствует truthiness строк/чисел/collections;
- строки не приводятся к числам и Boolean;
- `and/or` short-circuit;
- missing, null, unknown и evaluation error различаются;
- policy `on_missing/on_null/on_unknown` задаётся явно, безопасный default — fail;
- regex компилируется с size/complexity limits;
- evaluator не имеет filesystem, network, shell, clock или environment capabilities;
- local binding quantifier лексически ограничен predicate;
- secret values нельзя вывести в diagnostics.

Текстовый CEL-like editor может появиться позднее, но сериализуемым source of truth остаётся AST.

### 6.4. Исполняемый Graph IR и dominance

Schema v2 хранит layout-free graph отдельно от canvas:

```text
WorkflowGraph { version, entries, exits, nodes, edges }

GraphNode = Action | ForEach | If | Switch | Join
```

- `Action` содержит обычный `Step` и map typed bindings.
- `ForEach` содержит `collection`, item/index aliases, concurrency, error policy и nested body graph.
- `If` содержит typed Boolean rule и nested then/else graphs.
- `Switch` содержит selector, ordered literal cases и default graph.
- `Join` задаёт `all`, `any` или `first-successful`; для первого executable milestone достаточно `all`.
- Произвольные back-edges запрещены; циклы структурируются только control node.

Для каждого graph scope строится synthetic entry и dominator set:

```text
dom(entry) = {entry}
dom(node) = {node} union intersection(dom(predecessor))
```

Step output можно читать только если producer доминирует consumer. В nested body разрешены ancestor refs, если producer доминирует owning control node. Sibling/descendant refs запрещены. Loop item видим только внутри body и descendants. Branch-local output после Join невидим без explicit export/collect.

UI context picker должен получать уже отфильтрованный compiler view, а не просто «все блоки раньше по Vec index».

### 6.5. Error/control semantics

Порты actions: `success`, `failure`, `always`; у control nodes — `completed`, `failure`, а у ForEach дополнительно `empty`. Невыбранные branch nodes получают `skipped-by-control`, чтобы Join не ждал их бесконечно.

Первый GraphExecutor должен быть детерминированным и однопоточным. Parallel concurrency можно включать после появления:

- destination collision analysis;
- bounded scheduler;
- cancellation semantics;
- auth prompt coordination;
- deterministic reports;
- guarantees для non-idempotent actions.

### 6.6. Safe migration v1 → v2

Обязательные правила совместимости:

1. v1 `Task.steps` продолжает читаться и выполняться старым flat runner;
2. при явном upgrade обычные steps превращаются в Action nodes и связываются линейными success edges **по declaration order**;
3. `ComposerCanvas.parents` остаётся layout metadata и никогда автоматически не становится execution edge;
4. v1 `ForEach + ForEachGitCloneIfMissing` преобразуется в generic nested body только при доказуемо безопасной непосредственной паре; иначе migration возвращает diagnostic;
5. graph и `scenarios` нельзя смешивать, пока не реализован graph-aware template composition с prefix rewrite всех IDs/refs/edges;
6. schema v2 записывается только после успешной graph validation;
7. original v1 semantics и migration warnings доступны пользователю до сохранения.

Причина пункта 3 принципиальна: текущий canvas допускает несколько визуальных children у `start`, но runtime всё равно исполняет flat steps последовательно. Превращение `parents` в runtime graph без подтверждения изменит эффекты существующего сценария.

### 6.7. Минимальные limits v2

Значения должны быть централизованы и попадать в diagnostics; минимальный набор:

- максимальная глубина и число expression nodes;
- максимальная длина regex;
- максимальное число bindings на action;
- максимальная глубина FieldRef/JSON Pointer;
- максимальное число template parts и rendered bytes;
- максимальная глубина nested graphs;
- максимальное число статических graph nodes;
- максимальное число loop items и expanded node instances на run;
- concurrency limit с безопасным default `1`;
- максимальный размер inline context output; большие artifacts передаются ссылкой.

## 7. Что реально реализовано в текущей ветке

Срез сделан по незакоммиченному worktree ветки `codex/context-rules-v2` на 2026-08-11. Это moving target; раздел описывает только найденный код и не означает, что вся schema v2 уже доступна пользователю.

### Реализованные contracts и static/runtime primitives

- [`src/automation/context.rs`](../src/automation/context.rs): versioned `ContextType`, `ObjectSchema`, required/nullable, semantic formats, sensitivity, structural `FieldRef`, `Binding`, template parts, provenance и `ContextStore` с schema/value lookup.
- [`src/automation/block.rs`](../src/automation/block.rs): стабильный `ActionKind` и единый `BlockDefinition` registry для текущих actions с input/output schemas, read-only и secret-capability metadata.
- [`src/automation/binding.rs`](../src/automation/binding.rs): bounded resolver/materializer bindings, JSON Pointer targets, assignability/format checks, provenance aggregation, secret-flow rejection и limits.
- [`src/automation/expression.rs`](../src/automation/expression.rs): закрытый `ExpressionV1`, отдельный checker/evaluator, optional отдельно от nullable, missing/null/unknown diagnostics, bounded regex/quantifiers и отсутствие ambient capabilities.
- [`src/automation/task.rs`](../src/automation/task.rs): `StepCondition::Expression` с policy `on_null/on_missing/on_unknown`; `Task.graph: Option<WorkflowGraph>` и mutual-exclusion validation для `steps/scenarios/graph`.
- [`src/automation/graph.rs`](../src/automation/graph.rs): versioned `WorkflowGraph`, Action/ForEach/If/Switch/Join, nested scopes, typed edges/ports, bounded cycle/reachability/port checks, dominance-aware schema validation, static rule checking и conservative `from_linear_v1` migration.
- [`src/automation/runner.rs`](../src/automation/runner.rs): прежний linear executor сохранён отдельно; graph executor детерминированно исполняет bindings, Action, If, Switch, ForEach и Join, изолирует nested scopes, ограничивает expansion, делает глобальный policy preflight и fail-closed анализ dynamic mutation bindings, а также двухфазный loop preflight до первой мутации.
- [`src/bin/ppduster-ui.rs`](../src/bin/ppduster-ui.rs): schema registry используется для context picker и совместимых inputs; visual `when/require` editor поддерживает типизированные поля, `И/ИЛИ/НЕ`, сравнения, null/missing/unknown policies, empty и bounded regex.
- [`src/automation/loader.rs`](../src/automation/loader.rs): direct graph task сохраняется, а попытка вложить graph task в legacy scenario composition отклоняется вместо неявного flattening.
- [`tasks/github-account-clone-v2.yaml`](../tasks/github-account-clone-v2.yaml): исполняемый пример GitHub list → typed ForEach → guarded clone-if-missing с structural bindings.

### Ещё не реализовано или не подключено end-to-end

- Canvas по-прежнему редактирует плоский `Vec<Step>` и `ComposerCanvas.parents`; он не сериализует и не рисует `WorkflowGraph.edges` как source of execution truth.
- Context picker использует schema registry, но ещё не получает dominance-filtered scope от graph compiler.
- Graph executor намеренно однопоточный; `concurrency > 1` у ForEach пока отклоняется до появления collision analysis, cancellation и координации credential prompts.
- Graph-aware composition reusable scenarios, explicit branch exports/phi и collected typed outputs ForEach ещё не завершены.

Итого: **typed context, rules, static graph compiler и однопоточный executable graph работают end-to-end через YAML/CLI; следующий интеграционный слой — graph-native canvas и явные branch/loop exports**. Legacy tasks по-прежнему идут через прежний executor, а visual `parents` остаётся только layout metadata.

## 8. Проверки и acceptance criteria

Перед объединением schema v2 необходимы как минимум:

1. legacy YAML parse/roundtrip и runner equivalence;
2. доказательство, что visual `parents` не меняет v1 execution order;
3. schema registry completeness для каждого `ActionKind` и runtime output validation;
4. binding type/format/secret tests, включая nullable и missing source;
5. AST typecheck/evaluation tests для short-circuit, missing/null/unknown, regex и limits;
6. graph tests: duplicate IDs, invalid ports, cycles, unreachable nodes, dominance, sibling/descendant scope leaks;
7. generic ForEach с non-Git action, empty collection, nested loop и bounded expansion;
8. If/Switch only-selected-branch и Join без deadlock на skipped paths;
9. все item-derived mutating actions проходят preflight до первой мутации, когда их bindings разрешимы на входе цикла;
10. secret taint не попадает в plan/report/log/error;
11. v1 special foreach migration либо сохраняет порядок эффектов, либо отказывается с diagnostic;
12. UI rename/delete переписывает или блокирует все structural refs и edges;
13. report IDs включают stable node ID и iteration/scope path;
14. большие outputs переходят в artifact/reference, а не раздувают in-memory context.

## 9. Итог

Для ppduster не нужна ещё одна строковая подстановка поверх плоского списка steps. Нужна связанная система:

```text
Block schema registry
        ↓
typed ContextStore + provenance + sensitivity
        ↓
structural FieldRef / Binding
        ↓
closed checked Expression AST
        ↓
layout-free Graph IR + lexical scopes + dominance
        ↓
deterministic bounded executor and graph-aware UI
```

Выбранная архитектура совпадает с устойчивыми паттернами сильных аналогов, но сохраняет специфические требования ppduster: preflight перед мутациями, отсутствие произвольного condition code, строгая работа с секретами и безопасная миграция существующих flat scenarios.
