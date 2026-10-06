import assert from "node:assert/strict";
import test from "node:test";

import {
  computeTextChange,
  syntaxEngine,
} from "@peregon/syntax-engine";
import { LanguageRegistry } from "@peregon/syntax-engine/core";
import { createCodeLanguage } from "@peregon/syntax-engine/languages";

function tokenKinds(source, language) {
  const document = syntaxEngine.createDocument(source, language);
  return document.tokens().map((token) => token.kind);
}

function signature(document) {
  return document.tokens().map((token) => [
    token.kind,
    token.from,
    token.to,
    document.text.slice(token.from, token.to),
  ]);
}

function documentShape(document) {
  return document.lines.map((line) => ({
    text: line.text,
    lineBreak: line.lineBreak,
    tokens: line.tokens,
    diagnostics: line.diagnostics,
  }));
}

test("bundled registry exposes and detects every requested language", () => {
  assert.deepEqual(syntaxEngine.list().map(({ id }) => id), [
    "csharp",
    "rust",
    "javascript",
    "typescript",
    "python",
    "sql",
    "json",
    "xml",
    "yaml",
    "java",
    "ini",
  ]);
  assert.equal(syntaxEngine.detect("Program.cs")?.id, "csharp");
  assert.equal(syntaxEngine.detect("types.d.ts")?.id, "typescript");
  assert.equal(syntaxEngine.detect("settings.yml")?.id, "yaml");
  assert.equal(syntaxEngine.detect("document", "application/xml; charset=utf-8")?.id, "xml");
  assert.equal(syntaxEngine.detect("script.py", "text/plain")?.id, "python");
  assert.deepEqual(syntaxEngine.capabilities("json"), { validation: "structural", formatting: true });
  assert.deepEqual(syntaxEngine.capabilities("xml"), { validation: "structural", formatting: false });
  assert.deepEqual(syntaxEngine.capabilities("typescript"), { validation: "lexical", formatting: false });
});

test("registry accepts a custom stateful language and separate services", async () => {
  const registry = new LanguageRegistry();
  registry.register({
    id: "demo",
    name: "Demo",
    aliases: ["d"],
    extensions: ["demo"],
    initialState: () => false,
    stateKey: String,
    tokenizeLine(line, inBlock) {
      const next = line.endsWith("\\") ? !inBlock : inBlock;
      return {
        tokens: line ? [{ from: 0, to: line.length, kind: inBlock ? "string" : "keyword" }] : [],
        state: next,
      };
    },
  });
  registry.registerServices("demo", {
    validate: (source) => source.includes("!")
      ? [{ from: source.indexOf("!"), to: source.indexOf("!") + 1, severity: "warning", message: "demo" }]
      : [],
    format: (source) => ({ ok: true, text: source.toUpperCase() }),
  });

  assert.equal(registry.get("d")?.id, "demo");
  assert.equal(registry.detect("file.demo")?.id, "demo");
  assert.equal((await registry.validate("hello!", "demo"))[0]?.severity, "warning");
  assert.deepEqual(await registry.format("hello", "demo"), { ok: true, text: "HELLO" });
  assert.throws(() => registry.register({
    id: "other",
    name: "Other",
    aliases: ["d"],
    initialState: () => null,
    tokenizeLine: () => ({ tokens: [], state: null }),
  }), /уже принадлежит/u);
  assert.equal(registry.get("other"), undefined);

  assert.throws(() => registry.register({
    id: "partial",
    name: "Partial",
    aliases: ["fresh", "d"],
    initialState: () => null,
    tokenizeLine: () => ({ tokens: [], state: null }),
  }), /уже принадлежит/u);
  assert.equal(registry.get("partial"), undefined);
  assert.equal(registry.get("fresh"), undefined);

  registry.register({
    id: "mime-owner",
    name: "MIME owner",
    mimeTypes: ["application/x-demo"],
    initialState: () => null,
    tokenizeLine: () => ({ tokens: [], state: null }),
  });
  assert.throws(() => registry.register({
    id: "mime-conflict",
    name: "MIME conflict",
    mimeTypes: [" application/x-demo "],
    initialState: () => null,
    tokenizeLine: () => ({ tokens: [], state: null }),
  }), /уже принадлежит/u);
  assert.equal(registry.get("mime-conflict"), undefined);

  const canonical = new LanguageRegistry();
  canonical.register({
    id: " Demo ",
    name: "Canonical service id",
    initialState: () => null,
    tokenizeLine: (line) => ({ tokens: line ? [{ from: 0, to: line.length, kind: "text" }] : [], state: null }),
  });
  canonical.registerServices("demo", { format: (source) => ({ ok: true, text: source.toUpperCase() }) });
  assert.equal(canonical.capabilities(" Demo ").formatting, true);
  assert.deepEqual(await canonical.format("ok", "demo"), { ok: true, text: "OK" });
});

