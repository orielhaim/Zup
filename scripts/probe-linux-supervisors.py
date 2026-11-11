"""Compare stock OpenRC supervisors in an isolated Alpine QEMU overlay."""

import argparse
import hashlib
import json
import os
import platform
import shutil
from pathlib import Path
import secrets
import shlex
import socket
import subprocess
import tempfile
import time


def command(arguments, **options):
    return subprocess.run(arguments, check=True, capture_output=True, text=True,
                          timeout=options.pop("timeout", 60), **options)


class Guest:
    def __init__(self, base, cache, disk_size=None, architecture="x86_64", manager="openrc"):
        if architecture not in ("x86_64", "aarch64") or manager not in ("openrc", "systemd"):
            raise ValueError("unsupported native verification guest")
        self.architecture, self.manager = architecture, manager
        self.base = base.resolve(strict=True)
        cache.mkdir(parents=True, exist_ok=True)
        self.root = Path(tempfile.mkdtemp(prefix="supervisor-probe-", dir=cache))
        self.process = None
        self.disk_size = disk_size
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            self.port = reservation.getsockname()[1]

    def __enter__(self):
        try:
            self.start()
            return self
        except BaseException:
            self.close()
            raise

    def __exit__(self, *_):
        self.close()

    def close(self):
        if self.process is not None and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=10)

    def start(self):
        print(f"Preparing isolated guest: {self.root}", flush=True)
        for name in ("client", "host"):
            command(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(self.root / name)])
        public = (self.root / "client.pub").read_text().strip()
        host_public = (self.root / "host.pub").read_text().strip()
        configuration = {
            "disable_root": False,
            "ssh_pwauth": False,
            "ssh": {"emit_keys_to_console": False},
            "users": [{"name": "root", "lock_passwd": False,
                       "ssh_authorized_keys": [public]}],
            "chpasswd": {"expire": False, "users": [{"name": "root", "type": "text",
                                                       "password": secrets.token_hex(32)}]},
            "ssh_keys": {"ed25519_private": (self.root / "host").read_text(),
                         "ed25519_public": host_public},
            "runcmd": ([["rc-update", "add", "sshd", "default"], ["rc-service", "sshd", "start"]]
                       if self.manager == "openrc" else [["systemctl", "enable", "--now", "ssh"]]),
        }
        (self.root / "user-data").write_text("#cloud-config\n" + json.dumps(configuration))
        (self.root / "meta-data").write_text(json.dumps({
            "instance-id": self.root.name, "local-hostname": "zup-supervisor-probe"}))
        (self.root / "known-hosts").write_text(f"[127.0.0.1]:{self.port} {host_public}\n")
        command(["genisoimage", "-quiet", "-output", str(self.root / "seed.iso"),
                 "-volid", "cidata", "-joliet", "-rock", str(self.root / "user-data"),
                 str(self.root / "meta-data")])
        command(["qemu-img", "create", "-f", "qcow2", "-F", "qcow2", "-b",
                 str(self.base), str(self.root / "disk.qcow2")])
        if self.disk_size is not None:
            command(["qemu-img", "resize", str(self.root / "disk.qcow2"), self.disk_size])
        if "," in str(self.root):
            raise ValueError("QEMU drive paths must not contain commas")
        accelerator = "kvm" if platform.machine() == self.architecture and os.access("/dev/kvm", os.R_OK | os.W_OK) else "tcg"
        machine = "virt" if self.architecture == "aarch64" else "pc"
        arguments = [
            "qemu-system-" + self.architecture, "-machine", f"{machine},accel={accelerator}", "-cpu", "host" if accelerator == "kvm" else ("cortex-a72" if self.architecture == "aarch64" else "max"),
            "-m", "2048" if self.architecture == "aarch64" else "1024", "-smp", "2",
            "-drive", f"file={self.root}/disk.qcow2,if=virtio,format=qcow2",
            "-drive", f"file={self.root}/seed.iso,if=none,format=raw,readonly=on,id=seed",
            "-device", "virtio-scsi-pci,id=scsi", "-device", "scsi-cd,drive=seed",
            "-netdev", f"user,id=net0,hostfwd=tcp:127.0.0.1:{self.port}-:22",
            "-device", "virtio-net-pci,netdev=net0", "-device", "virtio-rng-pci",
            "-display", "none", "-serial", f"file:{self.root}/serial.log", "-monitor", "none",
        ]
        if self.architecture == "aarch64":
            firmware = Path("/usr/share/AAVMF/AAVMF_CODE.fd")
            variables = Path("/usr/share/AAVMF/AAVMF_VARS.fd")
            if not firmware.is_file() or not variables.is_file():
                raise RuntimeError("install qemu-efi-aarch64 for native ARM64 verification")
            shutil.copyfile(variables, self.root / "firmware-vars.fd")
            arguments += ["-drive", f"if=pflash,format=raw,unit=0,readonly=on,file={firmware}",
                          "-drive", f"if=pflash,format=raw,unit=1,file={self.root}/firmware-vars.fd"]
        (self.root / "launch.json").write_text(json.dumps(arguments, indent=2))
        with (self.root / "qemu.log").open("w") as log:
            self.process = subprocess.Popen(arguments, stdin=subprocess.DEVNULL,
                                            stdout=log, stderr=log, close_fds=True)
        self.wait_boot()
        status = self.run("cloud-init", "status", "--wait", timeout=180 if self.architecture == "aarch64" else 60, check=False)
        if status.returncode:
            details = self.run("cloud-init", "status", "--format", "json", check=False)
            diagnostic = details.stdout
            for secret in [configuration["chpasswd"]["users"][0]["password"],
                           configuration["ssh_keys"]["ed25519_private"]]:
                diagnostic = diagnostic.replace(secret, "<redacted>")
                for line in secret.splitlines():
                    diagnostic = diagnostic.replace(line, "<redacted>")
            raise RuntimeError(f"cloud-init status {status.returncode}: {diagnostic}")

    def run(self, *arguments, input=None, timeout=30, check=True):
        result = subprocess.run([
            "ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=3",
            "-o", "IdentitiesOnly=yes", "-o", "IdentityAgent=none",
            "-o", "StrictHostKeyChecking=yes", "-o", f"UserKnownHostsFile={self.root}/known-hosts",
            "-i", str(self.root / "client"), "-p", str(self.port), "root@127.0.0.1",
            shlex.join(arguments),
        ], capture_output=True, text=True, input=input, timeout=timeout)
        if check and result.returncode:
            raise RuntimeError(f"guest command {arguments!r} failed ({result.returncode}): {result.stdout}\n{result.stderr}")
        return result

    def wait_boot(self, previous=None):
        deadline = time.monotonic() + (480 if self.architecture == "aarch64" else 180)
        progress = 0
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise RuntimeError(f"QEMU exited: {self.root / 'qemu.log'}")
            result = self.run("cat", "/proc/sys/kernel/random/boot_id", check=False)
            if result.returncode == 0 and result.stdout.strip() != previous:
                return result.stdout.strip()
            if time.monotonic() >= progress:
                print("Waiting for pinned-key guest SSH", flush=True)
                progress = time.monotonic() + 15
            time.sleep(1)
        raise TimeoutError(f"guest boot failed; inspect {self.root / 'serial.log'}")

    def write(self, path, contents, mode="755"):
        self.run("tee", path, input=contents)
        self.run("chmod", mode, path)

    def observe(self, directory):
        source = r'''
import glob,json,os,sys
result=[]
for name in glob.glob('/proc/[0-9]*/exe'):
    try:
        executable=os.readlink(name)
        if executable.startswith(sys.argv[1] + '/'):
            parent=name.rsplit('/',1)[0]
            stat=open(parent+'/stat').read().rsplit(') ',1)[1].split()
            owner='/proc/'+stat[1]
            owner_stat=open(owner+'/stat').read().rsplit(') ',1)[1].split()
            cgroup=open(parent+'/cgroup').read()
            group=None
            if cgroup.startswith('0::/') and cgroup.strip()!='0::/':
                identity=os.stat('/sys/fs/cgroup'+cgroup.strip().split('::',1)[1])
                group={'device':identity.st_dev,'inode':identity.st_ino}
            result.append({'pid':int(parent.rsplit('/',1)[1]),'executable':executable,
                           'start_ticks':int(stat[19]),'cgroup':cgroup,'group':group,
                           'parent':{'pid':int(stat[1]),'executable':os.readlink(owner+'/exe'),
                                     'start_ticks':int(owner_stat[19])}})
    except (FileNotFoundError,ProcessLookupError,PermissionError):
        pass
print(json.dumps(result))
'''
        return json.loads(self.run("python3", "-c", source, directory).stdout)

    def wait_process(self, directory, generation):
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            processes = self.observe(directory)
            expected = [] if generation is None else [f"{directory}/{generation}/sleep"]
            if [item["executable"] for item in processes] == expected:
                return processes
            if len(processes) > 1:
                raise AssertionError(f"conflicting service processes: {processes}")
            time.sleep(0.2)
        raise TimeoutError(f"service did not converge to {expected}: {processes}")

    def kill(self, process):
        source = r'''
import json,os,signal,sys
expected=json.loads(sys.argv[1])
try:
    fd=os.pidfd_open(expected['pid'])
except ProcessLookupError:
    sys.exit(0)
try:
    parent='/proc/'+str(expected['pid'])
    stat=open(parent+'/stat').read().rsplit(') ',1)[1].split()
    assert int(stat[19])==expected['start_ticks'] and os.readlink(parent+'/exe')==expected['executable']
    signal.pidfd_send_signal(fd,signal.SIGKILL)
except (FileNotFoundError,ProcessLookupError):
    pass
finally:
    os.close(fd)
'''
        self.run("python3", "-c", source, json.dumps(process))


