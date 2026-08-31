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
  chaos: {
    runner: 'chaos',
    environment: {
      WOODLAND_SOAK_PLAYERS: '24',
      WOODLAND_SOAK_ROUNDS: '60',
      WOODLAND_SOAK_ACTIVATION_CONCURRENCY: '8',
      WOODLAND_SOAK_RACE_CONCURRENCY: '24',
      WOODLAND_SOAK_ROUND_DELAY_MS: '0',
      WOODLAND_SOAK_TREES_PER_ROUND: '1',
      WOODLAND_SOAK_RELOAD_EVERY: '20',
      WOODLAND_SOAK_RELOAD_COUNT: '6',
      WOODLAND_SOAK_CHAOS_FAIL_BEFORE_ROUND: '10',
      WOODLAND_SOAK_CHAOS_FAIL_AFTER_SUCCESS_ROUND: '30',
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
  renewal: {
    runner: 'soak',
    environment: {
      WOODLAND_SOAK_PLAYERS: '12',
      WOODLAND_SOAK_ROUNDS: '120',
      WOODLAND_SOAK_ACTIVATION_CONCURRENCY: '3',
      WOODLAND_SOAK_RACE_CONCURRENCY: '4',
      WOODLAND_SOAK_ROUND_DELAY_MS: '1000',
      WOODLAND_SOAK_TREES_PER_ROUND: '1',
      WOODLAND_SOAK_FORCE_TREE_RENEWAL_ROUND: '10',
      WOODLAND_SOAK_FORCE_POST_CHOP_RENEWAL_ROUND: '20',
      WOODLAND_SOAK_RELOAD_EVERY: '20',
      WOODLAND_SOAK_RELOAD_COUNT: '4',
    },
  },
  regrowth: {
    runner: 'regrowth',
    environment: {},
  },
});

const RELEASE_PLAN = Object.freeze([
  'full',
  'soak',
  'burst',
  'chaos',
  'fanout',
  'reload',
  'renewal',
  'regrowth',
]);
const EXPECTED_WORLD = Object.freeze({
  schemaVersion: 2,
  protocolVersion: 2,
  network: 'regtest',
  gameId: 'woodland.sh',
  playerLevelCurve: 'woodland-xp-v1',
  maxPlayerLevel: 99,
  baseLogDropBasisPoints: 2_000,
  levelLogDropBonusBasisPoints: 200,
  levelLogDropXpThresholds: Object.freeze([1_154, 4_470, 13_363, 37_224, 101_333]),
  maxLevelLogDropBasisPoints: 3_000,
  luckWindowBasisPoints: 10_000,
  initialLuckCredit: 8_000,
  dustSats: 330,
  activeLogsPerTree: 10,
  logReservePerTree: 50_000,
  xpPerTree: 50_000,
  treeCount: 420,
});
const PLAN = (process.env.WOODLAND_OVERNIGHT_PLAN || RELEASE_PLAN.join(','))
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

function manifestSummary(manifest) {
  if (!manifest) {
    return null;
  }
  return {
    schemaVersion: manifest.schemaVersion,
    protocolVersion: manifest.protocolVersion,
    network: manifest.network,
    gameId: manifest.gameId,
    genesisTxid: manifest.genesisTxid,
    playerLevelCurve: manifest.playerLevelCurve,
    maxPlayerLevel: manifest.maxPlayerLevel,
    rates: {
      baseLogDropBasisPoints: manifest.baseLogDropBasisPoints,
      levelLogDropBonusBasisPoints: manifest.levelLogDropBonusBasisPoints,
      levelLogDropXpThresholds: manifest.levelLogDropXpThresholds,
      maxLevelLogDropBasisPoints: manifest.maxLevelLogDropBasisPoints,
      luckWindowBasisPoints: manifest.luckWindowBasisPoints,
      initialLuckCredit: manifest.initialLuckCredit,
    },
    dustSats: manifest.dustSats,
    activeLogsPerTree: manifest.activeLogsPerTree,
    logReservePerTree: manifest.logReservePerTree,
    xpPerTree: manifest.xpPerTree,
    treeCount: Array.isArray(manifest.trees) ? manifest.trees.length : null,
  };
}

