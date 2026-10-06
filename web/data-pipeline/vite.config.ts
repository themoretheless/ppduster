import vue from "@vitejs/plugin-vue";
import { readFileSync } from "node:fs";
import { defineConfig } from "vite";
import * as vueCompiler from "vue/compiler-sfc";
import hostingConfig from "./.openai/hosting.json";
import { sites } from "./build/sites-vite-plugin";
import packageManifest from "./package.json";

const PLACEHOLDER_DATABASE_ID = "00000000-0000-4000-8000-000000000000";
const { d1, r2 } = hostingConfig;
const isCodexSandbox = process.env.CODEX_SANDBOX === "seatbelt";
const isGitHubPagesBuild = process.env.GITHUB_PAGES === "true";

function cargoValue(source: string, section: string, key: string): string {
  const sectionSource = source.match(new RegExp(`\\[${section}\\]([\\s\\S]*?)(?=\\n\\[|$)`))?.[1] ?? "";
  return sectionSource.match(new RegExp(`^${key}\\s*=\\s*"([^"]+)"`, "m"))?.[1] ?? "unknown";
}

function cargoDependencyNames(source: string): string[] {
  const dependencies = source.match(/\[dependencies\]([\s\S]*?)(?=\n\[|$)/)?.[1] ?? "";
  return [...dependencies.matchAll(/^([\w-]+)\s*=/gm)].map((match) => match[1]);
}

function cargoLockedVersion(lock: string, packageName: string): string {
  const escapedName = packageName.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  return lock.match(new RegExp(`\\[\\[package\\]\\]\\nname = "${escapedName}"\\nversion = "([^"]+)"`))?.[1]
    ?? "unknown";
}

const cargoManifest = readFileSync(new URL("./wasm/Cargo.toml", import.meta.url), "utf8");
const cargoLock = readFileSync(new URL("./wasm/Cargo.lock", import.meta.url), "utf8");
const versionInfo = {
  project: { name: packageManifest.name, version: packageManifest.version },
  packages: {
    npmRuntime: Object.entries(packageManifest.dependencies).map(([name, version]) => ({ name, version })),
    rustRuntime: cargoDependencyNames(cargoManifest).map((name) => ({
      name,
      version: cargoLockedVersion(cargoLock, name),
    })),
    build: Object.entries(packageManifest.devDependencies).map(([name, version]) => ({ name, version })),
  },
  engine: {
    name: cargoValue(cargoManifest, "package", "name"),
    version: cargoValue(cargoManifest, "package", "version"),
  },
};

function githubPagesBase(): string {
  const repositoryName = process.env.GITHUB_REPOSITORY?.split("/").at(-1);
  return repositoryName?.endsWith(".github.io") ? "/" : `/${repositoryName ?? "ppduster"}/`;
}

const workerConfig = {
  name: "server",
  main: "./worker/index.ts",
  compatibility_date: "2026-05-22",
  compatibility_flags: ["nodejs_compat"],
  assets: {
    binding: "ASSETS",
    not_found_handling: "single-page-application" as const,
    run_worker_first: true,
  },
  d1_databases: d1
    ? [
        {
          binding: d1,
          database_name: "peregon-d1",
          database_id: PLACEHOLDER_DATABASE_ID,
        },
      ]
    : [],
  r2_buckets: r2
    ? [
        {
          binding: r2,
          bucket_name: "peregon-r2",
        },
      ]
    : [],
};

export default defineConfig(async () => {
  process.env.WRANGLER_WRITE_LOGS ??= "false";
  process.env.WRANGLER_LOG_PATH ??= ".wrangler/logs";
  process.env.MINIFLARE_REGISTRY_PATH ??= ".wrangler/registry";

  if (isGitHubPagesBuild) {
    return {
      base: githubPagesBase(),
      define: { __PEREGON_VERSION_INFO__: JSON.stringify(versionInfo) },
      plugins: [vue({ compiler: vueCompiler })],
    };
  }

  const { cloudflare } = await import("@cloudflare/vite-plugin");

  return {
    define: { __PEREGON_VERSION_INFO__: JSON.stringify(versionInfo) },
    server: isCodexSandbox
      ? { watch: { useFsEvents: false, usePolling: true } }
      : undefined,
    plugins: [
      vue({ compiler: vueCompiler }),
      sites(),
      cloudflare({
        viteEnvironment: { name: "server" },
        config: workerConfig,
      }),
    ],
  };
});
