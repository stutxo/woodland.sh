#!/usr/bin/env node
import { spawn } from 'node:child_process';
import { createWriteStream } from 'node:fs';
import { mkdir, readFile, rename, statfs, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const HOURS = numberSetting('WOODLAND_OVERNIGHT_HOURS', 8, 0.01, 72);
const MAX_CYCLES = integerSetting('WOODLAND_OVERNIGHT_CYCLES', 0, 0, 10_000);
const MIN_FREE_GB = numberSetting('WOODLAND_OVERNIGHT_MIN_FREE_GB', 5, 1, 100);
const COOLDOWN_SECONDS = integerSetting('WOODLAND_OVERNIGHT_COOLDOWN_SECONDS', 5, 0, 3_600);
const SOAK_PLAYERS = integerSetting('WOODLAND_OVERNIGHT_PLAYERS', 12, 2, 32);
const SOAK_ROUNDS = integerSetting('WOODLAND_OVERNIGHT_ROUNDS', 50, 1, 1_000);
const SOAK_ACTIVATION_CONCURRENCY = integerSetting(
  'WOODLAND_OVERNIGHT_ACTIVATION_CONCURRENCY',
  3,
  1,
  8,
);
const SOAK_RACE_CONCURRENCY = integerSetting(
  'WOODLAND_OVERNIGHT_RACE_CONCURRENCY',
  Math.min(4, SOAK_PLAYERS),
  1,
  SOAK_PLAYERS,
);
const SOAK_ROUND_DELAY_MS = integerSetting(
  'WOODLAND_OVERNIGHT_ROUND_DELAY_MS',
  1_000,
  0,
  60_000,
);
const MEMPOOL_WEB_PORT = integerSetting(
  'WOODLAND_OVERNIGHT_MEMPOOL_WEB_PORT',
  13_000,
  1_024,
  65_535,
);
const PROFILE_CONFIGS = Object.freeze({
  full: { runner: 'full', environment: {} },
  soak: { runner: 'soak', environment: {} },
  burst: {
    runner: 'soak',
    environment: {
      WOODLAND_SOAK_PLAYERS: '24',
      WOODLAND_SOAK_ROUNDS: '100',
      WOODLAND_SOAK_ACTIVATION_CONCURRENCY: '8',
      WOODLAND_SOAK_RACE_CONCURRENCY: '24',
      WOODLAND_SOAK_ROUND_DELAY_MS: '0',
      WOODLAND_SOAK_TREES_PER_ROUND: '1',
      WOODLAND_SOAK_RELOAD_EVERY: '25',
      WOODLAND_SOAK_RELOAD_COUNT: '6',
    },
  },
  fanout: {
    runner: 'soak',
    environment: {
      WOODLAND_SOAK_PLAYERS: '24',
      WOODLAND_SOAK_ROUNDS: '80',
      WOODLAND_SOAK_ACTIVATION_CONCURRENCY: '8',
      WOODLAND_SOAK_RACE_CONCURRENCY: '24',
      WOODLAND_SOAK_ROUND_DELAY_MS: '100',
      WOODLAND_SOAK_TREES_PER_ROUND: '4',
      WOODLAND_SOAK_RELOAD_EVERY: '20',
      WOODLAND_SOAK_RELOAD_COUNT: '6',
    },
  },
  reload: {
    runner: 'soak',
    environment: {
      WOODLAND_SOAK_PLAYERS: '12',
      WOODLAND_SOAK_ROUNDS: '100',
      WOODLAND_SOAK_ACTIVATION_CONCURRENCY: '4',
      WOODLAND_SOAK_RACE_CONCURRENCY: '8',
      WOODLAND_SOAK_ROUND_DELAY_MS: '500',
      WOODLAND_SOAK_TREES_PER_ROUND: '2',
      WOODLAND_SOAK_RELOAD_EVERY: '20',
      WOODLAND_SOAK_RELOAD_COUNT: '12',
    },
  },
  regrowth: {
    runner: 'soak',
    environment: {
      WOODLAND_SOAK_PLAYERS: '12',
      WOODLAND_SOAK_ROUNDS: '120',
      WOODLAND_SOAK_ACTIVATION_CONCURRENCY: '3',
      WOODLAND_SOAK_RACE_CONCURRENCY: '4',
      WOODLAND_SOAK_ROUND_DELAY_MS: '1000',
      WOODLAND_SOAK_TREES_PER_ROUND: '1',
      WOODLAND_SOAK_RELOAD_EVERY: '20',
      WOODLAND_SOAK_RELOAD_COUNT: '4',
    },
  },
});
const PLAN = (process.env.WOODLAND_OVERNIGHT_PLAN || 'full,soak')
  .split(',')
  .map((profile) => profile.trim())
  .filter(Boolean);
if (!PLAN.length || PLAN.some((profile) => !Object.hasOwn(PROFILE_CONFIGS, profile))) {
  throw new Error(
    `WOODLAND_OVERNIGHT_PLAN must contain only ${Object.keys(PROFILE_CONFIGS).join(', ')}`,
  );
}

const runId = new Date().toISOString().replaceAll(':', '-').replaceAll('.', '-');
const outputRoot = path.resolve(
  ROOT,
  process.env.WOODLAND_OVERNIGHT_OUTPUT || `regtest/_build/overnight/${runId}`,
);
const summaryPath = path.join(outputRoot, 'overnight-summary.json');
const startedAt = Date.now();
const deadline = startedAt + HOURS * 60 * 60 * 1_000;
const summary = {
  status: 'running',
  startedAt: new Date(startedAt).toISOString(),
  plannedHours: HOURS,
  maxCycles: MAX_CYCLES || null,
  plan: PLAN,
  profileConfigs: Object.fromEntries(
    [...new Set(PLAN)].map((profile) => [profile, PROFILE_CONFIGS[profile]]),
  ),
  outputRoot,
  config: {
    soakPlayers: SOAK_PLAYERS,
    soakRounds: SOAK_ROUNDS,
    soakActivationConcurrency: SOAK_ACTIVATION_CONCURRENCY,
    soakRaceConcurrency: SOAK_RACE_CONCURRENCY,
    soakRoundDelayMs: SOAK_ROUND_DELAY_MS,
    minimumFreeDiskGb: MIN_FREE_GB,
    cooldownSeconds: COOLDOWN_SECONDS,
  },
  cycles: [],
};

let activeChild = null;
let interrupted = false;
for (const signal of ['SIGINT', 'SIGTERM']) {
  process.on(signal, () => {
    interrupted = true;
    if (activeChild && activeChild.exitCode === null) activeChild.kill(signal);
  });
}

function numberSetting(name, fallback, minimum, maximum) {
  const value = Number(process.env[name] || fallback);
  if (!Number.isFinite(value) || value < minimum || value > maximum) {
    throw new Error(`${name} must be a number from ${minimum} to ${maximum}`);
  }
  return value;
}

function integerSetting(name, fallback, minimum, maximum) {
  const value = numberSetting(name, fallback, minimum, maximum);
  if (!Number.isInteger(value)) throw new Error(`${name} must be an integer`);
  return value;
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function atomicSummary() {
  const temporary = `${summaryPath}.tmp`;
  await writeFile(temporary, `${JSON.stringify(summary, null, 2)}\n`);
  await rename(temporary, summaryPath);
}

async function availableDiskGb() {
  const info = await statfs(ROOT, { bigint: true });
  return Number(info.bavail * info.bsize) / (1024 ** 3);
}

async function readJson(filename) {
  try {
    return JSON.parse(await readFile(filename, 'utf8'));
  } catch {
    return null;
  }
}

async function runCycle(cycle, profile) {
  const cycleName = `cycle-${String(cycle).padStart(4, '0')}-${profile}`;
  const cycleDir = path.join(outputRoot, cycleName);
  const logPath = path.join(cycleDir, 'cycle.log');
  const soakReport = path.join(cycleDir, 'soak-report.json');
  const junitReport = path.join(cycleDir, 'e2e-junit.xml');
  await mkdir(cycleDir, { recursive: true });
  const log = createWriteStream(logPath, { flags: 'wx' });
  const cycleStartedAt = Date.now();
  const profileConfig = PROFILE_CONFIGS[profile];
  const baseSoakEnvironment = {
    WOODLAND_SOAK_PLAYERS: String(SOAK_PLAYERS),
    WOODLAND_SOAK_ROUNDS: String(SOAK_ROUNDS),
    WOODLAND_SOAK_ACTIVATION_CONCURRENCY: String(SOAK_ACTIVATION_CONCURRENCY),
    WOODLAND_SOAK_RACE_CONCURRENCY: String(SOAK_RACE_CONCURRENCY),
    WOODLAND_SOAK_ROUND_DELAY_MS: String(SOAK_ROUND_DELAY_MS),
    WOODLAND_SOAK_TREES_PER_ROUND: '1',
    WOODLAND_SOAK_RELOAD_EVERY: '0',
    WOODLAND_SOAK_RELOAD_COUNT: String(Math.min(4, SOAK_PLAYERS)),
  };
  console.log(`\n=== Overnight ${cycleName} ===`);
  const child = spawn('./scripts/test-regtest.sh', [profileConfig.runner], {
    cwd: ROOT,
    env: {
      ...process.env,
      MEMPOOL_WEB_PORT: String(MEMPOOL_WEB_PORT),
      WOODLAND_E2E_ARTIFACT_DIR: cycleDir,
      WOODLAND_E2E_JUNIT: junitReport,
      WOODLAND_SOAK_REPORT: soakReport,
      ...baseSoakEnvironment,
      ...profileConfig.environment,
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  activeChild = child;
  const copy = (chunk, stream) => {
    log.write(chunk);
    stream.write(chunk);
  };
  child.stdout.on('data', (chunk) => copy(chunk, process.stdout));
  child.stderr.on('data', (chunk) => copy(chunk, process.stderr));
  const outcome = await new Promise((resolve) => {
    child.once('error', (error) => resolve({ code: null, signal: null, error: error.message }));
    child.once('exit', (code, signal) => resolve({ code, signal, error: null }));
  });
  activeChild = null;
  await new Promise((resolve) => log.end(resolve));
  return {
    cycle,
    profile,
    status: outcome.code === 0 ? 'passed' : interrupted ? 'interrupted' : 'failed',
    startedAt: new Date(cycleStartedAt).toISOString(),
    finishedAt: new Date().toISOString(),
    durationMs: Date.now() - cycleStartedAt,
    exitCode: outcome.code,
    signal: outcome.signal,
    error: outcome.error,
    logPath,
    soak: profileConfig.runner === 'soak' ? await readJson(soakReport) : null,
    junitPath: profileConfig.runner === 'full' ? junitReport : null,
  };
}

await mkdir(outputRoot, { recursive: true });
await atomicSummary();
let cycle = 0;
try {
  while (!interrupted && Date.now() < deadline && (!MAX_CYCLES || cycle < MAX_CYCLES)) {
    const freeDiskGb = await availableDiskGb();
    if (freeDiskGb < MIN_FREE_GB) {
      throw new Error(
        `only ${freeDiskGb.toFixed(2)} GiB free; refusing next cycle below ${MIN_FREE_GB} GiB`,
      );
    }
    cycle += 1;
    const profile = PLAN[(cycle - 1) % PLAN.length];
    const result = await runCycle(cycle, profile);
    result.freeDiskGbAfter = await availableDiskGb();
    summary.cycles.push(result);
    await atomicSummary();
    if (result.status !== 'passed') {
      throw new Error(`overnight ${result.status} in cycle ${cycle} (${profile})`);
    }
    if (COOLDOWN_SECONDS && Date.now() < deadline && (!MAX_CYCLES || cycle < MAX_CYCLES)) {
      await sleep(COOLDOWN_SECONDS * 1_000);
    }
  }
  summary.status = interrupted ? 'interrupted' : 'passed';
} catch (error) {
  summary.status = interrupted ? 'interrupted' : 'failed';
  summary.error = error instanceof Error ? error.message : String(error);
  process.exitCode = 1;
} finally {
  summary.finishedAt = new Date().toISOString();
  summary.durationMs = Date.now() - startedAt;
  summary.completedCycles = summary.cycles.filter((cycleResult) => (
    cycleResult.status === 'passed'
  )).length;
  await atomicSummary();
  console.log(`\nOvernight status: ${summary.status}`);
  console.log(`Summary: ${summaryPath}`);
}
