#!/usr/bin/env node
import { copyFile, mkdir, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const [rawManifestPath, rawOutputPath] = process.argv.slice(2);
if (!rawManifestPath || !rawOutputPath) {
  throw new Error('usage: node scripts/assemble-web.mjs <world-manifest> <output-directory>');
}

const manifestPath = path.resolve(rawManifestPath);
const outputPath = path.resolve(rawOutputPath);
const manifestText = await readFile(manifestPath, 'utf8');
const manifest = JSON.parse(manifestText);
if (manifest.schemaVersion !== 1 || manifest.protocolVersion !== 1 || manifest.gameId !== 'woodland.sh') {
  throw new Error('web bundle requires a woodland.sh protocol v1 manifest');
}

const arkade = new URL(manifest.arkadeServiceUrl);
const emulator = new URL(manifest.emulatorUrl);
if (!['http:', 'https:'].includes(arkade.protocol) || !['http:', 'https:'].includes(emulator.protocol)) {
  throw new Error('world service URLs must use HTTP or HTTPS');
}
if (manifest.network === 'bitcoin' && (arkade.protocol !== 'https:' || emulator.protocol !== 'https:')) {
  throw new Error('mainnet web bundles require HTTPS service URLs');
}

const contentSecurityPolicy = [
  "default-src 'self'",
  "script-src 'self' 'wasm-unsafe-eval'",
  "style-src 'self' 'unsafe-inline'",
  `connect-src 'self' ${arkade.origin} ${emulator.origin}`,
  "img-src 'self' data:",
  "object-src 'none'",
  "base-uri 'none'",
  "form-action 'none'",
].join('; ');
const htmlAttribute = (value) => value.replaceAll('&', '&amp;').replaceAll('"', '&quot;');
const cspMarker = '  <!-- WOODLAND_CSP -->';
const indexTemplate = await readFile(path.join(ROOT, 'web/index.html'), 'utf8');
if (!indexTemplate.includes(cspMarker)) {
  throw new Error('web/index.html is missing its CSP build marker');
}
const index = indexTemplate.replace(
  cspMarker,
  `  <meta http-equiv="Content-Security-Policy" content="${htmlAttribute(contentSecurityPolicy)}">`,
);

await mkdir(outputPath, { recursive: true });
await Promise.all([
  copyFile(path.join(ROOT, 'web/404.html'), path.join(outputPath, '404.html')),
  copyFile(path.join(ROOT, 'web/app.js'), path.join(outputPath, 'app.js')),
  writeFile(path.join(outputPath, '.nojekyll'), ''),
  writeFile(path.join(outputPath, 'index.html'), index),
  writeFile(path.join(outputPath, 'world.json'), `${JSON.stringify(manifest, null, 2)}\n`),
]);