test("code-language factory rejects delimiters that cannot advance", () => {
  const base = {
    id: "unsafe",
    name: "Unsafe",
    keywords: [],
  };
  assert.throws(() => createCodeLanguage({ ...base, strings: [{ open: "", close: "" }] }), /пустым/u);
  assert.throws(() => createCodeLanguage({ ...base, operators: [""] }), /пустым/u);
  assert.throws(() => createCodeLanguage({ ...base, blockComment: { open: "", close: "x" } }), /пустым/u);

  const dynamic = createCodeLanguage({
    ...base,
    id: "dynamic-close",
    strings: [{ open: '"', close: () => "" }],
  });
  const registry = new LanguageRegistry().register(dynamic);
  assert.equal(registry.createDocument('"value', "dynamic-close").lexicalDiagnostics()[0]?.code, "provider-error");
});

test("documents reject stale and overlapping edit batches", () => {
  const document = syntaxEngine.createDocument("one two three", "javascript");
  const update = document.applyChanges([
    { from: 0, to: 3, insert: "1" },
    { from: 8, to: 13, insert: "3" },
  ], { expectedVersion: 1 });
  assert.equal(document.text, "1 two 3");
  assert.deepEqual([update.beforeVersion, update.afterVersion], [1, 2]);
  assert.throws(
    () => document.applyChange({ from: 0, to: 1, insert: "x" }, { expectedVersion: 1 }),
    /Устаревшая версия/u,
  );

  const overlapping = syntaxEngine.createDocument("abcdef", "javascript");
  assert.throws(() => overlapping.applyChanges([
    { from: 1, to: 4, insert: "x" },
    { from: 3, to: 5, insert: "y" },
  ]), /пересекаются/u);

  const manySource = "a".repeat(20_000);
  const many = syntaxEngine.createDocument(manySource, "csharp");
  const changes = Array.from({ length: 1_000 }, (_, index) => ({
    from: index * 20,
    to: index * 20 + 1,
    insert: "b",
  }));
  many.applyChanges(changes);
  assert.equal(many.text.length, manySource.length);
  assert.equal(many.text.match(/b/gu)?.length, changes.length);
});

test("a broken third-party provider is isolated to its line", () => {
  const registry = new LanguageRegistry();
  registry.register({
    id: "broken",
    name: "Broken",
    initialState: () => null,
    tokenizeLine(line) {
      return { tokens: line ? [{ from: 1, to: line.length + 1, kind: "keyword" }] : [], state: null };
    },
  });
  const document = registry.createDocument("hello\nworld", "broken");
  assert.equal(document.lines[0].diagnostics[0]?.code, "provider-error");
  assert.equal(document.lines[1].diagnostics[0]?.code, "provider-error");
  assert.equal(document.tokens().map((token) => document.text.slice(token.from, token.to)).join(""), document.text);

  const invalidDiagnosticRegistry = new LanguageRegistry().register({
    id: "bad-diagnostic",
    name: "Bad diagnostic",
    initialState: () => null,
    tokenizeLine: (line) => ({
      tokens: [],
      diagnostics: [{ from: Number.NaN, to: 0.5, severity: "error", message: "bad" }],
      state: null,
    }),
  });
  assert.equal(invalidDiagnosticRegistry.createDocument("x", "bad-diagnostic").lexicalDiagnostics()[0]?.code, "provider-error");

  const brokenFinalizeRegistry = new LanguageRegistry().register({
    id: "broken-finalize",
    name: "Broken finalize",
    initialState: () => null,
    tokenizeLine: (line) => ({ tokens: line ? [{ from: 0, to: line.length, kind: "text" }] : [], state: null }),
    finalize: () => { throw new Error("finalize failed"); },
  });
  assert.equal(brokenFinalizeRegistry.createDocument("x", "broken-finalize").lexicalDiagnostics()[0]?.code, "provider-error");

  const providerToken = { from: 0, to: 1, kind: "keyword", modifiers: ["declaration"] };
  const immutableRegistry = new LanguageRegistry().register({
    id: "immutable-output",
    name: "Immutable output",
    initialState: () => null,
    tokenizeLine: () => ({ tokens: [providerToken], state: null }),
  });
  const immutable = immutableRegistry.createDocument("x", "immutable-output");
  providerToken.kind = "invalid";
  providerToken.modifiers.push("changed");
  assert.equal(immutable.lines[0].tokens[0]?.kind, "keyword");
  assert.deepEqual(immutable.lines[0].tokens[0]?.modifiers, ["declaration"]);
  assert.ok(Object.isFrozen(immutable.lines[0].tokens[0]));
  assert.ok(Object.isFrozen(immutable.lines[0].tokens[0]?.modifiers));

  const hostileError = new Error("hidden");
  Object.defineProperty(hostileError, "message", {
    get() { throw new Error("message boom"); },
  });
  const throwingStateRegistry = new LanguageRegistry().register({
    id: "throwing-state-getter",
    name: "Throwing state getter",
    initialState: () => null,
    tokenizeLine: (line) => line === "y"
      ? (() => { throw hostileError; })()
      : line === "x"
      ? {
        tokens: [],
        get state() { throw new Error("state boom"); },
      }
      : { tokens: [], state: null },
  });
  const throwingState = throwingStateRegistry.createDocument("a\nb", "throwing-state-getter");
  assert.doesNotThrow(() => throwingState.applyChange({ from: 0, to: 1, insert: "x" }));
  assert.equal(throwingState.text, "x\nb");
  assert.equal(throwingState.version, 2);
  assert.equal(throwingState.lines[0].diagnostics[0]?.code, "provider-error");
  assert.equal(throwingState.lines.length, 2);
  assert.doesNotThrow(() => throwingState.applyChange({ from: 0, to: 1, insert: "y" }));
  assert.equal(throwingState.text, "y\nb");
  assert.equal(throwingState.version, 3);
  assert.equal(throwingState.lines[0].diagnostics[0]?.message, "Ошибка языкового провайдера");
});

