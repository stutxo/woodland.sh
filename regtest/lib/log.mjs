// Tiny colored logger — mirrors the green timestamped lines the old bash
// scripts produced, so CI output stays familiar.
const GREEN = '\x1b[0;32m';
const YELLOW = '\x1b[0;33m';
const RED = '\x1b[0;31m';
const RESET = '\x1b[0m';

function stamp() {
  return new Date().toISOString().slice(11, 19);
}

export function log(msg) {
  console.log(`${GREEN}[${stamp()}] ${msg}${RESET}`);
}

export function warn(msg) {
  console.log(`${YELLOW}[${stamp()}] WARNING: ${msg}${RESET}`);
}

// Aborts the current run. Throws a marked error rather than calling
// process.exit() so the event loop can drain pending handles first (calling
// process.exit() with pending timers crashes libuv on Windows). The top-level
// handler in regtest.mjs prints nothing extra for marked errors and sets a
// non-zero exit code.
export function fail(msg) {
  console.error(`${RED}[${stamp()}] ERROR: ${msg}${RESET}`);
  const err = new Error(msg);
  err.handled = true;
  throw err;
}
