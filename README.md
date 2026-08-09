# ppduster

Safe, rule-driven junk cleaner for **macOS**, **Linux**, and **Windows**.

Removes regenerable clutter: caches, logs, temp files, app leftovers, and package-manager caches. Defaults are conservative (dry-run, age filters, Trash, never-touch paths). No scareware, no fake “your Mac is 90% dirty” scores.

Inspired by product lessons from **CleanMyMac**, **CCleaner**, **BleachBit**, **Stacer**, OS built-in storage tools, and patterns from ~100 open-source cleaners / disk tools (duplicate finders, trash CLIs, dev cache cleaners, etc.).

## Features

| Area | Behavior |
|------|----------|
| Rule packs | YAML cleaners (BleachBit-style data, not hard-coded paths only) |
| Scan first | `scan` never deletes |
| Dry-run clean | `clean` without `--yes` only prints |
| Trash default | Moves to Trash / Recycle Bin (`trash` crate); `--permanent` needs typed confirm |
| Age filters | Per-rule `min_age_days` (override with `--min-age`) |
| Safety rails | Never-touch list (Documents, Desktop, `.ssh`, Keychains, `/System`, …), no symlink follow |
| Report-only | High-risk areas (Prefetch, Windows.old, Xcode Archives, pnpm store) list but never delete |
| Output | Human tables or `--output json` for scripting |
| Categories | `caches`, `logs`, `temp`, `leftovers`, `dev`, `package-cache`, `app-cache`, `browser-cache`, … |

## Top 10 product features

1. **Rule-driven cleanup packs** — cleaner logic lives in YAML rule packs, so coverage can grow without hard-coding every path in Rust.
2. **Read-only scanning** — `ppduster scan` never deletes and gives a safe first pass before any action.
3. **Dry-run clean by default** — `ppduster clean` previews deletions unless `--yes` is explicitly passed.
4. **Trash-first deletion** — normal cleanup goes to Trash / Recycle Bin instead of immediate permanent deletion.
5. **Explicit permanent-delete confirmation** — `--permanent` requires a typed confirmation phrase before irreversible deletion.
6. **Age-based cleanup controls** — each rule can enforce `min_age_days`, with a CLI override via `--min-age`.
7. **Never-touch safety rails** — protected paths like documents, keys, and system locations are blocked from cleanup.
8. **Report-only risky areas** — some large or sensitive targets can be surfaced in results but never auto-deleted.
9. **Cross-platform and category coverage** — rule packs cover macOS, Linux, Windows, plus caches for apps, browsers, dev tools, and package managers.
10. **Safe setup automation** — `ppduster setup` is isolated from cleanup and uses typed, trust-gated automation tasks instead of arbitrary shell YAML.

## Install / build

```bash
cargo build --release
./target/release/ppduster doctor
```

Run from the repo root so `./rules` is found, or pass `--rules-dir /path/to/rules`.

## Scenario Flow UI

`ppduster-ui` is a native `egui` application inspired by Peregon's visual pipeline
canvas. It presents every setup scenario as a connected route of typed steps, with a
searchable scenario library, step inspector, release-channel controls, permission
switches, dry-run planning, and explicit confirmation before supported scenarios are
applied. The library can also save the selected scenario as a portable YAML file and
load `.yaml`/`.yml` scenarios selected in the native file dialog. A loaded file is
validated as an external task pack and may replace a bundled scenario with the same id
for the current application session.

For a scenario that resolves to exactly one standalone `git-clone` step, the inspector
can load repositories visible to the current GitHub CLI account and select one or many
public repositories. The selection replaces that standalone step with one ordinary
HTTPS clone-or-update step per repository, synchronizes `main`, and checks out under
`<destination root>/<owner>/<repository>`, so plans and reports stay separate. A task
with downstream steps is deliberately ineligible: replacing its clone could otherwise
silently disconnect those steps from their original checkout. Private, archived,
empty, and no-`main` repositories cannot be selected; SSH is not offered in the GUI.

ppduster does not request or store a GitHub token. If `gh` is not authenticated, the
picker offers **Sign in with GitHub** and starts `gh auth login --web --clipboard` in the
background. GitHub CLI opens the browser, copies its one-time device code to the
clipboard, owns the OAuth exchange and credential storage, and then ppduster reloads the
repository list automatically. A copyable terminal command remains available as a
fallback. The picker resolves `gh` only from absolute executable paths, bounds process
runtime and output, and redacts diagnostics before showing them in the UI.

