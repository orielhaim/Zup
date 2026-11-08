import argparse
import json
import shlex
import subprocess
import time
import uuid
from pathlib import Path

parser = argparse.ArgumentParser(
    description="Exercise the strict controller in a disposable OpenRC guest"
)
parser.add_argument("--ssh-key", type=Path, required=True)
parser.add_argument("--known-hosts", type=Path, required=True)
parser.add_argument("--ssh-port", type=int, required=True)
parser.add_argument("--controller", default="/sbin/supervise-daemon")
parser.add_argument("--controller-receipt", required=True)
parser.add_argument("--service", type=Path, required=True)
options = parser.parse_args()
opts = [
    "-i",
    str(options.ssh_key),
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=10",
    "-o",
    "StrictHostKeyChecking=yes",
    "-o",
    "UserKnownHostsFile=" + str(options.known_hosts),
]
ssh = ["ssh", *opts, "-p", str(options.ssh_port), "root@127.0.0.1"]
native = options.controller


def run(*args, check=True):
    p = subprocess.run(
        ssh + [shlex.join(args)],
        text=True,
        capture_output=True,
        check=False,
        timeout=30,
    )
    if check and p.returncode:
        raise RuntimeError(f"{args}: {p.stdout}\n{p.stderr}")
    return p


def script(s):
    return subprocess.run(
        ssh + ["sh", "-se"],
        input=s,
        text=True,
        capture_output=True,
        check=True,
        timeout=60,
    )


receipt = json.loads(run("cat", options.controller_receipt).stdout)
locked = json.loads(
    (Path(__file__).resolve().parent.parent / "native/openrc/upstream.json").read_text()
)
assert receipt["upstream"] == locked, (
    "controller was not built from the current pinned source"
)
assert run("sha256sum", native).stdout.split()[0] == receipt["binary_sha256"], (
    "controller differs from its build receipt"
)
assert run(native, "--control-features").stdout.strip() == locked["control_protocol"]
print(json.dumps({"controller_build": receipt}), flush=True)

name = "zup-pidfd-proof-" + uuid.uuid4().hex[:12]
app = "/opt/" + name
marker = "/var/tmp/" + name
svc = "/etc/init.d/" + name
subprocess.run(
    [
        "scp",
        *opts,
        "-P",
        str(options.ssh_port),
        str(options.service),
        "root@127.0.0.1:/tmp/" + name,
    ],
    check=True,
    capture_output=True,
)
checks = []


def report(check, **details):
    checks.append(dict(check=check, **details))
    print(json.dumps(checks[-1]), flush=True)


def configure(args, no_restart=True, start_timeout=30, preexec=False):
    extra = "--no-respawn" if no_restart else ""
    preload = f"export LD_PRELOAD='{app}/delay.so'\n" if preexec else ""
    script(f"""mkdir -p {app} {marker}; chmod 0755 {app}; chown nobody:nobody {marker}; chmod 0700 {marker}
cp /tmp/{name} {app}/service; chmod 0755 {app}/service
cat > {svc} <<'SERVICE'
#!/sbin/openrc-run
export PATH='{native.rsplit("/", 1)[0]}':"$PATH"
{preload}
command='{app}/service'
command_args='{args}'
command_user='nobody:nobody'
pidfile='/run/{name}.pid'
retry='TERM/1/KILL/1'
no_new_privs=yes
supervisor=supervise-daemon
supervise_daemon_args='--pidfd-control --respawn-delay-step 0 --start-timeout {start_timeout} {extra}'
respawn_delay=1
respawn_max=2
respawn_period=60
rc_cgroup_mode=unified
rc_cgroup_cleanup=NO
stop() {{ supervise-daemon "$RC_SVCNAME" --shutdown --pidfile "$pidfile" --shutdown-timeout 5; }}
SERVICE
chmod 0755 {svc}
""")


def main_pid():
    p = run("cat", f"/run/openrc/options/{name}/child_pid", check=False)
    if p.returncode:
        return None
    pid = p.stdout.strip()
    status = run("cat", f"/proc/{pid}/stat", check=False)
    if status.returncode:
        return None
    fields = status.stdout.rsplit(") ", 1)[1].split()
    if fields[0] in ("Z", "X"):
        return None
    return pid, fields[19]


