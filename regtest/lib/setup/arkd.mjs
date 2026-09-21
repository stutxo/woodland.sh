// arkd bring-up: create/unlock/sync the server wallet, fund it, set intent fees.
// One code path — there is no longer a "nigiri built-in arkd" variant.
import { env } from '../env.mjs';
import { log, warn, fail } from '../log.mjs';
import { waitForOrFail, fetchJson, fetchText, httpOk } from '../wait.mjs';
import { faucet, mine } from '../chain.mjs';
import { dockerExec } from '../proc.mjs';

// Host-mapped arkd endpoints. Evaluated lazily (not at import time) so they pick
// up ARKD_PORT / ARKD_ADMIN_PORT after loadEnv() has populated process.env —
// these must match the host port mappings in docker/compose.ark.yml.
const arkdUrl = () => `http://localhost:${env('ARKD_PORT', '7070')}`;
const arkdAdminUrl = () => `http://localhost:${env('ARKD_ADMIN_PORT', '7071')}`;
const JSON_HEADERS = { 'Content-Type': 'application/json' };

async function isInitialized() {
  // arkd exposes its signer pubkey on /v1/info only once the wallet is created
  // and unlocked, so its presence is a reliable "already set up" signal.
  const { json } = await fetchJson(`${arkdUrl()}/v1/info`);
  return Boolean(json && (json.signerPubkey || json.pubkey));
}

async function walletStatus() {
  const { json } = await fetchJson(`${arkdAdminUrl()}/v1/admin/wallet/status`);
  return json || {};
}

async function setupWallet() {
  const password = env('ARKD_PASSWORD', 'secret');

  // arkd only serves its admin endpoint once it has connected to arkd-wallet,
  // which in turn waits on nbxplorer + bitcoind. On cold/loaded hosts that whole
  // chain (and any Docker restart backoff while it settles) can take a while.
  await waitForOrFail('arkd admin endpoint', () =>
    httpOk(`${arkdAdminUrl()}/v1/admin/wallet/status`),
    { attempts: 120, intervalMs: 3000 },
  );

  let status = await walletStatus();
  if (!status.initialized) {
    log('Creating arkd server wallet...');
    const { json: seedResp } = await fetchJson(`${arkdAdminUrl()}/v1/admin/wallet/seed`);
    const seed = seedResp && seedResp.seed;
    if (!seed) fail('Failed to generate wallet seed');
    const { text } = await fetchText(`${arkdAdminUrl()}/v1/admin/wallet/create`, {
      method: 'POST',
      headers: JSON_HEADERS,
      body: JSON.stringify({ seed, password }),
    });
    log(`Server wallet created: ${text}`);
  } else {
    log('arkd server wallet already initialized');
  }

  status = await walletStatus();
  if (!status.unlocked) {
    log('Unlocking arkd server wallet...');
    await fetchText(`${arkdAdminUrl()}/v1/admin/wallet/unlock`, {
      method: 'POST',
      headers: JSON_HEADERS,
      body: JSON.stringify({ password }),
    });
  }

  await waitForOrFail(
    'arkd wallet sync',
    async () => (await walletStatus()).synced === true,
    { attempts: 60, intervalMs: 3000 },
  );
  log('arkd wallet synced');

  // Fund the SERVER wallet with 21 confirmed txs so fee estimation has history.
  const { json: addrResp } = await fetchJson(`${arkdAdminUrl()}/v1/admin/wallet/address`);
  const serverAddr = addrResp && addrResp.address;
  if (!serverAddr) {
    warn('Could not get arkd server wallet address; skipping funding');
    return;
  }
  log(`Funding arkd server wallet at ${serverAddr} (21 txs for fee estimation)...`);
  for (let i = 0; i < 21; i++) faucet(serverAddr, 1);
  mine(1); // confirm the batch (faucet no longer mines per-tx)
  const { text: balance } = await fetchText(`${arkdAdminUrl()}/v1/admin/wallet/balance`);
  log(`Server wallet balance: ${balance}`);
}