test("providers without stateKey never reuse unequal object states", () => {
  const registry = new LanguageRegistry();
  registry.register({
    id: "map-state",
    name: "Map state",
    initialState: () => new Map([["mode", "root"]]),
    tokenizeLine(line, startState) {
      const mode = startState.get("mode");
      const nextMode = line === "OPEN" ? "string" : line === "CLOSE" ? "root" : mode;
      return {
        tokens: line ? [{ from: 0, to: line.length, kind: mode === "string" ? "string" : "text" }] : [],
        state: new Map([["mode", nextMode]]),
      };
    },
  });
  const document = registry.createDocument("noop\nvalue", "map-state");
  document.applyChange({ from: 0, to: 4, insert: "OPEN" });
  const cold = registry.createDocument("OPEN\nvalue", "map-state");
  assert.deepEqual(signature(document), signature(cold));
  assert.equal(document.lines[1].tokens[0]?.kind, "string");
});

test("C-like profiles cover directives, nested comments, regexes, raw strings and annotations", () => {
  const csharp = tokenKinds('#nullable enable\nvar text = @"one\ntwo";', "csharp");
  assert.ok(csharp.includes("directive"));
  assert.ok(csharp.filter((kind) => kind === "string").length >= 2);

  const rust = tokenKinds("/* outer /* nested */\nstill */ fn main<'a>(r#type: &'a str) { println!(r#\"ok\"#); }", "rust");
  assert.ok(rust.filter((kind) => kind === "comment").length >= 2);
  assert.ok(rust.includes("macro"));
  assert.ok(rust.includes("string"));
  assert.ok(rust.includes("decorator"));

  const rustRaw = `r${"#".repeat(17)}"value"${"#".repeat(17)}`;
  const rustRawDocument = syntaxEngine.createDocument(rustRaw, "rust");
  assert.deepEqual(rustRawDocument.lines[0].tokens, [{ from: 0, to: rustRaw.length, kind: "string" }]);

  const rustNotEqual = syntaxEngine.createDocument("let different = left != right;", "rust");
  assert.ok(!rustNotEqual.tokens().some((token) => rustNotEqual.text.slice(token.from, token.to) === "left!"));
  assert.equal(rustNotEqual.tokens().find((token) => rustNotEqual.text.slice(token.from, token.to) === "!=")?.kind, "operator");

  const javascript = tokenKinds('const pattern = /a+[0-9]/gi;\nconst text = `one\ntwo`;', "javascript");
  assert.ok(javascript.includes("regexp"));
  assert.ok(javascript.filter((kind) => kind === "string").length >= 2);

  const typescript = tokenKinds("interface User { name: string; active: boolean }", "typescript");
  assert.ok(typescript.includes("keyword"));
  assert.ok(typescript.includes("type"));

  const java = tokenKinds('@Override\npublic String value() { return """one\ntwo"""; }', "java");
  assert.ok(java.includes("decorator"));
  assert.ok(java.includes("function"));
  assert.ok(java.filter((kind) => kind === "string").length >= 2);

  const postfix = syntaxEngine.createDocument("const value = item++ / width / scale;", "javascript");
  const slashes = postfix.tokens().filter((token) => postfix.text.slice(token.from, token.to) === "/");
  assert.deepEqual(slashes.map((token) => token.kind), ["operator", "operator"]);

  const afterControl = syntaxEngine.createDocument("if (ok) /x/.test(value);", "javascript");
  assert.equal(
    afterControl.tokens().find((token) => afterControl.text.slice(token.from, token.to) === "/x/")?.kind,
    "regexp",
  );

  const keywordMember = syntaxEngine.createDocument("const value = object.default / 2 / other;", "javascript");
  assert.equal(
    keywordMember.tokens().find((token) => keywordMember.text.slice(token.from, token.to) === "default")?.kind,
    "identifier",
  );
  assert.deepEqual(
    keywordMember.tokens()
      .filter((token) => keywordMember.text.slice(token.from, token.to) === "/")
      .map((token) => token.kind),
    ["operator", "operator"],
  );

  const astral = syntaxEngine.createDocument("const icon = 😀;", "javascript");
  const emoji = astral.tokens().find((token) => astral.text.slice(token.from, token.to) === "😀");
  assert.deepEqual([emoji?.kind, emoji?.to - emoji?.from], ["operator", 2]);
  assert.equal(astral.lines[0].tokens.map((token) => astral.lines[0].text.slice(token.from, token.to)).join(""), astral.lines[0].text);

  const nonSealed = syntaxEngine.createDocument("public non-sealed class Child {}", "java");
  for (const keyword of ["non", "sealed"]) {
    assert.equal(nonSealed.tokens().find((token) => nonSealed.text.slice(token.from, token.to) === keyword)?.kind, "keyword");
  }

  for (const language of ["csharp", "rust", "javascript", "typescript", "java"]) {
    const invalidSuffix = syntaxEngine.createDocument("let value = 123hello;", language);
    assert.ok(!invalidSuffix.tokens().some((token) => invalidSuffix.text.slice(token.from, token.to) === "123hello" && token.kind === "number"));
  }

  const rustMultiline = syntaxEngine.createDocument('let value = "one\ntwo";', "rust");
  assert.deepEqual(rustMultiline.lexicalDiagnostics(), []);
  assert.equal(rustMultiline.lines[1].tokens[0]?.kind, "string");

  for (const language of ["javascript", "typescript", "python"]) {
    const continued = syntaxEngine.createDocument('value = "one\\\ntwo";', language);
    assert.deepEqual(continued.lexicalDiagnostics(), [], language);
    assert.equal(continued.lines[1].tokens[0]?.kind, "string");
  }

  for (const language of ["csharp", "rust", "python"]) {
    const dollar = syntaxEngine.createDocument("$name", language);
    assert.ok(!dollar.tokens().some((token) => dollar.text.slice(token.from, token.to) === "$name" && token.kind === "identifier"));
  }

  const matrix = syntaxEngine.createDocument("result = left @right", "python");
  assert.equal(matrix.tokens().find((token) => matrix.text.slice(token.from, token.to) === "@")?.kind, "operator");
  assert.ok(!matrix.tokens().some((token) => token.kind === "decorator"));
  const continuedMatrix = syntaxEngine.createDocument("result = (\n  left\n  @right\n)", "python");
  assert.equal(continuedMatrix.lines[2].tokens.find((token) => continuedMatrix.lines[2].text.slice(token.from, token.to) === "@")?.kind, "operator");
  assert.ok(!continuedMatrix.lines[2].tokens.some((token) => token.kind === "decorator"));
});

