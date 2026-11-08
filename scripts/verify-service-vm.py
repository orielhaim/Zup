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


def journal_phase(record):
    phase = record["phase"]
    if isinstance(phase, str):
        return phase
    continuation = phase["recovery_required"]["resume"]
    assert continuation in ("prepared", "applying", "rolling_back")
    return "recovery_required"


def verify(options):
    if options.interference_only and (options.manager != "openrc" or not options.qmp_socket):
        raise ValueError("--interference-only requires an isolated OpenRC QMP VM")
    suffix = uuid.uuid4().hex
    application = f"com.example.zup-openrc-{suffix}"
    ids = {name: f"{suffix}-{name}" for name in ("storage", "api")}
    names = {name: f"zup-openrc-{identity.encode().hex()}" if options.manager == "openrc" else f"zup-{identity}-{hashlib.sha256(identity.encode()).hexdigest()[:12]}.service" for name, identity in ids.items()}
    source_root = "/etc/init.d" if options.manager == "openrc" else "/usr/local/lib/systemd/system"
    ssh_options = ["-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "-i", str(options.ssh_key), "-o", "StrictHostKeyChecking=yes", "-o", f"UserKnownHostsFile={options.known_hosts}"]
    ssh = ["ssh", *ssh_options, "-p", str(options.ssh_port), "root@127.0.0.1"]

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
        record = f"/run/openrc/supervise-{native}.child"
        result = guest("cat", record, check=False)
        if result.returncode:
            return None
        fields = result.stdout.split()
        if len(fields) != 5 or fields[4] != "running":
            return None
        pid = fields[1]
        if guest("test", "-d", f"/proc/{pid}", check=False).returncode:
            return None
        return pid

    def control(name, action, check=True):
        if options.manager == "openrc":
            return guest("/sbin/rc-service", "--nodeps", names[name], action, check=check)
        return guest("/usr/bin/systemctl", action, names[name], check=check)

    def execution_generations():
        boot = guest("cat", "/proc/sys/kernel/random/boot_id").stdout.strip()
        result = {}
        for name in ids:
            pid = process(name)
            if pid:
                fields = guest("cat", f"/proc/{pid}/stat").stdout.rsplit(") ", 1)[1].split()
                result[name] = (boot, pid, fields[19])
            else:
                result[name] = None
        return result

    with tempfile.TemporaryDirectory(prefix="zup-openrc-build-") as temporary:
        root = Path(temporary)
        inputs = root / "inputs"
        inputs.mkdir()
        provenance = {}
        for attribute in ("zup", "runtime", "service_v1", "service_v2"):
            source = getattr(options, attribute)
            destination = inputs / source.name
            shutil.copy2(source, destination)
            provenance[attribute] = hashlib.sha256(destination.read_bytes()).hexdigest()
            setattr(options, attribute, destination)
            if attribute == "runtime":
                descriptor = source.with_name(source.name + ".zup-toolchain.json")
                shutil.copy2(descriptor, destination.with_name(destination.name + ".zup-toolchain.json"))
        print(json.dumps({"artifacts": provenance, "backend": options.manager, "target": options.target}), flush=True)
        (root / "dist").mkdir()
        manifest = root / "zup.toml"
        remote = f"/var/tmp/zup-verification-{suffix}"
        guest("mkdir", "-m", "700", remote)
        installed = False
        last_setup = None
        directory = None
        shutdown_markers = f"/var/tmp/zup-shutdown-{suffix}"
        markers_created = False

        def build(version, binary, config="initial", unrelated="initial", fail_start=False, registration=False, startup="automatic", app_version=None, stop_after_ms=None, stop_grace_seconds=None, display_suffix="", detached_child=False):
            for name in ids:
                shutil.copyfile(binary, root / "dist" / name)
            (root / "dist/shared.conf").write_text(config)
            (root / "dist/unrelated.txt").write_text(unrelated)
            declaration = ""
            for name, identity in ids.items():
                dependencies = f'requires_services = ["{ids["storage"]}"]\n' if name == "api" else ""
                arguments = ["literal space", "double\"quote", "single'quote", f"$(touch {remote}/injected)", "`id`", "; exit 1"] if name == "api" else []
                if detached_child and name == "api":
                    arguments += ["--detached-child-file", f"{shutdown_markers}/detached-child"]
                if fail_start and name == "api":
                    arguments += ["--fail-after-ms", "0"]
                if registration and name == "api": arguments += ["--registration-update"]
                if stop_after_ms is not None:
                    arguments += ["--stop-after-ms", str(stop_after_ms), "--stopped-file", f"{shutdown_markers}/{name}"]
                timeouts = ""
                if stop_grace_seconds is not None:
                    native = f'type = "openrc", start_timeout_seconds = 20, stop_grace_seconds = {stop_grace_seconds}, kill_wait_seconds = 2, settle_seconds = 5' if options.manager == "openrc" else f'type = "systemd", start_timeout_seconds = 20, stop_grace_seconds = {stop_grace_seconds}, abort_timeout_seconds = 2, settle_seconds = 5'
                    timeouts = f", timeouts = {{ {native} }}"
                policy = ""
                if options.supervised and name == "api":
                    policy = 'failure = { type = "openrc", delay_seconds = 1, max_respawns = 2, respawn_period_seconds = 60 }\n' if options.manager == "openrc" else 'failure = { type = "systemd", condition = "on_failure", delay_seconds = 1, start_limit_burst = 3, start_limit_interval_seconds = 60 }\n'
                execution = '{ type = "openrc_user", user = "nobody", group = "nobody" }' if options.manager == "openrc" else '{ type = "systemd_user", user = "nobody" }'
                readiness = 'readiness = "notify"\n' if options.manager == "systemd" and fail_start and name == "api" else ""
                declaration += f'''[[services]]
id = "{identity}"
name = "{name}"
display_name = "{name}{display_suffix}"
binary = "${{install}}/{name}"
arguments = {json.dumps(arguments)}
requires_files = ["${{install}}/shared.conf"]
{dependencies}{policy}{readiness}start = "{startup}"
execution = {execution}
lifecycle = {{ install = "{'stopped' if startup == 'disabled' else 'running'}", change = "preserve"{timeouts} }}
'''
            manifest.write_text(f'''schema = 1
frontend = "headless"
[app]
id = "{application}"
name = "Native OpenRC verification"
version = "{app_version or f'{version}.0.0'}"
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
            subprocess.run(["scp", *ssh_options, "-P", str(options.ssh_port), str(setup), f"root@127.0.0.1:{remote}/setup-{version}"], check=True, timeout=120)
            destination = f"{remote}/setup-{version}"
            guest("chmod", "700", destination)
            return destination

        def install(setup, action, check=True):
            return guest(setup, action, "--scope", "machine", "--non-interactive", "--yes", "--output", "json", check=check)

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

        def crash_matrix(first):
            if not options.qmp_socket or options.supervised:
                raise ValueError("crash verification requires a disposable QMP VM and an unsupervised fixture")
            second = build(2, options.service_v2, config="updated", registration=True)
            failed = build(3, options.service_v2, config="failed", fail_start=True, registration=True)
            guest("sysctl", "-w", "kernel.core_pattern=/dev/null")
            baseline = set(guest("find", "/var/lib/zup/transactions", "-maxdepth", "2", "-name", "transaction.json").stdout.splitlines())
            baseline_generations = execution_generations()

            tag = f"zup-{suffix}"
            monitor("stop")
            try:
                monitor(f"savevm {tag}")
            finally:
                monitor("cont")
            def restore():
                monitor("stop")
                try:
                    monitor(f"loadvm {tag}")
                finally:
                    monitor("cont")
            results = []
            scenarios = [
                ("intent_before", "service_quiesce", "committed", False),
                ("intent_persisted", "service_quiesce", "recovery_required", False),
                ("native_request_before", "stop", "recovery_required", False),
                ("native_request_submitted", "stop", "committed" if options.manager == "openrc" else "recovery_required", False),
                ("apply_returned", "service_quiesce", "committed", False),
                ("native_configuration_changed", "stop", "committed", False),
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
                    crashed = guest("env", f"ZUP_TEST_FAULT_POINT={point}", f"ZUP_TEST_FAULT_OPERATION={filter_for(selector)}", f"ZUP_TEST_FAULT_OCCURRENCE={occurrence}", setup, "upgrade", "--scope", "machine", "--non-interactive", "--yes", "--output", "json", check=False)
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
                    closed_stop = (point == "apply_returned" and selector == "service_quiesce"
                        or point == "native_request_submitted" and selector == "stop" and options.manager == "openrc")
                    if closed_stop:
                        quiescing = next(node["kind"]["operation"] for node in record["plan"]["nodes"]
                            if node["kind"]["kind"] == "service_control"
                            and node["kind"]["operation"]["stage"] == "quiesce"
                            and record["nodes"][node["id"]] == "running")
                        assert quiescing["configuration"]["registration"]["start"] == "disabled", "lost quiesce reply lacks a closed activation intent"
                        native = quiescing["native_name"]
                        if options.manager == "systemd":
                            assert guest("/usr/bin/systemctl", "is-enabled", native, check=False).stdout.strip() == "masked"
                            assert guest("/usr/bin/systemctl", "show", native, "--property=MainPID", "--value").stdout.strip() == "0"
                            scope = guest("/usr/bin/systemctl", "show", native, "--property=ControlGroup", "--value").stdout.strip()
                            if scope:
                                assert "populated 0" in guest("cat", f"/sys/fs/cgroup{scope}/cgroup.events").stdout
                        else:
                            assert "start_pre() { return 1; }" in guest("cat", f"{source_root}/{native}").stdout
                            scope = f"/sys/fs/cgroup/openrc.{native}"
                            if guest("test", "-d", scope, check=False).returncode == 0:
                                assert "populated 0" in guest("cat", f"{scope}/cgroup.events").stdout
                    if point == "native_configuration_changed" and selector == "stop" and options.manager == "openrc":
                        assert "start_pre() { return 1; }" in guest("cat", f"{source_root}/{names['api']}").stdout
                        assert execution_generations()["api"] == baseline_generations["api"], "inhibition must retain the authorized runtime before stop dispatch"
                    recovery_arguments = (setup, "recover", "--transaction", transaction, "--scope", "machine", "--output", "json")
                    if recovery_fault:
                        recovery_point, recovery_selector = recovery_fault
                        interrupted = guest("env", f"ZUP_TEST_FAULT_POINT={recovery_point}", f"ZUP_TEST_FAULT_OPERATION={filter_for(recovery_selector)}", *recovery_arguments, check=False)
                        assert interrupted.returncode != 0, "recovery fault did not fire"
                    recovered = guest(*recovery_arguments, check=False)
                    record = json.loads(guest("cat", journal).stdout)
                    if journal_phase(record) != expected:
                        print(json.dumps({"failed_crash_case": f"{point}/{selector}", "journal": record, "native_generations": execution_generations()}), flush=True)
                    assert journal_phase(record) == expected, f"{point}/{selector}: {recovered.stdout}\n{record.get('failure')}\n{record.get('recovery_failure')}\n{record.get('rollback_failure')}"
                    if expected == "recovery_required":
                        assert recovered.returncode != 0, "ambiguous recovery reported success"
                        assert guest("sha256sum", *paths).stdout == before_files, "ambiguous recovery changed payload"
                        assert {name: process(name) for name in ids} == before_processes, "ambiguous recovery controlled a runtime"
                    else:
                        assert recovered.returncode == (0 if expected == "committed" else 1), recovered.stdout
                        assert all(process(name) for name in ids), "recovery did not restore running state"
                        assert guest("cat", f"{directory}/shared.conf").stdout == ("updated" if expected == "committed" else "initial")
                    results.append({"point": point, "operation": selector, "phase": record["phase"], "recovery_interrupted": bool(recovery_fault)})
                    print(json.dumps(results[-1]), flush=True)
                if not options.crash_case or options.crash_case == "new_operation_requires_recovery":
                    restore()
                    crashed = guest("env", "ZUP_TEST_FAULT_POINT=rollback_intent_persisted", "ZUP_TEST_FAULT_OPERATION=service_restore",
                        failed, "upgrade", "--scope", "machine", "--non-interactive", "--yes", "--output", "json", check=False)
                    assert crashed.returncode != 0, "rollback fault did not fire"
                    journals = set(guest("find", "/var/lib/zup/transactions", "-maxdepth", "2", "-name", "transaction.json").stdout.splitlines()) - baseline
                    assert len(journals) == 1, f"unexpected rollback journals: {journals}"
                    journal = journals.pop()
                    before_record = guest("cat", journal).stdout
                    before_files = guest("sha256sum", *[f"{directory}/{name}" for name in ("api", "storage", "shared.conf")]).stdout
                    before_generations = execution_generations()
                    removed = install(first, "uninstall", check=False)
                    record = json.loads(guest("cat", journal).stdout)
                    assert removed.returncode == 7, f"unsettled removal did not report recovery required:\n{removed.stdout}"
                    assert guest("cat", journal).stdout == before_record, "removal implicitly resumed the interrupted transaction"
                    assert execution_generations() == before_generations, "unsettled removal controlled a runtime"
                    assert guest("sha256sum", *[f"{directory}/{name}" for name in ("api", "storage", "shared.conf")]).stdout == before_files, "unsettled removal mutated payload"
                    recovered = guest(failed, "recover", "--transaction", record["transaction_id"], "--scope", "machine", "--output", "json", check=False)
                    record = json.loads(guest("cat", journal).stdout)
                    assert recovered.returncode == 1, recovered.stdout
                    assert journal_phase(record) == "rolled_back", "the interrupted transaction did not compensate"
                    removed = install(first, "uninstall", check=False)
                    assert removed.returncode == 0, removed.stdout
                    assert not any(process(name) for name in ids), "uninstall left a native execution"
                    assert all(guest("test", "-e", f"{directory}/{name}", check=False).returncode != 0 for name in ids), "uninstall retained service payload"
                    results.append({"point": "new_operation_requires_recovery", "operation": "uninstall", "phase": record["phase"], "recovery_interrupted": False})
                    print(json.dumps(results[-1]), flush=True)
            finally:
                restore()
                monitor("stop")
                try:
                    monitor(f"delvm {tag}")
                finally:
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
            if options.manager == "openrc":
                orphan = guest("python3", "-c", "import subprocess; p=subprocess.Popen(['sleep','300'], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True); print(p.pid)").stdout.strip()
                ticks = guest("cat", f"/proc/{orphan}/stat").stdout.rsplit(") ", 1)[1].split()[19]
                identity = f"/run/openrc/supervise-{names['api']}.identity"
                guest("tee", identity, input_text=f"{'0' * 32} {orphan} {ticks}\n")
                guest("chmod", "600", identity)
                try:
                    assert install(first, "install", check=False).returncode != 0, "absent registration accepted a live controller record"
                    assert guest("test", "-e", f"{directory}/api", check=False).returncode != 0, "orphan rejection occurred after payload mutation"
                    assert guest("cat", f"/proc/{orphan}/stat").stdout.rsplit(") ", 1)[1].split()[19] == ticks, "orphan rejection controlled an unrelated process"
                finally:
                    guest("rm", identity)
                    guest("python3", "-c", "import os,signal,sys; p=int(sys.argv[1]); f=os.pidfd_open(p); assert open('/proc/%d/stat'%p).read().rsplit(') ',1)[1].split()[19]==sys.argv[2]; signal.pidfd_send_signal(f,signal.SIGKILL); os.close(f)", orphan, ticks)
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
            if options.interference_only:
                tag = f"zup-interference-{suffix}"
                guest("sync")
                monitor("stop")
                try:
                    monitor(f"savevm {tag}")
                finally:
                    monitor("cont")
                enabled_source = guest("cat", f"{source_root}/{names['api']}").stdout
                interference = build(20, options.service_v2, config="interference")
                latch = f"{remote}/dependent-state"
                paths = [f"{directory}/{name}" for name in ("api", "storage", "shared.conf")]
                original_files = guest("sha256sum", *paths).stdout
                original_storage = execution_generations()["storage"]
                arguments = ["env", "ZUP_TEST_FAULT_POINT=native_request_before", f"ZUP_TEST_FAULT_OPERATION={names['storage']}",
                    "ZUP_TEST_FAULT_ACTION=pause", f"ZUP_TEST_FAULT_LATCH={latch}", interference,
                    "upgrade", "--scope", "machine", "--non-interactive", "--yes", "--output", "json"]
                pending = subprocess.Popen([*ssh, shlex.join(arguments)], text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
                try:
                    deadline = time.monotonic() + 60
                    while guest("test", "-f", f"{latch}.ready", check=False).returncode:
                        assert pending.poll() is None, "upgrade exited before the interference boundary"
                        assert time.monotonic() < deadline, "upgrade did not reach the interference boundary"
                        time.sleep(0.05)
                    disabled_source = guest("cat", f"{source_root}/{names['api']}").stdout
                    guest("tee", f"{source_root}/{names['api']}", input_text=enabled_source)
                    control("api", "start")
                    unmarked_pid = process("api")
                    assert unmarked_pid, "external dependent did not start"
                    guest("tee", f"{source_root}/{names['api']}", input_text=disabled_source)
                    control("api", "zap")
                    guest("test", "-d", f"/proc/{unmarked_pid}")
                    assert guest("test", "-e", f"/run/openrc/started/{names['api']}", check=False).returncode != 0
                    guest("touch", f"{latch}.release")
                    output, _ = pending.communicate(timeout=240)
                    assert pending.returncode != 0, f"unmarked live dependent was accepted: {output}"
                    assert execution_generations()["storage"] == original_storage, "prerequisite stopped while its unmarked dependent was live"
                    assert guest("sha256sum", *paths).stdout == original_files, "interference allowed payload mutation"
                    records = guest("find", "/var/lib/zup/transactions", "-maxdepth", "2", "-name", "transaction.json").stdout.splitlines()
                    failed = [json.loads(guest("cat", path).stdout) for path in records]
                    failed = [record for record in failed if record["app_id"] == application and journal_phase(record) == "recovery_required"]
                    assert len(failed) == 1, "external interference did not retain one recovery intent"
                    recovered = guest(interference, "recover", "--transaction", failed[0]["transaction_id"], "--scope", "machine", "--output", "json", check=False)
                    assert recovered.returncode != 0 and json.loads(recovered.stdout)["outcome"] == "recovery_required", recovered.stdout
                    assert execution_generations()["storage"] == original_storage, "ambiguous recovery interrupted the prerequisite"
                    assert guest("sha256sum", *paths).stdout == original_files, "ambiguous recovery changed payload"
                finally:
                    guest("touch", f"{latch}.release", check=False)
                    if pending.poll() is None:
                        pending.communicate(timeout=240)
                    monitor("stop")
                    try:
                        monitor(f"loadvm {tag}")
                        monitor(f"delvm {tag}")
                    finally:
                        monitor("cont")
                assert execution_generations()["storage"] == original_storage, "VM baseline was not restored"
                print(json.dumps({"backend": options.manager, "target": options.target, "result": "passed",
                    "checks": ["live_dependent_without_started_marker_refused", "external_interference_recovery_fails_closed"]}), flush=True)
                return
            generations = execution_generations()
            for version, startup in [(6, "manual"), (7, "automatic")]:
                policy_setup = build(version, options.service_v1, startup=startup, app_version=f"1.{version}.0")
                install(policy_setup, "upgrade")
                last_setup = policy_setup
                assert execution_generations() == generations, "boot policy update replaced a running execution"
            metadata = build(13, options.service_v1, app_version="1.8.0", display_suffix=" updated")
            install(metadata, "upgrade")
            last_setup = metadata
            assert execution_generations() == generations, "display metadata update replaced native execution"
            if options.manager == "openrc":
                script = root / "script-service"
                script.write_text("#!/bin/sh\nexec /bin/sleep 600\n")
                invalid = build(9, script)
                before_images = guest("sha256sum", f"{directory}/storage", f"{directory}/api").stdout
                assert install(invalid, "upgrade", check=False).returncode != 0, "OpenRC accepted a shebang service image"
                assert {name: process(name) for name in ids} == before, "format rejection interrupted running services"
                assert guest("sha256sum", f"{directory}/storage", f"{directory}/api").stdout == before_images, "format rejection replaced installed images"
            if options.manager == "openrc":
                generation_record = f"/run/openrc/supervise-{names['api']}.identity"
                saved_generation = guest("cat", generation_record).stdout
                instance, controller_pid, _ = saved_generation.split()
                guest("tee", generation_record, input_text=f"{instance} {controller_pid} 0\n")
                try:
                    assert install(metadata, "repair", check=False).returncode != 0, "stale native PID generation was accepted"
                    assert execution_generations() == generations, "stale identity rejection controlled a runtime"
                finally:
                    guest("tee", generation_record, input_text=saved_generation)
                scope_control = f"/sys/fs/cgroup/openrc.{names['api']}/cgroup.procs"
                guest("chown", "nobody", scope_control)
                try:
                    assert install(metadata, "repair", check=False).returncode != 0, "delegated process scope was accepted"
                    assert execution_generations() == generations, "delegation rejection controlled a runtime"
                finally:
                    guest("chown", "root", scope_control)
            if options.supervised:
                if options.manager == "systemd":
                    control("api", "stop")
                    guest("/usr/bin/systemctl", "reset-failed", names["api"])
                    control("api", "start")
                    before["api"] = process("api")
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
                assert len(generations) == 3 and not process("api"), f"{options.manager} restart budget was not enforced: {generations}"
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
            for version, startup in [(11, "manual"), (12, "automatic")]:
                policy_setup = build(version, options.service_v2, config="updated", unrelated="unrelated update", startup=startup, app_version=f"3.{version}.0")
                install(policy_setup, "upgrade")
                last_setup = policy_setup
                assert not any(execution_generations().values()), "boot policy update started a stopped service"
            disabled = build(18, options.service_v2, config="updated", unrelated="unrelated update", startup="disabled", app_version="3.18.0")
            install(disabled, "upgrade")
            last_setup = disabled
            for name in ids:
                assert control(name, "start", check=False).returncode != 0, "disabled service accepted native activation"
            assert not any(execution_generations().values()), "disabled native activation created a process"
            disabled_update = build(21, options.service_v1, config="disabled update", unrelated="unrelated update", startup="disabled", app_version="3.18.1")
            baseline = set(guest("find", "/var/lib/zup/transactions", "-maxdepth", "2", "-name", "transaction.json").stdout.splitlines())
            interrupted = guest("env", "ZUP_TEST_FAULT_POINT=apply_returned", "ZUP_TEST_FAULT_OPERATION=service_quiesce",
                disabled_update, "upgrade", "--scope", "machine", "--non-interactive", "--yes", "--output", "json", check=False)
            assert interrupted.returncode != 0, "disabled quiescence fault did not fire"
            journals = set(guest("find", "/var/lib/zup/transactions", "-maxdepth", "2", "-name", "transaction.json").stdout.splitlines()) - baseline
            assert len(journals) == 1, "disabled upgrade did not retain one recovery journal"
            journal = journals.pop()
            transaction = json.loads(guest("cat", journal).stdout)["transaction_id"]
            guest(disabled_update, "recover", "--transaction", transaction, "--scope", "machine", "--output", "json")
            assert json.loads(guest("cat", journal).stdout)["phase"] == "committed"
            assert not any(execution_generations().values()), "disabled recovery started a stopped service"
            assert guest("cat", f"{directory}/shared.conf").stdout == "disabled update"
            last_setup = disabled_update
            enabled = build(19, options.service_v2, config="updated", unrelated="unrelated update", app_version="3.19.0")
            install(enabled, "upgrade")
            last_setup = enabled
            assert not any(execution_generations().values()), "reenabling boot startup started a stopped service"
            if options.manager == "openrc":
                invalid_stopped = build(10, script, config="updated")
                before_images = guest("sha256sum", f"{directory}/storage", f"{directory}/api").stdout
                assert install(invalid_stopped, "upgrade", check=False).returncode != 0, "OpenRC accepted a shebang image while stopped"
                assert not any(process(name) for name in ids), "format rejection started a stopped service"
                assert guest("sha256sum", f"{directory}/storage", f"{directory}/api").stdout == before_images, "format rejection replaced stopped images"
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
            guest("mkdir", "-m", "700", shutdown_markers)
            markers_created = True
            guest("chown", "nobody:nobody", shutdown_markers)
            if options.manager == "openrc":
                child_setup = build(20, options.service_v1, config="detached child", app_version="6.0.0", detached_child=True)
                install(child_setup, "upgrade")
                last_setup = child_setup
                deadline = time.monotonic() + 10
                child_record = f"{shutdown_markers}/detached-child"
                while guest("test", "-s", child_record, check=False).returncode:
                    assert time.monotonic() < deadline, "detached child did not start"
                    time.sleep(0.1)
                child = guest("cat", child_record).stdout.strip()
                membership = guest("cat", f"/proc/{child}/cgroup").stdout.strip()
                assert membership == f"0::/openrc.{names['api']}", "detached child escaped its native process scope"
                control("api", "stop")
                guest("test", "-d", f"/proc/{child}")
                before_images = guest("sha256sum", f"{directory}/storage", f"{directory}/api", f"{directory}/shared.conf").stdout
                assert install(child_setup, "repair", check=False).returncode != 0, "repair accepted a detached runtime after main process exit"
                assert guest("sha256sum", f"{directory}/storage", f"{directory}/api", f"{directory}/shared.conf").stdout == before_images, "ambiguous runtime observation mutated service inputs"
                guest("kill", "-KILL", child)
                deadline = time.monotonic() + 10
                while guest("cat", f"/sys/fs/cgroup/openrc.{names['api']}/cgroup.events").stdout.split("populated ")[1].splitlines()[0] != "0":
                    assert time.monotonic() < deadline, "detached child remained populated after exit"
                    time.sleep(0.1)
                # Replace the child-launching definition while stopped before starting it again.
            graceful = build(15, options.service_v1, config="custom shutdown", stop_after_ms=1500, stop_grace_seconds=4)
            install(graceful, "upgrade")
            last_setup = graceful
            if options.manager == "openrc":
                control("api", "start")
            old_generations = execution_generations()
            policy = build(16, options.service_v1, config="custom shutdown", stop_after_ms=1500, stop_grace_seconds=1)
            install(policy, "upgrade")
            last_setup = policy
            policy_generations = execution_generations()
            if options.manager == "openrc":
                for name in ids:
                    assert guest("cat", f"{shutdown_markers}/{name}").stdout.strip() == old_generations[name][1], "old shutdown grace was shortened by the replacement policy"
                    guest("rm", f"{shutdown_markers}/{name}")
                assert policy_generations != old_generations, "OpenRC retry update did not replace native supervision"
            else:
                assert policy_generations == old_generations, "systemd timeout update replaced native execution"
            bounded = build(17, options.service_v2, config="bounded shutdown", stop_after_ms=15000, stop_grace_seconds=1)
            install(bounded, "upgrade")
            last_setup = bounded
            for name in ids:
                guest("rm", "-f", f"{shutdown_markers}/{name}")
            guest("touch", f"{directory}/foreign.txt")
            install(graceful, "uninstall")
            installed = False
            for name in ids:
                assert guest("test", "-e", f"{shutdown_markers}/{name}", check=False).returncode != 0, "a slow shutdown outlived the configured native grace"
            for name in ids:
                assert guest("test", "-e", f"{source_root}/{names[name]}", check=False).returncode != 0
                if options.manager == "openrc": assert guest("test", "-L", f"/etc/runlevels/default/{names[name]}", check=False).returncode != 0
            guest("test", "-f", f"{directory}/foreign.txt")
            guest("rm", f"{directory}/foreign.txt")
            guest("rmdir", directory, check=False)
            checks = ["install", "foreign_service_refused", "argument_escaping", "least_privilege", "running_upgrade", "managed_service_dependency", "shared_configuration_dependency", "unchanged_repair", "unrelated_file_update", "stopped_upgrade", "definition_tampering", "startup_override_tampering", "failed_start_compensation", "uninstall", "foreign_file_preserved"]
            if options.supervised: checks.append("native_restart_budget")
            if options.manager == "openrc": checks.extend(["shebang_refused_before_mutation", "detached_child_refuses_false_quiescence", "stale_pid_generation_refused", "delegated_cgroup_refused"])
            checks.extend(["running_boot_policy_preserves_generation", "stopped_boot_policy_preserves_state"])
            checks.extend(["disabled_refuses_native_activation", "disabled_upgrade_lost_quiescence_reply"])
            checks.extend(["custom_graceful_shutdown", "native_timeout_escalation", "saved_timeout_policy_uninstall", "display_name_preserves_generation"])
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
                if markers_created:
                    guest("rm", "-rf", shutdown_markers)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    for name in ("zup", "runtime", "service-v1", "service-v2", "ssh-key", "known-hosts"):
        parser.add_argument(f"--{name}", type=Path, required=True)
    parser.add_argument("--ssh-port", type=int, required=True)
    parser.add_argument("--target", choices=["linux-x64-musl", "linux-arm64-musl", "linux-x64", "linux-arm64"], required=True)
    parser.add_argument("--manager", choices=["openrc", "systemd"], default="openrc")
    parser.add_argument("--qmp-socket", type=Path)
    parser.add_argument("--crash-matrix", action="store_true")
    parser.add_argument("--interference-only", action="store_true")
    parser.add_argument("--crash-case")
    parser.add_argument("--supervised", action="store_true")
    verify(parser.parse_args())
