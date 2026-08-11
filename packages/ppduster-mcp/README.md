# ppduster-mcp

`ppduster-mcp` is a local stdio MCP server for creating Scenario Flow project
files that open directly in `ppduster-ui`. It exposes the same typed block
catalog and validation used by ppduster itself.

The server does not execute, plan, or apply scenarios. `create_scheme` only
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

## Tools

- `list_blocks` returns every supported action kind with its versioned input
  and output schemas. Optional `kind` and `category` filters keep the response
  small.
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

Scenario steps execute in array order. The generated canvas is a deterministic
left-to-right chain matching that order. Canvas links and coordinates remain
presentation metadata and are never treated as runtime control flow.

## Development

The MCP package has its own lockfile because the repository root is not a Cargo
workspace:

```bash
cargo test --locked --manifest-path packages/ppduster-mcp/Cargo.toml
cargo clippy --locked --manifest-path packages/ppduster-mcp/Cargo.toml --all-targets -- -D warnings
```
