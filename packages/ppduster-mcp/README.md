# ppduster-mcp

`ppduster-mcp` is a local stdio MCP server for creating Scenario Flow project
files that open directly in `ppduster-ui`. It exposes the same typed block
catalog and validation used by ppduster itself.

`plan_scheme` uses the real executor in dry-run mode. `run_scheme` executes saved
scenarios only when the server is started with `--allow-apply`; shell, elevation,
and protected-folder approvals remain disabled. `create_scheme` only
creates a new YAML file below a configured output directory; it refuses path
traversal, symlink escapes, missing parent directories, non-YAML extensions,
and overwrites.

## Install

From the repository root:

```bash
cargo install --locked --path packages/ppduster-mcp
mkdir -p "$HOME/ppduster-projects"
```

Configure an MCP client to launch the binary over stdio. Use absolute paths in
client configuration:

```json
{
  "command": "/absolute/path/to/ppduster-mcp",
  "args": [
    "--output-dir",
    "/absolute/path/to/ppduster-projects"
  ]
}
```

The output directory must already exist. Nested `output_path` parent
directories must also exist.

## GitHub workflow

1. `list_github_repositories` loads an authoring preview from the existing GitHub CLI login.
2. `create_github_scheme` takes selected `repository_ids`, `destination_root`, and a new
   relative `output_path`. It uses the same recipe as the desktop composer.
3. `read_scheme` reopens the YAML, including diagnostics for unfinished drafts.
4. `plan_scheme` takes `path` and `scenario_id` and produces a real execution plan.
5. `run_scheme` with those same arguments performs authorized clone/fetch operations.

The saved file owns the selected repository values. Steps 3–5 do not require the
preview cache and never reload GitHub discovery, including after server restart.
Destinations are `<destination_root>/<owner>/<repository>`. Existing checkouts are
validated and fetched without changing HEAD or the working tree; new checkouts
are cloned and then fetched. This snapshot recipe currently supports public,
non-archived repositories with a default branch. Private metadata is not saved.

An invalid sibling draft is reported by `read_scheme` but does not block planning
or running another valid scenario. Invalid selected graphs still fail. File paths
are relative to the configured output directory; `create_scheme` also returns a
`relative_path` that can be passed directly to read/plan/run.

## Connect to Codex

Build into the repository's shared target directory and register the absolute binary:

```bash
CARGO_TARGET_DIR=target cargo build --locked --release --manifest-path packages/ppduster-mcp/Cargo.toml
mkdir -p output/mcp
codex mcp add ppduster -- "$PWD/target/release/ppduster-mcp" --output-dir "$PWD/output/mcp" --allow-apply
codex mcp get ppduster
```

The desktop MCP server list can restart the connection after configuration changes.
Omit `--allow-apply` for authoring and planning only.

## Tools

- `list_blocks` returns every supported action kind with its versioned input
  and output schemas plus stable bilingual `search_terms` used by catalog UIs.
  Optional `kind` and `category` filters keep the response small.
- `validate_scheme` builds the shared Rust project types, validates every
  `Step` and `Task`, and returns normalized project JSON plus a YAML preview.
- `create_scheme` performs the same validation and creates a `.yaml` or `.yml`
  file with no-clobber semantics. Publication is atomic on filesystems with
  hard-link support; a secure direct-create fallback keeps FAT/exFAT and
  restricted network filesystems usable. Existing files are never replaced.

A minimal scheme argument looks like this:

```json
{
  "scheme": {
    "id": "developer-workstation",
    "name": "Developer workstation",
    "description": "Local development scenarios.",
    "scenarios": [
      {
        "id": "prepare-workspace",
        "name": "Prepare workspace",
        "description": "Create and inspect the development workspace.",
        "platform": "any",
        "group_path": [
          { "id": "development", "name": "Development" }
        ],
        "steps": [
          {
            "id": "create-workspace",
            "name": "Create workspace",
            "type": "create-directory",
            "path": "$HOME/Developer"
          },
          {
            "id": "inspect-workspace",
            "name": "Inspect workspace",
            "type": "inspect-path",
            "path": "$HOME/Developer"
          }
        ]
      }
    ]
  },
  "output_path": "developer-workstation.ppduster.yaml"
}
```

A scenario accepts either `steps` or an explicit `workflow_graph` object (never both).
Explicit graphs use nodes shaped as `{ "kind": "action", "config": { "step": ..., "bindings": ... } }`;
control nodes such as `for-each` have their configuration and nested `body` under `config`.
The ordered block declarations are compiled at the MCP boundary into a
canonical `workflow_graph` v3. Its explicit edges preserve the declared order.
The generated canvas is a deterministic left-to-right layout; coordinates are
presentation metadata and are never treated as runtime control flow.

## Development

The MCP package has its own lockfile because the repository root is not a Cargo
workspace:

```bash
cargo test --locked --manifest-path packages/ppduster-mcp/Cargo.toml
cargo clippy --locked --manifest-path packages/ppduster-mcp/Cargo.toml --all-targets -- -D warnings
```

## Live protocol smoke test

`scripts/mcp-github-smoke.py` makes real GitHub requests through stdio MCP, saves
selection, restarts the server, reopens the file, plans, runs twice, and verifies
that a local uncommitted marker and HEAD survive the second run. Pass the exact
public repositories and a fresh destination explicitly; it never selects all repos.
The offline protocol tests cover graph save/reload, scoped planning, execution
opt-in, path confinement, and invalid-draft diagnostics without network access.