```bash
cargo run --bin ppduster-ui
```

Create a native macOS application bundle:

```bash
./scripts/build-macos-app.sh
open target/macos/ppduster.app
```

Scenarios that need a terminal prompt, App Store authentication, or vendor license UI
remain terminal-only. The desktop app produces the exact CLI command for them instead
of attempting to capture credentials. A repository selection is an in-memory scenario
configuration and is applied from the desktop UI after its generated plan is reviewed.

## Usage

```bash
# Self-check
ppduster doctor

# List rules / categories
ppduster rules list
ppduster rules list --all
ppduster categories
ppduster rules show macos-user-caches

# Scan (read-only)
ppduster scan
ppduster scan -c caches,logs
ppduster scan -c temp --min-age 0 --limit 50
ppduster scan -o json > report.json

# Audit trail for CLI operations
ppduster --audit-log /tmp/ppduster-audit.log scan
ppduster audit --limit 10

# Clean: always review scan first
ppduster clean -c caches          # dry-run
ppduster clean -c caches --yes    # to Trash
ppduster clean -c temp --yes --permanent   # asks you to type DELETE
```

## Mac App Store CLI

[`ppstore`](packages/ppstore/README.md) is the standalone `mas`-style package for
catalog search, installed-app inventory, installation, and updates. Install it
separately; Homebrew and the external `mas` binary are not required:

```bash
cargo install --locked --path packages/ppstore
```

```bash
# Catalog and local inventory (read-only)
ppstore search Xcode --country US --limit 10
ppstore list
ppstore outdated
ppstore doctor

# Installation/update is planned by default
ppstore install 497799835
ppstore upgrade

# Explicitly enqueue with Apple's App Store services
ppstore install 497799835 --yes
ppstore install 640199958 --get --yes
ppstore upgrade --yes

# Machine-readable output is available for every report
ppstore -o json list
```

`ppduster app-store` remains a compatibility proxy with the same subcommands, but it
executes the separately installed `ppstore` binary directly, without a shell. The
typed `app-store-install` automation action uses the versioned JSON protocol exposed
by `ppstore 0.1.x`. An explicit `PPDUSTER_PPSTORE_PATH` must be an absolute path to a
trusted executable; an invalid override is a hard error instead of falling back to a
different program. The legacy `PPDUSTER_APP_STORE_COUNTRY` override is forwarded by
the proxy and automation adapter; direct `ppstore` use supports `PPSTORE_COUNTRY`.

The storefront otherwise defaults to the Mac's Apple locale. Installed apps are
identified by a non-empty App Store receipt and bundle metadata. Native installation
is macOS-only and remains isolated inside `ppstore`, which runtime-checks private Apple
frameworks because Apple does not publish a CLI installation API. The Mac App Store
must already be signed in; neither program reads Apple Account credentials. Applying
always requires `--yes`. A `pending` result is not a safe retry signal: rescan first,
because Apple's background service may still complete the request.

## Setup automation

`ppduster setup` is a separate, safe-by-default task subsystem for constructive automation
such as cloning a repo, planning a brew install, or verifying command sequences.

