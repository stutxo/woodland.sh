#!/usr/bin/env python3
"""Private, offline host snapshots. Never execute environment-file contents."""

import argparse
import contextlib
import datetime
import fcntl
import gzip
import hashlib
from html.parser import HTMLParser
import json
import os
from pathlib import Path, PurePosixPath
import re
import shlex
import shutil
import signal
import stat
import subprocess
import sys
import tarfile


class BundleError(Exception):
    pass


def require(condition, message):
    if not condition:
        raise BundleError(message)


MANIFEST = Path("/etc/woodland/woodland-world.json")
WEB = Path("/opt/woodland/web")
REGISTRY = Path("/var/lib/woodland-server/players.json")
TOKEN = Path("/etc/woodland/cloudflared-token")
NIX_CONFIG = Path("/etc/nixos/configuration.nix")
NIX_MODULE = Path("/etc/nixos/woodland/host.nix")
ROLLOVER = "WOODLAND_ROLLOVER_SECRET"
VERSIONS = ("WOODLAND_EXPECTED_ARKADE_VERSION", "WOODLAND_EXPECTED_EMULATOR_VERSION")
NETWORK_FIELDS = {
    "WOODLAND_NETWORK": "network",
    "WOODLAND_ARKADE_SERVICE_URL": "arkadeServiceUrl",
    "WOODLAND_EMULATOR_URL": "emulatorUrl",
    "WOODLAND_EXPECTED_ARKADE_SIGNER": "operatorSigner",
    "WOODLAND_EXPECTED_EMULATOR_SIGNER": "emulatorSigner",
}
ENV_KEYS = set(NETWORK_FIELDS) | set(VERSIONS) | {
    ROLLOVER, "WOODLAND_WORLD_MANIFEST", "WOODLAND_SERVER_PUBLIC_URL",
    "WOODLAND_SERVER_ORIGIN", "WOODLAND_SERVER_BIND", "WOODLAND_SERVER_WEB_ROOT",
    "WOODLAND_SERVER_DB", "WOODLAND_SERVER_REFRESH_SECONDS",
}
TOOLS = (
    "scripts/export-host.sh", "scripts/retire-nixos-host.sh",
    "scripts/host-bundle.py", "scripts/restore-host-linux.sh",
    "mainnet/woodland-server.service", "mainnet/woodland-renewal.service",
    "mainnet/woodland-tunnel.service", "mainnet/woodland-backup.service",
    "mainnet/woodland-backup.timer", "mainnet/woodland-backup.sh",
)
WRITERS = ("woodland-backup.timer", "woodland-server.service")
UNITS = WRITERS + ("woodland-backup.service", "woodland-renewal.service", "woodland-tunnel.service")
SLEEP_KEYS = ("AllowSuspend", "AllowHibernation", "AllowHybridSleep", "AllowSuspendThenHibernate")
MAX_ARCHIVE = 512 * 1024 * 1024
MAX_MEMBER = 64 * 1024 * 1024


def run(argv, *, check=True, **kwargs):
    result = subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE, **kwargs)
    require(not check or result.returncode == 0,
            f"{Path(argv[0]).name} failed; command output withheld to protect credentials")
    return result


def text(path, limit=MAX_MEMBER):
    require(path.is_file() and path.stat().st_size <= limit, f"Missing or oversized file: {path.name}")
    return path.read_text(encoding="utf-8")


def unique_object(pairs):
    value = {}
    for key, item in pairs:
        require(key not in value, "Duplicate JSON key")
        value[key] = item
    return value


def read_json(path):
    value = json.loads(text(path), object_pairs_hook=unique_object)
    require(isinstance(value, dict), f"Expected JSON object: {path.name}")
    return value