test("Python and SQL profiles retain multi-line lexical state", () => {
  const python = tokenKinds('@logged\nasync def work():\n    text = f"""one\ntwo"""', "python");
  assert.ok(python.includes("decorator"));
  assert.ok(python.includes("keyword"));
  assert.ok(python.includes("function"));
  assert.ok(python.filter((kind) => kind === "string").length >= 2);

  const sql = tokenKinds('SELECT "name", $$one\ntwo$$ FROM users -- note', "sql");
  assert.ok(sql.filter((kind) => kind === "keyword").length >= 2);
  assert.ok(sql.includes("identifier"));
  assert.ok(sql.filter((kind) => kind === "string").length >= 2);
  assert.ok(sql.includes("comment"));

  const postgresOperator = syntaxEngine.createDocument("SELECT data #>> '{path}' FROM docs", "sql");
  const hashToken = postgresOperator.tokens().find((token) => postgresOperator.text.slice(token.from, token.to) === "#>>");
  assert.equal(hashToken?.kind, "operator");
  assert.ok(tokenKinds("  # mysql comment", "sql").includes("comment"));

  const multiline = syntaxEngine.createDocument("SELECT 'one\ntwo' AS value;", "sql");
  assert.equal(multiline.lines[0].tokens.at(-1)?.kind, "string");
  assert.equal(multiline.lines[1].tokens[0]?.kind, "string");
  assert.deepEqual(multiline.lexicalDiagnostics(), []);
});

test("fallback validation reports unterminated multi-line constructs", async () => {
  assert.equal((await syntaxEngine.validate("/* open", "rust"))[0]?.code, "unterminated-comment");
  assert.equal((await syntaxEngine.validate("<!-- open", "xml"))[0]?.code, "xml-unterminated-comment");
  assert.equal((await syntaxEngine.validate("<root>", "xml"))[0]?.code, "xml-unclosed-tag");
});