```bash
# List bundled setup tasks
ppduster setup list

# Show one task
ppduster setup show dev-brew-bootstrap

# Plan a task (default; no side effects)
ppduster setup run dev-brew-bootstrap

# Built-in macOS automation starters
ppduster setup show macos-top-01-brew-bootstrap
ppduster setup run macos-top-03-system-defaults --allow-shell
ppduster setup run macos-top-08-security-baseline --allow-elevation

# In a repository that contains package.json and a .NET solution/project:
# create project-level npm and NuGet registry files for DodoPizza packages
cd /path/to/mixed-node-dotnet-repository
ppduster setup run dev-dodopizza-package-registries
ppduster setup run dev-dodopizza-package-registries --yes

# create a separate password-encrypted credential vault (hidden prompts)
ppduster setup secrets init dev-dodopizza-package-registries

# decrypt only in memory and expose credentials to the guarded child environment
ppduster setup secrets exec dev-dodopizza-package-registries npm -- ci
ppduster setup secrets exec dev-dodopizza-package-registries dotnet -- restore

# LightBurn 2.1.03: inspect the plan, then download/install/confirm activation
ppduster setup run lightburn-install-activate
ppduster setup run lightburn-install-activate --yes

# Bambu Studio: choose latest stable release or latest beta
ppduster setup run bambu-studio-install --channel release
ppduster setup run bambu-studio-install --channel release --yes
ppduster setup run bambu-studio-install --channel beta
ppduster setup run bambu-studio-install --channel beta --yes

# Check the macOS baseline used with the separately installed ppstore client
ppduster setup run app-store-bootstrap
ppduster setup run app-store-bootstrap --yes

# A reusable template composed from six existing scenarios
ppduster setup show macos-developer-workstation
ppduster setup run macos-developer-workstation --allow-shell

# Typed folder/file operations, SHA-256, and filesystem conditions
ppduster setup run filesystem-basics
ppduster setup run filesystem-basics --yes

# External task packs are blocked unless explicitly trusted
ppduster --trust-external-packs setup run dev-brew-bootstrap --tasks-dir /path/to/tasks
```

Every scenario must explain its outcome in `description`. YAML block scalars are
recommended for a useful overview of changes, prerequisites, permissions, and what
the scenario intentionally leaves alone. The UI and `setup run` combine that overview
with a generated, action-by-action explanation of what will happen.

A reusable template groups existing scenarios without copying their steps:

```yaml
task:
  id: macos-developer-workstation
  name: macOS developer workstation
  description: |
    Prepare a development workstation in a deliberate, reviewable order.
    Child checks and safety gates remain active, and dry-run is still the default.
  platform: macos
  trust: bundled-only
  scenarios:
    - macos-top-01-brew-bootstrap
    - macos-top-02-dotfiles
    - macos-top-05-toolchains
```

Templates may include other templates. References are expanded in the listed order
before execution so all shell, elevation, authentication, and destination policies are
checked before the first change. Loading fails early on missing references, cycles,
duplicate children, platform mismatches, excessive expansion, or a reference from a
more-trusted pack to a less-trusted pack. A definition contains either `steps` or
`scenarios`, never both.

A `git-clone` step is idempotent when a branch is declared:

```yaml
- id: sync-repository
  name: Clone or update repository
  type: git-clone
  repo: https://github.com/example/project.git
  dest: $HOME/Library/Caches/project
  branch: main
```

The declared `dest` is the folder being ensured. If it is absent (or is an empty
directory), the repository is cloned. If the matching repository already exists,
ppduster fetches `origin/main` and performs only a safe fast-forward. The result report
distinguishes a new clone, an already current branch ref, and a repository that existed
but was outdated and was updated. If another branch is checked out, that checkout and
its local changes stay in place while the inactive local `main` ref is fast-forwarded,
and the report says which active branch was preserved. Local changes that would block
a required fast-forward, a mismatched origin, or diverged history are never reset,
stashed, merged, or overwritten; the step stops with an explicit error instead.

Filesystem scenarios do not need a shell. `create-directory` recursively creates
missing parents and is idempotent: an existing real directory is reported as already
satisfied, while a file or symlink at the destination is an error and is never
replaced.

An unconditional `inspect-path` is read-only and runs during both a normal dry-run and
an applied run. It reports structured `path-metadata` in JSON as well as a human
summary: existence, path kind, emptiness, modification time, an optional creation time
when the filesystem provides it, and size. Files are always measured. For directories,
set `recursive_size: true` to total regular-file bytes and count entries recursively
(a size expectation also enables that measurement). `empty` means the directory has
no immediate entries, while `modified_at` is the timestamp of the inspected entry,
not the newest child. Set `sha256: true` to hash a regular file; declaring
`expect.sha256` enables the same measurement automatically. SHA-256 is unavailable for
directories and symlinks, and symlinks are never followed.

```yaml
- id: create-state-directory
  name: Create state directory
  type: create-directory
  path: $HOME/.local/state/example

- id: inspect-state-directory
  name: Get size and dates
  type: inspect-path
  path: $HOME/.local/state/example
  recursive_size: true
  expect:
    exists: true
    kind: directory
    empty: true
    min_size_bytes: 0
    max_size_bytes: 0
    modified_at_or_after: 2000-01-01T00:00:00Z
    modified_at_or_before: 2100-01-01T00:00:00Z
```

