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

# Clean: always review scan first
ppduster clean -c caches          # dry-run
ppduster clean -c caches --yes    # to Trash
ppduster clean -c temp --yes --permanent   # asks you to type DELETE
```

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
