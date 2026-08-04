let meta = #{
    name: "research-cleaner",
    description: "Research 100 repos + commercial cleaners, synthesize junk-cleaner product spec with ~40 agents",
    phases: [
        #{ title: "Commercial", detail: "CleanMyMac, CCleaner, BleachBit and peers" },
        #{ title: "Repos", detail: "100 open-source cleaner/maintenance repos in batches" },
        #{ title: "Platforms", detail: "macOS/Linux/Windows junk categories and safe paths" },
        #{ title: "Safety", detail: "deletion safety, dry-run, trust model" },
        #{ title: "Synthesize", detail: "merge into product spec for ppduster" },
    ],
    when_to_use: "Bootstrap research for a system junk cleaner product",
};

let product_schema = #{
    "type": "object",
    "required": ["product", "features", "ui_model", "risks", "lessons"],
    "properties": #{
        "product": #{ "type": "string" },
        "features": #{ "type": "array", "items": #{ "type": "string" }, "maxItems": 20 },
        "ui_model": #{ "type": "string" },
        "risks": #{ "type": "array", "items": #{ "type": "string" }, "maxItems": 12 },
        "lessons": #{ "type": "array", "items": #{ "type": "string" }, "maxItems": 12 },
        "monetization_hooks": #{ "type": "array", "items": #{ "type": "string" }, "maxItems": 8 },
    },
};

let repos_schema = #{
    "type": "object",
    "required": ["batch_id", "repos"],
    "properties": #{
        "batch_id": #{ "type": "string" },
        "repos": #{
            "type": "array",
            "maxItems": 6,
            "items": #{
                "type": "object",
                "required": ["name", "url_or_slug", "language", "what_it_cleans", "technique", "safety", "steal"],
                "properties": #{
                    "name": #{ "type": "string" },
                    "url_or_slug": #{ "type": "string" },
                    "language": #{ "type": "string" },
                    "stars_approx": #{ "type": "string" },
                    "what_it_cleans": #{ "type": "string" },
                    "technique": #{ "type": "string" },
                    "safety": #{ "type": "string" },
                    "steal": #{ "type": "string" },
                },
            },
        },
    },
};

let platform_schema = #{
    "type": "object",
    "required": ["platform", "categories"],
    "properties": #{
        "platform": #{ "type": "string" },
        "categories": #{
            "type": "array",
            "maxItems": 16,
            "items": #{
                "type": "object",
                "required": ["id", "label", "paths_or_globs", "risk", "default_enabled", "notes"],
                "properties": #{
                    "id": #{ "type": "string" },
                    "label": #{ "type": "string" },
                    "paths_or_globs": #{ "type": "array", "items": #{ "type": "string" }, "maxItems": 20 },
                    "risk": #{ "type": "string" },
                    "default_enabled": #{ "type": "boolean" },
                    "notes": #{ "type": "string" },
                },
            },
        },
        "never_touch": #{ "type": "array", "items": #{ "type": "string" }, "maxItems": 20 },
    },
};

let safety_schema = #{
    "type": "object",
    "required": ["topic", "rules", "must_have"],
    "properties": #{
        "topic": #{ "type": "string" },
        "rules": #{ "type": "array", "items": #{ "type": "string" }, "maxItems": 15 },
        "must_have": #{ "type": "array", "items": #{ "type": "string" }, "maxItems": 12 },
        "anti_patterns": #{ "type": "array", "items": #{ "type": "string" }, "maxItems": 10 },
    },
};

let spec_schema = #{
    "type": "object",
    "required": ["product_name", "summary", "cli_commands", "modules", "default_categories", "safety_model", "stack", "mvp_scope", "repo_count_covered"],
    "properties": #{
        "product_name": #{ "type": "string" },
        "summary": #{ "type": "string" },
        "cli_commands": #{ "type": "array", "items": #{ "type": "string" }, "maxItems": 15 },
        "modules": #{ "type": "array", "items": #{ "type": "string" }, "maxItems": 20 },
        "default_categories": #{ "type": "array", "items": #{ "type": "string" }, "maxItems": 30 },
        "safety_model": #{ "type": "string" },
        "stack": #{ "type": "string" },
        "mvp_scope": #{ "type": "string" },
        "repo_count_covered": #{ "type": "integer" },
        "architecture_notes": #{ "type": "array", "items": #{ "type": "string" }, "maxItems": 20 },
        "file_layout": #{ "type": "array", "items": #{ "type": "string" }, "maxItems": 25 },
    },
};