def origin(value):
    # Canonical HTTPS DNS/IP origin, no path, credentials, default port, or escapes.
    from urllib.parse import urlsplit
    require(isinstance(value, str), "Public URL must be a canonical HTTPS origin")
    parts = urlsplit(value)
    require(parts.scheme == "https" and parts.hostname and not parts.username
            and not parts.password and not parts.path and not parts.query and not parts.fragment
            and parts.netloc == parts.netloc.lower() and parts.port != 443
            and (parts.port is None or 0 < parts.port < 65536)
            and re.fullmatch(r"https://[a-z0-9.\-\[\]:]+", value)
            and not parts.hostname.endswith("."), "Public URL must be a canonical HTTPS origin")
    host = parts.hostname
    authority = f"[{host}]" if ":" in host else host
    if parts.port is not None:
        authority += f":{parts.port}"
    require(parts.netloc == authority and (":" in host or all(
        re.fullmatch(r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?", label)
        for label in host.split("."))), "Public URL is not canonical")
    return value


class WebOrigin(HTMLParser):
    def __init__(self):
        super().__init__()
        self.origins = []

    def handle_starttag(self, tag, attrs):
        values = dict(attrs)
        if tag == "meta" and values.get("name") == "woodland-server":
            self.origins.append(values.get("content"))


def safe_value(value):
    require(isinstance(value, str) and re.fullmatch(r"[A-Za-z0-9_./:@+,%=\[\]~-]+", value),
            "Environment value cannot be represented safely")
    return value


def env_file(path, *, bundle=False):
    values = {}
    for line in text(path, 1024 * 1024).splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        require(re.match(r"[A-Z][A-Z0-9_]*=", line), "Unsupported environment-file syntax")
        key, raw = line.split("=", 1)
        require(key not in values, "Duplicate environment key")
        if bundle:
            require(key in {ROLLOVER, *VERSIONS}, "Bundle contains a forbidden environment key")
        fields = shlex.split(raw, comments=False, posix=True)
        require(len(fields) == 1, "Unsupported environment-file value")
        if key in ENV_KEYS:
            values[key] = safe_value(fields[0])
    return values


def write_private(path, content):
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    with path.open("x", encoding="utf-8") as stream:
        stream.write(content)
        stream.flush()
        os.fsync(stream.fileno())
    path.chmod(0o600)


def write_env(path, values):
    write_private(path, "".join(f"{key}={safe_value(value)}\n" for key, value in sorted(values.items())))


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def safe_path(name):
    require(isinstance(name, str) and len(name) <= 4096 and name
            and not any(ord(c) < 32 or ord(c) == 127 for c in name) and "\\" not in name,
            "Unsafe payload path")
    path = PurePosixPath(name)
    require(not path.is_absolute() and all(p not in ("", ".", "..") for p in name.split("/")),
            "Unsafe payload path")
    return name


def payload_files(root, *, private=True):
    require(root.is_dir() and not root.is_symlink(), "Bundle must be a real directory")
    files = {}
    for base, dirs, names in os.walk(root, followlinks=False):
        for path in [Path(base), *(Path(base) / n for n in dirs + names)]:
            mode = path.lstat().st_mode
            require(stat.S_ISDIR(mode) or stat.S_ISREG(mode), "Bundle contains a link or special file")
            if private:
                require(mode & 0o077 == 0, "Bundle permissions must exclude group and other users")
        for name in names:
            path = Path(base) / name
            relative = safe_path(path.relative_to(root).as_posix())
            files[relative] = path
    return files


def checksums(root):
    files = payload_files(root)
    content = "".join(f"{sha256(path)}  {name}\n" for name, path in sorted(files.items())
                      if name != "checksums.sha256")
    target = root / "checksums.sha256"
    temporary = root / ".checksums.tmp"
    write_private(temporary, content)
    os.replace(temporary, target)


def source_metadata(archive, revision):
    require(archive.stat().st_size <= MAX_ARCHIVE, "Source archive is too large")
    # Read to gzip EOF as tarfile alone accepts truncation after the tar end marker.
    with gzip.open(archive, "rb") as stream:
        expanded = 0
        while chunk := stream.read(1024 * 1024):
            expanded += len(chunk)
            require(expanded <= MAX_ARCHIVE, "Source archive exceeds expanded-size limit")
    content = {}
    seen = set()
    with tarfile.open(archive, "r:gz") as source:
        require(source.pax_headers.get("comment") == revision, "Source archive commit pin differs")
        for member in source:
            name = safe_path(member.name.rstrip("/"))
            require(name not in seen and len(seen) < 10000, "Duplicate or excessive source archive entries")
            seen.add(name)
            require(member.isfile() or member.isdir(), "Source archive links and special files are forbidden")
            require(0 <= member.size <= MAX_MEMBER and member.mode & 0o7000 == 0,
                    "Unsafe source archive member")
            if name in ("Cargo.toml", "src/world.rs"):
                require(member.isfile(), "Source metadata is not a regular file")
                content[name] = source.extractfile(member).read().decode("utf-8")
    require({"Cargo.toml", "Cargo.lock", "src/world.rs"}.issubset(seen), "Incomplete source archive")
    package = re.search(r"(?ms)^\[package\]\s*\n(.*?)(?=^\[|\Z)", content["Cargo.toml"])
    require(package is not None, "Unknown Cargo package metadata")
    versions = re.findall(r'^version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"\s*$', package[1], re.M)
    require(len(versions) == 1, "Unknown Cargo package version")
    constants = []
    for name in ("PROTOCOL_VERSION", "MANIFEST_SCHEMA_VERSION"):
        matches = re.findall(r"\bconst " + name + r"\s*:\s*u32\s*=\s*(\d+)\s*;", content["src/world.rs"])
        require(len(matches) == 1, "Unknown source protocol/schema metadata")
        constants.append(int(matches[0]))
    require(tuple(constants) in ((3, 3), (4, 4)) and int(versions[0].split(".")[0]) == constants[0],
            "Unsupported or inconsistent source major/protocol/schema")
    return versions[0], *constants


def network_values(manifest):
    require(manifest.get("gameId") == "woodland.sh", "Wrong manifest game")
    values = {key: safe_value(manifest.get(field)) for key, field in NETWORK_FIELDS.items()}
    require(manifest["network"] in ("bitcoin", "testnet", "testnet4", "signet", "regtest"),
            "Unsupported world network")
    for field in ("operatorSigner", "emulatorSigner", "rolloverSigner", "genesisTxid"):
        require(re.fullmatch(r"[0-9a-f]{64}", manifest.get(field, "")), "Invalid manifest identity encoding")
    return values


def validate(root):
    files = payload_files(root)
    required = {"bundle.json", "source.tar.gz", "config/woodland-world.json", "config/operations.env",
                "web/world.json", "web/index.html", "checksums.sha256", *("tools/" + n for n in TOOLS)}
    require(required <= files.keys(), "Bundle is incomplete")
    allowed_config = {"config/woodland-world.json", "config/operations.env", "config/cloudflared-token"}
    require(all(not name.startswith("config/") or name in allowed_config for name in files),
            "Bundle contains unexpected private configuration")
    indexed = {}
    for line in text(root / "checksums.sha256").splitlines():
        require(re.fullmatch(r"[0-9a-f]{64}  .+", line), "Malformed checksum index")
        digest, name = line.split("  ", 1)
        safe_path(name)
        require(name not in indexed and name != "checksums.sha256", "Duplicate checksum entry")
        indexed[name] = digest
    require(indexed.keys() == files.keys() - {"checksums.sha256"}, "Checksum inventory differs from payload")
    for name, digest in indexed.items():
        require(sha256(files[name]) == digest, f"Checksum mismatch: {name}")
    meta = read_json(root / "bundle.json")
    require(type(meta.get("formatVersion")) is int and meta["formatVersion"] == 1,
            "Unsupported bundle format")
    require(re.fullmatch(r"[0-9a-f]{40}", meta.get("sourceRevision", "")), "Invalid source revision")
    require(meta.get("rustToolchain") == "1.92.0", "Unsupported Rust toolchain")
    require(re.fullmatch(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ", meta.get("exportedAt", "")),
            "Invalid export timestamp")
    datetime.datetime.strptime(meta["exportedAt"], "%Y-%m-%dT%H:%M:%SZ")
    origin(meta.get("publicUrl"))
    manifest = read_json(root / "config/woodland-world.json")
    network_values(manifest)
    require(read_json(root / "web/world.json") == manifest,
            "Deployed web manifest differs from server manifest")
    web_origin = WebOrigin()
    web_origin.feed(text(root / "web/index.html"))
    require(len(web_origin.origins) == 1 and web_origin.origins[0] in ("self", meta["publicUrl"]),
            "Deployed web server origin differs from bundle")
    require(meta.get("genesisTxid") == manifest.get("genesisTxid"), "Wrong world genesis")
    version, protocol, schema = source_metadata(root / "source.tar.gz", meta["sourceRevision"])
    require(meta.get("packageVersion") == version and type(meta.get("protocolVersion")) is int
            and type(meta.get("schemaVersion")) is int and meta["protocolVersion"] == protocol
            and meta["schemaVersion"] == schema and type(manifest.get("protocolVersion")) is int
            and type(manifest.get("schemaVersion")) is int and manifest["protocolVersion"] == protocol
            and manifest["schemaVersion"] == schema
            and manifest.get("rulesetId") == f"woodland.sh/forest/v{protocol}",
            "Source, bundle, and manifest versions differ")
    operations = env_file(root / "config/operations.env", bundle=True)
    require(re.fullmatch(r"[0-9a-fA-F]{64}", operations.get(ROLLOVER, ""))
            and 0 < int(operations[ROLLOVER], 16) < 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141,
            "Missing or invalid rollover child key")
    if "config/cloudflared-token" in files:
        token = text(files["config/cloudflared-token"], 16384).strip()
        require(re.fullmatch(r"[A-Za-z0-9_+/=.-]+", token), "Invalid tunnel token encoding")
    if "state/players.json" in files:
        registry = read_json(files["state/players.json"])
        require(type(registry.get("schemaVersion")) is int and registry["schemaVersion"] == 1
                and isinstance(registry.get("players"), dict), "Unsupported player registry")
    return meta, manifest, operations


def configure(root, destination, port=8000):
    require(type(port) is int and 1024 <= port <= 65535, "Invalid unprivileged server port")
    meta, manifest, operations = validate(root)
    require(not destination.is_symlink(), "Configuration destination must not be a symlink")
    if destination.exists():
        require(destination.is_dir() and not any(destination.iterdir()), "Configuration destination must be empty")
    else:
        destination.mkdir(mode=0o700)
    destination.chmod(0o700)
    network = network_values(manifest) | {key: operations[key] for key in VERSIONS if key in operations}
    network["WOODLAND_WORLD_MANIFEST"] = "/etc/woodland/woodland-world.json"
    write_env(destination / "network.env", network)
    write_env(destination / "operations.env", network | {ROLLOVER: operations[ROLLOVER]})
    write_env(destination / "server.env", network | {
        ROLLOVER: operations[ROLLOVER], "WOODLAND_SERVER_PUBLIC_URL": meta["publicUrl"],
        "WOODLAND_SERVER_BIND": f"127.0.0.1:{port}", "WOODLAND_SERVER_WEB_ROOT": "/opt/woodland/web",
        "WOODLAND_SERVER_DB": "/var/lib/woodland-server/players.json", "WOODLAND_SERVER_REFRESH_SECONDS": "15",
    })
    copy_file(root / "config/woodland-world.json", destination / "woodland-world.json")


def property_value(unit, name):
    return run(["systemctl", "show", unit, "--property=" + name, "--value"]).stdout.decode().strip()


def installed_environment(unit):
    environment = {}
    for item in shlex.split(property_value(unit, "Environment")):
        key, separator, value = item.partition("=")
        require(separator, "Unsupported service environment")
        if key in ENV_KEYS:
            environment[key] = safe_value(value)
    raw = property_value(unit, "EnvironmentFiles")
    entries = re.findall(r"(\S+) \(ignore_errors=(yes|no)\)", raw)
    require(" ".join(f"{p} (ignore_errors={ignore})" for p, ignore in entries) == raw,
            "Unsupported service environment-file configuration")
    for name, optional in entries:
        path = Path(name)
        require(name in ("/etc/woodland/operations.env", "/etc/woodland/server.env", "/etc/woodland/mainnet.env"),
                "Unexpected service environment-file path")
        if optional == "yes" and not path.exists():
            continue
        environment.update(env_file(path))
    return environment


def installed_operations(manifest, public_url):
    network = network_values(manifest)
    server = installed_environment("woodland-server.service")
    renewal = installed_environment("woodland-renewal.service")
    for settings in (server, renewal):
        installed_network = settings.get("WOODLAND_NETWORK")
        installed_network = {"mutinynet": "signet", "mainnet": "bitcoin"}.get(installed_network, installed_network)
        require(installed_network == manifest["network"] and all(
            settings.get(key) == value for key, value in network.items() if key != "WOODLAND_NETWORK"),
            "Installed network/service pins differ from manifest")
        require(settings.get("WOODLAND_WORLD_MANIFEST") == str(MANIFEST), "Unexpected installed manifest path")
    require(server.get("WOODLAND_SERVER_PUBLIC_URL") == public_url, "Public URL differs from installed server origin")
    require(server.get("WOODLAND_SERVER_WEB_ROOT") == str(WEB)
            and server.get("WOODLAND_SERVER_DB") == str(REGISTRY), "Unexpected installed web or registry path")
    require(server.get("WOODLAND_SERVER_ORIGIN", public_url) == public_url,
            "Separate frontend origin requires an explicit migration plan")
    require(renewal.get(ROLLOVER) and server.get(ROLLOVER, renewal[ROLLOVER]) == renewal[ROLLOVER],
            "Installed services disagree on rollover credentials")
    for key in VERSIONS:
        require(server.get(key) == renewal.get(key), "Installed service version pins differ")
    return {key: renewal[key] for key in (ROLLOVER, *VERSIONS) if key in renewal}


def copy_file(source, destination):
    require(source.is_file(), f"Missing installed file: {source.name}")
    destination.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    with source.open("rb") as reader, destination.open("xb") as writer:
        shutil.copyfileobj(reader, writer)
        writer.flush()
        os.fsync(writer.fileno())
    destination.chmod(0o600)


def copy_web(destination):
    source = WEB.resolve(strict=True)
    files = payload_files(source, private=False)
    require(files and "world.json" in files and "index.html" in files, "Installed web bundle is incomplete")
    destination.mkdir(mode=0o700)
    for name, path in files.items():
        copy_file(path, destination / name)


@contextlib.contextmanager
def recovering():
    previous = {sig: signal.signal(sig, signal.SIG_IGN) for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)}
    try:
        yield
    finally:
        for sig, handler in previous.items():
            signal.signal(sig, handler)


def restart(units):
    failed = []
    with recovering():
        for unit in reversed(units):
            if run(["systemctl", "start", unit], check=False).returncode:
                failed.append(unit)
    require(not failed, "Recovery could not restart: " + ", ".join(failed) + "; start these units manually")


def stop_active(units, stopped):
    for unit in units:
        state = property_value(unit, "ActiveState")
        if state not in ("inactive", "failed", ""):
            # Record before stopping so even a partial stop failure is recovered.
            stopped.append(unit)
            run(["systemctl", "stop", unit])
            require(property_value(unit, "ActiveState") in ("inactive", "failed"), "Unit did not stop: " + unit)


def handoff(root):
    uid = os.environ.get("SUDO_UID")
    gid = os.environ.get("SUDO_GID")
    if uid is not None:
        require(uid.isdecimal() and gid is not None and gid.isdecimal(), "Invalid sudo ownership metadata")
        for base, dirs, files in os.walk(root):
            for path in [Path(base), *(Path(base) / name for name in dirs + files)]:
                os.chown(path, int(uid), int(gid), follow_symlinks=False)


def export(destination, revision, public_url):
    origin(public_url)
    require(re.fullmatch(r"[0-9a-fA-F]{7,40}", revision), "Revision must be an explicit commit hash")
    require(not destination.exists() and not destination.is_symlink(), "Export destination already exists")
    repository = Path(__file__).resolve().parent.parent
    git = ["git", "-c", f"safe.directory={repository}", "-C", str(repository)]
    seed = repository.parent if repository.name == "tools" else None
    if seed is not None:
        saved, _, _ = validate(seed)
        commit = saved["sourceRevision"]
        require(commit.startswith(revision.lower()), "Requested revision differs from the bundled source")
    else:
        commit = run([*git, "rev-parse", "--verify", revision + "^{commit}"]).stdout.decode().strip()
    require(re.fullmatch(r"[0-9a-f]{40}", commit), "Could not resolve source commit")
    manifest = read_json(MANIFEST)
    operations = installed_operations(manifest, public_url)
    destination.mkdir(mode=0o700)
    try:
        archive = destination / "source.tar.gz"
        if seed is not None:
            copy_file(seed / "source.tar.gz", archive)
        else:
            run([*git, "archive", "--format=tar.gz", "--output=" + str(archive), commit])
        archive.chmod(0o600)
        version, protocol, schema = source_metadata(archive, commit)
        require((manifest.get("protocolVersion"), manifest.get("schemaVersion")) == (protocol, schema),
                "Supplied source revision is incompatible with the installed world")
        copy_file(MANIFEST, destination / "config/woodland-world.json")
        write_env(destination / "config/operations.env", operations)
        if TOKEN.exists():
            copy_file(TOKEN, destination / "config/cloudflared-token")
        copy_web(destination / "web")
        for name in TOOLS:
            copy_file(repository / name, destination / "tools" / name)
            if name.endswith((".sh", ".py")):
                (destination / "tools" / name).chmod(0o700)
        stopped = []
        try:
            stop_active(WRITERS, stopped)
            if REGISTRY.exists():
                copy_file(REGISTRY, destination / "state/players.json")
        finally:
            restart(stopped)
        meta = {
            "formatVersion": 1, "sourceRevision": commit, "packageVersion": version,
            "protocolVersion": protocol, "schemaVersion": schema, "genesisTxid": manifest["genesisTxid"],
            "publicUrl": public_url, "exportedAt": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
            "rustToolchain": "1.92.0",
        }
        write_private(destination / "bundle.json", json.dumps(meta, indent=2) + "\n")
        checksums(destination)
        validate(destination)
        # Refuse a configuration change racing the snapshot, not a newer live registry.
        installed_matches(destination, registry=False)
    finally:
        # A failed partial export stays private and is never advertised as usable.
        handoff(destination)
    print("Export complete. Keep this directory private; validate it again after transfer.")


def same_optional(installed, bundled):
    return installed.exists() == bundled.exists() and (not installed.exists() or sha256(installed) == sha256(bundled))


def installed_matches(root, *, registry=True):
    meta = read_json(root / "bundle.json")
    manifest = read_json(MANIFEST)
    require(sha256(MANIFEST) == sha256(root / "config/woodland-world.json"), "Installed world differs from bundle")
    require(installed_operations(manifest, meta["publicUrl"]) == env_file(root / "config/operations.env", bundle=True),
            "Installed credentials or service pins differ from bundle; make a fresh export")
    require(same_optional(TOKEN, root / "config/cloudflared-token"), "Tunnel credentials differ; make a fresh export")
    installed_web = payload_files(WEB.resolve(strict=True), private=False)
    bundled_web = payload_files(root / "web")
    require(installed_web.keys() == bundled_web.keys()
            and all(sha256(path) == sha256(bundled_web[name]) for name, path in installed_web.items()),
            "Installed web differs from bundle; make a fresh export")
    if registry:
        require(same_optional(REGISTRY, root / "state/players.json"),
                "Registry changed since export; hosting is retained. Make and transfer a fresh export before retirement")


def replace_config(content, mode):
    temporary = NIX_CONFIG.with_name(".configuration.nix.woodland-retire")
    require(not temporary.exists() and not temporary.is_symlink(), "Retirement temporary configuration already exists")
    write_private(temporary, content)
    temporary.chmod(mode)
    os.replace(temporary, NIX_CONFIG)


def verify_retired():
    for unit in UNITS:
        require(property_value(unit, "ActiveState") in ("inactive", "failed", ""), "Hosting unit remains active: " + unit)
        require(not property_value(unit, "FragmentPath"), "Hosting unit remains installed: " + unit)
        require(property_value(unit, "UnitFileState") not in ("enabled", "enabled-runtime", "linked", "linked-runtime"),
                "Hosting unit remains enabled: " + unit)
    # systemd's own merged configuration view includes all sleep.conf.d overrides.
    sleep = run(["systemd-analyze", "cat-config", "systemd/sleep.conf"]).stdout.decode()
    for key in SLEEP_KEYS:
        require(not re.search(r"(?mi)^\s*" + key + r"\s*=\s*(?:no|false|0|off)\s*(?:#.*)?$", sleep),
                "Sleep-inhibiting override remains: " + key)
    for base in (Path("/etc/systemd/system"), Path("/run/systemd/system")):
        if base.exists():
            for directory in base.glob("*.wants"):
                require(not any((directory / unit).is_symlink() or (directory / unit).exists() for unit in UNITS),
                        "Hosting boot/timer dependency remains")


def retire(root):
    require(Path("/etc/NIXOS").exists(), "Retirement is NixOS-only")
    validate(root)
    installed_matches(root)
    require(NIX_CONFIG.is_file() and not NIX_CONFIG.is_symlink() and NIX_MODULE.is_file(),
            "Expected NixOS configuration/module is missing or indirect")
    original = text(NIX_CONFIG)
    # Only the known standalone import inside one simple imports list is editable.
    require(original.count("./woodland/host.nix") == 1, "Expected exactly one Woodland host import")
    imports = list(re.finditer(r"\bimports\s*=\s*\[([^\]]*)\]\s*;", original))
    matching = [m for m in imports if "./woodland/host.nix" in m[1]]
    require(len(matching) == 1 and re.search(r"(?m)^\s*\./woodland/host\.nix\s*$", matching[0][1]),
            "Host import is not an unambiguous standalone entry")
    changed = re.sub(r"(?m)^[ \t]*\./woodland/host\.nix[ \t]*\n", "", original, count=1)
    require(changed != original, "Could not remove host import")
    backup = root / "state/nixos-retirement"
    require(not backup.exists(), "Retirement backup already exists; preserve it and make a fresh export")
    copy_file(NIX_CONFIG, backup / "configuration.nix")
    copy_file(NIX_MODULE, backup / "host.nix")
    checksums(root)
    validate(root)
    handoff(root)
    stopped = []
    edited = False
    mode = stat.S_IMODE(NIX_CONFIG.stat().st_mode)
    try:
        stop_active(UNITS, stopped)
        # Recheck after all writers stop; never silently discard new registrations.
        installed_matches(root)
        require(text(NIX_CONFIG) == original, "NixOS configuration changed during retirement")
        replace_config(changed, mode)
        edited = True
        run(["nixos-rebuild", "switch"])
        run(["systemctl", "stop", *UNITS], check=False)
        run(["systemctl", "daemon-reload"])
        verify_retired()
    except BaseException:
        with recovering():
            recovery_failed = False
            if edited:
                try:
                    replace_config(original, mode)
                    recovery_failed = run(["nixos-rebuild", "switch"], check=False).returncode != 0
                except (BundleError, OSError):
                    recovery_failed = True
            restart(stopped)
            if recovery_failed:
                raise BundleError("Retirement failed and automatic NixOS recovery failed; restore configuration.nix from the private retirement backup and run nixos-rebuild switch. Deployment data is preserved") from None
        raise
    print("NixOS hosting retired: units/startup and sleep overrides removed. Deployment data and private configuration backups were preserved.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    for command in ("validate", "configure", "retire"):
        sub = commands.add_parser(command)
        sub.add_argument("bundle", type=Path)
        if command == "configure":
            sub.add_argument("destination", type=Path)
            sub.add_argument("--port", type=int, default=8000)
    sub = commands.add_parser("export")
    sub.add_argument("--revision", required=True)
    sub.add_argument("--public-url", required=True)
    sub.add_argument("destination", type=Path)
    args = parser.parse_args()
    os.umask(0o077)
    if args.command in ("export", "retire"):
        require(os.geteuid() == 0, "Run this command explicitly with sudo")
    if args.command == "validate":
        meta, _, _ = validate(args.bundle)
        print(f"Bundle verified: protocol {meta['protocolVersion']}, source {meta['sourceRevision']}, genesis {meta['genesisTxid']}")
    elif args.command == "configure":
        configure(args.bundle, args.destination, args.port)
    else:
        # Serialize export/retirement without changing installed configuration.
        with Path("/run/lock/woodland-host-migration.lock").open("w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            def interrupted(signum, frame):
                raise BundleError("Interrupted; recovering previously active services")
            for sig in (signal.SIGTERM, signal.SIGHUP, signal.SIGINT):
                signal.signal(sig, interrupted)
            if args.command == "export":
                export(args.destination.absolute(), args.revision, args.public_url)
            else:
                retire(args.bundle.absolute())


if __name__ == "__main__":
    try:
        main()
    except (BundleError, OSError, ValueError, KeyError, TypeError, tarfile.TarError, EOFError) as error:
        # Parsing errors may contain secret input, so only our own diagnostics are public.
        detail = str(error) if isinstance(error, BundleError) else "Invalid or inaccessible input; details withheld to protect credentials"
        print("error: " + detail, file=sys.stderr)
        sys.exit(1)
