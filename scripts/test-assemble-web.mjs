#!/usr/bin/env node
import assert from 'node:assert/strict';
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const temporary = await mkdtemp(path.join(os.tmpdir(), 'woodland-web-test-'));
const assembler = path.join(ROOT, 'scripts/assemble-web.mjs');
const base = {
  schemaVersion: 4,
  protocolVersion: 4,
  gameId: 'woodland.sh',
  rulesetId: 'woodland.sh/forest/v4',
  woodcuttingXpPerLog: 25,
  stoneAsset: `${'33'.repeat(32)}0300`,
  ironOreAsset: `${'33'.repeat(32)}0400`,
  stoneReservePerTree: 50_000,
  ironOreReservePerTree: 50_000,
  stoneDropBasisPoints: 1_000,
  ironOreDropBasisPoints: 200,
  ironOreUnlockLevel: 10,
  maxLogDropBasisPoints: 3_800,
  axeRecipes: [
    { axe: 'wooden', requiredLevel: 1, logCost: 1, stoneCost: 0, ironOreCost: 0 },
    { axe: 'stone', requiredLevel: 5, logCost: 2, stoneCost: 2, ironOreCost: 0 },
    { axe: 'iron', requiredLevel: 15, logCost: 5, stoneCost: 0, ironOreCost: 2 },
  ],
  deployerSigner: '11'.repeat(32),
  manifestSignature: '22'.repeat(64),
  network: 'signet',
  arkadeServiceUrl: 'https://arkade.example',
  emulatorUrl: 'https://emulator.example',
};

function run(manifestPath, outputPath, env = {}) {
  return spawnSync(process.execPath, [assembler, manifestPath, outputPath], {
    cwd: ROOT,
    encoding: 'utf8',
    env: { ...process.env, ...env },
  });
}

const generatedJs = `import { value } from './snippets/example/helper.js';
export const dependency = value;
export default () => new URL('woodland_bg.wasm', import.meta.url);
`;
const generatedWasm = Buffer.from([0, 97, 115, 109, 1, 0, 0, 0]);
async function seedPackage(outputPath, wasm = generatedWasm, snippet = 'export const value = 1;\n') {
  const pkg = path.join(outputPath, 'pkg');
  await mkdir(path.join(pkg, 'snippets/example'), { recursive: true });
  await Promise.all([
    writeFile(path.join(pkg, 'woodland.js'), generatedJs),
    writeFile(path.join(pkg, 'woodland_bg.wasm'), wasm),
    writeFile(path.join(pkg, 'snippets/example/helper.js'), snippet),
  ]);
}

