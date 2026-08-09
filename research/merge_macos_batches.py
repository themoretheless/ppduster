#!/usr/bin/env python3
"""Merge research macos-batch-*.json into YAML rule packs. Dedupe vs shipped rules."""

from __future__ import annotations

import json
import re
from collections import defaultdict
from pathlib import Path

ROOT = Path("/Users/themoretheless/Documents/Sources/ppduster")
RULES_DIR = ROOT / "rules"
RESEARCH = ROOT / "research"
OUT_DIR = RULES_DIR

# Existing packs to mine for already-shipped ids/paths (all YAML currently in rules/)
# Wave merge treats every present pack as already shipped so new batches only add deltas.
EXISTING_FILES = sorted(RULES_DIR.glob("*.yaml"))

# Map batch -> output pack file
BATCH_TO_PACK = {
    "01-apple-system": "macos-apple-extended",
    "02-browsers": "macos-browsers",
    "03-dev-toolchains": "dev-extended",
    "04-creative": "apps-creative",
    "05-comms": "apps-comms",
    "06-gaming": "apps-gaming",
    "07-office-electron": "apps-office-electron",
    "08-mobile-dev": "dev-mobile",
    "09-ml-ai": "dev-ml",
    "10-misc": "macos-misc",
    # wave 2 (30 agents)
    "11-security-vpn": "apps-security-vpn",
    "12-design-ui": "apps-design-ui",
    "13-music-daw": "apps-music-daw",
    "14-cad-print": "apps-cad-print",
    "15-photo-raw": "apps-photo-raw",
    "16-education": "apps-education",
    "17-finance": "apps-finance",
    "18-cloud-cli": "dev-cloud-cli",
    "19-databases": "dev-databases",
    "20-virt": "dev-virt",
    "21-pkg-exotic": "dev-pkg-exotic",
    "22-webservers": "dev-webservers",
    "23-observability": "dev-observability",
    "24-notes-pkm": "apps-notes-pkm",
    "25-social": "apps-social",
    "26-streaming": "apps-streaming",
    "27-backup-sync": "apps-backup-sync",
    "28-pwa-helpers": "dev-pwa-helpers",
    "29-apple-continuity": "macos-continuity",
    "30-fonts-icons": "macos-fonts-icons",
    "31-network-proxy": "apps-network-proxy",
    "32-jetbrains-deep": "dev-jetbrains-deep",
    "33-microsoft-deep": "apps-microsoft-deep",
    "34-asia-apps": "apps-asia",
    "35-cis-apps": "apps-cis",
    "36-js-monorepo": "dev-js-monorepo",
    "37-mdm-enterprise": "apps-mdm-enterprise",
    "38-homebrew-deep": "macos-homebrew-deep",
    "39-system-logs": "macos-system-logs",
    "40-containers-orphans": "macos-containers",
}

# Only process these batch id prefixes when set via env WAVE (e.g. "11-40")
# Empty = all batches found on disk.
WAVE_MIN = 11
WAVE_MAX = 40

# Generated paths are research-derived and have not received a path-by-path
# destructive-cleanup review. Publish them as opt-in reports only; promoting an
# individual rule to deletion requires a separate manual audit and tests.
FORCE_REPORT_ONLY = True

# Path segments / prefixes that must never become rule roots (ppduster never-touch)
NEVER_TOUCH_MARKERS = [
    "/Documents",
    "/Desktop",
    "/Downloads",
    "/Pictures",
    "/Movies",
    "/Music",
    "Photos Library.photoslibrary",
    "/.ssh",
    "/.gnupg",
    "/.aws",
    "/Library/Keychains",
    "/Library/Mail",
    "/Library/Messages",
    "/Library/Calendars",
    "/Library/Mobile Documents",
    "/Library/CloudStorage",
    "/Library/Containers/com.apple.mail",
    "$HOME/Documents",
    "$HOME/Desktop",
    "$HOME/Downloads",
    "$HOME/Pictures",
    "$HOME/Movies",
    "$HOME/Music",
    "$HOME/.ssh",
    "$HOME/.gnupg",
    "$HOME/.aws",
    "$HOME/Library/Keychains",
    "$HOME/Library/Mail",
    "$HOME/Library/Messages",
    "$HOME/Library/Calendars",
    "$HOME/Library/Mobile Documents",
    "$HOME/Library/CloudStorage",
    "$HOME/Library/Containers/com.apple.mail",
    "/System",
    "/Applications",
    "/Library/",  # system Library — but $HOME/Library is OK
    "/bin",
    "/sbin",
    "/usr",
    "/etc",
    "/dev",
    "/boot",
]

# System /Library (not under $HOME) is forbidden; $HOME/Library is fine.
SYSTEM_ONLY_PREFIXES = [
    "/Library/",
    "/System",
    "/Applications",
]


