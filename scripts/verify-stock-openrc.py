"""Run Zup's native OpenRC lifecycle checks in a disposable Alpine overlay."""

import argparse
import base64
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile


spec = importlib.util.spec_from_file_location(
    "supervisors", Path(__file__).with_name("probe-linux-supervisors.py"))
supervisors = importlib.util.module_from_spec(spec)
spec.loader.exec_module(supervisors)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--base-image", type=Path, required=True)
    parser.add_argument("--test-binary", type=Path, required=True)
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    options = parser.parse_args()
    report = {
        "state": "failed",
        "base_sha256": hashlib.file_digest(options.base_image.open("rb"), "sha256").hexdigest(),
        "test_sha256": hashlib.file_digest(options.test_binary.open("rb"), "sha256").hexdigest(),
        "scope": "stock OpenRC admission, service-control recovery and reboot; not full installer or power-loss proof",
    }
    if report["base_sha256"] != "53a02ce7466524a83f8b07b6fe4ef212f6019752c0e563db9538d9b54d468a8e":
        raise ValueError("unexpected Alpine verification image")
    try:
        with tempfile.TemporaryDirectory(prefix="openrc-admission-hook-") as temporary:
            hook = Path(temporary) / "admission.so"
            subprocess.run(["musl-gcc", "-shared", "-fPIC", "-O2", "-o", str(hook),
                            str(Path(__file__).resolve().parent.parent / "fixtures/openrc_admission_delay.c"), "-ldl"],
                           check=True, capture_output=True, timeout=30)
            hook_bytes = hook.read_bytes()
        report["delay_hook_sha256"] = hashlib.sha256(hook_bytes).hexdigest()
        with supervisors.Guest(options.base_image, options.cache, disk_size="4G") as guest:
            report["guest"] = str(guest.root)
            report["boot_id"] = guest.run("cat", "/proc/sys/kernel/random/boot_id").stdout.strip()
            guest.run("apk", "add", "--no-cache", "openrc=0.63.2-r0", timeout=180)
            configuration = guest.run("cat", "/etc/rc.conf").stdout
            guest.write("/etc/rc.conf", configuration + '\nrc_cgroup_mode="unified"\n', "644")
            guest.run("rc-update", "add", "cgroups", "boot")
            guest.run("rc-service", "cgroups", "start")
            report["packages"] = guest.run("apk", "list", "--installed", "openrc").stdout
            with options.test_binary.open("rb") as binary:
                transfer = subprocess.run([
                    "ssh", "-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes",
                    "-o", "IdentityAgent=none", "-o", "StrictHostKeyChecking=yes",
                    "-o", f"UserKnownHostsFile={guest.root}/known-hosts",
                    "-i", str(guest.root / "client"), "-p", str(guest.port),
                    "root@127.0.0.1", "cat > /root/zup-openrc-tests",
                ], stdin=binary, capture_output=True, timeout=120)
            if transfer.returncode:
                raise RuntimeError(f"test artifact transfer failed: {transfer.stderr.decode(errors='replace')}")
            guest.run("chmod", "700", "/root/zup-openrc-tests")
            guest.run("python3", "-c", "import base64,pathlib,sys; pathlib.Path('/root/zup-openrc-admission.so').write_bytes(base64.b64decode(sys.argv[1],validate=True))",
                      base64.b64encode(hook_bytes).decode())
            result = guest.run("env", "ZUP_TEST_STOCK_OPENRC=1", "/root/zup-openrc-tests",
                               "openrc_native::tests::stock_",
                               "--ignored", "--test-threads=1", "--nocapture", timeout=240, check=False)
            report["exit_code"] = result.returncode
            report["stdout"] = result.stdout
            report["stderr"] = result.stderr
            print(result.stdout, flush=True)
            print(result.stderr, flush=True)
            if result.returncode:
                raise RuntimeError("native OpenRC lifecycle checks failed")
            def boot_phase(phase):
                result = guest.run("env", "ZUP_TEST_STOCK_OPENRC=1", f"ZUP_ADMISSION_BOOT_PHASE={phase}",
                                   "/root/zup-openrc-tests", "openrc_native::tests::boot_admission",
                                   "--ignored", "--exact", "--nocapture", timeout=90, check=False)
                report[f"boot_{phase}"] = {"exit_code": result.returncode, "stdout": result.stdout, "stderr": result.stderr}
                print(result.stdout, flush=True)
                if result.returncode:
                    raise RuntimeError(f"OpenRC admission boot {phase} failed: {result.stderr}")
            boot_phase("prepare")
            guest.run("reboot", check=False)
            report["reboot_id"] = guest.wait_boot(report["boot_id"])
            boot_phase("verify")
            report["state"] = "completed"
    finally:
        options.output.parent.mkdir(parents=True, exist_ok=True)
        options.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