const FEE_FREE = {
  offchainInputFee: '0.0',
  onchainInputFee: '0.0',
  offchainOutputFee: '0.0',
  onchainOutputFee: '0.0',
};

async function postIntentFees(fees) {
  const result = await fetchText(`${arkdAdminUrl()}/v1/admin/intentFees`, {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify({ fees }),
  });
  if (!result.ok) {
    throw new Error(`Failed to set arkd intent fees (HTTP ${result.status}): ${result.text}`, {
      cause: result.error,
    });
  }
}

export async function applyArkdFees() {
  log('Configuring arkd intent fees...');
  const fees = {
    offchainInputFee: env('ARK_OFFCHAIN_INPUT_FEE'),
    onchainInputFee: env('ARK_ONCHAIN_INPUT_FEE'),
    offchainOutputFee: env('ARK_OFFCHAIN_OUTPUT_FEE'),
    onchainOutputFee: env('ARK_ONCHAIN_OUTPUT_FEE'),
  };
  await postIntentFees(fees);
  const { ok, status, json } = await fetchJson(`${arkdAdminUrl()}/v1/admin/intentFees`);
  if (!ok) {
    throw new Error(`Failed to read back arkd intent fees (HTTP ${status})`);
  }
  if (!json?.fees || Object.entries(fees).some(([field, value]) => json.fees[field] !== value)) {
    throw new Error(`arkd intent fee readback does not match configured fees: ${JSON.stringify(json)}`);
  }
  log(`arkd fees configured: ${JSON.stringify(json)}`);
}

// Initialize the `ark` client CLI inside the arkd container so `ark receive`,
// `ark balance`, `ark send`, etc. work out of the box, and seed it with offchain
// funds via a server-issued credit note. Redeeming a note registers an intent
// that must cover the offchain input fee, so we zero the fee for the redeem —
// setupArkd applies the real fees immediately afterward.
async function setupArkClient() {
  const password = env('ARKD_PASSWORD', 'secret');
  const explorer = env('ARK_CLIENT_EXPLORER', 'http://mempool_web/api');

  if (dockerExec('arkd', ['ark', 'config'], { capture: true }).code !== 0) {
    log('Initializing ark client CLI...');
    const init = dockerExec(
      'arkd',
      // This runs INSIDE the arkd container, where arkd always listens on the
      // fixed internal port 7070 — NOT the host-mapped ARKD_PORT. (The explorer
      // URL is likewise an in-network container address.) Don't use arkdUrl()
      // here; that's the host-side mapping, used only by the fetch() calls above.
      ['ark', 'init', '--password', password, '--server-url', 'http://localhost:7070', '--explorer', explorer],
      { capture: true },
    );
    if (init.code !== 0) {
      warn(`ark client init failed: ${init.stderr || init.stdout}`);
      return;
    }
  } else {
    log('ark client already initialized');
  }

  // Idempotent: skip funding if the client already holds offchain balance.
  let total = 0;
  try {
    total = JSON.parse(dockerExec('arkd', ['ark', 'balance'], { capture: true }).stdout).offchain_balance.total;
  } catch {
    /* not funded yet */
  }
  if (total > 0) {
    log('ark client wallet already funded');
    return;
  }

  log('Funding ark client wallet via a server credit note...');
  // Intent fees are already zeroed by setupArkd for the whole funding phase.
  const note = dockerExec('arkd', ['arkd', 'note', '--amount', '100000000'], { capture: true }).stdout.trim();
  if (note) {
    const redeem = dockerExec('arkd', ['ark', 'redeem-notes', '-n', note, '--password', password], { capture: true });
    if (redeem.code !== 0) warn(`ark client redeem-notes failed: ${redeem.stderr || redeem.stdout}`);
  } else {
    warn('Failed to create credit note for ark client funding');
  }
}

export async function setupArkd() {
  if (await isInitialized()) {
    log('arkd wallet already initialized, skipping wallet setup...');
  } else {
    await setupWallet();
  }
  // Zero intent fees while the seeded Ark client redeems its funding note.
  await postIntentFees(FEE_FREE);
  await setupArkClient(); // init + fund (with fees zeroed)
}