test("XML, YAML and INI profiles classify their structural syntax", () => {
  const xml = tokenKinds('<root enabled="yes">\n<!-- note --><child><![CDATA[value]]></child>\n</root>', "xml");
  assert.ok(xml.filter((kind) => kind === "tag").length >= 4);
  assert.ok(xml.includes("attribute"));
  assert.ok(xml.includes("comment"));
  assert.ok(xml.includes("string"));

  const yaml = tokenKinds("enabled: true\ntext: |\n  first\n  second\ncount: 2", "yaml");
  assert.ok(yaml.filter((kind) => kind === "property").length >= 3);
  assert.ok(yaml.includes("boolean"));
  assert.ok(yaml.filter((kind) => kind === "string").length >= 2);
  assert.ok(yaml.includes("number"));

  const ini = tokenKinds("[main]\nenabled = true\nname = value ; note", "ini");
  assert.ok(ini.includes("section"));
  assert.ok(ini.filter((kind) => kind === "property").length >= 2);
  assert.ok(ini.includes("boolean"));
  assert.ok(ini.includes("comment"));

  const escapedIni = syntaxEngine.createDocument('key = "a\\\\" ; comment', "ini");
  assert.equal(escapedIni.lines[0].tokens.find((token) => token.kind === "comment")?.kind, "comment");
});

test("XML structural validation handles prolog, DTD, entities and root constraints", async () => {
  const valid = '<?xml version="1.0"?><!DOCTYPE root [<!ELEMENT root (#PCDATA)><!ENTITY local "ok">]><root>&local;</root>';
  assert.deepEqual(await syntaxEngine.validate(valid, "xml"), []);
  assert.deepEqual(await syntaxEngine.validate(
    '<!DOCTYPE r [<!-- ] > --><!ELEMENT r EMPTY><!ATTLIST r id ID #IMPLIED>]><r/>',
    "xml",
  ), []);
  for (const source of [
    "<!DOCTYPE r [<!ELEMENT r ((header|body)+,footer?)>]><r/>",
    "<!DOCTYPE r [<!ELEMENT r (#PCDATA|em|strong)*>]><r/>",
    "<!DOCTYPE r [<!ELEMENT r (#PCDATA)*>]><r/>",
    '<!DOCTYPE r [<!ATTLIST r kind (x | y) "x" mode NOTATION (gif|jpeg) #IMPLIED>]><r/>',
    '<!DOCTYPE r [<!ENTITY image SYSTEM "image.bin" NDATA binary>]><r/>',
    '<!DOCTYPE r [<!ENTITY first "&second;"><!ENTITY second "ok">]><r>&first;</r>',
    '<!DOCTYPE r [<!ENTITY value "ok"><!ATTLIST r a CDATA "&value;">]><r/>',
    '<!DOCTYPE r [<!ENTITY % known "">%known;]><r/>',
  ]) {
    assert.deepEqual(await syntaxEngine.validate(source, "xml"), [], source);
  }
  assert.deepEqual(await syntaxEngine.validate("<á/>", "xml"), []);

  const invalid = [
    ["<a/><b/>", "xml-multiple-roots"],
    ["hello<a/>tail", "xml-text-outside-root"],
    ["<a x=bare></a>", "xml-unquoted-attribute"],
    ["<a/><?xml version=\"1.0\"?>", "xml-invalid-declaration"],
    ["<!doctype a><a/>", "xml-invalid-declaration"],
    ["<!DOCTYPE a><b/>", "xml-doctype-root-mismatch"],
    ["<!DOCTYPE r LOL><r/>", "xml-invalid-doctype"],
    ["<!DOCTYPE r SYSTEM><r/>", "xml-invalid-doctype"],
    ["<!DOCTYPE r [ garbage ]><r/>", "xml-invalid-doctype-subset"],
    ["<!DOCTYPE r [<!ENTITY local>]><r/>", "xml-invalid-doctype-subset"],
    ["<!DOCTYPE r [<!ATTLIST r garbage>]><r/>", "xml-invalid-doctype-subset"],
    ["<!DOCTYPE r [<!ELEMENT r (garbage ???)>]><r/>", "xml-invalid-doctype-subset"],
    ["<!DOCTYPE r [<!ELEMENT r (a,b|c)>]><r/>", "xml-invalid-doctype-subset"],
    ["<!DOCTYPE r [<!ELEMENT r (#PCDATA|em)>]><r/>", "xml-invalid-doctype-subset"],
    ["<!DOCTYPE r [<!ELEMENT r (#PCDATA,em)*>]><r/>", "xml-invalid-doctype-subset"],
    ["<!DOCTYPE r [<!ELEMENT r (#PCDATA|em|em)*>]><r/>", "xml-invalid-doctype-subset"],
    ['<!DOCTYPE r [<!ATTLIST r a (x||y) "x">]><r/>', "xml-invalid-doctype-subset"],
    ['<!DOCTYPE r [<!ENTITY x "foo" NDATA n>]><r/>', "xml-invalid-doctype-subset"],
    ['<!DOCTYPE r [<!ENTITY x SYSTEM "foo" NDATA n>]><r>&x;</r>', "xml-unparsed-entity-reference"],
    ['<!DOCTYPE r [<!ENTITY x "&bogus;">]><r>&x;</r>', "xml-undeclared-entity"],
    ['<!DOCTYPE r [<!ENTITY x "&#0;">]><r/>', "xml-invalid-character-reference"],
    ['<!DOCTYPE r [<!ATTLIST r a CDATA "&bogus;">]><r/>', "xml-undeclared-entity"],
    ["<!DOCTYPE r [%missing;]><r/>", "xml-undeclared-parameter-entity"],
    ['<!DOCTYPE r [<!ENTITY % p "%later;"><!ENTITY % later "">]><r/>', "xml-undeclared-parameter-entity"],
    ["<!DOCTYPE r [<!ELEMENT r EMPTY\u00a0>]><r/>", "xml-invalid-doctype-subset"],
    ['<!DOCTYPE r [<!-- <!ENTITY hidden "ok"> -->]><r>&hidden;</r>', "xml-undeclared-entity"],
    ["<a>&bogus;</a>", "xml-undeclared-entity"],
    ["<a>&#x110000;</a>", "xml-invalid-character-reference"],
    ["<a>bad]]></a>", "xml-invalid-cdata-close"],
    ["<?>\n<a/>", "xml-expected-processing-target"],
    ["<root/>\u00a0", "xml-text-outside-root"],
  ];
  for (const [source, code] of invalid) {
    assert.equal((await syntaxEngine.validate(source, "xml"))[0]?.code, code, source);
  }

  for (const source of [
    "<r><!-- open\n\nclose --></r>",
    "<r><![CDATA[open\n\nclose]]></r>",
    "<r><?work open\n\nclose?>ok</r>",
  ]) {
    assert.ok(!syntaxEngine.createDocument(source, "xml").lexicalDiagnostics().some((item) => item.code === "provider-error"));
  }
});