async function bundlePaths(outputPath) {
  const index = await readFile(path.join(outputPath, 'index.html'), 'utf8');
  const entry = index.match(/src="\.\/(app\.[0-9a-f]{64}\.js)"/)?.[1];
  assert.ok(entry, 'HTML must load a content-addressed root entrypoint');
  const app = await readFile(path.join(outputPath, entry), 'utf8');
  assert.equal(entry, `app.${createHash('sha256').update(app).digest('hex')}.js`);
  const module = app.match(/['"]\.\/(pkg\/[0-9a-f]{64}\/woodland\.js)['"]/)?.[1];
  assert.ok(module, 'entrypoint must load an addressed wasm-pack module');
  return { entry, module, app };
}

try {
  const validManifest = path.join(temporary, 'valid.json');
  const validOutput = path.join(temporary, 'valid');
  await writeFile(validManifest, JSON.stringify(base));
  await seedPackage(validOutput);
  await writeFile(path.join(validOutput, 'app.js'), 'obsolete entrypoint');
  await writeFile(path.join(validOutput, 'keep.txt'), 'unrelated output');
  await writeFile(path.join(validOutput, 'pkg', 'keep.txt'), 'unrelated package data');
  const valid = run(validManifest, validOutput, {
    WOODLAND_SERVER_URL: 'https://server.example',
  });
  assert.equal(valid.status, 0, valid.stderr);

  const index = await readFile(path.join(validOutput, 'index.html'), 'utf8');
  const notFound = await readFile(path.join(validOutput, '404.html'), 'utf8');
  assert.match(index, /http-equiv="Content-Security-Policy"/);
  assert.ok(index.includes(base.arkadeServiceUrl));
  assert.ok(index.includes(base.emulatorUrl));
  assert.match(index, /name="woodland-server" content="https:\/\/server\.example"/);
  assert.match(index, /connect-src[^"]*https:\/\/server\.example/);
  assert.doesNotMatch(index, /WOODLAND_CSP/);
  assert.match(notFound, /href="\.\/"/);
  assert.equal(await readFile(path.join(validOutput, '.nojekyll'), 'utf8'), '');
  await assert.rejects(readFile(path.join(validOutput, '_headers')));

  const original = await bundlePaths(validOutput);
  const pageUrl = new URL('https://pages.example/project/');
  const appUrl = new URL(original.entry, pageUrl);
  const moduleUrl = new URL(`./${original.module}`, appUrl);
  assert.equal(new URL('./world.json', appUrl).href, 'https://pages.example/project/world.json');
  const moduleSource = await readFile(path.join(validOutput, original.module), 'utf8');
  assert.equal(moduleSource, generatedJs);
  const wasmUrl = new URL('woodland_bg.wasm', moduleUrl);
  const wasmPath = path.join(validOutput, wasmUrl.pathname.slice(pageUrl.pathname.length));
  assert.deepEqual(await readFile(wasmPath), generatedWasm);
  assert.equal(
    await readFile(path.join(path.dirname(wasmPath), 'snippets/example/helper.js'), 'utf8'),
    'export const value = 1;\n',
  );
  for (const obsolete of ['app.js', 'pkg/woodland.js', 'pkg/woodland_bg.wasm', 'pkg/snippets/example/helper.js']) {
    await assert.rejects(readFile(path.join(validOutput, obsolete)), { code: 'ENOENT' });
  }
  assert.equal(await readFile(path.join(validOutput, 'keep.txt'), 'utf8'), 'unrelated output');
  assert.equal(await readFile(path.join(validOutput, 'pkg/keep.txt'), 'utf8'), 'unrelated package data');

  // Reassembling an existing bundle, or rebuilding the same generated inputs,
  // must not change URLs. Fresh world data must not bust the runtime cache.
  const repeated = run(validManifest, validOutput);
  assert.equal(repeated.status, 0, repeated.stderr);
  assert.deepEqual(await bundlePaths(validOutput), original);
  await seedPackage(validOutput);
  const rebuilt = run(validManifest, validOutput);
  assert.equal(rebuilt.status, 0, rebuilt.stderr);
  assert.deepEqual(await bundlePaths(validOutput), original);
  await writeFile(validManifest, JSON.stringify({ ...base, arkadeServiceUrl: 'https://new-world.example' }));
  const refreshedWorld = run(validManifest, validOutput);
  assert.equal(refreshedWorld.status, 0, refreshedWorld.stderr);
  assert.deepEqual(await bundlePaths(validOutput), original);
  assert.equal(
    JSON.parse(await readFile(path.join(validOutput, 'world.json'), 'utf8')).arkadeServiceUrl,
    'https://new-world.example',
  );
  await writeFile(validManifest, JSON.stringify(base));

  // Changing only WASM must change both the module and entrypoint URLs, while
  // the previous URLs remain usable by pages already loading that generation.
  const changedWasm = Buffer.concat([generatedWasm, Buffer.from([0, 2, 1, 120])]);
  await seedPackage(validOutput, changedWasm);
  const wasmRebuild = run(validManifest, validOutput);
  assert.equal(wasmRebuild.status, 0, wasmRebuild.stderr);
  const withNewWasm = await bundlePaths(validOutput);
  assert.notEqual(withNewWasm.entry, original.entry);
  assert.notEqual(withNewWasm.module, original.module);
  assert.deepEqual(
    await readFile(path.join(validOutput, path.dirname(withNewWasm.module), 'woodland_bg.wasm')),
    changedWasm,
  );
  assert.deepEqual(await readFile(wasmPath), generatedWasm);
  assert.equal(await readFile(path.join(validOutput, original.entry), 'utf8'), original.app);

  // Snippet-only changes are dependencies of the generated module too.
  await seedPackage(validOutput, changedWasm, 'export const value = 2;\n');
  const snippetRebuild = run(validManifest, validOutput);
  assert.equal(snippetRebuild.status, 0, snippetRebuild.stderr);
  const withNewSnippet = await bundlePaths(validOutput);
  assert.notEqual(withNewSnippet.entry, withNewWasm.entry);
  assert.notEqual(withNewSnippet.module, withNewWasm.module);
  assert.equal(
    await readFile(path.join(validOutput, path.dirname(withNewSnippet.module), 'snippets/example/helper.js'), 'utf8'),
    'export const value = 2;\n',
  );

  const reorderedManifest = path.join(temporary, 'reordered-keys.json');
  const reorderedOutput = path.join(temporary, 'reordered-keys');
  await writeFile(reorderedManifest, JSON.stringify({
    ...base,
    axeRecipes: base.axeRecipes.map((recipe) => (
      Object.fromEntries(Object.entries(recipe).sort(([left], [right]) => left.localeCompare(right)))
    )),
  }));
  await seedPackage(reorderedOutput);
  const reordered = run(reorderedManifest, reorderedOutput);
  assert.equal(reordered.status, 0, reordered.stderr);
  assert.deepEqual(
    JSON.parse(await readFile(path.join(reorderedOutput, 'world.json'), 'utf8')),
    base,
  );

  const { logCost: _logCost, ...missingRecipeField } = base.axeRecipes[0];
  for (const [name, axeRecipes] of [
    ['changed-recipe-value', [{ ...base.axeRecipes[0], logCost: 2 }, ...base.axeRecipes.slice(1)]],
    ['missing-recipe-field', [missingRecipeField, ...base.axeRecipes.slice(1)]],
    ['extra-recipe-field', [{ ...base.axeRecipes[0], bonus: 1 }, ...base.axeRecipes.slice(1)]],
    ['reordered-tiers', [...base.axeRecipes].reverse()],
  ]) {
    const manifestPath = path.join(temporary, `${name}.json`);
    const outputPath = path.join(temporary, name);
    await writeFile(manifestPath, JSON.stringify({ ...base, axeRecipes }));
    const result = run(manifestPath, outputPath);
    assert.notEqual(result.status, 0, name);
    await assert.rejects(readFile(path.join(outputPath, 'world.json')), { code: 'ENOENT' });
  }

  const sameOriginOutput = path.join(temporary, 'same-origin');
  await seedPackage(sameOriginOutput);
  const sameOrigin = run(validManifest, sameOriginOutput, {
    WOODLAND_SERVER_URL: 'self',
  });
  assert.equal(sameOrigin.status, 0, sameOrigin.stderr);
  const sameOriginIndex = await readFile(path.join(sameOriginOutput, 'index.html'), 'utf8');
  assert.match(sameOriginIndex, /name="woodland-server" content="self"/);
  assert.doesNotMatch(sameOriginIndex, /server\.example/);

  const unconfiguredOutput = path.join(temporary, 'unconfigured');
  await seedPackage(unconfiguredOutput);
  const unconfigured = run(validManifest, unconfiguredOutput, {
    WOODLAND_SERVER_URL: '',
  });
  assert.equal(unconfigured.status, 0, unconfigured.stderr);
  const unconfiguredIndex = await readFile(path.join(unconfiguredOutput, 'index.html'), 'utf8');
  assert.match(unconfiguredIndex, /name="woodland-server" content=""/);
  assert.doesNotMatch(unconfiguredIndex, /server\.example/);

  const insecureManifest = path.join(temporary, 'insecure-mainnet.json');
  await writeFile(insecureManifest, JSON.stringify({
    ...base,
    network: 'bitcoin',
    arkadeServiceUrl: 'http://arkade.invalid',
    emulatorUrl: 'https://emulator.invalid',
  }));
  const insecure = run(insecureManifest, path.join(temporary, 'insecure'));
  assert.notEqual(insecure.status, 0);

  const insecureServerManifest = path.join(temporary, 'insecure-server.json');
  await writeFile(insecureServerManifest, JSON.stringify({
    ...base,
    network: 'bitcoin',
    arkadeServiceUrl: 'https://arkade.invalid',
    emulatorUrl: 'https://emulator.invalid',
  }));
  const insecureServer = run(
    insecureServerManifest,
    path.join(temporary, 'insecure-server'),
    { WOODLAND_SERVER_URL: 'http://server.invalid' },
  );
  assert.notEqual(insecureServer.status, 0);

  const wrongProtocol = path.join(temporary, 'wrong-protocol.json');
  await writeFile(wrongProtocol, JSON.stringify({ ...base, protocolVersion: 1 }));
  const wrong = run(wrongProtocol, path.join(temporary, 'wrong'));
  assert.notEqual(wrong.status, 0);

  const previousWorld = path.join(temporary, 'previous-world.json');
  await writeFile(previousWorld, JSON.stringify({
    ...base, schemaVersion: 3, protocolVersion: 3, rulesetId: 'woodland.sh/forest/v3',
  }));
  const previous = run(previousWorld, path.join(temporary, 'previous-world'));
  assert.notEqual(previous.status, 0);

  const wrongXpScale = path.join(temporary, 'wrong-xp-scale.json');
  await writeFile(wrongXpScale, JSON.stringify({ ...base, woodcuttingXpPerLog: 1 }));
  const wrongScale = run(wrongXpScale, path.join(temporary, 'wrong-xp-scale'));
  assert.notEqual(wrongScale.status, 0);

  const missingProgression = path.join(temporary, 'missing-progression.json');
  const { stoneAsset: _stoneAsset, ...withoutProgression } = base;
  await writeFile(missingProgression, JSON.stringify(withoutProgression));
  const missing = run(missingProgression, path.join(temporary, 'missing-progression'));
  assert.notEqual(missing.status, 0);

  console.log('GitHub Pages artifact tests passed');
} finally {
  await rm(temporary, { recursive: true, force: true });
}