// ---------------------------------------------------------------------------
// Phase 1: commercial / well-known products (8 agents)
// ---------------------------------------------------------------------------
phase("Commercial");
log("Researching commercial and flagship cleaners (8 agents)");

let commercial_products = [
    "CleanMyMac X (MacPaw) - macOS cleaner suite",
    "CCleaner (Piriform/Avast) - Windows-first cleaner",
    "BleachBit - open-source cross-platform cleaner",
    "Stacer - Linux system optimizer UI",
    "OnyX / AppCleaner / DaisyDisk (macOS maintenance peers)",
    "Wise Disk Cleaner / Glary Utilities (Windows peers)",
    "Storage Sense / macOS Storage Management / built-in OS cleaners",
    "Dev-focused cleaners: npkill, docker system prune UIs, DevCleaner for Xcode",
];

let cjobs = [];
for p in commercial_products {
    let pr = "You are researching system junk cleaners to design a new CLI+library product called ppduster.\n";
    pr += "Target product: " + p + "\n";
    pr += "Use web_search and open_page/web_fetch on official docs, Wikipedia, GitHub, and recent reviews.\n";
    pr += "Do NOT invent features - only report what you can ground in sources.\n";
    pr += "Return structured output:\n";
    pr += "- product: short name\n";
    pr += "- features: concrete cleaning/scan features (caches, logs, leftovers, privacy, malware, etc.)\n";
    pr += "- ui_model: how users scan/select/delete (wizard, categories, smart scan, CLI)\n";
    pr += "- risks: known controversies, false positives, aggressive deletions, upsell, telemetry\n";
    pr += "- lessons: what a NEW safer open tool should copy or avoid\n";
    pr += "- monetization_hooks: optional, what freemium products upsell\n";
    pr += "Be specific about path categories when known (e.g. ~/Library/Caches, /tmp, browser profiles).\n";
    cjobs.push(#{
        prompt: pr,
        label: "commercial",
        capability_mode: "read-only",
        agent_type: "general-purpose",
        output_schema: product_schema,
    });
}
let cresults = parallel(cjobs);

let commercial_summaries = [];
for r in cresults {
    if r != () && r.success && r.output != () {
        commercial_summaries.push(r.output);
    }
}
log("Commercial products researched: " + commercial_summaries.len().to_string());

// ---------------------------------------------------------------------------
// Phase 2: 100 repos in 20 batches of 5 (20 agents)
// ---------------------------------------------------------------------------
phase("Repos");
log("Researching ~100 open-source repos in 20 parallel batches");

let batch_briefs = [
    "Batch 01 - classic cleaners: BleachBit, rmlint, fslint, tmpwatch/tmpreaper, bleachbit competitors on GitHub",
    "Batch 02 - duplicate finders: czkawka, dupeGuru, fdupes, jdupes, rdfind",
    "Batch 03 - disk usage explorers that enable cleanup: ncdu, dua-cli, dust, diskonaut, godu",
    "Batch 04 - trash/safe-delete: trash-cli, trashy, rip (rm Improved), gomi, macos-trash",
    "Batch 05 - Linux optimizers: Stacer, Ubuntu Cleaner, BleachBit Linux packs, Sweeper, KDE cleaner tools",
    "Batch 06 - macOS open cleaners: AppCleaner sources/clones, pearcleaner, Sensei peers, open-source Mac junk cleaners on GitHub",
    "Batch 07 - Windows open cleaners: Bulk Crap Uninstaller, BleachBit Windows, WizTree-related, open CCleaner alternatives",
    "Batch 08 - package manager caches: tools/scripts for apt/yum/dnf/pacman/homebrew/chocolatey cache cleanup",
    "Batch 09 - language toolchain caches: npm/yarn/pnpm, pip/poetry, cargo, go module cache, gradle/maven cleaners",
    "Batch 10 - container/VM junk: docker system prune wrappers, podman, lima, multipass, colima disk reclaim tools",
    "Batch 11 - IDE/build artifacts: Xcode DerivedData cleaners, Android Studio cleaners, JetBrains caches, node_modules scanners (npkill)",
    "Batch 12 - browser privacy cleaners: cookie/cache clean scripts for Chrome/Firefox/Safari, profilist-like tools",
    "Batch 13 - log maintenance: logrotate, journalctl vacuum helpers, macOS ASL/unified log cleaners, Windows CBS/ETL tools",
    "Batch 14 - leftover uninstaller helpers: AppCleaner-like, residual plist/preference scanners, Windows AppData remnant tools",
    "Batch 15 - large media/app caches: Steam, Spotify, Discord, Slack, Zoom, Teams cache cleaner scripts/repos",
    "Batch 16 - thumbnail/preview caches: Linux thumbnails, macOS Quick Look, Windows thumbnail cache cleaners",
    "Batch 17 - security-minded cleaners: srm, permanent erase, free-space wipe, BleachBit shred, privacy-focused cleaners",
    "Batch 18 - Rust/Go/Python modern rewrites: any popular 2020-2026 system cleaner CLIs written in Rust/Go/Python",
    "Batch 19 - monorepo maintenance bots: github actions that clean artifacts, cache prune bots, CI disk reclaim actions",
    "Batch 20 - awesome-lists and catalogs: mine awesome-sysadmin / awesome-mac / awesome-cli-apps for cleaner tools and list 5 more repos not covered above",
];