test("YAML retains block and quoted scalar state without misclassifying hashes", () => {
  const block = syntaxEngine.createDocument("items:\n  - |\n    first\nafter: true", "yaml");
  const blockIndicator = block.tokens().find((token) => block.text.slice(token.from, token.to) === "|");
  assert.equal(blockIndicator?.kind, "operator");
  assert.deepEqual(block.lines[2].tokens, [{ from: 0, to: 9, kind: "string" }]);
  assert.ok(block.lines[3].tokens.some((token) => token.kind === "property"));

  const plain = syntaxEngine.createDocument("value: abc#def", "yaml");
  const hashScalar = plain.tokens().find((token) => plain.text.slice(token.from, token.to) === "abc#def");
  assert.equal(hashScalar?.kind, "text");
  assert.ok(!plain.tokens().some((token) => token.kind === "comment"));

  const quoted = syntaxEngine.createDocument([
    'double: "first',
    "  second",
    '  third"',
    "single: 'one",
    "  two",
    "  three'",
    "done: true",
  ].join("\n"), "yaml");
  for (const line of [1, 2, 4, 5]) {
    assert.deepEqual(quoted.lines[line].tokens, [{
      from: 0,
      to: quoted.lines[line].text.length,
      kind: "string",
    }]);
  }
  assert.ok(quoted.lines[6].tokens.some((token) => token.kind === "property"));
  assert.ok(quoted.lines[6].tokens.some((token) => token.kind === "boolean"));
});

test("YAML recognizes mapping separators without splitting plain scalars", () => {
  const source = [
    "https://example.test/path",
    "a:1",
    "url: https://example.test/a:1",
    "message: hello true 123",
  ].join("\n");
  const document = syntaxEngine.createDocument(source, "yaml");

  assert.deepEqual(document.lines[0].tokens, [{
    from: 0,
    to: document.lines[0].text.length,
    kind: "text",
  }]);
  assert.deepEqual(document.lines[1].tokens, [{ from: 0, to: 3, kind: "text" }]);
  assert.equal(document.lines[2].tokens.find((token) => token.kind === "text")?.to, document.lines[2].text.length);

  const message = document.lines[3].tokens.find((token) => token.kind === "text");
  assert.equal(document.lines[3].text.slice(message?.from, message?.to), "hello true 123");
  assert.ok(!document.lines[3].tokens.some((token) => token.kind === "boolean" || token.kind === "number"));

  const folded = syntaxEngine.createDocument("message: hello\n  true\n  123\nafter: false", "yaml");
  for (const line of [1, 2]) {
    assert.equal(folded.lines[line].tokens.find((token) => token.kind === "text")?.kind, "text");
    assert.ok(!folded.lines[line].tokens.some((token) => token.kind === "boolean" || token.kind === "number"));
  }
  assert.ok(folded.lines[3].tokens.some((token) => token.kind === "property"));

  const sequenceMapping = syntaxEngine.createDocument("- name: hello\n  other: true", "yaml");
  assert.ok(sequenceMapping.lines[1].tokens.some((token) => token.kind === "property"));
});

