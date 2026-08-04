# ppduster final MVP spec

Synthesized from multi-agent research (8 commercial products, 100 repo batch entries / 79 unique, 6 platform taxonomies, 4 safety briefs) plus implementation constraints.

## Product

| Field | Value |
|-------|--------|
| Name | **ppduster** |
| Form | Rust CLI + library |
| Priority OS | macOS → Linux → Windows |
| Core loop | `scan` → review → `clean --yes` (Trash) |

## CLI

```
ppduster doctor
ppduster rules list|show
ppduster categories
ppduster scan [-c cat] [--all] [--min-age N] [-o table|json]
ppduster clean [-c cat] [--yes] [--permanent] [--min-age N]
```

## Modules (shipped)

1. **rules** – YAML packs with platform/risk/age/paths/globs  
2. **scan** – expand templates, walk, size, filter  
3. **safety** – never-touch, symlink/root guards, age  
4. **clean** – trash (default) or permanent + confirm  
5. **report** – table / JSON  

## Default-enabled categories (safe)

- `caches` (user app caches; exclude heavy IDE/browser when shared with opt-in rules)  
- `logs` (user logs, age ≥ 7d)  
- `temp` (TMPDIR, age ≥ 2d)  
- `leftovers` (Saved Application State, etc.)  
- `package-cache` (Homebrew/npm/yarn/pip age-gated)  
- `app-cache` (Spotify/Discord/Slack/Zoom)

## Off by default / report-only

- Xcode DerivedData / Device Support  
- JetBrains / Gradle / Cargo / Go caches  
- Browser disk caches  
- Prefetch, Windows.old, iOS backups, Xcode Archives, pnpm store  

## Safety model

1. Dry-run unless `--yes`  
2. Trash over unlink  
3. Permanent requires typed `DELETE`  
4. Never-touch: Documents/Desktop/Downloads/media, `.ssh`, Keychains, `/System`, `/usr`, …  
5. No scareware score, no telemetry, no registry “fixes”  
6. Honest sizes from matched paths only  

## Stack

`clap`, `serde_yaml`, `walkdir`, `globset`, `trash`, `tabled`, `dirs`, `anyhow`

## Out of MVP scope

GUI, malware, secure free-space wipe, duplicate finder UI, docker prune execution, deep AppCleaner leftover graph, Windows registry.
