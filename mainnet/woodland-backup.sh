#!/usr/bin/env bash
# Standard host snapshot, including the original portable migration bundle.
set -Eeuo pipefail
set +x
umask 077
export PATH=/usr/sbin:/usr/bin:/sbin:/bin
if (( EUID != 0 )); then
    printf '%s\n' 'woodland-backup must run as root to read the protected configuration.' >&2
    exit 1
fi
exec 9>/var/backups/woodland/.backup.lock
flock -n 9 || { printf '%s\n' 'Another Woodland backup is running.' >&2; exit 1; }
python3 - <<'PY'
import datetime
import json
import os
import pathlib
import re
import shutil
import tarfile
import tempfile

backups = pathlib.Path('/var/backups/woodland')
seed = pathlib.Path('/var/lib/woodland-restore/bundle')
config = pathlib.Path('/etc/woodland')
web = pathlib.Path('/opt/woodland')
registry = pathlib.Path('/var/lib/woodland-server/players.json')
for path in (backups, seed, config, web):
    if not path.is_dir() or path.is_symlink():
        raise SystemExit(f'Required backup directory is absent or a symlink: {path}')
if backups.stat().st_uid != 0 or backups.stat().st_mode & 0o077:
    raise SystemExit('Backup directory must be owned by root and mode 0700.')
if not (seed / 'checksums.sha256').is_file() or not (config / 'woodland-world.json').is_file():
    raise SystemExit('Original migration bundle or installed manifest is incomplete; refusing backup.')
stamp = datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%S.%fZ')
archive = backups / f'woodland-{stamp}.tar.gz'
partial = backups / f'.woodland-{stamp}.partial'
try:
    with tempfile.TemporaryDirectory(prefix='.snapshot-', dir=backups) as temporary:
        snapshot = pathlib.Path(temporary)
        # Configuration is normally immutable. Registry writes use atomic rename;
        # copyfile opens one complete generation before tar reads its size/data.
        shutil.copytree(config, snapshot / 'etc/woodland', symlinks=True)
        state = snapshot / 'var/lib/woodland-server'
        state.mkdir(parents=True, mode=0o700)
        if registry.exists():
            if not registry.is_file() or registry.is_symlink():
                raise SystemExit('Player registry must be a regular file.')
            shutil.copyfile(registry, state / 'players.json')
            (state / 'players.json').chmod(0o600)
            document = json.loads((state / 'players.json').read_text())
            if document.get('schemaVersion') != 1 or not isinstance(document.get('players'), dict):
                raise SystemExit('Player registry snapshot has an unsupported shape; no backup published.')
        def private(info):
            if info.issym() or info.islnk() or not (info.isfile() or info.isdir()):
                raise ValueError(f'Unexpected link or special file in backup: {info.name}')
            info.uid = info.gid = 0
            info.uname = info.gname = 'root'
            return info
        with partial.open('xb') as output:
            with tarfile.open(fileobj=output, mode='w:gz', format=tarfile.PAX_FORMAT) as tar:
                tar.add(snapshot / 'etc/woodland', arcname='etc/woodland', filter=private)
                tar.add(state, arcname='var/lib/woodland-server', filter=private)
                tar.add(seed, arcname='var/lib/woodland-restore/bundle', filter=private)
                tar.add(web, arcname='opt/woodland', filter=private)
                for name in ('woodland-server.service', 'woodland-renewal.service', 'woodland-tunnel.service',
                             'woodland-backup.service', 'woodland-backup.timer'):
                    unit = pathlib.Path('/etc/systemd/system') / name
                    if unit.exists():
                        tar.add(unit, arcname=f'etc/systemd/system/{name}', filter=private)
            output.flush()
            os.fsync(output.fileno())
        partial.chmod(0o600)
        partial.rename(archive)
        directory_fd = os.open(backups, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
    # Only successfully published files with our exact name participate in
    # retention. Never delete arbitrary files or the last good backup on failure.
    pattern = re.compile(r'woodland-\d{8}T\d{6}\.\d{6}Z\.tar\.gz')
    completed = sorted(path for path in backups.iterdir()
                       if pattern.fullmatch(path.name) and path.is_file() and not path.is_symlink())
    for old in completed[:-30]:
        old.unlink()
    print(f'Private host backup created: {archive}; retaining latest 30 archives.')
    print('The included original migration bundle is unchanged; use the archive current registry/config for recovery.')
finally:
    partial.unlink(missing_ok=True)
PY
