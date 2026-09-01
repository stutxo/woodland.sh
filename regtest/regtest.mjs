#!/usr/bin/env node
// woodland.sh's zero-dependency Arkade regtest orchestrator.
import { loadEnv, env } from './lib/env.mjs';
import { log, fail } from './lib/log.mjs';
import { ROOT, composeUp, composeStop, composeDown, ALL_PROFILES } from './lib/compose.mjs';
import { docker } from './lib/proc.mjs';
import { waitFor, waitForOrFail, httpOk, fetchJson } from './lib/wait.mjs';
import { bitcoinCli, bootstrapChain, mine } from './lib/chain.mjs';
import { setupArkd, applyArkdFees } from './lib/setup/arkd.mjs';

const PROFILE_DEPS = {
  base: [],
  ark: ['base'],
  emulator: ['ark'],
};

function resolveProfiles(requested) {
  const profiles = new Set();
  const add = (profile) => {
    if (profiles.has(profile)) return;
    if (!(profile in PROFILE_DEPS)) fail(`unknown profile "${profile}"`);
    profiles.add(profile);
    PROFILE_DEPS[profile].forEach(add);
  };
  requested.forEach(add);
  return [...profiles];
}

function parseArgs(argv) {
  const options = {
    command: argv[0],
    env: '',
    clean: false,
    prune: false,
    profiles: [],
    positional: [],
  };
  for (let index = 1; index < argv.length; index += 1) {
    const argument = argv[index];
    if (argument === '--env') options.env = argv[++index] || fail('--env requires a path');
    else if (argument === '--clean') options.clean = true;
    else if (argument === '--prune') options.prune = true;
    else if (argument === '--profile') {
      const value = argv[++index] || fail('--profile requires a name');
      options.profiles.push(...value.split(',').map((profile) => profile.trim()).filter(Boolean));
    } else options.positional.push(argument);
  }
  return options;
}

async function startEmulator() {
  const port = env('EMULATOR_PORT', '7073');
  log(`Starting emulator (${env('EMULATOR_IMAGE')})...`);
  composeUp(['emulator'], { profiles: ['base', 'ark', 'emulator'] });
  const ready = await waitFor(
    'emulator /v1/info',
    () => httpOk(`http://localhost:${port}/v1/info`, 2000),
    { attempts: 120, intervalMs: 2000 },
  );
  if (!ready) {
    const state = docker(
      ['inspect', '--format', '{{.State.Status}} exit={{.State.ExitCode}} error={{.State.Error}}', 'emulator'],
      { capture: true },
    );
    const logs = docker(['logs', '--tail', '200', 'emulator'], { capture: true });
    console.error(`emulator container: ${state.stdout || state.stderr || 'unavailable'}`);
    if (logs.stdout) console.error(logs.stdout);
    if (logs.stderr) console.error(logs.stderr);
    fail('emulator /v1/info did not become ready in time');
  }
  const { json } = await fetchJson(`http://localhost:${port}/v1/info`);
  log(`Emulator ready (signerPubkey: ${json?.signerPubkey || '?'})`);
}

function banner(active) {
  console.log([
    '',
    'woodland.sh regtest ready',
    `  Arkd:     http://localhost:${env('ARKD_PORT', '7070')}`,
    `  Emulator: http://localhost:${env('EMULATOR_PORT', '7073')}`,
    `  Profiles: ${[...active].join(', ')}`,
    '',
  ].join('\n'));
}

async function clean(options) {
  log('Removing woodland.sh regtest containers and volumes...');
  composeDown({ volumes: true });
  if (options.prune) {
    docker(['image', 'prune', '-f']);
    docker(['volume', 'prune', '-f']);
  }
  log('Clean-up complete.');
}

async function start(options) {
  if (options.clean) await clean(options);
  const configured = env('REGTEST_PROFILES')
    .split(',')
    .map((profile) => profile.trim())
    .filter(Boolean);
  const requested = options.profiles.length
    ? options.profiles
    : configured.length
      ? configured
      : ALL_PROFILES;
  const active = new Set(resolveProfiles(requested));

  log(`Starting woodland.sh regtest (${[...active].join(', ')})...`);
  let result = composeUp([], { profiles: ['base'] });
  if (result.code !== 0) fail('docker compose base startup failed');

  await waitForOrFail('Bitcoin Core RPC', () => (
    bitcoinCli(['getblockchaininfo'], { capture: true }).code === 0
  ));
  await bootstrapChain();
  await waitForOrFail('mempool Esplora API', () => (
    httpOk(`http://localhost:${env('MEMPOOL_WEB_PORT', '3000')}/api/blocks/tip/height`)
  ), { attempts: 60, intervalMs: 3000 });

  if (active.has('ark')) {
    result = composeUp([], { profiles: ['base', 'ark'] });
    if (result.code !== 0) fail('docker compose ark startup failed');
    // Adding Ark services may recreate bitcoind. Core does not auto-load an
    // existing wallet after that restart, so restore it before funding arkd.
    await bootstrapChain();
    await setupArkd();
  }
  if (active.has('emulator')) await startEmulator();
  if (active.has('ark')) await applyArkdFees();
  banner(active);
}

async function main() {
  const argv = process.argv.slice(2);
  if (argv[0] === 'ark' || argv[0] === 'arkd') {
    process.exitCode = docker(['exec', 'arkd', ...argv]).code;
    return;
  }
  if (argv[0] === 'rpc') {
    process.exitCode = docker([
      'exec',
      'bitcoin',
      'bitcoin-cli',
      '-regtest',
      '-rpcuser=admin1',
      '-rpcpassword=123',
      ...argv.slice(1),
    ]).code;
    return;
  }

  const options = parseArgs(argv);
  if (!options.command) fail('usage: node regtest.mjs <start|fees|stop|clean|mine|rpc|ark|arkd>');
  loadEnv(ROOT, options.env);

  switch (options.command) {
    case 'start':
      await start(options);
      break;
    case 'fees':
      await applyArkdFees();
      break;
    case 'stop':
      log('Stopping woodland.sh regtest...');
      composeStop();
      break;
    case 'clean':
      await clean(options);
      break;
    case 'mine': {
      const blocks = Number.parseInt(options.positional[0] || '1', 10);
      if (!Number.isFinite(blocks) || blocks < 1 || !mine(blocks)) fail('mine failed');
      log(`Mined ${blocks} block(s)`);
      break;
    }
    default:
      fail(`unknown command: ${options.command}`);
  }
}

main().catch((error) => {
  if (!error?.handled) console.error(error?.stack || String(error));
  process.exitCode = 1;
});
