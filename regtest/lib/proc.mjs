// Thin wrappers around spawning `docker`. No third-party process libs — just
// node:child_process. All calls inherit the orchestrator's environment (which
// the env loader has already populated) so docker-compose interpolation works.
import { spawnSync } from 'node:child_process';

// Run `docker <args>`. With { capture: true } stdout/stderr are returned as
// trimmed strings; otherwise they stream to the terminal.
export function docker(args, { capture = false } = {}) {
  const res = spawnSync('docker', args, {
    encoding: 'utf8',
    stdio: capture ? ['ignore', 'pipe', 'pipe'] : 'inherit',
    env: process.env,
  });
  if (res.error) {
    return { code: 1, stdout: '', stderr: String(res.error.message || res.error) };
  }
  return {
    code: res.status ?? 1,
    stdout: (res.stdout || '').trim(),
    stderr: (res.stderr || '').trim(),
  };
}

// Run a command inside a container by name: `docker exec <container> <argv...>`.
export function dockerExec(container, argv, opts = {}) {
  return docker(['exec', container, ...argv], opts);
}