def containment(guest, results, save):
    for mode in ("initial_host", "cgroup_v2"):
        results[mode] = {}
        save()
        if mode == "cgroup_v2":
            configuration = guest.run("cat", "/etc/rc.conf").stdout
            configuration += '\nrc_cgroup_mode="unified"\nrc_cgroup_cleanup="yes"\nrc_timeout_stopsec=3\n'
            guest.write("/etc/rc.conf", configuration, "644")
            guest.run("rc-service", "cgroups", "start")
            guest.run("python3", "-c", "assert any(line.split()[4]=='/sys/fs/cgroup' and line.split(' - ',1)[1].split()[0]=='cgroup2' for line in open('/proc/self/mountinfo'))")
        for supervisor in ("supervise-daemon", "s6"):
            print(f"Testing descendants and controller loss: {mode}/{supervisor}", flush=True)
            name = f"zup-tree-{mode.replace('_', '-')}-{supervisor}"
            directory = f"/opt/{name}"
            guest.run("mkdir", "-p", directory)
            for executable in ("sh", "sleep"):
                guest.run("cp", "-L", f"/bin/{executable}", f"{directory}/{executable}")
            setsid = guest.run("which", "setsid").stdout.strip()
            body = f"trap '' TERM; {shlex.quote(setsid)} {shlex.quote(directory + '/sleep')} 1000 & trap - TERM; wait"
            arguments = shlex.join(["-c", body])
            guest.write(f"/etc/init.d/{name}", f'''#!/sbin/openrc-run
supervisor={shlex.quote(supervisor)}
command={shlex.quote(directory + '/sh')}
command_args={shlex.quote(arguments)}
respawn_delay=1
respawn_max=10
retry="TERM/1/KILL/1"
timeout_ready=10000
timeout_down=10000
timeout_kill=2000
depend() {{
    need localmount {"s6-svscan" if supervisor == "s6" else ""}
}}
''')

            def start_tree():
                guest.run("rc-service", name, "start")
                deadline = time.monotonic() + 15
                while time.monotonic() < deadline:
                    processes = guest.observe(directory)
                    if sorted(Path(item["executable"]).name for item in processes) == ["sh", "sleep"]:
                        return processes
                    time.sleep(0.2)
                raise TimeoutError(f"descendant fixture failed: {processes}")

            started = start_tree()
            stopped = guest.run("rc-service", name, "stop", check=False)
            time.sleep(1)
            remaining = guest.observe(directory)
            results[mode][supervisor] = {
                "started": started, "stop_status": stopped.returncode,
                "after_stop": remaining,
            }
            save()
            if remaining:
                results[mode][supervisor]["kernel_cleanup"] = kernel_cleanup(guest, name, started)
                remaining = guest.observe(directory)
                results[mode][supervisor]["after_kernel_cleanup"] = remaining
                save()
            for process in remaining:
                guest.kill(process)
            guest.wait_process(directory, None)

            restarted = start_tree()
            main = next(process for process in restarted if process["executable"].endswith("/sh"))
            controller = main["parent"]
            expected_controller = "s6-supervise" if supervisor == "s6" else supervisor
            if Path(controller["executable"]).name != expected_controller:
                raise AssertionError(f"unexpected native controller: {controller}")
            guest.kill(controller)
            time.sleep(2)
            after_loss = guest.observe(directory)
            stopped = guest.run("rc-service", name, "stop", check=False)
            time.sleep(1)
            remaining = guest.observe(directory)
            results[mode][supervisor]["controller_loss"] = {
                "controller": controller, "after_loss": after_loss,
                "stop_status": stopped.returncode, "after_stop": remaining,
            }
            save()
            if remaining:
                results[mode][supervisor]["controller_loss"]["kernel_cleanup"] = kernel_cleanup(guest, name, restarted)
                remaining = guest.observe(directory)
                results[mode][supervisor]["controller_loss"]["after_kernel_cleanup"] = remaining
                save()
            for process in remaining:
                guest.kill(process)
            guest.wait_process(directory, None)
    return results


