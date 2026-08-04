# ppduster final MVP spec

Machine-readable source: [`final-spec.json`](final-spec.json)  
Full multi-agent dump: [`research-report.md`](research-report.md)

Produced by workflow **research-cleaner** (~41 agents): 8 commercial products, **100** open-source repo batch entries, 6 platform taxonomies, 4 safety briefs, 2 synthesizers + merge.

## Product

| Field | Value |
|-------|--------|
| Name | **ppduster** |
| Form | Rust CLI + library |
| Priority OS | macOS → Linux → Windows |
| Core loop | `scan` → review → `clean --yes` (Trash) |
| Repos covered | 100 |

## Summary

Local-first, open-source, rule-driven junk cleaner. Reclaims regenerable user caches, logs, temp files, package-manager download caches, thumbnails, saved application state, and selected app caches via versioned YAML packs.

No scareware scores, telemetry, GUI, malware claims, registry cleaners, or silent bulk wipe.

## CLI (target)

```
ppduster doctor
ppduster rules list|show
ppduster categories
ppduster scan [-c …] [--all] [--min-age N] [-o table|json]
ppduster scan --plan plan.json
ppduster clean …                 # dry-run
ppduster clean … --yes           # Trash
ppduster clean … --yes --permanent   # typed DELETE
ppduster suggest                 # native-tool hints for report-only
```

## Safety model (binding)

1. `scan` never mutates; `clean` without `--yes` is dry-run of the same pipeline  
2. Default-enabled = **risk=low** only, with age floors  
3. medium / high / report-only = inventory only on the normal clean path  
4. Trash default; permanent needs typed `DELETE`  
5. Hard never-touch denylist wins over rules and allowlists  
6. Canonicalize + refuse symlink escapes  
7. Prefer contents-only so rule roots stay  
8. Honest measured bytes; no dirty-score meters  
9. Exit codes: 0 success, 1 partial/ops failure, 2 usage/config  

## Shipped vs post-MVP

| Shipped in repo now | Spec still wants (next) |
|---------------------|-------------------------|
| rules / scan / safety / clean / report | plan export/import |
| YAML packs macos/linux/windows/dev/apps | risk_gate hard-enforce medium+ |
| doctor, rules, categories, scan, clean | suggest / native_hints |
| Trash + permanent confirm | audit JSONL log |
| tests for doctor/rules/scan dry-run | plan + schema tests, docs/safety.md |

## Explicitly out of MVP

GUI, Smart Care upsell, registry, malware, shred/wipe, full AppCleaner leftover graph, duplicate engines, Docker/journal mutation, unbounded home walks, scheduled daemon, telemetry.