let rjobs = [];
let bi = 0;
for brief in batch_briefs {
    bi += 1;
    let pr = "You research open-source system/junk cleaners for designing ppduster.\n";
    pr += brief + "\n";
    pr += "REQUIREMENTS:\n";
    pr += "1) Use web_search (GitHub, awesome lists) and open real repo pages. Prefer exact github.com owner/repo.\n";
    pr += "2) Return EXACTLY 5 distinct real repositories (not duplicates across your list).\n";
    pr += "3) For each repo fill: name, url_or_slug, language, stars_approx (string ok), what_it_cleans, technique, safety, steal.\n";
    pr += "   - technique: how it finds junk (rules DB, walk+heuristics, package APIs, mtime, etc.)\n";
    pr += "   - safety: dry-run, trash vs unlink, allowlists, confirmations\n";
    pr += "   - steal: one concrete idea worth copying into a new cleaner\n";
    pr += "4) batch_id must be like B01..B20 matching your batch number.\n";
    pr += "5) If a famous tool has no OSS core, pick the closest OSS alternative and note that.\n";
    pr += "Do not pad with fake repos. If you only find 4 solid ones, still return 5 by expanding search - dig deeper.\n";
    rjobs.push(#{
        prompt: pr,
        label: "repos-batch",
        capability_mode: "read-only",
        agent_type: "general-purpose",
        output_schema: repos_schema,
    });
}
let rresults = parallel(rjobs);

let all_repos = [];
let repo_batches_ok = 0;
for r in rresults {
    if r != () && r.success && r.output != () && r.output.repos != () {
        repo_batches_ok += 1;
        for repo in r.output.repos {
            all_repos.push(repo);
        }
    }
}
log("Repo batches OK: " + repo_batches_ok.to_string() + " repos collected: " + all_repos.len().to_string());

// ---------------------------------------------------------------------------
// Phase 3: platform junk taxonomies (6 agents)
// ---------------------------------------------------------------------------
phase("Platforms");
log("Platform-specific junk categories (6 agents)");

let platforms = [
    "macOS (Darwin) user-space junk: Library/Caches, Logs, Saved Application State, Xcode, Homebrew, iOS device support, Mail downloads, browser caches, /private/var/folders, /tmp",
    "macOS dangerous zones: never auto-delete System, user Documents/Desktop/Photos libraries, Keychains, SSH keys, browser profiles without explicit privacy mode",
    "Linux user+system: ~/.cache, thumbnails, journald, apt/pacman/dnf caches, systemd-tmpfiles, Flatpak/Snap leftovers, old kernels (report-only)",
    "Windows: %TEMP%, Prefetch (careful), WinSxS (report-only), Delivery Optimization, browser caches, Windows.old (report-only), Recycle Bin, thumbnail DB",
    "Developer machine junk (cross-platform): node_modules, target/, dist/, __pycache__, .gradle, DerivedData, Android SDK leftovers, Docker images/volumes, asdf/mise old versions",
    "Privacy category (opt-in): recent docs lists, clipboard history stores, shell history (never default), browser cookies/localstorage (opt-in only)",
];

let pjobs = [];
for plat in platforms {
    let pr = "Design safe junk categories for ppduster cleaner.\n";
    pr += "Focus: " + plat + "\n";
    pr += "Use web_search to verify real default paths for the current OS versions (2024-2026).\n";
    pr += "Return platform string and categories with: id, label, paths_or_globs, risk (low|medium|high|report-only), default_enabled, notes.\n";
    pr += "Also never_touch: absolute rules of paths/patterns that must never be deleted automatically.\n";
    pr += "Prefer env-relative paths ($HOME, %LOCALAPPDATA%, XDG_CACHE_HOME) over hard-coded usernames.\n";
    pjobs.push(#{
        prompt: pr,
        label: "platform",
        capability_mode: "read-only",
        agent_type: "general-purpose",
        output_schema: platform_schema,
    });
}
let presults = parallel(pjobs);

