#!/usr/bin/env bash
# Restore an exported world; never deploy, issue assets, or create signing keys.
set -Eeuo pipefail
set +x
umask 077

usage() {
    cat <<'EOF'
Usage: sudo bash restore-host-linux.sh [--port PORT] [--no-start] [--without-tunnel] BUNDLE_DIRECTORY

Restore a trusted Woodland host bundle as a fresh game installation on Ubuntu
22.04+ or Amazon Linux 2023 with systemd. Requires python3 already installed;
Amazon Linux also requires curl (either curl or curl-minimal).
Installs native build dependencies and Rust 1.92.0, builds the bundle's pinned
source as woodland-build, and preserves its exact web assets, origin and keys.
No deployment funding, asset issuance, key generation, DNS changes, or Nix needed.

A bundled Cloudflare token enables woodland-tunnel by default. --without-tunnel
keeps the server private on loopback (port 8000 by default); arrange your own
HTTPS ingress at the UNCHANGED public origin. Tunnel ingress must match --port.
Stop/quiesce the old server before the final export to avoid losing registrations;
do not leave two renewal watchers running after cutover.
Use --no-start to stage/build/preflight first, then stop the old watcher and apply
any final registry snapshot before explicitly starting the new services.

Existing deployment directories, accounts, or units are refused, not overwritten.
This is a real installer, not a dry run. Only use bundles you trust: checksums
protect integrity, not authenticity, and bundled source/tools will be executed.
Builds use one job to limit memory; allow sufficient disk and RAM for release LTO.
On failure, staged/restored files and any started services are retained for repair.
Do not rerun over partial state or delete it blindly. The original bundle is never
modified. Inspect the reported phase and systemd journal before manual recovery.

Daily root-private backups in /var/backups/woodland retain the latest 30 archives.
Each standard host tar archive includes the unchanged original bundle plus current
config, registry, binaries and web files. The original bundle's registry is still
export-time state: recover the current registry/config from the archive, not that
stale copy. Copy backups off-instance securely; they contain the operational key.

Options:
  --no-start        Build/install/preflight, but do not enable/start services.
  --without-tunnel  Do not install/start Cloudflare Tunnel even if a token exists.
  --port PORT      Private loopback listener; use 8090 when 8000 is occupied.
  -h, --help        Show this help without changing the host.
EOF
}

fail() { printf 'Restore error: %s\n' "$*" >&2; exit 1; }
phase='argument checks'
stage=/var/lib/woodland-restore
build_home=/var/lib/woodland-build
mutated=0
on_exit() {
    local status=$?
    if (( status != 0 )); then
        printf '\nRestore failed during: %s (exit %s).\n' "$phase" "$status" >&2
        if (( mutated )); then
            printf 'State is preserved in %s, %s, /etc/woodland and /opt/woodland where created.\n' "$stage" "$build_home" >&2
            printf '%s\n' 'Started services are NOT stopped automatically; do not leave a second old-host watcher running.' \
                'Inspect: sudo systemctl status woodland-server woodland-renewal woodland-tunnel woodland-backup.timer' \
                'Logs: sudo journalctl -u woodland-server -u woodland-renewal -u woodland-tunnel -u woodland-backup --since today' \
                'Fix the reported failure manually; the installer deliberately refuses to overwrite partial restores.' >&2
            printf 'Read-only preflight diagnostics, if created: %s/status.out, status.err and signer.err (root-private).\n' "$stage" >&2
        fi
    fi
}
trap on_exit EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

