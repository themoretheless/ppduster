# ppduster Data Pipelines

The data-pipeline module of ppduster is a visual data pipeline builder powered by Vue 3 and a
Rust/WebAssembly engine. It connects data sources, filters, field projections,
and output transformations into JSON, CSV, XML, SQL, or plain-text lists.

## Features

- Rust-powered JSON validation and formatting;
- automatic array discovery and field merging across objects;
- field selection and reordering;
- field-based filters with all/any matching logic;
- comma, semicolon, newline, and custom delimiters;
- empty-value skipping and deduplication;
- CSV-compatible value escaping;
- result copying and downloading;
- local auto-save for the current pipeline and quick presets for common workflows;
- processing in a dedicated Web Worker, so data never leaves the browser;
- a custom incremental syntax engine with no external editor dependency,
  supporting C#, Rust, JavaScript, TypeScript, Python, SQL, JSON, XML, YAML,
  Java, and INI;
- an extensible registry for custom languages, validators, and formatters;
- local line recomputation after edits, structural JSON/XML validation, and
  lossless JSON formatting.

## Local development

Requires Node.js 22+, Rust 1.96, and `wasm-pack`.

```bash
npm install
npm run dev
```

`npm run dev` compiles the crate in `wasm/` to WebAssembly and then starts Vite.
Run the complete test suite with:

```bash
npm test
```

## Static build

Run from this directory:

```bash
cargo fetch --manifest-path wasm/Cargo.toml --locked
npm run build:pages
```

The static bundle is generated in `dist/`. Set `GITHUB_REPOSITORY` to choose
the repository base path. The default is `/ppduster/`. The migration does not
transfer the former Peregon deployment or publish the new bundle.

## Architecture

- `src/` — Vue interface, background-process client, and Web Worker;
- [`themoretheless/tokenizer`](https://github.com/themoretheless/tokenizer) —
  a standalone `themoretheless-tokenizer` Rust crate pinned to an exact Git
  revision; Peregon converts its UTF-8 byte spans to UTF-16 offsets at the WASM
  boundary;
- `packages/syntax-engine/` — a standalone npm package with an extensible
  tokenization core, language profiles, diagnostics, and a JSON formatter; its
  public API is documented in
  [`packages/syntax-engine/README.md`](packages/syntax-engine/README.md);
- `wasm/` — Rust engine for data analysis and transformation;
- `worker/` — minimal Cloudflare Worker that serves the SPA;
- `tests/` — production-build smoke tests.

The architecture boundary between the current Rust/WASM runtime and an
optional future DuckDB backend is documented in
[`docs/runtime-backends.md`](docs/runtime-backends.md).
