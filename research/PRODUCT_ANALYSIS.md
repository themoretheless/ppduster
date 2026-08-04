# Product and repository analysis (ppduster)

This document captures the design input used for **ppduster**: commercial cleaners, built-in OS tools, and open-source families (~100 repos researched via multi-agent batches).

## Commercial / flagship products

### CleanMyMac X (MacPaw)
- **Model:** GUI suite: Smart Scan categories (System Junk, Mail Attachments, Trash Bins, iTunes Junk, similar), large Xcode/iOS support, malware, privacy, optimization upsells.
- **Steal:** Clear category map, large regenerable developer artifacts as first-class targets, size-first UX.
- **Avoid:** Scare/optimization theater, aggressive menu-bar upsell, broad “system” claims without path transparency.

### CCleaner (Piriform / Avast)
- **Model:** Windows-first; temp/browser/privacy; registry cleaner historically; freemium.
- **Steal:** Browser cache vs cookie separation, simple category checklist.
- **Avoid:** Registry “fix” as default, trust incidents (bundled software / security events), dark patterns around paid upgrade.

### BleachBit
- **Model:** Cross-platform open source; **cleaners as data files**; deep/overwrite options; CLI + GUI.
- **Steal (primary):** Versioned rule packs, per-app cleaners, dry-run mindset, honest open lists of what will be deleted.
- **Avoid:** Overwrite/shred complexity in MVP (optional later).

### Stacer (Linux)
- **Model:** Qt dashboard: system cleaner, startup apps, services, resources.
- **Steal:** Package cache + crash reports + app caches as separate toggles.
- **Avoid:** Service/startup management in a pure junk cleaner (out of scope for ppduster MVP).

### OnyX / AppCleaner / DaisyDisk (macOS peers)
- **OnyX:** Maintenance scripts, caches, databases rebuilds — power-user, not daily junk.
- **AppCleaner:** Leftover files after uninstall (plist, support dirs) — pattern for future “leftovers” module.
- **DaisyDisk:** Visual disk map — different product; ppduster stays rule-driven, not a treemap UI.

### Wise Disk Cleaner / Glary (Windows peers)
- Category checkboxes, scheduled clean, prefetch/thumbnails — many medium-risk defaults we keep **off**.

### OS built-ins
- **macOS Storage Management / Optimize Storage**
- **Windows Storage Sense / Disk Cleanup**
- Lesson: Prefer user-space regenerable junk; do not fight OS-protected locations; report-only for Windows.old / WinSxS.

### Dev-focused tools
- **npkill** — interactive `node_modules` reclamation.
- **docker system prune** — explicit filters, dangling vs all.
- **Xcode DerivedData / device support cleaners** — huge wins on Mac developer machines; medium risk (rebuild cost).

## Open-source repository families (~100 targets)

Research was sharded into 20 batches × ~5 repos:

| Batch theme | Example projects / families | Idea stolen |
|-------------|----------------------------|-------------|
| Classic cleaners | BleachBit, rmlint, fslint, tmpreaper/tmpwatch | Age-based temp, rule DBs |
| Duplicate finders | czkawka, dupeGuru, fdupes, jdupes, rdfind | Content hashing (future module, not MVP delete) |
| Disk usage | ncdu, dua-cli, dust, diskonaut, godu | Fast size aggregation UX |
| Safe delete | trash-cli, trashy, rip, gomi | Trash over unlink |
| Linux optimizers | Stacer, Ubuntu Cleaner, Sweeper | Package + thumbnail categories |
| macOS OSS cleaners | Pearcleaner, open AppCleaner-likes | App leftover paths |
| Windows OSS | Bulk Crap Uninstaller, BleachBit Win | Leftover uninstall reports |
| Package caches | apt/dnf/pacman/brew scripts | `package-cache` category |
| Toolchain caches | npm/yarn/pip/cargo/go/gradle | `dev.yaml` rules |
| Containers | docker/podman prune wrappers | Report-only or future plugin |
| IDE artifacts | DerivedData, JetBrains caches, npkill | Medium risk, off by default |
| Browser privacy | cache-only cleaners | Never cookies by default |
| Logs | logrotate, journal vacuum helpers | Age + report-only journald |
| Uninstall leftovers | residual plist/AppData scanners | Future leftovers engine |
| App caches | Spotify, Discord, Slack, Zoom | `apps.yaml` |
| Thumbnails | Linux thumbs, Quick Look, Win thumbs | Safe regenerable caches |
| Secure delete | srm, shred, BleachBit wipe | Out of MVP scope |
| Modern CLIs | Rust/Go cleaners 2020–2026 | clap + JSON output |
| CI reclaim | GitHub Actions cache prune | Noninteractive flags |
| Awesome lists | awesome-cli-apps / sysadmin | Discovery of long-tail tools |

## Design decisions for ppduster

1. **Rust CLI** — single static-ish binary, safe path handling, good for scripting.
2. **YAML rules as data** — BleachBit lesson; ship packs for macos/linux/windows/dev/apps.
3. **Dry-run default + Trash** — trash-cli / modern safer delete lesson.
4. **No scareware score** — anti-CCleaner-freemium-dark-pattern.
5. **macOS first** (this machine), Linux/Windows packs ready.
6. **Report-only for dangerous system areas** — Prefetch, Windows.old, Xcode Archives, pnpm store.
7. **MVP out of scope:** GUI, registry, malware, secure wipe, duplicate finder, docker prune execution, AppCleaner-deep leftover graph.

## Agent orchestration

- Workflow: `research-cleaner` (~41 logical agents)
  - 8 commercial product researchers
  - 20 repo batches (5 each → ~100)
  - 6 platform taxonomy agents
  - 4 safety/UX agents
  - 2 synthesizers + 1 merge

Artifacts from the workflow (when complete) live in the session workflow scratch; key conclusions are folded into this file and into `rules/`.