without_tunnel=0
no_start=0
port=8000
bundle=''
while (( $# )); do
    case "$1" in
        -h|--help) usage; exit 0 ;;
        --without-tunnel) without_tunnel=1 ;;
        --no-start) no_start=1 ;;
        --port) (( $# >= 2 )) || fail '--port requires a value.'; port=$2; shift ;;
        --) shift; (( $# == 1 )) || fail 'Expected one bundle directory after --.'; bundle=$1; shift; break ;;
        -*) fail "Unknown option: $1" ;;
        *) [[ -z "$bundle" ]] || fail 'Expected only one bundle directory.'; bundle=$1 ;;
    esac
    shift
done
[[ -n "$bundle" ]] || { usage >&2; exit 2; }
(( EUID == 0 )) || fail 'Run this installer explicitly with sudo.'
[[ "$port" =~ ^[1-9][0-9]{3,4}$ ]] && (( port >= 1024 && port <= 65535 )) || fail 'Port must be an integer from 1024 to 65535.'
export PATH=/usr/sbin:/usr/bin:/sbin:/bin
command -v python3 >/dev/null || fail 'Install python3 before running the installer.'
script_dir=$(dirname -- "$(realpath -- "${BASH_SOURCE[0]}")")
helper=$script_dir/host-bundle.py
[[ -f "$helper" ]] || fail 'Missing sibling host-bundle.py; keep the exported tools layout intact.'
bundle=$(realpath -e -- "$bundle")
phase='read-only bundle validation'
python3 "$helper" validate "$bundle"

phase='read-only Linux platform and collision checks'
platform=$(python3 - <<'PY'
import pathlib, re
values = {}
for line in pathlib.Path('/etc/os-release').read_text().splitlines():
    if '=' in line:
        key, value = line.split('=', 1)
        values[key] = value.strip('"\'')
version = values.get('VERSION_ID', '')
if values.get('ID') == 'ubuntu' and re.fullmatch(r'\d+\.\d+', version) and tuple(map(int, version.split('.'))) >= (22, 4):
    platform = 'ubuntu'
elif values.get('ID') == 'amzn' and version == '2023':
    platform = 'amazon'
else:
    raise SystemExit('Restore requires Ubuntu 22.04+ or Amazon Linux 2023.')
if pathlib.Path('/proc/1/comm').read_text().strip() != 'systemd' or not pathlib.Path('/run/systemd/system').is_dir():
    raise SystemExit('Restore requires systemd running as PID 1, not merely an installed systemctl.')
print(platform)
PY
)
package_manager=apt-get
[[ "$platform" != amazon ]] || package_manager=dnf
for command in systemctl journalctl "$package_manager" runuser flock; do
    command -v "$command" >/dev/null || fail "Required host utility is missing: $command"
done
if [[ "$platform" == amazon ]]; then
    command -v curl >/dev/null || fail 'Install curl or curl-minimal before running this installer on Amazon Linux.'
fi
# The lock is the first mutation, after all bundle validation and platform checks.
exec 9>/run/lock/woodland-restore.lock
flock -n 9 || fail 'Another Woodland restore is running.'
for path in /opt/woodland /etc/woodland /var/lib/woodland-server "$stage" "$build_home" /var/backups/woodland; do
    [[ ! -e "$path" && ! -L "$path" ]] || fail "Existing deployment path refused: $path"
done
units=(woodland-server.service woodland-renewal.service woodland-tunnel.service woodland-backup.service woodland-backup.timer)
for unit in "${units[@]}"; do
    [[ $(systemctl show "$unit" --property=LoadState --value) == not-found ]] || fail "Existing systemd unit refused: $unit"
    for directory in /etc/systemd/system /run/systemd/system /usr/lib/systemd/system /lib/systemd/system; do
        [[ ! -e "$directory/$unit" && ! -L "$directory/$unit" && ! -e "$directory/$unit.d" ]] || fail "Existing unit path refused: $directory/$unit"
    done
done
for account in woodland woodland-server woodland-build woodland-tunnel; do
    if getent passwd "$account" >/dev/null || getent group "$account" >/dev/null; then
        fail "Existing deployment account/group refused: $account"
    fi
done
with_tunnel=0
if [[ -f "$bundle/config/cloudflared-token" ]] && (( ! without_tunnel )); then
    with_tunnel=1
    for path in /etc/apt/sources.list.d/woodland-cloudflared.list /usr/share/keyrings/woodland-cloudflare.gpg /etc/yum.repos.d/woodland-cloudflared.repo /etc/cloudflared; do
        [[ ! -e "$path" && ! -L "$path" ]] || fail "Existing Cloudflare configuration refused: $path"
    done
    [[ $(systemctl show cloudflared.service --property=LoadState --value) == not-found ]] || fail 'Existing cloudflared.service refused.'
    (( port != 2000 )) || fail 'Game port conflicts with the tunnel metrics listener.'
fi
python3 - "$with_tunnel" "$port" <<'PY'
import socket, sys
ports = [int(sys.argv[2])] + ([2000] if sys.argv[1] == '1' else [])
for port in ports:
    with socket.socket() as listener:
        try:
            listener.bind(('127.0.0.1', port))
        except OSError:
            raise SystemExit(f'Required loopback port {port} is already occupied.')
PY

phase='private bundle staging'
mutated=1
mkdir -m 0700 -- "$stage"
cp -a -- "$bundle" "$stage/bundle"
chown -R root:root "$stage/bundle"
chmod -R go-rwx "$stage/bundle"
bundle=$stage/bundle
helper=$bundle/tools/scripts/host-bundle.py
templates=$bundle/tools/mainnet
python3 "$helper" validate "$bundle"
for file in "${units[@]}" woodland-backup.sh; do
    [[ -f "$templates/$file" ]] || fail "Bundle is missing a required service template: $file"
done

phase='native dependencies and unprivileged build account'
if [[ "$platform" == ubuntu ]]; then
    apt-get update
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        build-essential pkg-config libssl-dev clang libclang-dev cmake protobuf-compiler \
        git curl ca-certificates python3 xz-utils tar util-linux
else
    dnf install -y --setopt=install_weak_deps=False gcc gcc-c++ make git \
        pkgconf-pkg-config ca-certificates python3 xz tar util-linux
fi
useradd --system --user-group --create-home --home-dir "$build_home" --shell /usr/sbin/nologin woodland-build
chmod 0700 "$build_home"
install -d -o woodland-build -g woodland-build -m 0700 "$build_home/source"
install -o woodland-build -g woodland-build -m 0600 "$bundle/source.tar.gz" "$build_home/source.tar.gz"
runuser -u woodland-build -- tar --extract --gzip --file "$build_home/source.tar.gz" \
    --directory "$build_home/source" --no-same-owner --no-same-permissions
phase='pinned Rust 1.92.0 installation and release build (woodland-build, one job)'
curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location \
    https://sh.rustup.rs --output "$build_home/rustup-init.sh"
chown woodland-build:woodland-build "$build_home/rustup-init.sh"
runuser -u woodland-build -- env -i HOME="$build_home" USER=woodland-build LOGNAME=woodland-build \
    PATH=/usr/bin:/bin CARGO_HOME="$build_home/.cargo" RUSTUP_HOME="$build_home/.rustup" \
    /bin/sh "$build_home/rustup-init.sh" -y --profile minimal --default-toolchain 1.92.0 --no-modify-path
runuser -u woodland-build -- env -i HOME="$build_home" USER=woodland-build LOGNAME=woodland-build \
    PATH="$build_home/.cargo/bin:/usr/bin:/bin" CARGO_HOME="$build_home/.cargo" RUSTUP_HOME="$build_home/.rustup" \
    LC_ALL=C.UTF-8 /bin/bash -c 'cd "$1"; exec nice -n 10 cargo +1.92.0 build --release --locked --features server --bin woodland-operator --bin woodland-server --jobs 1' \
    _ "$build_home/source"

phase='service accounts, exact web bundle and protected configuration'
useradd --system --user-group --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin woodland
useradd --system --user-group --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin woodland-server
install -d -o root -g root -m 0755 /opt/woodland
install -o root -g root -m 0755 "$build_home/source/target/release/woodland-operator" /opt/woodland/woodland-operator
install -o root -g root -m 0755 "$build_home/source/target/release/woodland-server" /opt/woodland/woodland-server
cp -a -- "$bundle/web" /opt/woodland/web
python3 - <<'PY'
import os, pathlib
root = pathlib.Path('/opt/woodland/web')
for path in [root, *root.rglob('*')]:
    os.chown(path, 0, 0)
    path.chmod(0o755 if path.is_dir() else 0o644)
PY
python3 "$helper" configure "$bundle" /etc/woodland --port "$port"
chown root:root /etc/woodland /etc/woodland/network.env /etc/woodland/woodland-world.json
chmod 0755 /etc/woodland
chmod 0644 /etc/woodland/network.env /etc/woodland/woodland-world.json
chown root:woodland /etc/woodland/operations.env
chown root:woodland-server /etc/woodland/server.env
chmod 0640 /etc/woodland/operations.env /etc/woodland/server.env
install -d -o woodland-server -g woodland-server -m 0700 /var/lib/woodland-server
if [[ -f "$bundle/state/players.json" ]]; then
    install -o woodland-server -g woodland-server -m 0600 "$bundle/state/players.json" /var/lib/woodland-server/players.json
fi

phase='read-only live world status and rollover-signer preflight'
# Load generated KEY=value data without shell execution or inherited credentials.
# Python drops uid/gid before exec; --no-start starts no transient service either.
python3 - "$stage" <<'PY'
import pathlib, re, subprocess, sys
stage = pathlib.Path(sys.argv[1])
config = pathlib.Path('/etc/woodland')
def environment(name):
    values = {'PATH': '/usr/bin:/bin', 'HOME': '/nonexistent', 'LANG': 'C.UTF-8'}
    for line in (config / name).read_text().splitlines():
        if not line or line.startswith('#'):
            continue
        key, separator, value = line.partition('=')
        if not separator or not re.fullmatch(r'WOODLAND_[A-Z_]+', key) or key in values:
            raise SystemExit('Invalid generated preflight environment; no command executed.')
        values[key] = value
    return values
def preflight(command, values, output, error_name):
    with (stage / error_name).open('wb') as errors:
        try:
            result = subprocess.run(
                ['/opt/woodland/woodland-operator', command, str(config / 'woodland-world.json')],
                env=values, cwd='/opt/woodland', user='woodland', group='woodland', extra_groups=(),
                stdin=subprocess.DEVNULL, stdout=output, stderr=errors, timeout=300)
        except subprocess.TimeoutExpired:
            raise SystemExit(f'Read-only {command} timed out; inspect protected {stage / error_name}.')
    if result.returncode:
        raise SystemExit(f'Read-only {command} failed; inspect protected {stage / error_name}. No deployment attempted.')
# Scalar 1 is PUBLIC test data, not a deployer credential. With this validated
# existing manifest status uses only its Secp context, never funding/issuance.
network = environment('network.env')
network['WOODLAND_DEPLOYER_SECRET'] = '0' * 63 + '1'
with (stage / 'status.out').open('wb') as output:
    preflight('status', network, output, 'status.err')
if (stage / 'status.out').read_text().strip() != 'ready\t-\t0':
    raise SystemExit(f'Operator status is not ready; inspect protected {stage / "status.out"}. No deployment attempted.')
# No real deployer is loaded. Rollover comes only from the protected file, never
# argv or a shell; suppress even the public address printed by renewal-address.
preflight('renewal-address', environment('operations.env'), subprocess.DEVNULL, 'signer.err')
PY

phase='service unit installation'
for unit in woodland-server.service woodland-renewal.service woodland-backup.service woodland-backup.timer; do
    install -o root -g root -m 0644 "$templates/$unit" "/etc/systemd/system/$unit"
done
install -o root -g root -m 0755 "$templates/woodland-backup.sh" /opt/woodland/woodland-backup.sh
install -d -o root -g root -m 0700 /var/backups/woodland
if (( with_tunnel )); then
    phase='Cloudflare official repository and token-file service installation'
    # Official repositories: https://pkg.cloudflare.com/index.html
    if [[ "$platform" == ubuntu ]]; then
        curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location \
            https://pkg.cloudflare.com/cloudflare-main.gpg --output "$stage/cloudflare-main.gpg"
        install -o root -g root -m 0644 "$stage/cloudflare-main.gpg" /usr/share/keyrings/woodland-cloudflare.gpg
        printf '%s\n' 'deb [signed-by=/usr/share/keyrings/woodland-cloudflare.gpg] https://pkg.cloudflare.com/cloudflared any main' \
            > /etc/apt/sources.list.d/woodland-cloudflared.list
        chmod 0644 /etc/apt/sources.list.d/woodland-cloudflared.list
        apt-get update
        DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends cloudflared
    else
        curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location \
            https://pkg.cloudflare.com/cloudflared.repo --output "$stage/cloudflared.repo"
        install -o root -g root -m 0644 "$stage/cloudflared.repo" /etc/yum.repos.d/woodland-cloudflared.repo
        dnf install -y --setopt=install_weak_deps=False cloudflared
    fi
    useradd --system --user-group --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin woodland-tunnel
    install -o root -g woodland-tunnel -m 0640 "$bundle/config/cloudflared-token" /etc/woodland/cloudflared-token
    install -o root -g root -m 0644 "$templates/woodland-tunnel.service" /etc/systemd/system/woodland-tunnel.service
fi
if [[ "$platform" == amazon ]]; then
    # Cohosted public services have no reason to access the instance-role credentials.
    for unit in woodland-server.service woodland-renewal.service woodland-tunnel.service; do
        if [[ -f "/etc/systemd/system/$unit" ]]; then
            printf '\n[Service]\nIPAddressDeny=169.254.169.254/32\nIPAddressDeny=fd00:ec2::254/128\n' \
                >> "/etc/systemd/system/$unit"
        fi
    done
fi
systemctl daemon-reload
if (( no_start )); then
    phase='staged, deliberately not started'
    printf '\n%s\n' 'Staging complete: build and read-only status/signer preflight passed.' \
        'No Woodland service has been enabled or started. Runtime readiness is NOT yet verified.' \
        'Stop the old host services (including its tunnel), install any final registry snapshot with woodland-server ownership and mode 0600, then run:' \
        '  sudo systemctl enable --now woodland-server.service woodland-renewal.service' \
        "  curl --fail http://127.0.0.1:$port/health.json" \
        '  sudo journalctl -u woodland-server -u woodland-renewal --since "5 minutes ago" --no-pager' \
        'Require recent lastRefreshAt, lastError:null, delegationAvailable:true and the expected player count.' \
        'Require watcher ready with no subsequent/unrecovered errors; v3 ready:true alone is NOT sufficient.'
    if (( with_tunnel )); then
        printf '%s\n' 'Only after local readiness is verified:' \
            '  sudo systemctl enable --now woodland-tunnel.service' \
            '  curl --fail http://127.0.0.1:2000/ready'
    else
        printf '%s\n' 'No tunnel was installed; provide HTTPS ingress at the unchanged bundle publicUrl.'
    fi
    printf '%s\n' 'Then enable backups:' \
        '  sudo systemctl start woodland-backup.service' \
        '  sudo systemctl enable --now woodland-backup.timer' \
        'Verify the unchanged public HTTPS origin before retiring the old host.'
    exit 0
fi
phase='server and renewal watcher startup and observable readiness'
printf '%s\n' 'Starting LIVE services now. The old host renewal watcher must already be stopped; never run both watchers.'
systemctl enable --now woodland-server.service woodland-renewal.service
python3 - "$bundle" "$port" <<'PY'
import json, pathlib, subprocess, sys, time, urllib.request
bundle = pathlib.Path(sys.argv[1])
local_url = f'http://127.0.0.1:{int(sys.argv[2])}'
registry = bundle / 'state/players.json'
expected_players = len(json.loads(registry.read_text())['players']) if registry.exists() else 0
expected_world = json.loads((bundle / 'web/world.json').read_text())
web_verified = False
start = time.time()
deadline = time.monotonic() + 300
stable_since = None
invocations = {}
reason = 'waiting for startup'
def properties(unit):
    output = subprocess.check_output(['systemctl', 'show', unit, '--property=ActiveState,SubState,InvocationID,NRestarts'], text=True)
    return dict(line.split('=', 1) for line in output.splitlines() if '=' in line)
while time.monotonic() < deadline:
    good = True
    for unit in ('woodland-server.service', 'woodland-renewal.service'):
        props = properties(unit)
        invocation = props.get('InvocationID')
        if props.get('NRestarts') != '0':
            raise SystemExit(f'{unit} restarted during verification; inspect its journal before cutover.')
        if props.get('ActiveState') != 'active' or props.get('SubState') != 'running' or not invocation:
            good, reason = False, f'{unit} is not running'
        elif unit in invocations and invocations[unit] != invocation:
            raise SystemExit(f'{unit} changed invocation during verification; inspect its journal.')
        else:
            invocations[unit] = invocation
    invocation = invocations.get('woodland-renewal.service')
    if invocation:
        logs = subprocess.check_output(['journalctl', '--quiet', '--no-pager', '--output=cat', f'_SYSTEMD_INVOCATION_ID={invocation}'], text=True)
        ready, error, missing_expiry = False, False, False
        for line in logs.splitlines():
            if 'woodland.sh renewal watcher ready' in line:
                ready = True  # v3 emits this EVEN AFTER an initial renewal failure.
            if 'woodland.sh renewal watcher recovered' in line:
                error = False
            if 'cannot be renewed' in line:
                missing_expiry = True
            if any(marker in line for marker in ('woodland.sh renewal watcher:', 'woodland.sh renewal reconnect:', 'woodland.sh rollover failed:', 'woodland.sh rollover paused')):
                error = True
        if not ready or error or missing_expiry:
            good, reason = False, 'watcher lacks readiness, has an unrecovered error, or reported unrenewable expiry'
    else:
        good = False
    try:
        with urllib.request.urlopen(local_url + '/health.json', timeout=5) as response:
            health = json.load(response)
        if not web_verified:
            with urllib.request.urlopen(local_url + '/world.json', timeout=5) as response:
                if json.load(response) != expected_world:
                    raise SystemExit('Server is serving a different world.json; tunnel has NOT been started.')
            web_verified = True
        refreshed = health.get('lastRefreshAt')
        healthy = (health.get('ready') is True and health.get('delegationAvailable') is True
                   and health.get('lastError') is None and isinstance(refreshed, (int, float))
                   and start - 5 <= refreshed <= time.time() + 5 and time.time() - refreshed < 120
                   and health.get('registeredPlayers') == expected_players)
        # v3 has no renewal timestamp: do not pretend ready=true proves progress.
        if 'lastRenewalAt' in health:
            renewed = health['lastRenewalAt']
            healthy = healthy and isinstance(renewed, (int, float)) and 0 <= time.time() - renewed < 120
        if not healthy:
            good, reason = False, 'health lacks fresh successful verification, delegation or the restored player count'
    except (OSError, ValueError):
        good, reason = False, 'server health endpoint is unavailable'
    if good:
        stable_since = stable_since or time.monotonic()
        if time.monotonic() - stable_since >= 15:
            break
    else:
        stable_since = None
    time.sleep(2)
else:
    raise SystemExit(f'Readiness timed out: {reason}. Tunnel has NOT been started; inspect service journals.')
print('Server refresh and restored registry verified; watcher ready without an outstanding renewal error.')
PY

if (( with_tunnel )); then
    phase='Cloudflare connection readiness (no DNS changes)'
    systemctl enable --now woodland-tunnel.service
    connected=0
    for (( attempt=0; attempt<60; attempt++ )); do
        if systemctl is-active --quiet woodland-tunnel.service && curl --fail --silent --max-time 2 http://127.0.0.1:2000/ready >/dev/null; then
            connected=1
            break
        fi
        sleep 2
    done
    (( connected )) || fail 'Cloudflare did not establish a ready connection. Local services remain running; inspect woodland-tunnel journal and existing remote ingress configuration.'
fi
phase='initial private backup and daily timer'
systemctl start woodland-backup.service
systemctl enable --now woodland-backup.timer
systemctl is-active --quiet woodland-backup.timer
phase='complete'
printf '\n%s\n' 'Restore complete. Existing world, public origin, web assets and operational signer preserved.' \
    'Status: sudo systemctl status woodland-server woodland-renewal woodland-tunnel woodland-backup.timer' \
    'Logs: sudo journalctl -u woodland-server -u woodland-renewal -u woodland-tunnel -u woodland-backup -f' \
    "Local health: curl --fail http://127.0.0.1:$port/health.json" \
    'Backups: /var/backups/woodland (root-private, latest 30). Copy securely off-instance.' \
    'After verifying the unchanged public HTTPS origin, retire the old host; never leave two watchers running.'
if (( ! with_tunnel )); then
    printf '%s\n' 'No tunnel was started. Configure your own HTTPS ingress at the unchanged bundle publicUrl.'
else
    printf '%s\n' 'Tunnel connection verified; existing Cloudflare DNS/ingress was not changed. Verify the public origin before retirement.'
fi
