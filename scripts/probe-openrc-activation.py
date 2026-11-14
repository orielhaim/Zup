"""Exercise stock OpenRC admission, pidfd shutdown and a persistent boot fence."""

import argparse
import hashlib
import importlib.util
import json
from pathlib import Path


spec = importlib.util.spec_from_file_location(
    "supervisors", Path(__file__).with_name("probe-linux-supervisors.py"))
supervisors = importlib.util.module_from_spec(spec)
spec.loader.exec_module(supervisors)


PREPARE = r'''
import fcntl,json,os,select,signal,subprocess,time,uuid
from pathlib import Path
name='zup-openrc-'+uuid.uuid4().hex
root=Path('/opt')/name
root.mkdir(mode=0o700)
for generation in ['a','b']:
    directory=root/generation
    directory.mkdir(mode=0o755)
    binary=directory/'sleep'
    binary.write_bytes(Path('/bin/busybox').read_bytes())
    binary.chmod(0o755)
root.chmod(0o755)
definition=Path('/etc/init.d')/name
ready=root/'paused'
pause_pid=root/'pause-pid'
def source(generation,disabled=False,pause=False):
    gate='return 1' if disabled else 'return 0'
    if pause:
        gate='printf "%s" "$$" > '+str(pause_pid)+'; : > '+str(ready)+'; kill -STOP "$$"; '+gate
    return ('#!/sbin/openrc-run\ncommand='+str(root/generation/'sleep')+
        '\ncommand_args="600"\ncommand_user="nobody:nobody"\npidfile="/run/'+name+'.pid"'+
        '\nsupervisor=supervise-daemon\nsupervise_daemon_args="--respawn-delay-step 0"'+
        '\nrespawn_delay=1\nrespawn_max=3\nrespawn_period=60\nretry="TERM/1/KILL/1"'+
        '\nrc_cgroup_mode=unified\nrc_cgroup_cleanup=NO\nno_new_privs=yes'+
        '\nstart_pre() { '+gate+'; }\n')
def publish(text):
    temporary=definition.with_suffix('.preparing')
    with temporary.open('x') as output:
        output.write(text); output.flush(); os.fsync(output.fileno())
    temporary.chmod(0o755)
    temporary.rename(definition)
    directory=os.open(definition.parent,os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
    try:os.fsync(directory)
    finally:os.close(directory)
def invoke(*arguments):
    return subprocess.run(arguments,capture_output=True,text=True,timeout=20)
publish(source('a',pause=True))
request=subprocess.Popen(['/sbin/rc-service',name,'start'],stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
deadline=time.monotonic()+15
while not ready.exists():
    if request.poll() is not None:raise RuntimeError('start ended before pause: '+str(request.communicate()))
    if time.monotonic()>deadline:raise TimeoutError('start_pre pause')
    time.sleep(.02)
lease=os.open(Path('/run/openrc/exclusive')/name,os.O_WRONLY|os.O_NOFOLLOW|os.O_CLOEXEC)
blocked=False
try:
    try:fcntl.flock(lease,fcntl.LOCK_EX|fcntl.LOCK_NB)
    except BlockingIOError:blocked=True
    if not blocked:raise AssertionError('admission barrier did not include active start_pre')
    publish(source('a',disabled=True))
    process=os.pidfd_open(int(pause_pid.read_text()))
    try:signal.pidfd_send_signal(process,signal.SIGCONT)
    finally:os.close(process)
    stdout,stderr=request.communicate(timeout=20)
    if request.returncode:raise RuntimeError('admitted start failed: '+stdout+stderr)
    fcntl.flock(lease,fcntl.LOCK_EX|fcntl.LOCK_NB)
finally:os.close(lease)
scope=Path('/sys/fs/cgroup')/('openrc.'+name)
scope_fd=os.open(scope,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW|os.O_CLOEXEC)
try:
    identity=os.fstat(scope_fd).st_ino
    controller=int((Path('/run')/(name+'.pid')).read_text())
    pidfd=os.pidfd_open(controller)
    try:
        executable=os.readlink('/proc/'+str(controller)+'/exe')
        if not executable.endswith('/supervise-daemon'):raise AssertionError(executable)
        if Path('/proc/'+str(controller)+'/cgroup').read_text().strip()!='0::/openrc.'+name:raise AssertionError('controller escaped scope')
        signal.pidfd_send_signal(pidfd,signal.SIGTERM)
        poll=select.poll();poll.register(pidfd,select.POLLIN)
        if not poll.poll(15000):raise TimeoutError('native supervisor did not terminate')
    finally:os.close(pidfd)
    control=os.open('cgroup.kill',os.O_WRONLY|os.O_NOFOLLOW|os.O_CLOEXEC,dir_fd=scope_fd)
    try:os.write(control,b'1')
    finally:os.close(control)
    deadline=time.monotonic()+10
    while 'populated 0' not in (scope/'cgroup.events').read_text():
        if time.monotonic()>deadline:raise TimeoutError('native cgroup is populated')
        time.sleep(.02)
    if os.stat(scope,follow_symlinks=False).st_ino!=identity:raise AssertionError('scope replaced')
finally:os.close(scope_fd)
reset=invoke('/sbin/rc-service',name,'zap')
if reset.returncode:raise RuntimeError(reset.stdout+reset.stderr)
attempt=invoke('/sbin/rc-service',name,'start')
if attempt.returncode==0:raise AssertionError('disabled native definition accepted a start')
if 'populated 0' not in (scope/'cgroup.events').read_text():raise AssertionError('blocked start left a live scope')
result={'name':name,'root':str(root),'active_start_holds_barrier':blocked,
    'pidfd_supervisor_shutdown':True,'empty_identity_checked_scope':identity,
    'disabled_start_exit':attempt.returncode,'disabled_definition':str(definition)}
(root/'prepared-b').write_text(source('b'))
enable=invoke('/sbin/rc-update','add',name,'default')
if enable.returncode:raise RuntimeError(enable.stdout+enable.stderr)
print(json.dumps(result))
'''


