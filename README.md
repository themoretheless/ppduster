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

# Install/refresh the mas CLI used by app-store-install actions
ppduster setup run app-store-bootstrap
ppduster setup run app-store-bootstrap --yes

# A reusable template composed from six existing scenarios
ppduster setup show macos-developer-workstation
ppduster setup run macos-developer-workstation --allow-shell

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

Current safety posture:

- separate from `rules/` and the scan/clean pipeline
- sealed typed actions only; no arbitrary YAML shell strings
- dry-run planning by default; `--yes` is required to apply a setup task
- shell-capable steps require `dangerous: true` and `--allow-shell`
- elevated steps require `--allow-elevation`
- external task packs require `--trust-external-packs`
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
failures use one generic error in stderr and the audit log. Use a short-lived GitHub
token with the minimum `read:packages` permission. Encryption protects the secrets at
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
- typed `app-store-install` steps use a numeric App Store ID through a standard Homebrew `mas` installation; they require explicit elevation permission and an App Store account that owns the app
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
The bundled bootstrap installs the current Homebrew Core `mas` and therefore requires
macOS 14 or newer.

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
rules/         YAML rule packs (macos, linux, windows, dev, apps)
tasks/         Typed setup automation tasks
tests/         Integration tests
research/      Research notes from multi-agent analysis (when present)
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

## License

MIT