def empty():
    cg = "/sys/fs/cgroup/openrc." + name
    p = run("cat", cg + "/cgroup.events", check=False)
    if p.returncode:
        return run("test", "-e", cg, check=False).returncode == 1
    return "populated 0" in p.stdout


def wait_for(predicate, seconds=5):
    deadline = time.monotonic() + seconds
    while True:
        value = predicate()
        if value:
            return value
        assert time.monotonic() < deadline, "native state did not settle"
        time.sleep(0.02)


def start():
    run("/sbin/rc-service", name, "start")
    return wait_for(main_pid)


def stop():
    begin = time.monotonic()
    run("/sbin/rc-service", name, "stop")
    elapsed = time.monotonic() - begin
    wait_for(empty)
    return elapsed


def stop_owned_process(pid, ticks):
    code = "import os,signal,sys; p=int(sys.argv[1]); expected=sys.argv[2]; f=os.pidfd_open(p); stat=open('/proc/%d/stat'%p).read().rsplit(') ',1)[1].split(); assert stat[19]==expected; signal.pidfd_send_signal(f,signal.SIGKILL); os.close(f)"
    run("python3", "-c", code, pid, ticks)


def generation(pid):
    return run("cat", f"/proc/{pid}/stat").stdout.rsplit(") ", 1)[1].split()[19]