def is_never_touch_path(p: str) -> bool:
    s = p.replace("\\", "/")
    # Allow $HOME/Library...
    if s.startswith("$HOME/Library") or s.startswith("$HOME/."):
        # still block sensitive Library children
        for bad in [
            "$HOME/Library/Keychains",
            "$HOME/Library/Mail",
            "$HOME/Library/Messages",
            "$HOME/Library/Calendars",
            "$HOME/Library/Mobile Documents",
            "$HOME/Library/CloudStorage",
            "$HOME/Library/Containers/com.apple.mail",
        ]:
            if s == bad or s.startswith(bad + "/"):
                return True
        return False
    # Explicit user data roots
    for bad in [
        "$HOME/Documents",
        "$HOME/Desktop",
        "$HOME/Downloads",
        "$HOME/Pictures",
        "$HOME/Movies",
        "$HOME/Music",
        "$HOME/.ssh",
        "$HOME/.gnupg",
        "$HOME/.aws",
    ]:
        if s == bad or s.startswith(bad + "/"):
            return True
    if "Photos Library.photoslibrary" in s:
        return True
    # Bare system trees (not $HOME)
    if s.startswith("/Library/") or s in ("/Library", "/System", "/Applications"):
        return True
    if s.startswith("/System/") or s.startswith("/Applications/"):
        return True
    for prefix in ("/bin", "/sbin", "/usr/", "/etc/", "/dev/", "/boot", "/lib/", "/lib64/"):
        if s == prefix.rstrip("/") or s.startswith(prefix):
            return True
    return False


def yaml_escape(s: str) -> str:
    if s is None:
        return '""'
    if any(c in s for c in ':#{}[]&*!|>\'"%@`') or s.strip() != s or s == "" or s.lower() in (
        "true",
        "false",
        "null",
        "yes",
        "no",
    ):
        return json.dumps(s, ensure_ascii=False)
    return s


def emit_rule(r: dict) -> str:
    lines = []
    lines.append(f"  - id: {r['id']}")
    lines.append(f"    name: {yaml_escape(r.get('name', r['id']))}")
    if r.get("description"):
        lines.append(f"    description: {yaml_escape(r['description'])}")
    lines.append(f"    category: {r.get('category', 'misc')}")
    lines.append(f"    platform: {r.get('platform', 'macos')}")
    risk = r.get("risk", "low")
    if risk == "report-only" or r.get("report_only"):
        risk = "report-only"
        r = dict(r)
        r["report_only"] = True
        r["default_enabled"] = False
    lines.append(f"    risk: {risk}")
    de = bool(r.get("default_enabled", False))
    if risk != "low":
        de = False
    lines.append(f"    default_enabled: {'true' if de else 'false'}")
    if r.get("report_only") or risk == "report-only":
        lines.append("    report_only: true")
    lines.append(f"    min_age_days: {int(r.get('min_age_days', 7))}")
    lines.append("    paths:")
    for p in r["paths"]:
        lines.append(f"      - {yaml_escape(p)}")
    ig = r.get("include_globs") or []
    eg = r.get("exclude_globs") or []
    if ig:
        lines.append("    include_globs:")
        for g in ig:
            lines.append(f"      - {yaml_escape(g)}")
    if eg:
        lines.append("    exclude_globs:")
        for g in eg:
            lines.append(f"      - {yaml_escape(g)}")
    dm = r.get("delete_mode", "contents")
    if dm not in ("contents", "path"):
        dm = "contents"
    lines.append(f"    delete_mode: {dm}")
    md = r.get("max_depth", 6)
    try:
        md = int(md)
    except Exception:
        md = 6
    lines.append(f"    max_depth: {md}")
    lines.append("")
    return "\n".join(lines)


def extract_existing_ids_and_paths() -> tuple[set[str], set[str]]:
    ids: set[str] = set()
    paths: set[str] = set()
    id_re = re.compile(r"^\s*-\s*id:\s*(\S+)")
    path_re = re.compile(r'^\s*-\s*("([^"]+)"|(\S+))')
    in_paths = False
    for f in EXISTING_FILES:
        if not f.exists():
            continue
        for line in f.read_text().splitlines():
            m = id_re.match(line)
            if m:
                ids.add(m.group(1))
                in_paths = False
                continue
            if re.match(r"^\s*paths:\s*$", line):
                in_paths = True
                continue
            if in_paths:
                if re.match(r"^\s+\S", line) and line.strip().startswith("-"):
                    m2 = path_re.match(line)
                    if m2:
                        p = m2.group(2) if m2.group(2) is not None else m2.group(3)
                        if p:
                            paths.add(p)
                elif re.match(r"^\s+\w", line) and not line.strip().startswith("-"):
                    in_paths = False
                elif re.match(r"^\s*-\s*id:", line):
                    in_paths = False
    return ids, paths