function releaseManifestErrors(manifest) {
  if (!manifest) {
    return ['world manifest is missing after a successful cycle'];
  }
  const actual = manifestSummary(manifest);
  const errors = [];
  for (const [field, expected] of Object.entries(EXPECTED_WORLD)) {
    const value = field === 'treeCount' ? actual.treeCount : manifest[field];
    if (JSON.stringify(value) !== JSON.stringify(expected)) {
      errors.push(`${field}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(value)}`);
    }
  }
  if (!/^[0-9a-f]{64}$/u.test(actual.genesisTxid ?? '')) {
    errors.push(`genesisTxid: expected 64 lowercase hex characters, got ${JSON.stringify(actual.genesisTxid)}`);
  }
  return errors;
}

async function cycleArtifactErrors(profileConfig, paths, reports) {
  const errors = releaseManifestErrors(reports.manifest);
  if (profileConfig.runner === 'full') {
    try {
      const junit = await readFile(paths.junit, 'utf8');
      if (!junit.includes('<testsuite')) {
        errors.push('full-cycle JUnit report has no testsuite');
      }
    } catch {
      errors.push('full-cycle JUnit report is missing');
    }
  }
  if (['soak', 'chaos'].includes(profileConfig.runner)) {
    if (!reports.soak) {
      errors.push('soak report is missing');
    } else if (reports.soak.profile !== profileConfig.runner) {
      errors.push(
        `soak report profile: expected ${profileConfig.runner}, got ${reports.soak.profile}`,
      );
    }
  }
  if (profileConfig.runner === 'regrowth' && !reports.regrowth) {
    errors.push('regrowth report is missing');
  }
  return errors;
}

async function runCycle(cycle, profile) {
  const cycleName = `cycle-${String(cycle).padStart(4, '0')}-${profile}`;
  const cycleDir = path.join(outputRoot, cycleName);
  const logPath = path.join(cycleDir, 'cycle.log');
  const soakReport = path.join(cycleDir, 'soak-report.json');
  const regrowthReport = path.join(cycleDir, 'regrowth-report.json');
  const junitReport = path.join(cycleDir, 'e2e-junit.xml');
  const paths = {
    cycleDir,
    log: logPath,
    soak: soakReport,
    regrowth: regrowthReport,
    junit: junitReport,
    world: path.join(cycleDir, 'world.json'),
  };
  await mkdir(cycleDir, { recursive: true });
  const log = createWriteStream(logPath, { flags: 'wx' });
  const cycleStartedAt = Date.now();
  const startedAt = new Date(cycleStartedAt).toISOString();
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
      WOODLAND_REGROWTH_REPORT: regrowthReport,
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
  const [manifest, soak, regrowth] = await Promise.all([
    readJson(paths.world),
    readJson(paths.soak),
    readJson(paths.regrowth),
  ]);
  const reports = { manifest, soak, regrowth };
  const artifactErrors = outcome.code === 0
    ? await cycleArtifactErrors(profileConfig, paths, reports)
    : [];
  const error = outcome.error ?? (artifactErrors.length > 0 ? artifactErrors.join('; ') : null);
  return {
    cycle,
    profile,
    runner: profileConfig.runner,
    status: interrupted ? 'interrupted' : (outcome.code === 0 && !error ? 'passed' : 'failed'),
    exitCode: outcome.code,
    signal: outcome.signal,
    error,
    startedAt,
    finishedAt: new Date().toISOString(),
    elapsedSeconds: Number(((Date.now() - cycleStartedAt) / 1000).toFixed(1)),
    environment: profileConfig.environment,
    world: manifestSummary(manifest),
    artifacts: {
      directory: path.relative(ROOT, paths.cycleDir),
      log: path.relative(ROOT, paths.log),
      junit: path.relative(ROOT, paths.junit),
      worldManifest: path.relative(ROOT, paths.world),
      soakReport: soak ? path.relative(ROOT, paths.soak) : null,
      regrowthReport: regrowth ? path.relative(ROOT, paths.regrowth) : null,
    },
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
    const profileConfig = PROFILE_CONFIGS[profile];
    summary.activeCycle = {
      cycle,
      profile,
      runner: profileConfig.runner,
      startedAt: new Date().toISOString(),
    };
    await atomicSummary();
    const result = await runCycle(cycle, profile);
    result.freeDiskGbAfter = await availableDiskGb();
    summary.cycles.push(result);
    summary.activeCycle = null;
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