let platform_specs = [];
for r in presults {
    if r != () && r.success && r.output != () {
        platform_specs.push(r.output);
    }
}
log("Platform specs: " + platform_specs.len().to_string());

// ---------------------------------------------------------------------------
// Phase 4: safety / UX / architecture deep dives (4 agents)
// ---------------------------------------------------------------------------
phase("Safety");
log("Safety and product rules (4 agents)");

let safety_topics = [
    "Deletion safety model for a system cleaner: dry-run default, trash vs permanent delete, age thresholds, open-file checks, symlink escape prevention, path canonicalization",
    "Rule engine design: YAML/JSON clean rules (BleachBit-style cleaners), allowlist/denylist, per-app plugins, versioned rule packs",
    "CLI UX for cleaners: subcommands scan/clean/doctor/rules, table output, JSON for scripting, exit codes, noninteractive CI mode, progress",
    "Legal/ethical product rules: no fake 'PC is dirty' scareware, no forced upsell, honest size estimates, accessibility of logs of what was deleted",
];

let sjobs = [];
for t in safety_topics {
    let pr = "You are a staff engineer defining non-negotiable product rules for ppduster, a new open-source junk cleaner.\n";
    pr += "Topic: " + t + "\n";
    pr += "Ground advice in how BleachBit, trash-cli, docker prune, and reputable cleaners behave. Use web_search if needed.\n";
    pr += "Return topic, rules (concrete), must_have (MVP features), anti_patterns (what never to do).\n";
    sjobs.push(#{
        prompt: pr,
        label: "safety",
        capability_mode: "read-only",
        agent_type: "general-purpose",
        output_schema: safety_schema,
    });
}
let sresults = parallel(sjobs);

let safety_specs = [];
for r in sresults {
    if r != () && r.success && r.output != () {
        safety_specs.push(r.output);
    }
}
log("Safety specs: " + safety_specs.len().to_string());

// ---------------------------------------------------------------------------
// Phase 5: two synthesizer agents (adversarial pair) then merge
// ---------------------------------------------------------------------------
phase("Synthesize");
log("Synthesizing product specs (2 agents)");

let commercial_json = json_encode(commercial_summaries);
let repos_json = json_encode(all_repos);
let platforms_json = json_encode(platform_specs);
let safety_json = json_encode(safety_specs);

// Cap huge payloads for prompt size
if commercial_json.len() > 60000 {
    commercial_json = commercial_json.sub_string(0, 60000);
}
if repos_json.len() > 90000 {
    repos_json = repos_json.sub_string(0, 90000);
}
if platforms_json.len() > 50000 {
    platforms_json = platforms_json.sub_string(0, 50000);
}
if safety_json.len() > 40000 {
    safety_json = safety_json.sub_string(0, 40000);
}

let syn_jobs = [];

let pr1 = "Synthesize an MVP product specification for ppduster, a cross-platform junk cleaner CLI.\n";
pr1 += "You are synthesizer A (practical MVP). Prefer shipping a solid Rust or Python CLI that is safe by default.\n";
pr1 += "Primary OS priority: macOS first, then Linux, then Windows.\n";
pr1 += "INPUT commercial research JSON:\n" + commercial_json + "\n\n";
pr1 += "INPUT repos research JSON (target ~100):\n" + repos_json + "\n\n";
pr1 += "INPUT platform categories JSON:\n" + platforms_json + "\n\n";
pr1 += "INPUT safety JSON:\n" + safety_json + "\n\n";
pr1 += "Produce product_name, summary, cli_commands, modules, default_categories, safety_model, stack, mvp_scope,\n";
pr1 += "repo_count_covered (integer estimate of distinct repos reflected), architecture_notes, file_layout.\n";
pr1 += "MVP must be implementable in one focused codebase without GUI. Rules as data files. Dry-run default.\n";

syn_jobs.push(#{
    prompt: pr1,
    label: "synth-mvp",
    capability_mode: "read-only",
    agent_type: "general-purpose",
    output_schema: spec_schema,
});