def kernel_cleanup(guest, name, started):
    expected = f"0::/openrc.{name}"
    if any(process["cgroup"].strip() != expected or process["group"] is None for process in started):
        return {"eligible": False, "reason": "workload is not in its isolated service cgroup"}
    identity = started[0]["group"]
    if any(process["group"] != identity for process in started):
        raise AssertionError("service cgroup changed during observation")
    source = r'''
import json,os,sys,time
name,expected=sys.argv[1],json.loads(sys.argv[2])
assert name.startswith('zup-tree-') and '/' not in name and name not in ['.','..']
root=os.open('/sys/fs/cgroup',os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW|os.O_CLOEXEC)
group=os.open('openrc.'+name,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW|os.O_CLOEXEC,dir_fd=root)
try:
    stat=os.fstat(group)
    assert (stat.st_dev,stat.st_ino)==(expected['device'],expected['inode'])
    kill=os.open('cgroup.kill',os.O_WRONLY|os.O_NOFOLLOW|os.O_CLOEXEC,dir_fd=group)
    try: os.write(kill,b'1\n')
    finally: os.close(kill)
    deadline=time.monotonic()+5
    while True:
        events=os.open('cgroup.events',os.O_RDONLY|os.O_NOFOLLOW|os.O_CLOEXEC,dir_fd=group)
        try: state=dict(line.split() for line in os.read(events,4096).decode().splitlines())
        finally: os.close(events)
        if state.get('populated')=='0' or time.monotonic()>deadline:break
        time.sleep(0.02)
    print(json.dumps({'eligible':True,'identity':expected,'events':state}))
finally:
    os.close(group)
    os.close(root)
'''
    return json.loads(guest.run("python3", "-c", source, name, json.dumps(identity)).stdout)


