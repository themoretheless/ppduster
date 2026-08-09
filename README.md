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
applied.

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
of attempting to capture credentials.

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

`ppduster app-store` provides its own `mas`-style catalog, inventory, install,
and update commands. It does not require Homebrew or the external `mas` binary,
and it can run outside the repository without loading cleanup rules.

```bash
# Catalog and local inventory (read-only)
ppduster app-store search Xcode --country US --limit 10
ppduster app-store list
ppduster app-store outdated
ppduster app-store doctor

# Installation/update is planned by default
ppduster app-store install 497799835
ppduster app-store upgrade

# Explicitly enqueue with Apple's App Store services
ppduster app-store install 497799835 --yes
ppduster app-store install 640199958 --get --yes
ppduster app-store upgrade --yes

# Machine-readable output is available for every report
ppduster -o json app-store list
```

The storefront defaults to the Mac's Apple locale and can be overridden with
`--country` or `PPDUSTER_APP_STORE_COUNTRY`. Installed apps are identified by a
non-empty App Store receipt and their bundle metadata; `outdated` compares those
versions with Apple's catalog and reports incompatible or unmatched apps separately.
Because Apple's catalog can lag behind App Store delivery, these are update candidates,
not a transactional guarantee of the release the signed-in account will receive.

Native install/update is macOS-only and uses isolated, runtime-checked private Apple
frameworks because Apple does not publish a CLI installation API. This backend may
need maintenance after macOS updates. The Mac App Store must already be signed in;
ppduster never reads Apple Account credentials. `--get` is restricted to catalogued
free apps, while paid apps must first be purchased in Apple's UI. Applying always
requires `--yes`; omit it for a side-effect-free plan. By default ppduster waits for
the receipt/version, while `--no-wait` returns after the bounded submission check. A
`pending` result is deliberately not treated as a safe retry signal: rescan `list` and
`outdated` first, because Apple's background service may still finish the request.

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

# LightBurn 2.1.03: inspect the plan, then download/install/confirm activation
ppduster setup run lightburn-install-activate
ppduster setup run lightburn-install-activate --yes

# Bambu Studio: choose latest stable release or latest beta
ppduster setup run bambu-studio-install --channel release
ppduster setup run bambu-studio-install --channel release --yes
ppduster setup run bambu-studio-install --channel beta
ppduster setup run bambu-studio-install --channel beta --yes

# Check native App Store prerequisites used by app-store-install actions
ppduster setup run app-store-bootstrap
ppduster setup run app-store-bootstrap --yes

# A reusable template composed from six existing scenarios
ppduster setup show macos-developer-workstation
ppduster setup run macos-developer-workstation --allow-shell

# Typed folder creation and read-only path metadata
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

`inspect-path` is read-only and runs during both a normal dry-run and an applied run.
It reports structured `path-metadata` in JSON as well as a human summary: existence,
path kind, emptiness, modification time, an optional creation time when the filesystem
provides it, and size. Files are always measured. For directories, set
`recursive_size: true` to total regular-file bytes and count entries recursively
(a size expectation also enables that measurement). `empty` means the directory has
no immediate entries, while `modified_at` is the timestamp of the inspected entry,
not the newest child. Symlinks are listed as symlinks and are never followed.

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
type, size, emptiness, or timestamps. An unmet expectation marks the step failed and
skips later steps. Omit `expect` to observe a missing path successfully as
`exists: false`.

The older `check.path_exists` and `check.command_succeeds` fields have a different,
unchanged purpose: either one can declare a mutating step already satisfied so its
action is skipped. They are not assertions or branching conditions.

A `run-script` step executes an existing script file through an explicitly selected
interpreter. `script` is a path, not inline source; `args`, `cwd`, and `env` are
optional. Every script step must declare `dangerous: true`, and execution requires
both the normal `--yes` apply flag and `--allow-shell`:

```yaml
- id: configure-posix
  name: Apply the portable workstation baseline
  type: run-script
  interpreter: sh
  script: $HOME/.config/workstation/configure.sh
  args: ["--profile", "developer"]
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
- `inspect-path` performs read-only metadata checks during dry-run; recursive size walks never follow symlinks and fail rather than report a partial total
- download steps require `checksum.sha256`
- `extract-archive` supports `zip`, `tar`, `tar.gz`/`tgz`, `tar.bz2`, and `tar.xz`; it rejects links, special files, traversal, duplicate output files, oversized output, and existing destinations before atomically publishing the extracted directory
- DMG installation verifies the image, mounts it read-only, validates the app signature and Gatekeeper assessment, stages the bundle in `~/Applications`, and refuses elevation or overwriting an existing app
- typed `app-store-install` steps use a numeric App Store ID through ppduster's native, runtime-checked backend; they require explicit elevation permission and an App Store account that owns the app
- the sealed `activate-license` action accepts only a provider and method, and task loading rejects `license_key` / `license-key` fields at any nesting level; enter the key directly in the vendor UI

An App Store installation step looks like this:

```yaml
- id: install-xcode
  name: Install Xcode
  auth: sudo
  allow_elevation: allow
  type: app-store-install
  app_id: 497799835
  operation: install
```

Use `operation: install` for an app already obtained or purchased by the signed-in
Apple Account. Use `operation: get` to obtain and install a free app. Apply the task
with `--yes --allow-elevation`; Apple Account authentication remains in Apple's UI.
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
rules/         YAML rule packs (core + macOS extended packs)
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