test("YAML handles indicators, quoted keys and verbatim tags contextually", () => {
  const source = [
    "-",
    '"key": true',
    "---",
    "...",
    "---suffix",
    "  ---",
    "kind: !<tag:yaml.org,2002:str> value",
  ].join("\n");
  const document = syntaxEngine.createDocument(source, "yaml");

  assert.deepEqual(document.lines[0].tokens, [{ from: 0, to: 1, kind: "punctuation" }]);
  assert.equal(document.lines[1].tokens[0]?.kind, "property");
  const directives = document.tokens().filter((token) => token.kind === "directive");
  assert.deepEqual(directives.map((token) => document.text.slice(token.from, token.to)), ["---", "..."]);
  assert.ok(document.lines[4].tokens.every((token) => token.kind !== "directive"));
  assert.ok(document.lines[5].tokens.every((token) => token.kind !== "directive"));

  const tag = document.lines[6].tokens.find((token) => token.kind === "type");
  assert.equal(document.lines[6].text.slice(tag?.from, tag?.to), "!<tag:yaml.org,2002:str>");
});

test("YAML applies the 1.2 Core scalar resolver", () => {
  const samples = [
    ["true", "boolean"],
    ["FALSE", "boolean"],
    ["yes", "text"],
    ["off", "text"],
    ["null", "null"],
    ["~", "null"],
    [".inf", "number"],
    ["-.Inf", "number"],
    [".NaN", "number"],
    ["42", "number"],
    ["0x2A", "number"],
    ["0o52", "number"],
    ["1.5e2", "number"],
    ["0o7e2", "text"],
    ["+0o7", "text"],
    ["-0x2A", "text"],
    ["0b101", "text"],
    ["1_000", "text"],
  ];

  for (const [value, expectedKind] of samples) {
    const document = syntaxEngine.createDocument(`value: ${value}`, "yaml");
    const token = document.lines[0].tokens.find((candidate) => (
      document.lines[0].text.slice(candidate.from, candidate.to) === value
    ));
    assert.equal(token?.kind, expectedKind, value);
  }
});

test("YAML reports an unterminated multi-line quoted scalar at EOF", async () => {
  const source = 'value: "first\nstill open';
  const diagnostics = await syntaxEngine.validate(source, "yaml");
  assert.equal(diagnostics[0]?.code, "yaml-unterminated-quoted-scalar");
  assert.deepEqual([diagnostics[0]?.from, diagnostics[0]?.to], [source.length, source.length]);
});

test("JSON provider validates structure and formats without rounding numbers", async () => {
  const source = '{"name":"Москва","large":9007199254740993,"items":[true,null]}';
  const document = syntaxEngine.createDocument(source, "json");
  assert.ok(document.tokens().some((token) => token.kind === "property"));
  assert.deepEqual(await syntaxEngine.validate(source, "json"), []);

  const invalid = await syntaxEngine.validate('{"items":[1,]}', "json");
  assert.equal(invalid[0]?.code, "json-trailing-comma");
  assert.ok(invalid[0].from < invalid[0].to);

  const formatted = await syntaxEngine.format(source, "json", { indent: 2, finalNewline: true });
  assert.equal(formatted.ok, true);
  assert.match(formatted.text, /9007199254740993/u);
  assert.ok(formatted.text.endsWith("\n"));
  assert.deepEqual(await syntaxEngine.validate(formatted.text, "json"), []);

  for (const trailing of ["[1,]", '{"value":1,}']) {
    assert.equal(syntaxEngine.createDocument(trailing, "json").lexicalDiagnostics()[0]?.code, "json-trailing-comma");
  }


  const badUnicodeEscape = syntaxEngine.createDocument('{"x":"\\u12"}', "json");
  assert.deepEqual(
    badUnicodeEscape.lexicalDiagnostics().map((item) => item.code),
    ["json-invalid-escape"],
  );

  for (const indent of [-1, 2.5, Number.POSITIVE_INFINITY, 17]) {
    const result = await syntaxEngine.format(source, "json", { indent });
    assert.equal(result.ok, false);
    assert.equal(result.diagnostics[0]?.code, "invalid-format-options");
  }
});

test("deep JSON/XML lexical state is bounded and recovers once overflow closes", () => {
  const json = '{"a":\n'.repeat(2_049) + "0\n" + "}\n".repeat(2_049) + "true";
  assert.deepEqual(
    syntaxEngine.createDocument(json, "json").lexicalDiagnostics().map((item) => item.code),
    ["json-depth-limit", "json-unexpected-token"],
  );

  const xml = "<a>\n".repeat(2_100) + "value\n" + "</a>\n".repeat(2_100);
  assert.deepEqual(
    syntaxEngine.createDocument(xml, "xml").lexicalDiagnostics().map((item) => item.code),
    ["xml-depth-limit"],
  );
});

test("incremental edits reuse stable lines and match a cold tokenization", () => {
  const original = "let first = 1;\nlet second = 2;\nlet third = 3;\n";
  const document = syntaxEngine.createDocument(original, "javascript");
  const originalIds = document.lines.map((line) => line.id);
  const changed = original.replace("second", "renamed");
  const edit = computeTextChange(original, changed);
  assert.ok(edit);
  const update = document.applyChange(edit);

  assert.equal(update.retokenizedLines, 1);
  assert.equal(document.lines[0].id, originalIds[0]);
  assert.notEqual(document.lines[1].id, originalIds[1]);
  assert.equal(document.lines[2].id, originalIds[2]);
  assert.deepEqual(signature(document), signature(syntaxEngine.createDocument(changed, "javascript")));
});

