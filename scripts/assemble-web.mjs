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
if (
  manifest.schemaVersion !== 3
  || manifest.protocolVersion !== 3
  || manifest.gameId !== 'woodland.sh'
  || manifest.rulesetId !== 'woodland.sh/forest/v3'
  || !/^[0-9a-f]{64}$/.test(manifest.deployerSigner || '')
  || !/^[0-9a-f]{128}$/.test(manifest.manifestSignature || '')
) {
  throw new Error('web bundle requires a signed woodland.sh protocol v3 schema 3 manifest');
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
const htmlAttribute = (value) => value.replaceAll('&', '&amp;').replaceAll('"', '&quot;');
const cspMarker = '  <!-- WOODLAND_CSP -->';
const serverMarker = '  <!-- WOODLAND_SERVER -->';
const indexTemplate = await readFile(path.join(ROOT, 'web/index.html'), 'utf8');
if (!indexTemplate.includes(cspMarker) || !indexTemplate.includes(serverMarker)) {
  throw new Error('web/index.html is missing a web configuration marker');
}
const index = indexTemplate
  .replace(
    cspMarker,
    `  <meta http-equiv="Content-Security-Policy" content="${htmlAttribute(contentSecurityPolicy)}">`,
  )
  .replace(
    serverMarker,
    `  <meta name="woodland-server" content="${htmlAttribute(sameOriginServer ? 'self' : server?.origin || '')}">`,
  );

await mkdir(outputPath, { recursive: true });
await Promise.all([
  copyFile(path.join(ROOT, 'web/404.html'), path.join(outputPath, '404.html')),
  copyFile(path.join(ROOT, 'web/app.js'), path.join(outputPath, 'app.js')),
  writeFile(path.join(outputPath, '.nojekyll'), ''),
  writeFile(path.join(outputPath, 'index.html'), index),
  writeFile(path.join(outputPath, 'world.json'), `${JSON.stringify(manifest, null, 2)}\n`),
]);
