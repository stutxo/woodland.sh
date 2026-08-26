// docker compose helpers. Both compose files are passed on every invocation as
// a single merged project ("arkade-regtest"), so services in compose.base.yml
// and compose.ark.yml share one network and resolve each other by service name.
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { docker } from './proc.mjs';

const here = dirname(fileURLToPath(import.meta.url));
export const ROOT = join(here, '..');
const BASE = join(ROOT, 'docker', 'compose.base.yml');
const ARK = join(ROOT, 'docker', 'compose.ark.yml');

function baseArgs(profiles = []) {
  return [
    'compose',
    '-f', BASE,
    '-f', ARK,
    ...profiles.flatMap((p) => ['--profile', p]),
  ];
}

export function compose(args, { profiles = [], capture = false } = {}) {
  return docker([...baseArgs(profiles), ...args], { capture });
}

// `docker compose up -d [services]` — empty services list brings up everything
// not gated behind a profile.
export function composeUp(services = [], { profiles = [] } = {}) {
  return compose(['up', '-d', ...services], { profiles });
}

// Every service is profile-gated, so stop/down must enable all profiles to
// target the whole project (compose ignores profiled services otherwise).
export const ALL_PROFILES = ['base', 'ark', 'emulator'];

export function composeStop() {
  return compose(['stop'], { profiles: ALL_PROFILES });
}

export function composeDown({ volumes = false } = {}) {
  const args = ['down', '--remove-orphans'];
  if (volumes) args.push('--volumes');
  return compose(args, { profiles: ALL_PROFILES });
}

export function composePs(capture = true) {
  return compose(['ps', '--format', '{{.Names}}'], { capture });
}