def probe(base, cache, output):
    with Guest(base, cache) as guest:
        print("Installing native supervisors from authenticated Alpine repositories", flush=True)
        guest.run("apk", "add", "--no-cache", "s6", timeout=180)
        guest.run("apk", "fix", "--no-cache", "openrc", timeout=180)
        guest.run("test", "-x", "/etc/init.d/s6-svscan")
        guest.run("rc-service", "s6-svscan", "start")
        results = {"state": "probing", "guest_directory": str(guest.root),
                   "base_sha256": hashlib.file_digest(base.open("rb"), "sha256").hexdigest(),
                   "packages": guest.run("apk", "list", "--installed", "openrc", "s6").stdout,
                   "kernel": guest.run("uname", "-a").stdout.strip(),
                   "supervisors": {}}
        def save():
            output.write_text(json.dumps(results, indent=2) + "\n")
        save()
        for supervisor in ("supervise-daemon", "s6"):
            print(f"Testing {supervisor}", flush=True)
            name = f"zup-probe-{supervisor}"
            directory = f"/opt/{name}"
            for generation in ("a", "b"):
                guest.run("mkdir", "-p", f"{directory}/{generation}")
                guest.run("cp", "-L", "/bin/sleep", f"{directory}/{generation}/sleep")

            def definition(generation):
                return f'''#!/sbin/openrc-run
supervisor={shlex.quote(supervisor)}
command={shlex.quote(directory + '/' + generation + '/sleep')}
command_args="1000"
respawn_delay=1
respawn_max=10
timeout_ready=10000
timeout_down=10000
timeout_kill=2000
depend() {{
    need localmount {"s6-svscan" if supervisor == "s6" else ""}
}}
'''

            guest.write(f"/etc/init.d/{name}", definition("a"))
            guest.run("rc-service", name, "start")
            before = guest.wait_process(directory, "a")[0]
            time.sleep(1.1)
            guest.write(f"/etc/init.d/{name}", definition("b"))
            guest.run("kill", "-KILL", str(before["pid"]))
            restarted = guest.wait_process(directory, "a")[0]
            assert (before["pid"], before["start_ticks"]) != (restarted["pid"], restarted["start_ticks"])
            guest.run("rc-service", name, "stop")
            guest.wait_process(directory, None)
            guest.run("rc-service", name, "start")
            activated = guest.wait_process(directory, "b")[0]
            guest.run("rc-update", "add", name, "default")
            results["supervisors"][supervisor] = {
                "initial": before, "respawn_before_activation": restarted, "activated": activated}
            save()
        boot = guest.run("cat", "/proc/sys/kernel/random/boot_id").stdout.strip()
        print("Rebooting the disposable guest", flush=True)
        guest.run("reboot", check=False)
        results["boot_before"] = boot
        results["boot_after"] = guest.wait_boot(boot)
        for supervisor, evidence in results["supervisors"].items():
            evidence["after_reboot"] = guest.wait_process(f"/opt/zup-probe-{supervisor}", "b")[0]
        results["containment"] = {}
        save()
        containment(guest, results["containment"], save)
        results["scope"] = "native lifecycle, reboot, escaped descendant and controller-loss observations; no power-loss proof"
        results["state"] = "completed"
        save()
        return results


if __name__ == "__main__":
    if os.name != "posix":
        raise RuntimeError("run this experiment on the Linux QEMU host")
    parser = argparse.ArgumentParser()
    parser.add_argument("--base-image", type=Path, required=True)
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    options = parser.parse_args()
    options.output.write_text(json.dumps({"state": "preparing"}) + "\n")
    try:
        result = probe(options.base_image, options.cache, options.output)
    except BaseException as failure:
        result = json.loads(options.output.read_text())
        result.update(state="failed", failure=f"{type(failure).__name__}: {failure}")
        options.output.write_text(json.dumps(result, indent=2) + "\n")
        raise
    print(f"Native supervisor evidence: {options.output}")
