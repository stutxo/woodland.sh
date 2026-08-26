#!/usr/bin/env node
import { readFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';

const [address, rawSats = '330'] = process.argv.slice(2);
if (!address || !/^tark1[0-9a-z]{80,}$/.test(address)) {
  console.error('usage: node scripts/fund-mutinynet.mjs <tark1-address> [sats]');
  process.exit(2);
}
const sats = Number(rawSats);
if (!Number.isSafeInteger(sats) || sats <= 0 || sats > 1_000_000) {
  console.error('sats must be an integer from 1 to 1000000');
  process.exit(2);
}

const tokenPath = process.env.MUTINYNET_TOKEN_FILE
  || path.join(os.homedir(), '.mutinynet', 'token');
const token = (await readFile(tokenPath, 'utf8')).trim();
if (!token) throw new Error(`Mutinynet token is empty: ${tokenPath}`);

const response = await fetch('https://faucet.mutinynet.com/api/arkade', {
  method: 'POST',
  headers: {
    authorization: `Bearer ${token}`,
    'content-type': 'application/json',
  },
  body: JSON.stringify({ address, sats }),
});
const body = await response.text();
if (!response.ok) {
  throw new Error(`Mutinynet Arkade faucet failed (${response.status}): ${body}`);
}
const result = JSON.parse(body);
if (typeof result.txid !== 'string') throw new Error('Mutinynet faucet returned no txid');
console.log(JSON.stringify({ address, sats, txid: result.txid }));
