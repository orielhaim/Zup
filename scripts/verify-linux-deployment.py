"""Verify composed hybrid installers against native Linux services in disposable VMs."""

import argparse
import base64
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import time
import uuid


spec = importlib.util.spec_from_file_location(
    "supervisors", Path(__file__).with_name("probe-linux-supervisors.py"))
supervisors = importlib.util.module_from_spec(spec)
spec.loader.exec_module(supervisors)


def upload(guest, source, destination):
    with source.open("rb") as artifact:
        result = subprocess.run([
            "ssh", "-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes",
            "-o", "IdentityAgent=none", "-o", "StrictHostKeyChecking=yes",
            "-o", f"UserKnownHostsFile={guest.root}/known-hosts",
            "-i", str(guest.root / "client"), "-p", str(guest.port),
            "root@127.0.0.1", f"cat > {destination}",
        ], stdin=artifact, capture_output=True, timeout=180)
    if result.returncode:
        raise RuntimeError(result.stderr.decode(errors="replace"))
    guest.run("chmod", "700", destination)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--base-image", type=Path, required=True)
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--zup", type=Path, required=True)
    parser.add_argument("--runtime", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--recovery-reboot", action="store_true")
    parser.add_argument("--manager", choices=["openrc", "systemd"], default="openrc")
    parser.add_argument("--base-sha512")
    parser.add_argument("--architecture", choices=["x86_64", "aarch64"], default="x86_64")
    options = parser.parse_args()
    report = {"state": "failed", "checks": [], "artifacts": {}}
    for name in ("base_image", "zup", "runtime"):
        with getattr(options, name).open("rb") as artifact:
            report["artifacts"][name] = hashlib.file_digest(artifact, "sha256").hexdigest()
    expected_image = {
        "x86_64": "53a02ce7466524a83f8b07b6fe4ef212f6019752c0e563db9538d9b54d468a8e",
        "aarch64": "4b9a0f612f2737166a795a04befa7513ffc56d4195a22e72da7e857ffb2665d8",
    }[options.architecture]
    report["architecture"] = options.architecture
    target = "linux-arm64" if options.architecture == "aarch64" else "linux-x64"
    if options.manager == "openrc":
        target += "-musl"
        if report["artifacts"]["base_image"] != expected_image:
            raise ValueError("unexpected Alpine verification image")
    else:
        with options.base_image.open("rb") as image:
            if not options.base_sha512 or hashlib.file_digest(image, "sha512").hexdigest() != options.base_sha512:
                raise ValueError("systemd image requires its verified primary-source SHA-512 checksum")
    report["manager"] = options.manager
    try:
        with supervisors.Guest(options.base_image, options.cache, disk_size="4G", architecture=options.architecture, manager=options.manager) as guest, tempfile.TemporaryDirectory(prefix="zup-openrc-deployment-") as project:
            report["guest"] = str(guest.root)
            report["filesystem"] = guest.run("df", "-Pk", "/").stdout
            if guest.run("uname", "-m").stdout.strip() != options.architecture:
                raise RuntimeError("guest did not boot the requested native architecture")
            if options.manager == "openrc":
                guest.run("apk", "add", "--no-cache", "openrc=0.63.2-r0", timeout=180)
                configuration = guest.run("cat", "/etc/rc.conf").stdout
                guest.write("/etc/rc.conf", configuration + '\nrc_cgroup_mode="unified"\n', "644")
                guest.run("rc-update", "add", "cgroups", "boot")
                guest.run("rc-service", "cgroups", "start")
            project = Path(project)
            distribution = project / "dist"
            distribution.mkdir()
            payload = "/bin/busybox" if options.manager == "openrc" else "/usr/bin/sleep"
            (distribution / "sleep").write_bytes(base64.b64decode(guest.run("base64", payload).stdout))
            tag = uuid.uuid4().hex
            app = f"com.example.openrc-{tag}"
            state_root = "/var/lib/zup/apps/" + hashlib.sha256(b"zup:application-state:1\0" + app.encode()).hexdigest()
            service = f"daemon-{tag}"
            native = f"zup-probe-{tag}" + (".service" if options.manager == "systemd" else "")
            group = guest.run("id", "-gn", "nobody").stdout.strip()
            execution = f'{{ type = "{options.manager}_user", user = "nobody", group = "{group}" }}'
            failure = 'failure = { type = "openrc", delay_seconds = 1, max_respawns = 3, respawn_period_seconds = 60 }' if options.manager == "openrc" else ""
            lifecycle = ('{ install = "running", change = "preserve", timeouts = { type = "openrc", start_timeout_seconds = 10, stop_grace_seconds = 1, kill_wait_seconds = 2, settle_seconds = 2 } }'
                         if options.manager == "openrc" else '{ install = "running", change = "preserve" }')
            data = f"/var/opt/{app}"
            install_root = f"/opt/{app}"

            def build(version):
                print(f"Building installer {version}", flush=True)
                (distribution / "version.txt").write_text(version)
                (distribution / "shared.conf").write_text(version)
                manifest = project / "zup.toml"
                manifest.write_text(f'''schema = 1
frontend = "headless"
[app]
id = "{app}"
name = "OpenRC hybrid deployment"
version = "{version}"
[build.targets.default]
target = "{target}"
source = {{ directory = "dist" }}
[install]
scope = "machine"
[install.directory]
machine = "${{location.programs}}/{app}"
[[install.data]]
id = "state"
path = "{data}"
[[files]]
id = "program"
source = "sleep"
executable = true
[[files]]
id = "version"
source = "version.txt"
[[files]]
id = "fixed"
source = "shared.conf"
placement = {{ kind = "fixed_file", path = "${{install}}/shared.conf", replacement = "quiescent" }}
[[services]]
id = "{service}"
native_name = "{native}"
display_name = "Daemon"
binary = "${{payload}}/sleep"
arguments = ["600"]
requires_files = ["${{install}}/shared.conf"]
execution = {execution}
{failure}
start = "automatic"
lifecycle = {lifecycle}
''')
                setup = project / ("setup-" + version)
                result = subprocess.run([str(options.zup), "build", "--manifest", str(manifest), "--runtime", str(options.runtime), "--output", str(setup)], capture_output=True, text=True, timeout=240)
                if result.returncode:
                    raise RuntimeError(f"installer build failed: {result.stdout}\n{result.stderr}")
                remote = "/root/setup-" + version
                print(f"Transferring installer {version}", flush=True)
                upload(guest, setup, remote)
                return remote

            def invoke(setup, *arguments):
                print(f"Running {arguments[0]}", flush=True)
                result = guest.run(setup, *arguments, "--scope", "machine", "--output", "json", "--non-interactive", "--yes", timeout=180, check=False)
                if result.returncode:
                    raise RuntimeError(f"installer {arguments} failed: {result.stdout}\n{result.stderr}")
                return json.loads(result.stdout)

            def image():
                pid = (guest.run("cat", f"/run/openrc/options/{native}/child_pid").stdout.strip()
                       if options.manager == "openrc" else guest.run("systemctl", "show", native, "--property=MainPID", "--value").stdout.strip())
                return guest.run("readlink", f"/proc/{pid}/exe").stdout.strip()

            a = build("1.0.0")
            report["install"] = invoke(a, "install")
            first = invoke(a, "inspect")
            image_a = image()
            assert image_a.startswith(install_root + "/.zup/deployments/")
            guest.write(data + "/sentinel", "user data", "600")
            guest.write(install_root + "/foreign.txt", "unrelated", "644")
            report["checks"].append("fresh hybrid generation/fixed/persistent native service install")
            b = build("2.0.0")
            if options.recovery_reboot:
                baseline = set(guest.run("find", state_root, "-name", "transaction.json").stdout.splitlines())
                crashed = guest.run("env", "ZUP_TEST_FAULT_POINT=verified_persisted",
                    "ZUP_TEST_FAULT_OPERATION=service_quiesce", b, "upgrade", "--scope", "machine",
                    "--output", "json", "--non-interactive", "--yes", timeout=180, check=False)
                assert crashed.returncode != 0, "quiescence interruption did not fire"
                journals = set(guest.run("find", state_root, "-name", "transaction.json").stdout.splitlines()) - baseline
                assert len(journals) == 1, "interrupted upgrade must have one protected journal"
                journal = journals.pop()
                interrupted = json.loads(guest.run("cat", journal).stdout)
                assert interrupted["app_id"] == app
                transaction = interrupted["transaction_id"]
                before_boot = guest.run("cat", "/proc/sys/kernel/random/boot_id").stdout.strip()
                guest.run("reboot", check=False)
                assert guest.wait_boot(before_boot) != before_boot
                stopped = guest.run("rc-service", native, "status", check=False) if options.manager == "openrc" else guest.run("systemctl", "is-active", native, check=False)
                assert stopped.returncode != 0
                report["cross_boot_recovery"] = invoke(b, "recover", "--transaction", transaction)
                recovered = json.loads(guest.run("cat", journal).stdout)
                assert recovered["phase"] == "rolled_back", recovered.get("rollback_failure")
                assert image() == image_a
                assert guest.run("cat", install_root + "/shared.conf").stdout == "1.0.0"
                assert guest.run("cat", data + "/sentinel").stdout == "user data"
                report["checks"].append("interrupted closed activation survives reboot and restores A using fresh native authority")
            report["upgrade"] = invoke(b, "upgrade")
            assert image() != image_a
            assert guest.run("cat", install_root + "/shared.conf").stdout == "2.0.0"
            guest.run("rm", a)
            for source in distribution.iterdir():
                source.unlink()
            report["rollback"] = invoke(b, "rollback", "--revision", str(first["current"]["revision"]))
            assert image() == image_a
            assert guest.run("cat", install_root + "/shared.conf").stdout == "1.0.0"
            assert guest.run("cat", data + "/sentinel").stdout == "user data"
            report["checks"].append("A to B and source-independent retained rollback to A")
            before_boot = guest.run("cat", "/proc/sys/kernel/random/boot_id").stdout.strip()
            guest.run("reboot", check=False)
            after_boot = guest.wait_boot(before_boot)
            assert after_boot != before_boot
            deadline = time.monotonic() + 45
            while True:
                try:
                    if image() == image_a:
                        break
                except RuntimeError:
                    pass
                if time.monotonic() >= deadline:
                    status = guest.run("rc-service", native, "status", check=False) if options.manager == "openrc" else guest.run("systemctl", "status", native, check=False)
                    raise RuntimeError(f"native boot activation did not reach pinned A: {status.stdout}{status.stderr}")
                time.sleep(0.1)
            report["checks"].append("native service starts pinned A after actual reboot without Zup")
            report["uninstall"] = invoke(b, "uninstall")
            if options.manager == "openrc":
                assert guest.run("test", "-e", f"/etc/init.d/{native}", check=False).returncode != 0
                assert guest.run("test", "-e", f"/etc/conf.d/{native}", check=False).returncode != 0
            else:
                assert guest.run("systemctl", "show", native, "--property=LoadState", "--value").stdout.strip() == "not-found"
            assert guest.run("cat", data + "/sentinel").stdout == "user data"
            assert guest.run("cat", install_root + "/foreign.txt").stdout == "unrelated"
            report["checks"].append("uninstall preserves user data and unrelated files")
            report["state"] = "completed"
            report["scope"] = f"full composed native {options.manager} lifecycle and reboot on {options.architecture}; not power-loss proof"
    except BaseException as error:
        report["failure"] = f"{type(error).__name__}: {error}"
        raise
    finally:
        options.output.parent.mkdir(parents=True, exist_ok=True)
        options.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
