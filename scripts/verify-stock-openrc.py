"""Run Zup's native OpenRC lifecycle checks in a disposable Alpine overlay."""

import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess


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
        "scope": "native adapter and service-control recovery; not full installer or power-loss proof",
    }
    if report["base_sha256"] != "53a02ce7466524a83f8b07b6fe4ef212f6019752c0e563db9538d9b54d468a8e":
        raise ValueError("unexpected Alpine verification image")
    try:
        with supervisors.Guest(options.base_image, options.cache) as guest:
            report["guest"] = str(guest.root)
            report["boot_id"] = guest.run("cat", "/proc/sys/kernel/random/boot_id").stdout.strip()
            guest.run("apk", "add", "--no-cache", "openrc=0.63.2-r0", timeout=180)
            configuration = guest.run("cat", "/etc/rc.conf").stdout
            guest.write("/etc/rc.conf", configuration + '\nrc_cgroup_mode="unified"\n', "644")
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
            result = guest.run("env", "ZUP_TEST_STOCK_OPENRC=1", "/root/zup-openrc-tests",
                               "openrc_native::tests::stock_native_activation_and_interrupted_shutdown",
                               "--ignored", "--test-threads=1", "--nocapture", timeout=120, check=False)
            report["exit_code"] = result.returncode
            report["stdout"] = result.stdout
            report["stderr"] = result.stderr
            print(result.stdout, flush=True)
            print(result.stderr, flush=True)
            if result.returncode:
                raise RuntimeError("native OpenRC lifecycle checks failed")
            report["state"] = "completed"
    finally:
        options.output.parent.mkdir(parents=True, exist_ok=True)
        options.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