def normalize_rule(raw: dict) -> dict | None:
    rid = raw.get("id")
    if not rid or not isinstance(rid, str):
        return None
    paths = raw.get("paths") or []
    if not isinstance(paths, list) or not paths:
        return None
    clean_paths = []
    seen_p = set()
    for p in paths:
        if not isinstance(p, str) or not p.strip():
            continue
        p = p.strip()
        if is_never_touch_path(p):
            continue
        if p in seen_p:
            continue
        seen_p.add(p)
        clean_paths.append(p)
    if not clean_paths:
        return None
    risk = raw.get("risk", "low")
    if risk not in ("low", "medium", "high", "report-only"):
        risk = "medium"
    report_only = bool(raw.get("report_only", False)) or risk == "report-only"
    if FORCE_REPORT_ONLY:
        report_only = True
    if report_only:
        risk = "report-only"
    default_enabled = bool(raw.get("default_enabled", False)) and risk == "low" and not report_only
    platform = raw.get("platform", "macos")
    if platform not in ("macos", "linux", "windows", "any"):
        platform = "macos"
    category = raw.get("category") or "misc"
    # sanitize category to simple tokens
    category = re.sub(r"[^a-z0-9\-]", "-", str(category).lower())[:40] or "misc"
    try:
        min_age = int(raw.get("min_age_days", 7))
    except Exception:
        min_age = 7
    if min_age < 0:
        min_age = 0
    try:
        max_depth = int(raw.get("max_depth", 6))
    except Exception:
        max_depth = 6
    max_depth = max(1, min(max_depth, 16))
    delete_mode = raw.get("delete_mode", "contents")
    if delete_mode not in ("contents", "path"):
        delete_mode = "contents"
    include_globs = [g for g in (raw.get("include_globs") or []) if isinstance(g, str) and g]
    exclude_globs = [g for g in (raw.get("exclude_globs") or []) if isinstance(g, str) and g]
    return {
        "id": rid,
        "name": str(raw.get("name") or rid)[:120],
        "description": str(raw.get("description") or "")[:300],
        "category": category,
        "platform": platform,
        "risk": risk,
        "default_enabled": default_enabled,
        "report_only": report_only,
        "min_age_days": min_age,
        "paths": clean_paths,
        "include_globs": include_globs,
        "exclude_globs": exclude_globs,
        "delete_mode": delete_mode,
        "max_depth": max_depth,
    }


def main() -> None:
    existing_ids, existing_paths = extract_existing_ids_and_paths()
    packs: dict[str, list[dict]] = defaultdict(list)
    seen_ids: set[str] = set(existing_ids)
    all_new_paths: set[str] = set()
    stats = {
        "rules_in": 0,
        "rules_kept": 0,
        "rules_skip_id": 0,
        "rules_skip_paths": 0,
        "paths_in": 0,
        "paths_kept": 0,
        "paths_dup_existing": 0,
        "paths_never_touch": 0,
    }

    batch_files = sorted(RESEARCH.glob("macos-batch-*.json"))
    for bf in batch_files:
        data = json.loads(bf.read_text())
        batch = data.get("batch") or bf.stem.replace("macos-batch-", "")
        # wave filter: only batches 11-40 for this merge run
        mnum = re.match(r"^(\d+)-", str(batch))
        if mnum:
            n = int(mnum.group(1))
            if n < WAVE_MIN or n > WAVE_MAX:
                continue
        pack_name = BATCH_TO_PACK.get(batch, f"macos-{batch}")
        for raw in data.get("rules") or []:
            stats["rules_in"] += 1
            stats["paths_in"] += len(raw.get("paths") or [])
            # count never-touch removals
            for p in raw.get("paths") or []:
                if isinstance(p, str) and is_never_touch_path(p):
                    stats["paths_never_touch"] += 1
            rule = normalize_rule(raw)
            if rule is None:
                stats["rules_skip_paths"] += 1
                continue
            if rule["id"] in seen_ids:
                # try unique suffix once
                alt = rule["id"] + "-ext"
                if alt in seen_ids:
                    stats["rules_skip_id"] += 1
                    continue
                rule["id"] = alt
            # drop paths already exactly present in shipped packs (still keep rule if new paths remain)
            new_paths = []
            for p in rule["paths"]:
                if p in existing_paths:
                    stats["paths_dup_existing"] += 1
                    continue
                new_paths.append(p)
            if not new_paths:
                stats["rules_skip_paths"] += 1
                continue
            rule["paths"] = new_paths
            seen_ids.add(rule["id"])
            for p in new_paths:
                all_new_paths.add(p)
            stats["paths_kept"] += len(new_paths)
            packs[pack_name].append(rule)
            stats["rules_kept"] += 1

    written_files = []
    for pack_name, rules in sorted(packs.items()):
        # stable order by id
        rules.sort(key=lambda r: r["id"])
        out = OUT_DIR / f"{pack_name}.yaml"
        body = [f"pack: {pack_name}", "rules:", ""]
        for r in rules:
            body.append(emit_rule(r))
        out.write_text("\n".join(body).rstrip() + "\n")
        written_files.append((out.name, len(rules), sum(len(r["paths"]) for r in rules)))

    summary = {
        "stats": stats,
        "unique_new_paths": len(all_new_paths),
        "packs": [
            {"file": f, "rules": rc, "paths": pc} for f, rc, pc in written_files
        ],
        "existing_ids": len(existing_ids),
        "existing_paths": len(existing_paths),
    }
    summary_path = RESEARCH / "macos-merge-summary.json"
    summary_path.write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
