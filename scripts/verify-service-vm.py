import argparse
import hashlib
import json
from pathlib import Path
import shlex
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import uuid


def verify(options):
    suffix = uuid.uuid4().hex
    application = f"com.example.zup-openrc-{suffix}"
    ids = {name: f"{suffix}-{name}" for name in ("storage", "api")}
    names = {name: f"zup-openrc-{identity.encode().hex()}" if options.manager == "openrc" else f"zup-{identity}-{hashlib.sha256(identity.encode()).hexdigest()[:12]}.service" for name, identity in ids.items()}
    source_root = "/etc/init.d" if options.manager == "openrc" else "/usr/local/lib/systemd/system"
    ssh = ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "-i", str(options.ssh_key), "-o", "StrictHostKeyChecking=accept-new", "-o", f"UserKnownHostsFile={options.known_hosts}", "-p", str(options.ssh_port), "root@127.0.0.1"]

    def guest(*arguments, check=True, input_text=None):
        result = subprocess.run([*ssh, shlex.join(str(value) for value in arguments)], text=True, input=input_text, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=240)
        if check and result.returncode:
            raise RuntimeError(f"guest command {arguments!r} failed ({result.returncode}):\n{result.stdout}")
        return result

    def process(name):
        native = names[name]
        if options.manager == "systemd":
            result = guest("/usr/bin/systemctl", "show", native, "--property=MainPID", "--value")
            return result.stdout.strip() if result.stdout.strip() != "0" else None
        record = f"/run/openrc/options/{native}/{'child_pid' if options.supervised and name == 'api' else 'zup_pid'}"
        result = guest("cat", record, check=False)
        if result.returncode:
            return None
        pid = result.stdout.strip()
        if guest("test", "-d", f"/proc/{pid}", check=False).returncode:
            return None
        return pid

    def control(name, action):
        if options.manager == "openrc":
            return guest("/sbin/rc-service", "--nodeps", names[name], action)
        return guest("/usr/bin/systemctl", action, names[name])

    with tempfile.TemporaryDirectory(prefix="zup-openrc-build-") as temporary:
        root = Path(temporary)
        (root / "dist").mkdir()
        manifest = root / "zup.toml"
        remote = f"/var/tmp/zup-verification-{suffix}"
        guest("mkdir", "-m", "700", remote)
        installed = False
        last_setup = None
        directory = None

        def build(version, binary, config="initial", unrelated="initial", fail_start=False, registration=False):
            for name in ids:
                shutil.copyfile(binary, root / "dist" / name)
            (root / "dist/shared.conf").write_text(config)
            (root / "dist/unrelated.txt").write_text(unrelated)
            declaration = ""
            for name, identity in ids.items():
                dependencies = f'requires_services = ["{ids["storage"]}"]\n' if name == "api" else ""
                arguments = ["literal space", "double\"quote", "single'quote", f"$(touch {remote}/injected)", "`id`", "; exit 1"] if name == "api" else []
                if fail_start and name == "api":
                    arguments += ["--fail-after-ms", "0"]
                if registration and name == "api": arguments += ["--registration-update"]
                policy = ""
                if options.supervised and name == "api":
                    policy = 'failure = { type = "openrc", delay_seconds = 1, max_respawns = 2, respawn_period_seconds = 60 }\n' if options.manager == "openrc" else 'failure = { type = "systemd", condition = "on_failure", delay_seconds = 1, start_limit_burst = 3, start_limit_interval_seconds = 60 }\n'
                execution = '{ type = "openrc_user", user = "nobody", group = "nobody" }' if options.manager == "openrc" else '{ type = "systemd_user", user = "nobody" }'
                declaration += f'''[[services]]
id = "{identity}"
name = "{name}"
binary = "${{install}}/{name}"
arguments = {json.dumps(arguments)}
requires_files = ["${{install}}/shared.conf"]
{dependencies}{policy}start = "automatic"
execution = {execution}
lifecycle = {{ install = "running", change = "preserve" }}
'''
            manifest.write_text(f'''schema = 1
frontend = "headless"
[app]
id = "{application}"
name = "Native OpenRC verification"
version = "{version}.0.0"
[build.targets.default]
target = "{options.target}"
source = {{ directory = "dist" }}
[install]
scope = "machine"
[install.directory]
machine = "${{location.programs}}/{application}"
[[files]]
source = "storage"
destination = "${{install}}"
executable = true
[[files]]
source = "api"
destination = "${{install}}"
executable = true
[[files]]
source = "shared.conf"
destination = "${{install}}"
[[files]]
source = "unrelated.txt"
destination = "${{install}}"
{declaration}''')
            setup = root / f"setup-{version}"
            subprocess.run([str(options.zup), "build", "--manifest", str(manifest), "--runtime", str(options.runtime), "--output", str(setup)], check=True, timeout=240)
            subprocess.run(["scp", "-i", str(options.ssh_key), "-o", f"UserKnownHostsFile={options.known_hosts}", "-P", str(options.ssh_port), str(setup), f"root@127.0.0.1:{remote}/setup-{version}"], check=True, timeout=120)
            destination = f"{remote}/setup-{version}"
            guest("chmod", "700", destination)
            return destination

        def install(setup, action, check=True):
            return guest(setup, action, "--scope", "machine", "--non-interactive", "--yes", "--output", "json", check=check)

        def crash_matrix(first):
            if not options.qmp_socket or options.supervised:
                raise ValueError("crash verification requires a disposable QMP VM and an unsupervised fixture")
            second = build(2, options.service_v2, config="updated", registration=True)
            failed = build(3, options.service_v2, config="failed", fail_start=True, registration=True)
            guest("sysctl", "-w", "kernel.core_pattern=/dev/null")
            baseline = set(guest("find", "/var/lib/zup/transactions", "-maxdepth", "2", "-name", "transaction.json").stdout.splitlines())

            def monitor(command):
                with socket.socket(socket.AF_UNIX) as connection:
                    connection.settimeout(120)
                    connection.connect(str(options.qmp_socket))
                    with connection.makefile("rwb", buffering=0) as stream:
                        stream.readline()
                        def request(execute, arguments=None):
                            stream.write((json.dumps({"execute": execute, "arguments": arguments or {}}) + "\n").encode())
                            while True:
                                response = json.loads(stream.readline())
                                if "error" in response: raise RuntimeError(response["error"])
                                if "return" in response: return response["return"]
                        request("qmp_capabilities")
                        result = request("human-monitor-command", {"command-line": command})
                        if result: raise RuntimeError(result)

            tag = f"zup-{suffix}"
            monitor("stop")
            monitor(f"savevm {tag}")
            monitor("cont")
            def restore():
                monitor("stop")
                monitor(f"loadvm {tag}")
                monitor("cont")
            results = []
            scenarios = [
                ("intent_before", "service_quiesce", "committed", False),
                ("intent_persisted", "service_quiesce", "recovery_required", False),
                ("native_request_before", "stop", "recovery_required", False),
                ("native_request_submitted", "stop", "recovery_required", False),
                ("apply_returned", "service_quiesce", "recovery_required", False),
                ("receipt_persisted", "service_quiesce", "committed", False),
                ("verified_persisted", "service_quiesce", "committed", False),
                ("apply_returned", "file_mutation:api", "committed", False),
                ("apply_returned", "file_mutation:shared.conf", "committed", False),
                ("apply_returned", "registration", "committed", False),
                ("native_request_before", "start", "rolled_back", False),
                ("native_request_submitted", "start", "recovery_required", False),
                ("apply_returned", "service_restore", "recovery_required", False),
                ("receipt_persisted", "service_restore", "committed", False),
                ("verify_before", "service_restore", "committed", False),
                ("verify_returned", "service_restore", "committed", False),
                ("verified_persisted", "service_restore", "committed", False),
                ("commit_before", "", "committed", False),
                ("committed", "", "committed", False),
                ("ledger_publish_before", "", "committed", False),
                ("ledger_published", "", "committed", False),
                ("rollback_intent_persisted", "service_restore", "rolled_back", True),
                ("rollback_returned", "service_restore", "rolled_back", True),
                ("rollback_returned", "file_mutation:api", "rolled_back", True),
                ("rollback_returned", "service_quiesce", "recovery_required", True),
            ]

            def filter_for(selector):
                if selector in ("start", "stop"): return names["api"]
                if selector == "registration": return f"linux:service:{names['api']}"
                if selector.startswith("file_mutation:"):
                    filename = selector.split(":", 1)[1]
                    return f'file_replace:{{"file":{{"destination":"{directory}/{filename}"}}}}'
                return selector

            try:
                repeated = [
                    ("apply_returned", "file_mutation:api", "committed", False, "receipt_persisted", "service_restore"),
                    ("rollback_intent_persisted", "service_restore", "rolled_back", True, "rollback_returned", "file_mutation:api"),
                ]
                for point, selector, expected, failure, *recovery_fault in [*scenarios, *repeated]:
                    if options.crash_case and options.crash_case != f"{point}/{selector}":
                        continue
                    restore()
                    setup = failed if failure else second
                    # The first matching native request is Stop; the second is Start.
                    occurrence = "2" if selector == "start" else "1"
                    crashed = guest("env", f"ZUP_TEST_CRASH_POINT={point}", f"ZUP_TEST_CRASH_OPERATION={filter_for(selector)}", f"ZUP_TEST_CRASH_OCCURRENCE={occurrence}", setup, "upgrade", "--scope", "machine", "--non-interactive", "--yes", "--output", "json", check=False)
                    assert crashed.returncode != 0, f"fault did not fire: {point}/{selector}"
                    journals = set(guest("find", "/var/lib/zup/transactions", "-maxdepth", "2", "-name", "transaction.json").stdout.splitlines()) - baseline
                    assert len(journals) == 1, f"{point}/{selector}: unexpected crash journals: {journals}\n{crashed.stdout}"
                    journal = journals.pop()
                    record = json.loads(guest("cat", journal).stdout)
                    assert record["app_id"] == application
                    transaction = record["transaction_id"]
                    paths = [f"{directory}/{name}" for name in ("api", "storage", "shared.conf")]
                    before_files = guest("sha256sum", *paths).stdout
                    before_processes = {name: process(name) for name in ids}
                    recovery_arguments = (setup, "recover", "--transaction", transaction, "--scope", "machine", "--output", "json")
                    if recovery_fault:
                        recovery_point, recovery_selector = recovery_fault
                        interrupted = guest("env", f"ZUP_TEST_CRASH_POINT={recovery_point}", f"ZUP_TEST_CRASH_OPERATION={filter_for(recovery_selector)}", *recovery_arguments, check=False)
                        assert interrupted.returncode != 0, "recovery fault did not fire"
                    recovered = guest(*recovery_arguments, check=False)
                    record = json.loads(guest("cat", journal).stdout)
                    assert record["phase"] == expected, f"{point}/{selector}: {recovered.stdout}\n{record.get('failure')}\n{record.get('recovery_failure')}\n{record.get('rollback_failure')}"
                    if expected == "recovery_required":
                        assert guest("sha256sum", *paths).stdout == before_files, "ambiguous recovery changed payload"
                        assert {name: process(name) for name in ids} == before_processes, "ambiguous recovery controlled a runtime"
                    else:
                        assert recovered.returncode == (0 if expected == "committed" else 1), recovered.stdout
                        assert all(process(name) for name in ids), "recovery did not restore running state"
                        assert guest("cat", f"{directory}/shared.conf").stdout == ("updated" if expected == "committed" else "initial")
                    results.append({"point": point, "operation": selector, "phase": record["phase"], "recovery_interrupted": bool(recovery_fault)})
                    print(json.dumps(results[-1]), flush=True)
            finally:
                restore()
                monitor("stop")
                monitor(f"delvm {tag}")
                monitor("cont")
            print(json.dumps({"backend": options.manager, "target": options.target, "crash_scenarios": results}), flush=True)
            assert results, "no crash cases matched the requested filter"

        try:
            first = build(1, options.service_v1)
            plan = json.loads(subprocess.check_output([str(options.zup), "plan", "--manifest", str(manifest), "--scope", "machine", "--format", "json"], text=True))
            directory = plan["details"]["install_directory"]
            installed = True
            last_setup = first
            foreign = f"{source_root}/{names['api']}"
            guest("mkdir", "-p", source_root)
            foreign_source = "#!/sbin/openrc-run\nstart() { touch " + shlex.quote(remote + "/foreign-executed") + "; }\n" if options.manager == "openrc" else f"[Service]\nExecStart=/usr/bin/touch {remote}/foreign-executed\n"
            guest("tee", foreign, input_text=foreign_source)
            guest("chmod", "755", foreign)
            assert install(first, "install", check=False).returncode != 0, "foreign service was adopted"
            assert guest("cat", foreign).stdout == foreign_source
            assert guest("test", "-e", f"{remote}/foreign-executed", check=False).returncode != 0, "foreign script executed during observation"
            guest("rm", foreign)
            install(first, "install")
            before = {name: process(name) for name in ids}
            assert all(before.values()), "new services did not start"
            for name, pid in before.items():
                status = guest("cat", f"/proc/{pid}/status").stdout
                uids = next(line.split()[1:] for line in status.splitlines() if line.startswith("Uid:"))
                assert all(int(uid) != 0 for uid in uids), "OpenRC identity unexpectedly runs as root"
                if options.manager == "openrc":
                    guest("test", "-L", f"/etc/runlevels/default/{names[name]}")
                else:
                    assert guest("/usr/bin/systemctl", "is-enabled", names[name]).stdout.strip() == "enabled"
            assert guest("test", "-e", f"{remote}/injected", check=False).returncode != 0, "manifest arguments became shell instructions"
            command_line = guest("cat", f"/proc/{before['api']}/cmdline").stdout.split("\0")
            assert "single'quote" in command_line and "double\"quote" in command_line and "literal space" in command_line, "OpenRC argument encoding changed values"
            if options.crash_matrix:
                crash_matrix(first)
                return
            if options.supervised:
                generations = {before["api"]}
                for _ in range(3):
                    pid = process("api")
                    assert pid
                    guest("kill", "-KILL", pid)
                    deadline = time.monotonic() + 12
                    while time.monotonic() < deadline:
                        current = process("api")
                        if current and current != pid:
                            generations.add(current)
                            break
                        exhausted = guest("test", "-f", f"/run/{names['api']}.pid", check=False).returncode if options.manager == "openrc" else guest("/usr/bin/systemctl", "show", names["api"], "--property=ActiveState", "--value").stdout.strip() == "failed"
                        if exhausted:
                            break
                        time.sleep(0.1)
                assert len(generations) == 3 and not process("api"), "OpenRC respawn limit was not enforced"
                control("api", "stop")
                if options.manager == "systemd": guest("/usr/bin/systemctl", "reset-failed", names["api"])
                control("api", "start")
                before["api"] = process("api")
            second = build(2, options.service_v2, config="updated")
            install(second, "upgrade")
            last_setup = second
            after = {name: process(name) for name in ids}
            assert all(after.values()) and all(after[name] != before[name] for name in ids), "running upgrade did not restore affected services"
            assert guest("cat", f"{directory}/shared.conf").stdout == "updated"
            install(second, "repair")
            assert {name: process(name) for name in ids} == after, "unchanged repair restarted services"
            third = build(3, options.service_v2, config="updated", unrelated="unrelated update")
            install(third, "upgrade")
            last_setup = third
            assert {name: process(name) for name in ids} == after, "unrelated file update restarted services"
            for name in ("api", "storage"):
                control(name, "stop")
            fourth = build(4, options.service_v1, config="stopped update")
            install(fourth, "upgrade")
            last_setup = fourth
            assert not any(process(name) for name in ids), "stopped services started during upgrade"
            source = f"{source_root}/{names['api']}"
            original = guest("cat", source).stdout
            guest("tee", source, input_text=original + "# external modification\n")
            assert install(fourth, "repair", check=False).returncode != 0, "modified definition was overwritten"
            assert guest("cat", source).stdout == original + "# external modification\n"
            guest("tee", source, input_text=original)
            foreign_runlevel = f"/etc/runlevels/boot/{names['api']}" if options.manager == "openrc" else f"/etc/systemd/system/{names['api']}.d/override.conf"
            if options.manager == "openrc": guest("ln", "-s", source, foreign_runlevel)
            else:
                guest("mkdir", "-p", str(Path(foreign_runlevel).parent))
                guest("tee", foreign_runlevel, input_text="[Service]\nUser=root\n")
                guest("/usr/bin/systemctl", "daemon-reload")
            assert install(fourth, "repair", check=False).returncode != 0, "foreign runlevel was adopted"
            guest("test", "-e", foreign_runlevel)
            guest("rm", foreign_runlevel)
            if options.manager == "systemd":
                guest("rmdir", str(Path(foreign_runlevel).parent))
                guest("/usr/bin/systemctl", "daemon-reload")
            for name in ("storage", "api"):
                control(name, "start")
            failed = build(5, options.service_v2, config="failed update", fail_start=True)
            result = install(failed, "upgrade", check=False)
            assert result.returncode != 0, "failed startup reported success"
            assert guest("cat", f"{directory}/shared.conf").stdout == "stopped update", result.stdout
            assert all(process(name) for name in ids), "failed start did not restore previous running services"
            guest("touch", f"{directory}/foreign.txt")
            install(fourth, "uninstall")
            installed = False
            for name in ids:
                assert guest("test", "-e", f"{source_root}/{names[name]}", check=False).returncode != 0
                if options.manager == "openrc": assert guest("test", "-L", f"/etc/runlevels/default/{names[name]}", check=False).returncode != 0
            guest("test", "-f", f"{directory}/foreign.txt")
            guest("rm", f"{directory}/foreign.txt")
            guest("rmdir", directory, check=False)
            checks = ["install", "foreign_service_refused", "argument_escaping", "least_privilege", "running_upgrade", "managed_service_dependency", "shared_configuration_dependency", "unchanged_repair", "unrelated_file_update", "stopped_upgrade", "definition_tampering", "startup_override_tampering", "failed_start_compensation", "uninstall", "foreign_file_preserved"]
            if options.supervised: checks.append("native_restart_budget")
            print(json.dumps({"backend": options.manager, "target": options.target, "result": "passed", "checks": checks}))
        finally:
            retain = False
            if installed and last_setup:
                cleanup = install(last_setup, "uninstall", check=False)
                if cleanup.returncode:
                    retain = True
                    message = f"guest cleanup requires intervention; retained {remote}:\n{cleanup.stdout}"
                    if failure := sys.exception():
                        failure.add_note(message)
                    else:
                        raise RuntimeError(message)
            if not retain:
                guest("rm", "-rf", remote)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    for name in ("zup", "runtime", "service-v1", "service-v2", "ssh-key", "known-hosts"):
        parser.add_argument(f"--{name}", type=Path, required=True)
    parser.add_argument("--ssh-port", type=int, required=True)
    parser.add_argument("--target", choices=["linux-x64-musl", "linux-arm64-musl", "linux-x64", "linux-arm64"], required=True)
    parser.add_argument("--manager", choices=["openrc", "systemd"], default="openrc")
    parser.add_argument("--qmp-socket", type=Path)
    parser.add_argument("--crash-matrix", action="store_true")
    parser.add_argument("--crash-case")
    parser.add_argument("--supervised", action="store_true")
    verify(parser.parse_args())
