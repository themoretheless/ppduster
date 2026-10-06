import assert from "node:assert/strict";
import { access, readFile, readdir } from "node:fs/promises";
import test from "node:test";

test("Sites build contains the Vue client and Worker entry", async () => {
  await Promise.all([
    access("dist/client/index.html"),
    access("dist/server/index.js"),
    access("dist/.openai/hosting.json"),
  ]);

  const html = await readFile("dist/client/index.html", "utf8");
  assert.match(html, /Peregon/);
  assert.match(html, /assets\//);
});

test("client build exposes project and direct package versions", async () => {
  const { version: projectVersion } = JSON.parse(
    await readFile("package.json", "utf8"),
  );
  const assetNames = await readdir("dist/client/assets");
  const scriptName = assetNames.find((name) => /^index-.*\.js$/.test(name));
  assert.ok(scriptName, "client entry script is missing");

  const script = await readFile(`dist/client/assets/${scriptName}`, "utf8");
  for (const expected of [
    "peregon",
    projectVersion,
    "@peregon/syntax-engine",
    "vue",
    "peregon_engine",
    "themoretheless-tokenizer",
  ]) {
    assert.match(script, new RegExp(expected.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")));
  }
});
