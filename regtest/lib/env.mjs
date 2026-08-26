// Environment loading. Mirrors the old lib/env.sh precedence:
//   .env.defaults (base)  <  first override found  <  pre-set process env
// Override discovery order (first found wins):
//   1. explicit --env <path>
//   2. ../.env.regtest   (parent repo, typical submodule case)
//   3. ./.env            (local override)
import { existsSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { log } from './log.mjs';

// Parse a dotenv-style file into [key, value] pairs. Handles `export` prefixes,
// `#` comments, blank lines, and surrounding single/double quotes. No variable
// expansion is performed (the defaults file needs none).
function parseEnvFile(path) {
  const out = {};
  for (let line of readFileSync(path, 'utf8').split(/\r?\n/)) {
    line = line.trim();
    if (!line || line.startsWith('#')) continue;
    if (line.startsWith('export ')) line = line.slice(7).trim();
    const eq = line.indexOf('=');
    if (eq === -1) continue;
    const key = line.slice(0, eq).trim();
    let val = line.slice(eq + 1).trim();
    if (
      (val.startsWith('"') && val.endsWith('"')) ||
      (val.startsWith("'") && val.endsWith("'"))
    ) {
      val = val.slice(1, -1);
    }
    out[key] = val;
  }
  return out;
}

function findOverride(root, userEnv) {
  if (userEnv && existsSync(userEnv)) return userEnv;
  const parent = join(root, '..', '.env.regtest');
  if (existsSync(parent)) return parent;
  const local = join(root, '.env');
  if (existsSync(local)) return local;
  return null;
}

// Load defaults + override into process.env. A value already present in the
// real environment (e.g. `ARKD_IMAGE=… node regtest.mjs start`) wins over the
// files, matching how an exported shell var would have survived `source`.
export function loadEnv(root, userEnv) {
  const defaultsPath = join(root, '.env.defaults');
  if (!existsSync(defaultsPath)) {
    throw new Error(`${defaultsPath} not found (run: git submodule update --init)`);
  }

  const merged = parseEnvFile(defaultsPath);

  const override = findOverride(root, userEnv);
  if (override) {
    log(`Loading overrides from ${override}`);
    Object.assign(merged, parseEnvFile(override));
  }

  for (const [k, v] of Object.entries(merged)) {
    if (process.env[k] === undefined) process.env[k] = v;
  }
}

// Convenience accessor with a fallback default.
export function env(key, fallback = '') {
  const v = process.env[key];
  return v === undefined || v === '' ? fallback : v;
}
