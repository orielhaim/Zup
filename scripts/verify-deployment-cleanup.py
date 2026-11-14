"""Exercise the public build and installer cleanup workflow in a fresh user fixture."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
import traceback
import uuid


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--zup", type=Path, required=True)
    parser.add_argument("--runtime", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--history-fault", choices=["history_archived", "history_member_removed", "history_journal_member_removed", "history_directory_removed"])
    options = parser.parse_args()
    options.zup = options.zup.resolve(strict=True)
    options.runtime = options.runtime.resolve(strict=True)
    windows = os.name == "nt"
    target = "windows-x64" if windows else "linux-x64"
    base = Path(os.environ["LOCALAPPDATA"]) if windows else None
    root = Path(tempfile.mkdtemp(prefix="zup-cleanup-e2e-", dir=base))
    report = {"state": "failed", "fixture": str(root), "target": target, "checks": [], "timings": {}, "artifacts": {}}
    process = None
    succeeded = False

    def run(label, command, *, parse=False):
        start = time.monotonic()
        result = subprocess.run([str(value) for value in command], cwd=root, capture_output=True, text=True, timeout=240)
        report["timings"][label] = round(time.monotonic() - start, 3)
        if result.returncode:
            raise RuntimeError(f"{label} exited {result.returncode}: {result.stdout[-5000:]}\n{result.stderr[-5000:]}")
        return json.loads(result.stdout) if parse else result.stdout

    try:
        distribution = root / "dist"
        distribution.mkdir()
        name = "ping.exe" if windows else "sleep"
        shutil.copyfile(Path(os.environ.get("SystemRoot", r"C:\Windows")) / "System32/PING.EXE" if windows else Path("/usr/bin/sleep"), distribution / name)
        (distribution / name).chmod(0o755)
        (distribution / "version.txt").write_text("first", encoding="utf8")
        application = "com.zup.cleanup-e2e-" + uuid.uuid4().hex
        run("init", [options.zup, "init", "--name", application, "--app-id", application, "--source", "dist", "--target", target, "--scope", "user", "--frontend", "headless", "--main", name, "--yes"])
        manifest = root / "zup.toml"
        authored = manifest.read_text(encoding="utf8")
        run("default-check", [options.zup, "check"])
        run("default-plan", [options.zup, "plan", "--format", "json"], parse=True)
        report["checks"].append("init and default check/plan require no generation configuration")
        authored = authored.replace("[install]\n", '[install]\ngeneration_readers = "native_executables"\n')
        authored += '\n[[install.data]]\nid = "state"\npath = ' + json.dumps(str(root / "data")) + '\n'
        setups = []
        for version, contents in [("0.1.0", "first"), ("0.2.0", "second"), ("0.3.0", "third")]:
            (distribution / "version.txt").write_text(contents, encoding="utf8")
            manifest.write_text(authored.replace('version = "0.1.0"', f'version = "{version}"'), encoding="utf8")
            run("check-" + version, [options.zup, "check"])
            run("plan-" + version, [options.zup, "plan", "--format", "json"], parse=True)
            setup = root / ("setup-" + version + (".exe" if windows else ""))
            run("build-" + version, [options.zup, "build", "--runtime", options.runtime, "--output", setup])
            report["artifacts"][version] = {"sha256": hashlib.file_digest(setup.open("rb"), "sha256").hexdigest(), "bytes": setup.stat().st_size}
            setups.append(setup)
        state = root / "state"
        install = root / "application"

        def invoke(label, setup, verb, *arguments):
            return run(label, [setup, verb, *arguments, "--scope", "user", "--state-root", state, "--output", "json", *([] if verb == "inspect" else ["--non-interactive", "--yes"])], parse=True)

        report["install"] = invoke("install", setups[0], "install", "--install-directory", install)
        first = invoke("inspect-first", setups[0], "inspect")
        old = install / ".zup/deployments" / str(first["current"]["materialization"])
        process = subprocess.Popen([str(old / name), *(["-n", "120", "127.0.0.1"] if windows else ["120"])], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, creationflags=subprocess.CREATE_NO_WINDOW if windows else 0)
        (root / "data/sentinel").write_text("user data", encoding="utf8")
        (install / "foreign.txt").write_text("unrelated", encoding="utf8")
        report["upgrade"] = invoke("upgrade", setups[1], "upgrade")
        second = invoke("inspect-second", setups[1], "inspect")
        assert process.poll() is None, "generation A process must still run after upgrade"
        report["busy_cleanup"] = invoke("cleanup-busy", setups[1], "cleanup", "--keep", "0")
        retired = invoke("inspect-retired", setups[1], "inspect")
        assert retired["current"] == second["current"]
        assert not retired["retained"] and old.exists()
        assert (old / "version.txt").read_text(encoding="utf8") == "first"
        report["checks"].append("running generation A remains intact after upgrade and deferred cleanup")
        process.terminate()
        process.wait(timeout=15)
        process = None
        report["cleanup"] = invoke("cleanup-unused", setups[1], "cleanup", "--keep", "0")
        assert not old.exists(), "unused generation A must be collected"
        invoke("cleanup-history", setups[1], "cleanup", "--keep", "0")
        current = invoke("inspect-collected", setups[1], "inspect")
        assert not current["retired"] and current["current"] == second["current"]
        report["checks"].append("repeated cleanup collects A and forgets verified collected history without changing B")
        report["third_upgrade"] = invoke("upgrade-third", setups[2], "upgrade")
        third = invoke("inspect-third", setups[2], "inspect")
        invoke("cleanup-third-retire", setups[2], "cleanup", "--keep", "0")
        if options.history_fault:
            environment = dict(os.environ, ZUP_TEST_FAULT_POINT=options.history_fault, ZUP_TEST_FAULT_OPERATION="history:")
            interrupted = subprocess.run([str(setups[2]), "cleanup", "--keep", "0", "--scope", "user", "--state-root", str(state), "--output", "json", "--non-interactive", "--yes"], cwd=root, capture_output=True, text=True, env=environment, timeout=240)
            assert interrupted.returncode != 0, "history interruption did not fire"
            pending = []
            for path in (state / "transactions").glob("*/transaction.json"):
                record = json.loads(path.read_text(encoding="utf8"))
                if record["app_id"] == application and record["phase"] == "committed" and any(value == "running" for value in record["nodes"].values()):
                    pending.append(record)
            assert len(pending) == 1, "history interruption must preserve one committed recoverable cleanup"
            report["history_recovery"] = run("recover-history", [setups[2], "recover", "--transaction", pending[0]["transaction_id"], "--scope", "user", "--state-root", state, "--output", "json"], parse=True)
            report["checks"].append("interrupted native history collection recovers without reverting the committed application")
        else:
            invoke("cleanup-third-forget", setups[2], "cleanup", "--keep", "0")
        journal_counts = []
        metadata_counts = []
        for index in range(4):
            invoke(f"cleanup-bounded-history-{index}", setups[2], "cleanup", "--keep", "0")
            journal_counts.append(len(list((state / "transactions").glob("*/transaction.json"))))
            metadata_counts.append({name: len(list((state / name).rglob("*.json"))) for name in ("materializations", "collections", "data")})
        report["journal_counts"] = journal_counts
        report["metadata_counts"] = metadata_counts
        assert journal_counts[-1] <= journal_counts[-2] <= journal_counts[-3], "settled metadata maintenance must not accumulate journals"
        assert all(metadata_counts[-1][name] <= metadata_counts[-2][name] <= metadata_counts[-3][name] for name in metadata_counts[-1]), "settled sidecar collection must not accumulate metadata"
        assert not any((state / "history").glob("*")), "completed history collection must leave no archived journal residue"
        current = invoke("inspect-bounded-history", setups[2], "inspect")
        assert current["current"] == third["current"] and not current["retained"] and not current["retired"]
        report["checks"].append("journal and sidecar collection keeps history bounded while preserving current and data provenance")
        active = install / ".zup/deployments" / str(current["current"]["materialization"])
        offline = root / "state-offline"
        state.rename(offline)
        try:
            run("without-zup-state", [active / name, *(["-n", "1", "127.0.0.1"] if windows else ["0.01"])])
        finally:
            offline.rename(state)
        report["checks"].append("native executable runs while deployment metadata is unavailable")
        report["uninstall"] = invoke("uninstall", setups[2], "uninstall")
        assert (root / "data/sentinel").read_text(encoding="utf8") == "user data"
        assert (install / "foreign.txt").read_text(encoding="utf8") == "unrelated"
        report["checks"].append("uninstall preserves persistent data and unrelated files")
        invoke("cleanup-uninstalled-retire", setups[2], "cleanup", "--keep", "0")
        invoke("cleanup-uninstalled-forget", setups[2], "cleanup", "--keep", "0")
        assert not active.exists(), "removed installations must permit explicit collection of their final generation"
        assert (root / "data/sentinel").read_text(encoding="utf8") == "user data"
        report["checks"].append("removed installation cleanup reclaims the final generation while preserving user data")
        report["state"] = "passed"
        succeeded = True
    except Exception as error:
        report["error"] = str(error)
        report["traceback"] = traceback.format_exc()
    finally:
        if process is not None and process.poll() is None:
            process.terminate()
            process.wait(timeout=15)
        options.output.parent.mkdir(parents=True, exist_ok=True)
        options.output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf8")
        if succeeded:
            expected_parent = base.resolve() if base else Path(tempfile.gettempdir()).resolve()
            if root.is_symlink() or root.resolve().parent != expected_parent or not root.name.startswith("zup-cleanup-e2e-"):
                raise RuntimeError("refusing fixture cleanup outside its verified temporary parent")
            shutil.rmtree(root)
    print(json.dumps({"state": report["state"], "report": str(options.output), "error": report.get("error")}))
    return 0 if succeeded else 1


if __name__ == "__main__":
    raise SystemExit(main())