test("state-changing edits propagate only until the cached state converges", () => {
  const original = "/* open\ninside\n*/\nconst ready = true;\n";
  const document = syntaxEngine.createDocument(original, "javascript");
  const changed = original.replace("/*", "  ");
  const update = document.applyChange(computeTextChange(original, changed));
  assert.equal(update.stabilizedAtLine, 3);
  assert.equal(update.retokenizedLines, 3);
  assert.deepEqual(signature(document), signature(syntaxEngine.createDocument(changed, "javascript")));
});

test("document offsets use UTF-16 and preserve CRLF line breaks", () => {
  const source = '"😀"\r\n{"ключ": 1}\r\n';
  const document = syntaxEngine.createDocument(source, "json");
  assert.deepEqual(document.positionAt(source.indexOf("{")), { line: 1, column: 0 });
  assert.equal(document.offsetAt({ line: 1, column: 0 }), source.indexOf("{"));
  assert.equal(document.tokens().map((token) => source.slice(token.from, token.to)).join(""), source);

  const canonical = syntaxEngine.createDocument("a\r\nb", "json");
  assert.deepEqual(canonical.positionAt(2), { line: 0, column: 1 });
  assert.throws(() => canonical.offsetAt({ line: 0, column: 2 }), /Колонка/u);
});

test("forming CRLF across an edit boundary invalidates the previous line", () => {
  const document = syntaxEngine.createDocument("\rX", "csharp");
  document.applyChange({ from: 1, to: 1, insert: "\n" });
  const cold = syntaxEngine.createDocument("\r\nX", "csharp");
  assert.deepEqual(signature(document), signature(cold));
  assert.equal(document.tokens().map((token) => document.text.slice(token.from, token.to)).join(""), "\r\nX");

  const deletion = syntaxEngine.createDocument("\rC\nD", "json");
  deletion.applyChange({ from: 1, to: 2, insert: "" });
  assert.deepEqual(signature(deletion), signature(syntaxEngine.createDocument("\r\nD", "json")));
});

test("local line splices match cold tokenization across short newline boundary edits", () => {
  const sources = ["", "a", "\r", "\n", "\r\n", "a\rX", "a\n\n", "\rC\nD", "a\r\nb"];
  const inserts = ["", "x", "\r", "\n", "\r\n", "\n\r", "😀"];
  for (const source of sources) {
    for (let from = 0; from <= source.length; from += 1) {
      for (let to = from; to <= source.length; to += 1) {
        for (const insert of inserts) {
          if (source.slice(from, to) === insert) continue;
          for (const language of ["json", "xml", "javascript"]) {
            const document = syntaxEngine.createDocument(source, language);
            document.applyChange({ from, to, insert });
            const expected = syntaxEngine.createDocument(
              `${source.slice(0, from)}${insert}${source.slice(to)}`,
              language,
            );
            assert.deepEqual(documentShape(document), documentShape(expected), JSON.stringify({ source, from, to, insert, language }));
          }
        }
      }
    }
  }
});

test("incremental and cold tokenization stay equivalent across deterministic random edits", () => {
  const samples = {
    csharp: "class Demo { string Value = \"ok\"; }\n",
    rust: "fn main() { println!(\"ok\"); }\n",
    javascript: "const value = { ok: true };\n",
    typescript: "interface Value { ok: boolean }\n",
    python: "def value():\n    return True\n",
    sql: "SELECT value FROM table_name;\n",
    json: '{"value":[true,null,1]}\n',
    xml: '<root value="1"><child /></root>\n',
    yaml: "value:\n  nested: true\n",
    java: "class Demo { boolean value = true; }\n",
    ini: "[main]\nvalue = true\n",
  };
  const inserts = ["", "x", " ", "\n", "\r\n", '"', "/*", "*/", "{}", "[]", "😀", "#"];
  let seed = 0x5eed1234;
  const random = () => {
    seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0;
    return seed / 0x1_0000_0000;
  };

  for (const [language, initial] of Object.entries(samples)) {
    const document = syntaxEngine.createDocument(initial, language);
    for (let iteration = 0; iteration < 60; iteration += 1) {
      const from = Math.floor(random() * (document.text.length + 1));
      const removed = Math.floor(random() * Math.min(5, document.text.length - from + 1));
      const edit = {
        from,
        to: from + removed,
        insert: inserts[Math.floor(random() * inserts.length)],
      };
      document.applyChange(edit);
      const cold = syntaxEngine.createDocument(document.text, language);
      assert.deepEqual(signature(document), signature(cold), `${language}, edit ${iteration}`);
    }
  }
});
