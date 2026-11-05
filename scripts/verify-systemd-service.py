import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import uuid


def run(*arguments):
    return subprocess.check_output([str(value) for value in arguments], text=True, timeout=180)


def verify(zup, runtime, plugin=None):
    if os.geteuid() != 0:
        raise RuntimeError("native machine-service verification requires root")
    suffix = uuid.uuid4().hex
    application = f"com.example.zup-systemd-{suffix}"
    service = f"{application}.worker" if plugin else f"worker-{suffix}"
    escaped = "".join(chr(byte) if chr(byte).isascii() and (chr(byte).isalnum() or chr(byte) in "-_") else f"\\x{byte:02x}" for byte in service.encode())
    unit = f"zup-{escaped}-{hashlib.sha256(service.encode()).hexdigest()[:12]}.service"

    def property(name):
        return run("/usr/bin/systemctl", "show", unit, f"--property={name}", "--value").strip()

    with tempfile.TemporaryDirectory(prefix="zup-systemd-") as temporary:
        root = Path(temporary)
        (root / "dist").mkdir()
        binary = "worker" if plugin else "sleep"
        if plugin:
            (root / "dist/worker").write_text('''#!/usr/bin/python3
import os, signal, socket
from pathlib import Path
assert "version:" in Path(__file__).with_name("configure.txt").read_text()
signal.signal(signal.SIGTERM, lambda *_: exit(0))
address = os.environ["NOTIFY_SOCKET"]
if address.startswith("@"):
    address = "\\0" + address[1:]
with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as notification:
    notification.sendto(b"READY=1", address)
while True:
    signal.pause()
''')
            shutil.copyfile(plugin, root / "configure.wasm")
        else:
            shutil.copyfile("/usr/bin/sleep", root / "dist/sleep")
        manifest = root / "zup.toml"
        installers = []
        installed = False
        directory = None

        def build(version, contents):
            (root / "dist/worker.conf").write_text(contents)
            declaration = f'''[[services]]
id = "{service}"
name = "Native systemd verification"
binary = "${{install}}/sleep"
arguments = ["infinity"]
requires_files = ["${{install}}/worker.conf"]
start = "manual"
execution = {{ type = "systemd_user", user = "root" }}
lifecycle = {{ install = "running", change = "preserve" }}
''' if not plugin else '''[[components]]
id = "service"
name = "Worker service"
[[plugins]]
id = "configure"
source = "configure.wasm"
'''
            manifest.write_text(f'''schema = 1
frontend = "headless"
[app]
id = "{application}"
name = "Native systemd verification"
version = "{version}.0.0"
[build.targets.default]
target = "linux-{'arm64' if os.uname().machine == 'aarch64' else 'x64'}"
source = {{ directory = "dist" }}
[install]
scope = "machine"
[install.directory]
machine = "${{location.programs}}/{application}"
[[files]]
source = "{binary}"
destination = "${{install}}"
executable = true
[[files]]
source = "worker.conf"
destination = "${{install}}"
{declaration}''')
            installer = root / f"setup-{version}"
            run(zup, "build", "--manifest", manifest, "--runtime", runtime, "--output", installer)
            installers.append(installer)
            return installer

        try:
            first = build(1, "first")
            plan = json.loads(run(zup, "plan", "--manifest", manifest, "--scope", "machine", "--format", "json"))
            directory = Path(plan["details"]["install_directory"])
            installed = True
            run(first, "install", "--scope", "machine", "--non-interactive", "--yes")
            assert property("ActiveState") == "active"
            invocation = property("InvocationID")
            assert invocation and set(invocation) != {"0"}
            if plugin:
                assert property("DynamicUser") == "yes"
                pid = int(property("MainPID"))
                status = Path(f"/proc/{pid}/status").read_text()
                uids = next(line.split()[1:] for line in status.splitlines() if line.startswith("Uid:"))
                assert all(int(uid) != 0 for uid in uids), "dynamic service unexpectedly runs as root"
                assert property("InvocationID") == invocation
            second = build(2, "second")
            run(second, "upgrade", "--scope", "machine", "--non-interactive", "--yes")
            assert (directory / "worker.conf").read_text() == "second"
            assert property("ActiveState") == "active"
            assert property("InvocationID") != invocation, "configuration-only upgrade did not restart its consumer"
            invocation = property("InvocationID")
            run(second, "repair", "--scope", "machine", "--non-interactive", "--yes")
            assert property("InvocationID") == invocation, "unchanged repair restarted the service"
            run("/usr/bin/busctl", "--system", "call", "org.freedesktop.systemd1", "/org/freedesktop/systemd1",
                "org.freedesktop.systemd1.Manager", "StopUnit", "ss", unit, "fail")
            deadline = time.monotonic() + 10
            while property("ActiveState") != "inactive":
                if time.monotonic() >= deadline:
                    raise RuntimeError("native stop did not complete")
                time.sleep(0.02)
            third = build(3, "third")
            run(third, "upgrade", "--scope", "machine", "--non-interactive", "--yes")
            assert property("ActiveState") == "inactive", "stopped service started during upgrade"
            (directory / "foreign.txt").write_text(suffix)
            run(third, "uninstall", "--scope", "machine", "--non-interactive", "--yes")
            installed = False
            assert not (directory / binary).exists()
            assert not (directory / "worker.conf").exists()
            assert (directory / "foreign.txt").read_text() == suffix
            assert property("LoadState") == "not-found"
            (directory / "foreign.txt").unlink()
            directory.rmdir()
        finally:
            if installed:
                try:
                    run(installers[-1], "uninstall", "--scope", "machine", "--non-interactive", "--yes")
                except Exception as error:
                    raise RuntimeError(f"native cleanup requires recovery for {application}, unit {unit}: {error}") from error
    print(f"Native {'plugin notify' if plugin else 'declared manager'} worker lifecycle passed: install, configuration-only upgrade, unchanged repair, stopped upgrade, ownership-safe uninstall")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--zup", type=Path, required=True)
    parser.add_argument("--runtime", type=Path, required=True)
    parser.add_argument("--plugin", type=Path)
    parser.add_argument("--inside-mount-namespace", action="store_true", help=argparse.SUPPRESS)
    options = parser.parse_args()
    if os.geteuid() != 0:
        parser.error("native machine-service verification requires root")
    if not options.inside_mount_namespace:
        raise SystemExit(subprocess.call([
            "unshare", "--mount", "--propagation", "private", sys.executable,
            str(Path(__file__).resolve()), *sys.argv[1:], "--inside-mount-namespace",
        ]))
    machine_state = Path("/var/lib/zup")
    if machine_state.is_symlink():
        raise RuntimeError("machine-state mount target is a symlink")
    machine_state.mkdir(mode=0o700, exist_ok=True)
    isolated = Path(tempfile.mkdtemp(prefix="zup-native-state-"))
    subprocess.run(["mount", "--bind", str(isolated), str(machine_state)], check=True)
    succeeded = False
    try:
        verify(options.zup.resolve(), options.runtime.resolve())
        if options.plugin:
            verify(options.zup.resolve(), options.runtime.resolve(), options.plugin.resolve())
        succeeded = True
    finally:
        subprocess.run(["umount", str(machine_state)], check=True)
        if succeeded:
            shutil.rmtree(isolated)
        else:
            print(f"Native verification state retained at {isolated}", file=sys.stderr)