All populated `expect` fields are combined with logical AND. Size and timestamp
boundaries are inclusive; timestamps use RFC 3339 and are compared as instants before
being reported in UTC. `exists: false` must stand alone because a missing path has no
type, size, emptiness, timestamps, or SHA-256. An unmet expectation marks the step
failed and skips later steps. Omit `expect` to observe a missing path successfully as
`exists: false`.

`write-file` treats `content` as exact UTF-8 bytes. It performs no interpolation and
does not add a newline; YAML's `|-` block style is useful when the final newline must
be omitted. The default `on_conflict: fail` leaves a different existing file untouched,
while an identical file is already satisfied. `on_conflict: replace` atomically
replaces only a regular file. Its parent directory must already exist, so directory
creation remains an explicit scenario step. There is deliberately no append mode
because appending cannot provide strict idempotency. Task YAML is plaintext, not a
secret store: never embed passwords or tokens in `content`; use the dedicated secrets
workflow instead.

`copy-path` accepts `src` and the exact `dest`. It copies a regular file or directory
without following symlinks. A source tree containing a symlink is rejected, an
identical destination is already satisfied, and any different existing destination is
left untouched with an error. The destination parent must already exist (use an
explicit `create-directory` step); directory copies are bounded to 100,000 entries,
10 GiB of logical file bytes, and 128 levels. `copy-path` has no overwrite or
conflict-policy field.
`remove-path` moves its declared path to the system Trash or Recycle Bin and never
falls back to permanent deletion. A missing path is already satisfied; moving a final
symlink moves the link itself rather than its target.

```yaml
- id: write-state
  type: write-file
  path: $HOME/.local/state/example/state.txt
  content: |-
    ready
  on_conflict: fail

- id: copy-state
  type: copy-path
  src: $HOME/.local/state/example/state.txt
  dest: $HOME/.local/state/example/state-copy.txt

- id: verify-state-copy
  when:
    type: path
    path: $HOME/.local/state/example/state-copy.txt
    expect: { exists: true }
  type: inspect-path
  path: $HOME/.local/state/example/state-copy.txt
  sha256: true
  expect:
    kind: file
    sha256: b24d6d33736ecd5604a4b17bc9c6481039fac362bb7df044ef1c10a2bfd21db6

- id: remove-obsolete-marker
  when:
    type: path
    path: $HOME/.local/state/example/obsolete.txt
    expect: { exists: true }
  type: remove-path
  path: $HOME/.local/state/example/obsolete.txt
```

Steps can use `when` to skip optional work or `require` to enforce a prerequisite.
Both fields contain a tagged condition tree: `type: path` reuses `PathExpectation`,
`type: exit-code` observes an earlier script step, `type: all` and `type: any` contain
a non-empty `conditions` list, and `type: not` contains one `condition`. For example:

```yaml
require:
  type: all
  conditions:
    - type: path
      path: $TMPDIR
      expect:
        exists: true
        kind: directory
    - type: any
      conditions:
        - type: path
          path: $TMPDIR
          expect: { kind: directory }
        - type: path
          path: $TMPDIR
          expect: { kind: other }
    - type: not
      condition:
        type: path
        path: $TMPDIR
        expect: { kind: symlink }
```

Guards are evaluated only during an applied run, immediately before their step, so
they see changes made by earlier applied steps. A dry-run does not evaluate guards or
simulate earlier mutations; a guarded `inspect-path` therefore stays planned, while an
unconditional `inspect-path` remains observable in dry-run. Other guarded actions keep
their normal dry-run planning and intrinsic idempotency checks. A false `when` marks
the step skipped and continues the scenario. A false `require` fails the step and skips
later work. `when` is evaluated before `require`; `not` negates only a condition
mismatch, never an I/O error.

The older `check.path_exists` and `check.command_succeeds` fields have a different,
unchanged purpose: either one can declare a mutating step already satisfied so its
action is skipped. They are not assertions or branching conditions.

A `run-script` step executes an existing script file through an explicitly selected
interpreter. `script` is a path, not inline source; `args`, `cwd`, and `env` are
optional. `success_exit_codes` declares which normal process exit codes count as a
successful step and defaults to `[0]`. Every script step must declare
`dangerous: true`, and execution requires both the normal `--yes` apply flag and
`--allow-shell`:

```yaml
- id: configure-posix
  name: Apply the portable workstation baseline
  type: run-script
  interpreter: sh
  script: $HOME/.config/workstation/configure.sh
  args: ["--profile", "developer"]
  success_exit_codes: [0]
  cwd: $HOME/.config/workstation
  env:
    PPDUSTER_MODE: setup
  dangerous: true

- id: configure-bash
  name: Apply Bash-specific setup
  type: run-script
  interpreter: bash
  script: $HOME/.config/workstation/configure-bash.sh
  dangerous: true

- id: configure-windows
  name: Apply the Windows workstation baseline
  type: run-script
  interpreter: powershell
  script: "%USERPROFILE%/workstation/configure.ps1"
  args: ["-Profile", "Developer"]
  dangerous: true
```

The actual exit code is retained in the step report and shown even when a configured
non-zero code is accepted. A normally terminated script whose code is not listed in
`success_exit_codes` fails the step, halts the scenario, and skips every later step.
Termination by a signal has no exit code, never matches
`success_exit_codes`, and is always reported as a failure rather than being confused
with an ordinary non-zero result.

Machine-readable reports expose this result as structured output, for example:

```json
{
  "type": "process-exit",
  "value": {
    "exit_code": 10,
    "accepted": true,
    "success_exit_codes": [0, 10, 20]
  }
}
```

For signal termination, `exit_code` is `null`, `accepted` is `false`, and
`termination_signal` is included when the operating system exposes it.

Later steps can branch declaratively on the actual code from an earlier script step.
The probe must accept every code that represents a valid state; unexpected codes still
fail the scenario before a branch is selected:

```yaml
- id: probe
  name: Detect the workstation state
  type: run-script
  interpreter: sh
  script: $HOME/.config/workstation/probe.sh
  success_exit_codes: [0, 10, 20]
  dangerous: true

- id: configure-missing
  name: Configure a missing workstation
  when: { type: exit-code, step: probe, codes: [10] }
  type: run-script
  interpreter: bash
  script: $HOME/.config/workstation/configure-bash.sh
  dangerous: true

- id: repair-outdated
  name: Repair an outdated workstation
  when: { type: exit-code, step: probe, codes: [20] }
  type: run-script
  interpreter: powershell
  script: $HOME/.config/workstation/repair.ps1
  dangerous: true
```

A dry-run cannot know the probe's future code, so its plan shows every conditional
branch for review. During an applied run, each `when: exit-code` condition is evaluated
after its referenced step completes; branches whose `codes` do not contain the actual
code are marked skipped and are not executed. The referenced step must be an earlier
`run-script` step, and every condition code must also appear in that step's
`success_exit_codes`. Both code lists must be non-empty and contain no duplicates.

The interpreter names map to these executables, tried in order:

| `interpreter` | macOS / Linux | Windows |
|---|---|---|
| `sh` | `/bin/sh`, then `sh` | `sh.exe`, then `sh` |
| `bash` | `bash`, then `/bin/bash` | `bash.exe`, then `bash` |
| `powershell` | `pwsh`, then `powershell` | `pwsh.exe`, then `powershell.exe` |

PowerShell scripts run with `-NoLogo -NoProfile -NonInteractive -File`; Windows also
uses process-scoped `-ExecutionPolicy Bypass`. The script must be a regular file;
script and `cwd` symlinks are rejected, and relative script paths are resolved from
`cwd` when it is supplied. Script steps stay terminal-only in Scenario Flow, which
shows the exact CLI command instead of trying to capture script interaction.

Current safety posture:

- separate from `rules/` and the scan/clean pipeline
- sealed typed actions only; no arbitrary YAML shell strings
- dry-run planning by default; `--yes` is required to apply a setup task
- shell-capable steps require `dangerous: true` and `--allow-shell`
- elevated steps require `--allow-elevation`
- external task packs require `--trust-external-packs`
- `create-directory` expands only declared paths, blocks protected destinations, creates parents without overwriting, and rejects a symlink at the target
- `inspect-path` performs read-only metadata and SHA-256 checks during dry-run when unconditional; recursive size walks never follow symlinks and fail rather than report a partial total
- `write-file` is an exact, atomic, size-bounded write with no interpolation or append mode
- `copy-path` never follows symlinks or overwrites different existing content
- `remove-path` uses Trash/Recycle Bin only and never performs permanent deletion
- download steps require `checksum.sha256`