try:
    script(f"mkdir -p {app}\n")
    code = r"""#define _GNU_SOURCE
#include <dlfcn.h>
#include <string.h>
#include <time.h>
int execvp(const char *file, char *const argv[]) {
    if (strstr(file, "/zup-pidfd-proof-") && strstr(file, "/service")) {
     struct timespec delay = { .tv_sec = 2 };
     while (nanosleep(&delay, &delay) < 0) {}
    }
    int (*next)(const char *, char *const[]) = dlsym(RTLD_NEXT, "execvp");
    return next(file, argv);
}
"""
    script(
        f"cat > {app}/delay.c <<'CODE'\n{code}\nCODE\ncc -shared -fPIC -o {app}/delay.so {app}/delay.c -ldl\n"
    )
    configure("", start_timeout=6, preexec=True)
    begin = time.monotonic()
    pid, ticks = start()
    elapsed = time.monotonic() - begin
    assert elapsed >= 1.8 and elapsed < 10
    stop()
    report("custom_start_timeout_allows_slow_exec", seconds=elapsed)
    configure("", no_restart=False, start_timeout=1, preexec=True)
    begin = time.monotonic()
    failed = run("/sbin/rc-service", name, "start", check=False)
    elapsed = time.monotonic() - begin
    assert failed.returncode != 0, "exec handoff outlived its native startup deadline"
    wait_for(empty)
    assert main_pid() is None
    run("/sbin/rc-service", name, "zap")
    report("native_start_timeout_reaps_bootstrap_without_respawn", seconds=elapsed)
    configure("", no_restart=False, start_timeout=1, preexec=True)
    run(
        "python3",
        "-c",
        """import os, signal, subprocess, sys, time
name = sys.argv[1]
process = subprocess.Popen(['/sbin/rc-service', name, 'start'], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
handle = os.pidfd_open(process.pid)
try:
    deadline = time.monotonic() + 5
    while not os.path.exists('/run/openrc/supervise-' + name + '.identity'):
        assert process.poll() is None and time.monotonic() < deadline
        time.sleep(.01)
    signal.pidfd_send_signal(handle, signal.SIGKILL)
    process.wait(timeout=5)
finally:
    os.close(handle)
""",
        name,
    )
    wait_for(empty)
    assert main_pid() is None
    run("/sbin/rc-service", name, "zap")
    report("interrupted_start_retains_native_deadline_and_suppresses_respawn")

    configure(f"--stop-after-ms 350 --stopped-file {marker}/stopped")
    pid, ticks = start()
    status = run("cat", f"/proc/{pid}/status").stdout
    assert "Uid:\t65534\t65534\t65534\t65534" in status
    assert "NoNewPrivs:\t1" in status
    assert run("cat", f"/proc/{pid}/cgroup").stdout.strip() == "0::/openrc." + name
    elapsed = stop()
    assert 0.3 <= elapsed < 5
    assert run("cat", marker + "/stopped").stdout.strip() == pid
    report("graceful_stop_and_identity", seconds=elapsed)
    configure("--ignore-term")
    pid, ticks = start()
    elapsed = stop()
    assert 0.9 <= elapsed < 5
    report("native_escalation", seconds=elapsed)
    configure("--ignore-term")
    pid, ticks = start()
    controller = run("cat", "/run/" + name + ".pid").stdout.strip()
    run("kill", "-ALRM", controller)
    time.sleep(0.3)
    assert main_pid() == (pid, ticks), (
        "strict controller entered legacy healthcheck termination"
    )
    child = run("cat", "/run/openrc/supervise-" + name + ".child").stdout.split()
    assert child[1:] == [pid, ticks, "1", "running"], child
    stop()
    report("strict_signal_cannot_enter_legacy_healthcheck")
    configure("--fail-after-ms 1500")
    pid, ticks = start()
    wait_for(lambda: main_pid() is None)
    time.sleep(1)
    assert main_pid() is None and empty()
    run("/sbin/rc-service", name, "zap")
    report("no_respawn", generation=[pid, ticks])
    configure("--ignore-term")
    pid, ticks = start()
    foreign = run(
        "python3",
        "-c",
        "import subprocess; p=subprocess.Popen(['sleep','120'],stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True); print(p.pid)",
    ).stdout.strip()
    foreign_ticks = generation(foreign)
    script(f"printf '%s\\n' {foreign} > /run/{name}.pid\n")
    refused = run(
        "env",
        "RC_SVCNAME=" + name,
        native,
        name,
        "--shutdown",
        "--pidfile",
        "/run/" + name + ".pid",
        "--shutdown-timeout",
        "2",
        check=False,
    )
    assert refused.returncode == 0 and generation(foreign) == foreign_ticks
    wait_for(empty)
    stop_owned_process(foreign, foreign_ticks)
    run("/sbin/rc-service", name, "zap")
    report("pidfile_is_advisory_and_foreign_process_survives")
    configure("--ignore-term")
    pid, ticks = start()
    identity_path = "/run/openrc/supervise-" + name + ".identity"
    identity = run("cat", identity_path).stdout.strip()
    parts = identity.split()
    parts[0] = "0" * 32
    script(f"printf '%s\\n' '{' '.join(parts)}' > {identity_path}\n")
    refused = run(
        "env",
        "RC_SVCNAME=" + name,
        native,
        name,
        "--shutdown",
        "--shutdown-timeout",
        "2",
        check=False,
    )
    assert refused.returncode != 0 and main_pid() == (pid, ticks), (
        "stale controller identity stopped an unrelated instance"
    )
    script(f"printf '%s\\n' '{identity}' > {identity_path}\n")
    stop()
    report("stale_controller_instance_refused_before_mutation")
    configure("--ignore-term")
    pid, ticks = start()
    instance = run("cat", "/run/openrc/supervise-" + name + ".identity").stdout.split()[
        0
    ]
    guarded = [
        "env",
        "RC_SVCNAME=" + name,
        native,
        name,
        "--shutdown",
        "--shutdown-timeout",
        "2",
        "--expected-instance",
        instance,
    ]
    refused = run(
        *guarded,
        "--expected-child",
        pid + ":" + str(int(ticks) + 1) + ":1",
        check=False,
    )
    assert refused.returncode != 0 and main_pid() == (pid, ticks), (
        "stale child generation was signaled"
    )
    run(*guarded, "--expected-child", pid + ":" + ticks + ":1")
    wait_for(empty)
    report("guarded_shutdown_binds_controller_and_child_generation")
    configure("--ignore-term")
    pid, ticks = start()
    instance = run("cat", "/run/openrc/supervise-" + name + ".identity").stdout.split()[
        0
    ]
    arguments = [
        "env",
        "RC_SVCNAME=" + name,
        native,
        name,
        "--shutdown",
        "--shutdown-timeout",
        "5",
        "--expected-instance",
        instance,
        "--expected-child",
        pid + ":" + ticks + ":1",
    ]
    pending = subprocess.Popen(
        ssh + [shlex.join(arguments)],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    time.sleep(0.1)
    refused = run(*arguments, check=False)
    assert (
        refused.returncode != 0
        and "Another shutdown observation" in refused.stdout + refused.stderr
    )
    output, _ = pending.communicate(timeout=8)
    assert pending.returncode == 0, output
    wait_for(empty)
    report("conflicting_shutdown_observer_refused")
    configure("--ignore-term")
    pid, ticks = start()
    controller = run("cat", "/run/" + name + ".pid").stdout.strip()
    controller_ticks = generation(controller)
    pending = subprocess.Popen(
        ssh
        + [
            shlex.join(
                [
                    "env",
                    "RC_SVCNAME=" + name,
                    native,
                    name,
                    "--shutdown",
                    "--pidfile",
                    "/run/" + name + ".pid",
                    "--shutdown-timeout",
                    "3",
                ]
            )
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    time.sleep(0.1)
    stop_owned_process(controller, controller_ticks)
    output, _ = pending.communicate(timeout=5)
    assert pending.returncode != 0, (
        "controller crash falsely acknowledged child shutdown: " + output
    )
    assert generation(pid) == ticks, (
        "crash fixture did not retain its ignored-TERM child"
    )
    stop_owned_process(pid, ticks)
    wait_for(empty)
    run("/sbin/rc-service", name, "zap")
    report("controller_crash_does_not_prove_child_shutdown")
    configure("", False)
    script(
        f"sed -i 's/^respawn_delay=1$/respawn_delay=30/;s/^respawn_period=60$/respawn_period=120/' {svc}\n"
    )
    pid, ticks = start()
    result = run(
        "python3",
        "-c",
        """import os, subprocess, sys, time, json
native,name,pid,ticks=sys.argv[1:]; pid=int(pid)
assert open('/proc/%d/stat'%pid).read().rsplit(') ',1)[1].split()[19]==ticks
subprocess.run(['env','RC_SVCNAME='+name,native,name,'--signal','KILL'],check=True)
deadline=time.monotonic()+5
while os.path.exists('/proc/%d'%pid):
 assert time.monotonic()<deadline
 time.sleep(.01)
last='/proc/sys/kernel/ns_last_pid'; saved=open(last).read(); foreign=None
try:
 for attempt in range(16):
  with open(last,'w') as f:f.write(str(pid-1))
  candidate=subprocess.Popen(['sleep','60'],stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
  if candidate.pid==pid:foreign=candidate;break
  candidate.terminate();candidate.wait()
 assert foreign is not None,'could not force actual PID reuse in isolated VM'
 generation=open('/proc/%d/stat'%pid).read().rsplit(') ',1)[1].split()[19]
 assert generation != ticks
 instance=open('/run/openrc/supervise-'+name+'.identity').read().split()[0]
 result=subprocess.run(['env','RC_SVCNAME='+name,native,name,'--shutdown','--shutdown-timeout','3','--expected-instance',instance,'--expected-child',str(pid)+':'+ticks+':1'],text=True,capture_output=True)
 assert result.returncode==0,result.stdout+result.stderr
 assert foreign.poll() is None,'native controller signaled the reused foreign PID'
 assert open('/proc/%d/stat'%pid).read().rsplit(') ',1)[1].split()[19]==generation
 print(json.dumps({'old_pid':pid,'old_start_ticks':ticks,'reused_start_ticks':generation}))
finally:
 if foreign is not None:
  foreign.terminate();foreign.wait()
 with open(last,'w') as f:f.write(saved)
""",
        native,
        name,
        pid,
        ticks,
    )
    wait_for(empty)
    report(
        "actual_pid_reuse_does_not_redirect_native_shutdown",
        **json.loads(result.stdout),
    )
    configure("", False)
    generations = []
    for attempt in range(3):
        if attempt == 0:
            current = start()
        else:
            wait_for(lambda: main_pid() is not None and main_pid() != generations[-1])
            current = main_pid()
        generations.append(current)
        run("env", "RC_SVCNAME=" + name, native, name, "--signal", "KILL")
    wait_for(lambda: main_pid() is None and empty())
    assert len(set(generations)) == 3
    run("/sbin/rc-service", name, "zap")
    report("bounded_native_restarts", generations=generations)
    print(json.dumps({"checks": checks, "name": name}), flush=True)
finally:
    run("/sbin/rc-service", name, "stop", check=False)
    print("retained native proof files:", name, flush=True)
