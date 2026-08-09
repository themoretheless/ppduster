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

# LightBurn 2.1.03: inspect the plan, then download/install/confirm activation
ppduster setup run lightburn-install-activate
ppduster setup run lightburn-install-activate --yes

# Bambu Studio 2.7.1.62: inspect the plan, then download/install
ppduster setup run bambu-studio-install
ppduster setup run bambu-studio-install --yes

# Install/refresh the mas CLI used by app-store-install actions
ppduster setup run app-store-bootstrap
ppduster setup run app-store-bootstrap --yes

# External task packs are blocked unless explicitly trusted
ppduster --trust-external-packs setup run dev-brew-bootstrap --tasks-dir /path/to/tasks
```

Current safety posture:

- separate from `rules/` and the scan/clean pipeline
- sealed typed actions only; no arbitrary YAML shell strings
- dry-run planning by default; `--yes` is required to apply a setup task
- shell-capable steps require `dangerous: true` and `--allow-shell`
- elevated steps require `--allow-elevation`
- external task packs require `--trust-external-packs`
- download steps require `checksum.sha256`
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

The bundled Bambu Studio task pins the official GitHub release DMG and its
publisher-provided SHA-256. It requires macOS 10.15 or newer and verifies the exact
`com.bambulab.bambu-studio` bundle, signing team, and version before installing it in
`~/Applications`.

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
