#!/usr/bin/env node
import { spawn } from 'node:child_process';
import { mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const reportPath = path.resolve(
  ROOT,
  process.env.WOODLAND_E2E_JUNIT || 'regtest/_build/e2e-junit.xml',
);
const stages = [
  ['tree browser protocol', 'scripts/e2e-tree-regtest.mjs'],
  ['tree batch renewal', 'scripts/e2e-renewal-regtest.mjs'],
  ['multiplayer convergence and races', 'scripts/e2e-multiplayer-regtest.mjs'],
];
const results = [];

const escapeXml = (value) => String(value)
  .replaceAll('&', '&amp;')
  .replaceAll('<', '&lt;')
  .replaceAll('>', '&gt;')
  .replaceAll('"', '&quot;')
  .replaceAll("'", '&apos;');

function runStage(name, script) {
  const started = Date.now();
  return new Promise((resolve) => {
    const child = spawn(process.execPath, [path.join(ROOT, script)], {
      cwd: ROOT,
      env: process.env,
      stdio: 'inherit',
    });
    child.on('error', (error) => resolve({
      name,
      seconds: (Date.now() - started) / 1000,
      error: error.message,
    }));
    child.on('exit', (code, signal) => resolve({
      name,
      seconds: (Date.now() - started) / 1000,
      error: code === 0 ? null : `exited with ${signal || `status ${code}`}`,
    }));
  });
}

for (const [name, script] of stages) {
  const result = await runStage(name, script);
  results.push(result);
  if (result.error) break;
}

const failures = results.filter((result) => result.error).length;
const seconds = results.reduce((total, result) => total + result.seconds, 0);
const cases = results.map((result) => {
  const failure = result.error
    ? `<failure message="${escapeXml(result.error)}"/>`
    : '';
  return `  <testcase classname="woodland.regtest" name="${escapeXml(result.name)}" time="${result.seconds.toFixed(3)}">${failure}</testcase>`;
}).join('\n');
const report = [
  '<?xml version="1.0" encoding="UTF-8"?>',
  `<testsuite name="woodland regtest" tests="${results.length}" failures="${failures}" time="${seconds.toFixed(3)}">`,
  cases,
  '</testsuite>',
  '',
].join('\n');
await mkdir(path.dirname(reportPath), { recursive: true });
await writeFile(reportPath, report);

if (failures) process.exitCode = 1;
