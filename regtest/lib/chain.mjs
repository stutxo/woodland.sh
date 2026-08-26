// Bitcoin Core helpers — the in-house replacement for `nigiri faucet` and
// `nigiri rpc --generate`.
//
//   mine(n)            = bitcoin-cli -generate n   (to the node's default wallet)
//   faucet(addr, amt)  = bitcoin-cli sendtoaddress + mine 1 to confirm
//
// Startup and explicit funding mine synchronously. The local Compose fixture
// also runs a slow auto-miner so later batch operations can settle unattended.
import { dockerExec } from './proc.mjs';
import { log, warn } from './log.mjs';
import { waitForOrFail } from './wait.mjs';

const CLI = ['bitcoin-cli', '-regtest', '-rpcuser=admin1', '-rpcpassword=123'];

// Name used only if we have to create a wallet ourselves (see bootstrapChain).
const WALLET = 'default';

function loadedWalletCount() {
  try {
    return JSON.parse(bitcoinCli(['listwallets'], { capture: true }).stdout).length;
  } catch {
    return 0;
  }
}

export function bitcoinCli(args, opts = {}) {
  return dockerExec('bitcoin', [...CLI, ...args], opts);
}

// Spendable balance including the wallet's own unconfirmed (trusted) change —
// `getbalance "*" 0` — so chained faucet sends don't look "broke" between blocks.
export function getBalance() {
  const r = bitcoinCli(['getbalance', '*', '0'], { capture: true });
  const n = parseFloat(r.stdout);
  return Number.isFinite(n) ? n : 0;
}

export function mine(n = 1) {
  const r = bitcoinCli(['-generate', String(n)], { capture: true });
  if (r.code !== 0) warn(`mine ${n} failed: ${r.stderr || r.stdout}`);
  return r.code === 0;
}

// Ensure the node has a loaded wallet and 101+ blocks (matured coinbase).
// The bitcoind RPC answers before its wallet subsystem is fully ready, so each
// step is retried — otherwise createwallet/getnewaddress can silently fail under
// load, leaving the chain at 0 blocks (which wedges fulcrum on "downloading
// headers" and stalls the whole stack).
export async function bootstrapChain() {
  // Ensure EXACTLY ONE wallet is loaded, so bitcoin-cli (and Boltz) can route
  // wallet RPCs without an explicit -rpcwallet. The wrinkle: btcpay/Core images
  // differ — Core 30's image auto-creates the empty-named "" wallet on startup,
  // while Core 31 creates none and rejects empty names. So we first wait for the
  // image's own wallet when already present, otherwise immediately create our
  // named wallet. createwallet is retried while the wallet subsystem finishes
  // startup; waiting for Core 31 to auto-create a wallet wastes 30 seconds
  // because that image intentionally creates none.
  if (loadedWalletCount() === 0) {
    await waitForOrFail('Bitcoin Core wallet (created)', () => {
      if (loadedWalletCount() > 0) return true;
      bitcoinCli(['createwallet', WALLET], { capture: true });
      return loadedWalletCount() > 0;
    }, { attempts: 15, intervalMs: 1000 });
  }

  if (getBalance() >= 1) return;

  // 2. Get a spendable address (retry until the wallet yields one).
  let addr = '';
  await waitForOrFail('Bitcoin Core address', () => {
    addr = bitcoinCli(['getnewaddress'], { capture: true }).stdout.trim();
    return Boolean(addr);
  }, { attempts: 15, intervalMs: 1000 });

  // 3. Mine 101 blocks and confirm the chain actually advanced.
  log('Mining 101 blocks to fund the node wallet...');
  bitcoinCli(['generatetoaddress', '101', addr], { capture: true });
  await waitForOrFail('Bitcoin Core blocks', () => {
    const h = parseInt(bitcoinCli(['getblockcount'], { capture: true }).stdout, 10);
    return Number.isFinite(h) && h >= 101;
  }, { attempts: 15, intervalMs: 1000 });
}

// Send `amountBtc` to `address` from the node wallet's spendable balance. Mines
// 101 blocks to top the wallet up ONLY when it can't cover the amount — it does
// NOT mine to confirm the send. Pass { confirm: true } to mine one block so the
// send confirms immediately; otherwise it sits in the mempool until the next
// block (the auto-miner, an explicit `mine`, or a later faucet --confirm).
export function faucet(address, amountBtc, { confirm = false } = {}) {
  const amt = parseFloat(amountBtc);
  let guard = 0;
  while (getBalance() < amt && guard++ < 5) {
    log(`Node wallet has < ${amt} BTC spendable; mining 101 blocks to top up...`);
    mine(101);
  }
  const r = bitcoinCli(['sendtoaddress', address, String(amountBtc)], { capture: true });
  if (r.code !== 0) {
    warn(`faucet ${amountBtc} -> ${address} failed: ${r.stderr || r.stdout}`);
    return false;
  }
  if (confirm) mine(1);
  return true;
}