let pr2 = "Synthesize an alternative product specification for ppduster as synthesizer B (safety-maximalist).\n";
pr2 += "Challenge aggressive cleaning. Prefer report-only for anything medium+ risk. Still must be useful.\n";
pr2 += "Same inputs follow.\n";
pr2 += "INPUT commercial JSON:\n" + commercial_json + "\n\n";
pr2 += "INPUT repos JSON:\n" + repos_json + "\n\n";
pr2 += "INPUT platforms JSON:\n" + platforms_json + "\n\n";
pr2 += "INPUT safety JSON:\n" + safety_json + "\n\n";
pr2 += "Produce the same schema fields. stack should still be realistic (Rust preferred for CLI).\n";

syn_jobs.push(#{
    prompt: pr2,
    label: "synth-safe",
    capability_mode: "read-only",
    agent_type: "general-purpose",
    output_schema: spec_schema,
});

let syn_results = parallel(syn_jobs);

let specs = [];
for r in syn_results {
    if r != () && r.success && r.output != () {
        specs.push(r.output);
    }
}

// Final merge agent
let merge_prompt = "Merge these two ppduster product specs into ONE final MVP spec.\n";
merge_prompt += "Take practical modules from A and safety defaults from B.\n";
merge_prompt += "Stack: prefer Rust CLI with embedded YAML rules if either suggests it; else Python if both do.\n";
merge_prompt += "Specs JSON:\n" + json_encode(specs) + "\n";
merge_prompt += "Also set repo_count_covered to the best estimate from inputs (around " + all_repos.len().to_string() + " repos collected).\n";
merge_prompt += "file_layout must list concrete relative paths for a new git repo (src/, rules/, tests/, README.md, Cargo.toml or pyproject.toml).\n";

let merged = agent(merge_prompt, #{
    label: "merge-spec",
    capability_mode: "read-only",
    agent_type: "general-purpose",
    output_schema: spec_schema,
});

let final_spec = #{};
if merged != () && merged.success && merged.output != () {
    final_spec = merged.output;
} else if specs.len() > 0 {
    final_spec = specs[0];
} else {
    final_spec = #{
        product_name: "ppduster",
        summary: "Fallback: research incomplete; implement safe dry-run cleaner with YAML rules.",
        cli_commands: ["scan", "clean", "rules", "doctor"],
        modules: ["scan", "rules", "trash", "report"],
        default_categories: ["user_cache", "user_logs", "tmp"],
        safety_model: "dry-run default; trash; age filters; never touch home documents",
        stack: "Rust + clap + serde_yaml",
        mvp_scope: "macOS+Linux user caches/logs/tmp",
        repo_count_covered: all_repos.len(),
        architecture_notes: ["rule packs as YAML", "json output"],
        file_layout: ["Cargo.toml", "src/main.rs", "rules/", "README.md"],
    };
}

// Persist research artifacts to scratch for the parent session
let report = "# ppduster research report\n\n";
report += "## Counts\n";
report += "- commercial summaries: " + commercial_summaries.len().to_string() + "\n";
report += "- repo batches ok: " + repo_batches_ok.to_string() + "\n";
report += "- repos collected: " + all_repos.len().to_string() + "\n";
report += "- platform specs: " + platform_specs.len().to_string() + "\n";
report += "- safety specs: " + safety_specs.len().to_string() + "\n";
report += "- synthesizer specs: " + specs.len().to_string() + "\n\n";
report += "## Final spec JSON\n\n```json\n" + json_encode(final_spec) + "\n```\n\n";
report += "## Commercial (JSON)\n\n```json\n" + json_encode(commercial_summaries) + "\n```\n\n";
report += "## Repos (JSON)\n\n```json\n" + json_encode(all_repos) + "\n```\n\n";
report += "## Platforms (JSON)\n\n```json\n" + json_encode(platform_specs) + "\n```\n\n";
report += "## Safety (JSON)\n\n```json\n" + json_encode(safety_specs) + "\n```\n";

let report_path = write_scratch_file("research-report.md", report);
let spec_path = write_scratch_file("final-spec.json", json_encode(final_spec));
let repos_path = write_scratch_file("repos.json", json_encode(all_repos));

log("Research complete. agents used ~39 (8+20+6+4+2+1)");

complete(#{
    summary: "Research complete for ppduster",
    agents_target: 41,
    commercial_count: commercial_summaries.len(),
    repo_batches_ok: repo_batches_ok,
    repos_collected: all_repos.len(),
    platform_count: platform_specs.len(),
    safety_count: safety_specs.len(),
    final_spec: final_spec,
    report_path: report_path,
    spec_path: spec_path,
    repos_path: repos_path,
});