AFTER_BOOT = r'''
import json,os,subprocess,sys,time
from pathlib import Path
result=json.loads(sys.argv[1]);root=Path(result['root']);definition=Path(result['disabled_definition'])
def images():
    found=[]
    for entry in Path('/proc').iterdir():
        if not entry.name.isdecimal():continue
        try:executable=os.readlink(entry/'exe')
        except (FileNotFoundError,ProcessLookupError):continue
        if executable.startswith(str(root)+'/'):found.append(executable)
    return found
if images():raise AssertionError('disabled definition started application code after reboot')
result['disabled_after_reboot']=True
temporary=definition.with_suffix('.preparing')
with temporary.open('x') as output:
    output.write((root/'prepared-b').read_text());output.flush();os.fsync(output.fileno())
temporary.chmod(0o755);temporary.rename(definition)
started=subprocess.run(['/sbin/rc-service',result['name'],'start'],capture_output=True,text=True,timeout=20)
if started.returncode:raise RuntimeError(started.stdout+started.stderr)
deadline=time.monotonic()+15
while images()!=[str(root/'b'/'sleep')]:
    if time.monotonic()>deadline:raise TimeoutError('new generation did not become the only running image')
    time.sleep(.05)
result['running_after_restore']=images()
print(json.dumps(result))
'''


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--base-image", type=Path, required=True)
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    options = parser.parse_args()
    digest = hashlib.sha256(options.base_image.read_bytes()).hexdigest()
    if digest != "53a02ce7466524a83f8b07b6fe4ef212f6019752c0e563db9538d9b54d468a8e":
        raise ValueError("Alpine base image differs from the verified artifact")
    report = {"state": "preparing", "base_sha256": digest}
    def save():
        options.output.write_text(json.dumps(report, indent=2) + "\n")
    save()
    try:
        with supervisors.Guest(options.base_image, options.cache) as guest:
            report["guest_directory"] = str(guest.root)
            save()
            guest.run("apk", "add", "--no-cache", "openrc=0.63.2-r0", timeout=180)
            configuration = guest.run("cat", "/etc/rc.conf").stdout
            guest.write("/etc/rc.conf", configuration + '\nrc_cgroup_mode="unified"\n', "644")
            guest.run("rc-service", "cgroups", "start")
            report["before_reboot"] = json.loads(guest.run("python3", "-c", PREPARE, timeout=90).stdout)
            save()
            boot = guest.run("cat", "/proc/sys/kernel/random/boot_id").stdout.strip()
            guest.run("reboot", check=False)
            report["boot_before"] = boot
            report["boot_after"] = guest.wait_boot(boot)
            report["after_reboot"] = json.loads(guest.run("python3", "-c", AFTER_BOOT, json.dumps(report["before_reboot"]), timeout=45).stdout)
            report.update(state="completed", scope="stock native admission, pidfd control, cgroup cleanup and reboot; not Zup transaction or power-loss proof")
            save()
    except BaseException as error:
        report.update(state="failed", failure=f"{type(error).__name__}: {error}")
        save()
        raise


if __name__ == "__main__":
    main()
