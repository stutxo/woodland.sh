#!/usr/bin/env node
import { access, copyFile, mkdir, readFile, readdir, rm, writeFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { isDeepStrictEqual } from 'node:util';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const [rawManifestPath, rawOutputPath] = process.argv.slice(2);
if (!rawManifestPath || !rawOutputPath) {
  throw new Error('usage: node scripts/assemble-web.mjs <world-manifest> <output-directory>');
}

const manifestPath = path.resolve(rawManifestPath);
const outputPath = path.resolve(rawOutputPath);
const manifestText = await readFile(manifestPath, 'utf8');
const manifest = JSON.parse(manifestText);
const expectedAxeRecipes = [
  { axe: 'wooden', requiredLevel: 1, logCost: 1, stoneCost: 0, ironOreCost: 0 },
  { axe: 'stone', requiredLevel: 5, logCost: 2, stoneCost: 2, ironOreCost: 0 },
  { axe: 'iron', requiredLevel: 15, logCost: 5, stoneCost: 0, ironOreCost: 2 },
];
if (
  manifest.schemaVersion !== 4
  || manifest.protocolVersion !== 4
  || manifest.gameId !== 'woodland.sh'
  || manifest.rulesetId !== 'woodland.sh/forest/v4'
  || manifest.woodcuttingXpPerLog !== 25
  || !/^[0-9a-f]{68}$/.test(manifest.stoneAsset || '')
  || !/^[0-9a-f]{68}$/.test(manifest.ironOreAsset || '')
  || manifest.stoneReservePerTree !== 50_000
  || manifest.ironOreReservePerTree !== 50_000
  || manifest.stoneDropBasisPoints !== 1_000
  || manifest.ironOreDropBasisPoints !== 200
  || manifest.ironOreUnlockLevel !== 10
  || manifest.maxLogDropBasisPoints !== 3_800
  || !isDeepStrictEqual(manifest.axeRecipes, expectedAxeRecipes)
  || !/^[0-9a-f]{64}$/.test(manifest.deployerSigner || '')
  || !/^[0-9a-f]{128}$/.test(manifest.manifestSignature || '')
) {
  throw new Error('web bundle requires a signed woodland.sh protocol v4 schema 4 manifest');
}

const arkade = new URL(manifest.arkadeServiceUrl);
const emulator = new URL(manifest.emulatorUrl);
if (!['http:', 'https:'].includes(arkade.protocol) || !['http:', 'https:'].includes(emulator.protocol)) {
  throw new Error('world service URLs must use HTTP or HTTPS');
}
if (manifest.network === 'bitcoin' && (arkade.protocol !== 'https:' || emulator.protocol !== 'https:')) {
  throw new Error('mainnet web bundles require HTTPS service URLs');
}

const serverValue = (process.env.WOODLAND_SERVER_URL || '').trim();
const sameOriginServer = serverValue === 'self';
const server = serverValue && !sameOriginServer ? new URL(serverValue) : null;
if (
  server
  && (
    !['http:', 'https:'].includes(server.protocol)
    || !server.hostname
    || server.pathname !== '/'
    || server.username
    || server.password
    || server.search
    || server.hash
    || (manifest.network === 'bitcoin' && server.protocol !== 'https:')
  )
) {
  throw new Error('WOODLAND_SERVER_URL must be a canonical HTTPS origin on mainnet');
}

const contentSecurityPolicy = [
  "default-src 'self'",
  "script-src 'self' 'wasm-unsafe-eval'",
  "style-src 'self' 'unsafe-inline'",
  `connect-src 'self' ${arkade.origin} ${emulator.origin}${server ? ` ${server.origin}` : ''}`,
  "img-src 'self' data:",
  "object-src 'none'",
  "base-uri 'none'",
  "form-action 'none'",
].join('; ');

const packagePath = path.join(outputPath, 'pkg');
const generatedEntries = [
  'woodland.js',
  'woodland_bg.wasm',
  'woodland.d.ts',
  'woodland_bg.wasm.d.ts',
  'snippets',
  'package.json',
  '.gitignore',
];
let packageSource = packagePath;
try {
  await access(path.join(packageSource, 'woodland.js'));
} catch (error) {
  if (error.code !== 'ENOENT') throw error;
  // Reassembly can reuse the last addressed package after its stable inputs
  // have been removed. A fresh wasm-pack output always takes precedence.
  const previousIndex = await readFile(path.join(outputPath, 'index.html'), 'utf8');
  const previousEntry = previousIndex.match(/src="\.\/(app\.[0-9a-f]{64}\.js)"/)?.[1];
  if (!previousEntry) throw new Error('web bundle requires wasm-pack output in pkg/');
  const previousApp = await readFile(path.join(outputPath, previousEntry), 'utf8');
  const previousPackage = previousApp.match(/['"]\.\/pkg\/([0-9a-f]{64})\/woodland\.js['"]/)?.[1];
  if (!previousPackage) throw new Error('previous web entrypoint is missing its addressed package');
  packageSource = path.join(packagePath, previousPackage);
}

const packageFiles = new Map();
async function collectPackageFiles(relativePath) {
  const fullPath = path.join(packageSource, relativePath);
  let entries;
  try {
    entries = await readdir(fullPath, { withFileTypes: true });
  } catch (error) {
    if (error.code !== 'ENOTDIR') throw error;
    packageFiles.set(relativePath, await readFile(fullPath));
    return;
  }
  for (const entry of entries) {
    if (!entry.isDirectory() && !entry.isFile()) {
      throw new Error(`unsupported generated package entry: ${relativePath}/${entry.name}`);
    }
    await collectPackageFiles(`${relativePath}/${entry.name}`);
  }
}
for (const entry of generatedEntries) {
  try {
    await collectPackageFiles(entry);
  } catch (error) {
    if (error.code !== 'ENOENT' || entry === 'woodland.js' || entry === 'woodland_bg.wasm') {
      throw error;
    }
  }
}

// Hash the whole generated tree, including snippets and WASM, so an unchanged
// wrapper can never refer to changed dependencies at an already cached URL.
const packageHash = createHash('sha256');
for (const filename of [...packageFiles.keys()].sort()) {
  const content = packageFiles.get(filename);
  packageHash.update(`${filename}\0${content.length}\0`).update(content);
}
const packageDigest = packageHash.digest('hex');
const appSource = await readFile(path.join(ROOT, 'web/app.js'), 'utf8');
const packageImport = "'./pkg/woodland.js'";
if (!appSource.includes(packageImport)) {
  throw new Error('web/app.js is missing its wasm-pack import');
}
const app = appSource.replaceAll(packageImport, `'./pkg/${packageDigest}/woodland.js'`);
const appFilename = `app.${createHash('sha256').update(app).digest('hex')}.js`;
const htmlAttribute = (value) => value.replaceAll('&', '&amp;').replaceAll('"', '&quot;');
const cspMarker = '  <!-- WOODLAND_CSP -->';
const serverMarker = '  <!-- WOODLAND_SERVER -->';
const faucetMarker = '      <!-- WOODLAND_FAUCET -->';
const indexTemplate = await readFile(path.join(ROOT, 'web/index.html'), 'utf8');
if (
  !indexTemplate.includes(cspMarker)
  || !indexTemplate.includes(serverMarker)
  || !indexTemplate.includes(faucetMarker)
  || !indexTemplate.includes('src="./app.js"')
) {
  throw new Error('web/index.html is missing a web configuration marker');
}
const index = indexTemplate
  .replace('src="./app.js"', `src="./${appFilename}"`)
  .replace(
    cspMarker,
    `  <meta http-equiv="Content-Security-Policy" content="${htmlAttribute(contentSecurityPolicy)}">`,
  )
  .replace(
    serverMarker,
    `  <meta name="woodland-server" content="${htmlAttribute(sameOriginServer ? 'self' : server?.origin || '')}">`,
  )
  .replace(
    faucetMarker,
    manifest.network === 'signet' && arkade.origin === 'https://mutinynet.arkade.sh'
      ? '      <p class="muted">Need test sats? <a href="https://faucet.mutinynet.com/" target="_blank" rel="noopener noreferrer">Mutinynet faucet</a>.</p>'
      : '',
  );

async function writeAddressedFile(filename, content) {
  try {
    await writeFile(filename, content, { flag: 'wx' });
  } catch (error) {
    if (error.code !== 'EEXIST') throw error;
    // Never truncate an immutable URL while an existing page is reading it.
    const existing = await readFile(filename);
    if (!existing.equals(Buffer.isBuffer(content) ? content : Buffer.from(content))) {
      throw new Error(`content-addressed asset differs from its existing bytes: ${filename}`);
    }
  }
}

await mkdir(outputPath, { recursive: true });
await Promise.all([...packageFiles].map(async ([filename, content]) => {
  const destination = path.join(packagePath, packageDigest, filename);
  await mkdir(path.dirname(destination), { recursive: true });
  await writeAddressedFile(destination, content);
}));
await Promise.all([
  writeAddressedFile(path.join(outputPath, appFilename), app),
  copyFile(path.join(ROOT, 'web/404.html'), path.join(outputPath, '404.html')),
  writeFile(path.join(outputPath, '.nojekyll'), ''),
  writeFile(path.join(outputPath, 'world.json'), `${JSON.stringify(manifest, null, 2)}\n`),
]);
await writeFile(path.join(outputPath, 'index.html'), index);

// Keep older addressed assets for in-flight pages and leave unrelated output
// data alone. Only the superseded stable runtime paths are removed.
await rm(path.join(outputPath, 'app.js'), { force: true });
if (packageSource === packagePath) {
  await Promise.all([...packageFiles.keys()].map((filename) => (
    rm(path.join(packagePath, filename), { force: true })
  )));
}