The `dev-dodopizza-package-registries` task creates `.npmrc` and `NuGet.Config`
in the current project root. Those files never contain credential values: they contain
only `${GITHUB_PACKAGES_TOKEN}`, `%GITHUB_PACKAGES_USER%`, and
`%GITHUB_PACKAGES_TOKEN%` references.

`setup secrets init` stores the real GitHub username and token in a separate binary
[age](https://age-encryption.org/) passphrase-encrypted vault. The default file is
outside the repository under the user config directory at
`ppduster/secrets/v1/dodopizza-github-packages-<workspace-id>.age` (`~/Library/Application Support`
on macOS and `$XDG_CONFIG_HOME`/`~/.config` on Linux). Windows config-file generation
still works, but vault init/exec currently fail closed until owner-only ACL creation and
validation are implemented.
Interactive password and token prompts disable terminal echo; the optional stdin modes
require redirected input. Secret values are never accepted as command-line arguments or
task-YAML values. New Unix directories/files use `0700`/`0600`; ciphertext is staged and
installed without overwriting an existing vault. A custom `--file` path is allowed
only outside the current Git repository and its existing parent must already be
owner-only. Losing the vault password is unrecoverable.
The encrypted payload is bound to the canonical repository path, so a different
checkout cannot reuse it merely by copying the generated config files.
An interrupted Unix no-clobber install can leave a second owner-only encrypted staging
hardlink; the target remains unlockable, and no plaintext staging file is ever created.

`setup secrets exec` accepts only the exact commands `npm ci`, `npm install`, or
`dotnet restore` (no additional package-manager arguments), launches
the selected executable directly without a shell, and injects credentials into that
child process only. It isolates inherited npm user/global configs and pins dotnet to the
generated `NuGet.Config`; arguments that override registries or credential configs are
rejected. npm lifecycle scripts are forcibly disabled while the token is present. Exact
username/token occurrences in child output are redacted, and unlock
failures use one generic error in stderr and the audit log. For local authentication,
use a short-lived GitHub [personal access token (classic)](https://docs.github.com/en/packages/working-with-a-github-packages-registry/working-with-the-nuget-registry#authenticating-with-a-personal-access-token)
with only the `read:packages` permission; GitHub Packages does not accept fine-grained
personal access tokens for this flow. Encryption protects the secrets at
rest; the selected child environment and its descendants, a debugger running as the same
user, or malicious project tooling can still access or encode the values. The npm/dotnet
executable resolved from the caller's `PATH` is also trusted, so run the wrapper only in
a trusted checkout and shell environment.

The task preserves the inherited default npm registry, routes `@dodopizza/*` npm
packages to GitHub Packages, keeps other NuGet packages on nuget.org, and maps
`Dodo.*` NuGet package IDs to GitHub Packages using Package Source Mapping.
Applying must run from the Git repository root and requires both `package.json`
and a .NET solution or project in that directory. Existing, different config files
and symlink targets are conflicts; ppduster will not merge or overwrite them.
Matching files are left byte-for-byte unchanged on repeated runs (LF and CRLF
checkouts are treated as equivalent) and are safe to commit because they contain
no credential values. If an interruption happens after the first file is created,
ppduster leaves that exact file in place; the next run creates only the missing
file instead of deleting a pathname that another process may have replaced. A
hard crash can also leave a random `.ppduster.*.tmp` hardlink containing only
placeholders; exact targets remain recoverable, while any differing hardlinked
target is rejected.

The bundled mappings are exclusive: use this task only after confirming that the
GitHub feeds contain every `@dodopizza/*` and `Dodo.*` package required by the
project, including packages also published publicly. If the private IDs use a
narrower namespace, change the task to that confirmed scope/prefix first. NuGet
Package Source Mapping requires NuGet 6.0+ or .NET SDK 6.0.100+ in every restore
environment; [older clients ignore the mapping](https://learn.microsoft.com/en-us/nuget/consume-packages/package-source-mapping#package-source-mapping-rules).

- `extract-archive` supports `zip`, `tar`, `tar.gz`/`tgz`, `tar.bz2`, and `tar.xz`; it rejects links, special files, traversal, duplicate output files, oversized output, and existing destinations before atomically publishing the extracted directory
- DMG installation verifies the image, mounts it read-only, validates the app signature and Gatekeeper assessment, stages the bundle in `~/Applications`, and refuses elevation or overwriting an existing app
- typed `app-store-install` steps pass a numeric App Store ID to the separately installed, runtime-checked `ppstore 0.1.x` client through a versioned JSON protocol; they never use a shell, must not request sudo/elevation, and require an App Store account that owns the app
- the sealed `activate-license` action accepts only a provider and method, and task loading rejects `license_key` / `license-key` fields at any nesting level; enter the key directly in the vendor UI

An App Store installation step looks like this:

```yaml
- id: install-xcode
  name: Install Xcode
  type: app-store-install
  app_id: 497799835
  operation: install
```

Use `operation: install` for an app already obtained or purchased by the signed-in
Apple Account. Use `operation: get` to obtain and install a free app. Apply the task
with `--yes`; Apple Account authentication remains in Apple's UI. Native App Store
steps reject `auth: sudo` and `allow_elevation: allow` because the backend runs in the
signed-in user session and does not invoke sudo.
The bundled bootstrap performs read-only macOS and App Store metadata checks; it does
not install Homebrew or the external `mas` utility.

An archive extraction step can detect the format from its file name or accept an
explicit `format`:

```yaml
- id: unpack-tool
  type: extract-archive
  src: $HOME/Library/Caches/ppduster/downloads/tool.tar.gz
  dest: $HOME/Library/Caches/ppduster/unpacked/tool
  check:
    path_exists: $HOME/Library/Caches/ppduster/unpacked/tool
  format: auto
  max_unpacked_bytes: 10737418240
```

Allowed explicit formats are `zip`, `tar`, `tar-gz`, `tar-bz2`, and `tar-xz`.
The default unpacked-size limit is 10 GiB.

The bundled LightBurn task pins the official macOS 2.1.03 DMG and an independently
computed SHA-256 (LightBurn does not publish a SHA-256 manifest for this release). It
requires macOS 12 or newer; Apple Silicon also requires Rosetta. After installation,
the task verifies the pinned bundle ID, signing team, and exact version before it
launches a new LightBurn instance from `~/Applications` (any running LightBurn must be
closed first). An unactivated copy shows the License Page automatically;
otherwise use **Help → License Management**. Paste the key exactly, including dashes,
complete activation, then type `ACTIVATED` in the terminal. `ppduster` does not see the
key. Version 2.1.03 must also fall within the key's update-validity period.

LightBurn's documented `-l` command is intentionally not used: after normal activation
it converts a closed installation to System Locked mode (and is also the first step for
a specially tagged Floating key), while exposing the key in the process argument list.

The bundled Bambu Studio task resolves the latest stable release by default; pass
`--channel beta` to select the newest prerelease. On apply it reads the official GitHub
asset SHA-256, verifies the exact `com.bambulab.bambu-studio` bundle and signing team,
and compares the signed installed version before updating `~/Applications`. Equal or
newer installed versions are left untouched.

Bundled macOS setup starters currently cover the top 50 bootstrap areas, starting with:

1. Brew bootstrap
2. Dotfiles sync
3. System defaults baseline
4. Power management (`pmset`)
5. Developer toolchains (`mise`)
6. `launchd` automation
7. Git / SSH / GPG baseline
8. Security baseline
9. Drift detection
10. Rollback snapshot

Additional built-in tasks cover network identity, locale/time, keyboard and trackpad
preferences, menu bar and Spotlight, notifications and Focus, wallpaper and lock
screen, privacy/TCC, FileVault, firewall, Gatekeeper, software updates, App Store,
terminal UX, tmux, CLI tools, certificates, VPN/proxy/Wi-Fi/Bluetooth, AirDrop,
audio, default apps, browsers, printers, fonts, Shortcuts, AppleScript,
Hammerspoon, Raycast, Hazel, window management, backup, sync, observability, and
recovery workflows.

## Rule format

See files under [`rules/`](rules/):

```yaml
pack: macos
rules:
  - id: macos-user-logs
    name: User Library Logs
    description: Application log files in ~/Library/Logs.
    category: logs
    platform: macos          # macos | linux | windows | any
    risk: low                # low | medium | high | report-only
    default_enabled: true
    min_age_days: 7
    paths:
      - "$HOME/Library/Logs"
    exclude_globs: []
    include_globs: []
    delete_mode: contents    # contents | path
    max_depth: 8
    report_only: false
```

Path templates: `$HOME`, `~`, `$TMPDIR`, `$XDG_CACHE_HOME`, `$XDG_DATA_HOME`, `%LOCALAPPDATA%`, `%APPDATA%`, `%USERPROFILE%`, …

## Safety model (non-negotiable)

1. **Scan ≠ clean.** Nothing is removed by `scan`.
2. **`clean` is dry-run** until `--yes`.
3. **Trash by default**; permanent delete is explicit and confirmed.
4. **Never-touch** user documents, mail/messages data, SSH/GPG keys, system roots.
5. **No scareware metrics.** Honest byte counts from matched paths only.
6. **Medium/high risk off by default**; some targets are report-only forever in MVP.

## Project layout

```
src/           Rust library + CLI
rules/         YAML rule packs (reviewed core + quarantined extended packs)
tasks/         Typed setup automation tasks
tests/         Integration tests
research/      Research notes and multi-agent macOS path batches
```

## Research basis

Commercial / flagship products studied conceptually:

- CleanMyMac X – category smart scan, large Xcode/iOS support, freemium suite
- CCleaner – registry/privacy focus on Windows, history of trust issues (lesson: no silent upsell/telemetry surprises)
- BleachBit – open cleaner definitions as data (primary architecture influence)
- Stacer – Linux GUI optimizer categories
- OnyX / AppCleaner / DaisyDisk – macOS maintenance and leftover removal patterns
- OS built-ins – Storage Sense, macOS Storage Management (prefer cooperating, not fighting the OS)
- Dev tools – npkill, docker prune, Xcode DerivedData cleaners

Open-source families covered in research batches: classic cleaners, duplicate finders (czkawka, dupeGuru, …), disk usage tools (ncdu, dust, …), trash CLIs, package/toolchain caches, browser/app caches, log maintenance, CI disk reclaim actions.

## macOS rule coverage notes

- **Safe-by-default targets:** `~/Library/Caches`, `~/Library/Logs`, saved app state, temp dirs, Quick Look, Safari favicon/preview caches, Firefox profile caches, Chromium-family code/GPU caches, and user crash reports.
- **Opt-in dev cleanup:** Xcode DerivedData, iOS DeviceSupport, and CoreSimulator caches/logs stay disabled by default because they are regenerable but can slow the next build or simulator launch.
- **Report-only areas:** Xcode Archives and iOS device backups are listed for review but never auto-deleted.
- **Intentionally excluded:** cookies, history, saved passwords, Mail data, Keychains, iCloud/mobile documents, and broad `~/Library/Application Support` or `~/Library/Containers` wipes.
- **Extended packs (wave 1, 10 agents):** `macos-apple-extended`, `macos-browsers`, `macos-misc`, `dev-extended`, `dev-mobile`, `dev-ml`, `apps-comms`, `apps-creative`, `apps-gaming`, `apps-office-electron`.
- **Extended packs (wave 2, 30 agents):** security/VPN, design UI, music DAW, CAD/print, photo RAW, education, finance, cloud CLIs, databases, virt, exotic pkg managers, webservers, observability, notes/PKM, social, streaming, backup/sync, PWA helpers, Continuity, fonts/icons, network proxies, JetBrains deep, Microsoft deep, Asia apps, CIS apps, JS monorepo, MDM, Homebrew deep, system logs, Containers residuals (30 YAML packs under `rules/`).
- **Quarantine policy:** every research-generated extended rule is disabled and `report-only`. Even `--all --yes` cannot delete its findings. Promote rules to cleanup only after a path-level audit and a regression test; broad `Application Support` roots are not considered safe cleanup targets.
- Together with core packs: **~1500+ rules** and **~7500+** path templates. Only reviewed core rules can delete; extended coverage is currently an opt-in inventory.
- **Personal pack:** `apps-personal.yaml` is built from a **live inventory** of apps on this Mac (Claude, Codex, Kimi, Copilot, Steam, JetBrains, Edge, Telegram, etc.) and is also disabled/report-only.
- **Research sources:** `research/macos-batch-01` … `41` JSON + `research/macos-merge-summary.json`.

## License

MIT
