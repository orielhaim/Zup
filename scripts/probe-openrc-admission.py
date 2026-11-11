"""Test stock OpenRC service admission locks in a disposable Alpine guest."""

import argparse
import base64
import hashlib
import importlib.util
import json
from pathlib import Path
import shutil
import subprocess
import tempfile

spec = importlib.util.spec_from_file_location("supervisors", Path(__file__).with_name("probe-linux-supervisors.py"))
supervisors = importlib.util.module_from_spec(spec)
spec.loader.exec_module(supervisors)


DELAY_HOOK = r'''
#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <limits.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <unistd.h>
static char expected[PATH_MAX], ready[PATH_MAX];
static int gated;
__attribute__((constructor)) static void configure(void) {
    const char *value=getenv("ZUP_PROBE_LOCK");
    if(value) snprintf(expected,sizeof expected,"%s",value);
    value=getenv("ZUP_PROBE_READY");
    if(value) snprintf(ready,sizeof ready,"%s",value);
}
int flock(int fd,int operation) {
    int (*native)(int,int)=dlsym(RTLD_NEXT,"flock");
    char descriptor[64],path[PATH_MAX];
    snprintf(descriptor,sizeof descriptor,"/proc/self/fd/%d",fd);
    ssize_t length=readlink(descriptor,path,sizeof path-1);
    if(length>=0) path[length]=0;
    if(!gated && expected[0] && length>=0 && !strcmp(path,expected) && (operation&LOCK_EX)) {
        gated=1;
        int marker=open(ready,O_WRONLY|O_CREAT|O_EXCL|O_CLOEXEC,0600);
        if(marker<0) _exit(90);
        close(marker);
        raise(SIGSTOP);
    }
    if(!native) _exit(91);
    return native(fd,operation);
}
'''


EXPERIMENT = r'''
import base64,fcntl,hashlib,json,os,signal,subprocess,sys,tempfile,time,uuid
from pathlib import Path
results=[]
for nodeps in [False,True]:
    name='zup-admission-probe-'+uuid.uuid4().hex
    directory=Path('/opt')/name
    directory.mkdir(mode=0o700)
    marker=directory/'started'
    definition=Path('/etc/init.d')/name
    with definition.open('x') as output:
        output.write('#!/sbin/openrc-run\nstart() { printf started > '+str(marker)+'; }\nstop() { return 0; }\n')
    definition.chmod(0o755)
    baseline=subprocess.run(['/sbin/rc-service',name,'start'],capture_output=True,text=True,timeout=15)
    if baseline.returncode or not marker.exists():
        raise RuntimeError('Unblocked fixture did not start: '+baseline.stdout+baseline.stderr)
    reset=subprocess.run(['/sbin/rc-service',name,'zap'],capture_output=True,text=True,timeout=15)
    if reset.returncode:raise RuntimeError('Cannot reset fixture: '+reset.stderr)
    marker.unlink()
    lock=Path('/run/openrc/exclusive')/name
    original=os.open(lock,os.O_WRONLY|os.O_CREAT|os.O_NOFOLLOW|os.O_CLOEXEC,0o644)
    replacement=None
    try:
        fcntl.flock(original,fcntl.LOCK_EX|fcntl.LOCK_NB)
        old=os.fstat(original)
        arguments=['/sbin/rc-service']+(['--nodeps'] if nodeps else [])+[name,'start']
        request=subprocess.run(arguments,capture_output=True,text=True,timeout=15)
        missing=not lock.exists()
        replacement=os.open(lock,os.O_WRONLY|os.O_CREAT|os.O_NOFOLLOW|os.O_CLOEXEC,0o644)
        new=os.fstat(replacement)
        try:
            fcntl.flock(replacement,fcntl.LOCK_EX|fcntl.LOCK_NB)
            replacement_locked=True
        except BlockingIOError:
            replacement_locked=False
        results.append({'nodeps':nodeps,'baseline_started':True,'exit_code':request.returncode,
            'request_output':request.stdout+request.stderr,
            'started_while_held':marker.exists(),'lock_unlinked_while_held':missing,
            'original_identity':[old.st_dev,old.st_ino],
            'replacement_identity':[new.st_dev,new.st_ino],
            'replacement_locked_while_original_held':replacement_locked})
    finally:
        if replacement is not None:os.close(replacement)
        os.close(original)
        cleanup=subprocess.run(['/sbin/rc-service',name,'zap'],capture_output=True,text=True,timeout=15)
        if cleanup.returncode:raise RuntimeError('Cannot clear disposable service bookkeeping: '+cleanup.stderr)
        definition.unlink()
        lock.unlink(missing_ok=True)
        marker.unlink(missing_ok=True)
        directory.rmdir()
with tempfile.TemporaryDirectory(prefix='zup-admission-delay-',dir='/opt') as temporary:
    directory=Path(temporary)
    hook=directory/'hook.so'
    binary=base64.b64decode(sys.argv[1],validate=True)
    if hashlib.sha256(binary).hexdigest()!=sys.argv[2]:raise RuntimeError('Native delay fixture hash mismatch')
    hook.write_bytes(binary)
    name='zup-admission-delay-'+uuid.uuid4().hex
    definition=Path('/etc/init.d')/name
    replacement=definition.with_suffix('.new')
    marker=directory/'generation'
    ready=directory/'ready'
    lock=Path('/run/openrc/exclusive')/name
    def source(generation):
        return '#!/sbin/openrc-run\nstart() { printf '+generation+' > '+str(marker)+'; }\nstop() { return 0; }\n'
    with definition.open('x') as output:output.write(source('a'))
    definition.chmod(0o755)
    environment=dict(os.environ,LD_PRELOAD=str(hook),ZUP_PROBE_LOCK=str(lock),ZUP_PROBE_READY=str(ready))
    request=subprocess.Popen(['/sbin/rc-service',name,'start'],env=environment,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
    process=os.pidfd_open(request.pid)
    lease=None
    try:
        deadline=time.monotonic()+15
        while not ready.exists():
            if request.poll() is not None:raise RuntimeError('Start exited before admission delay: '+str(request.communicate()))
            if time.monotonic()>=deadline:raise TimeoutError('Native start did not reach admission')
            time.sleep(0.01)
        while Path('/proc/'+str(request.pid)+'/stat').read_text().rsplit(') ',1)[1].split()[0]!='T':
            if time.monotonic()>=deadline:raise TimeoutError('Native start did not stop at admission')
            time.sleep(0.01)
        lease=os.open(lock,os.O_WRONLY|os.O_NOFOLLOW|os.O_CLOEXEC)
        fcntl.flock(lease,fcntl.LOCK_EX|fcntl.LOCK_NB)
        held=os.fstat(lease)
        entry=os.stat(lock,follow_symlinks=False)
        if (held.st_dev,held.st_ino)!=(entry.st_dev,entry.st_ino):raise RuntimeError('Admission entry changed')
        with replacement.open('x') as output:output.write(source('b'))
        replacement.chmod(0o755)
        replacement.rename(definition)
        os.close(lease)
        lease=None
        signal.pidfd_send_signal(process,signal.SIGCONT)
        stdout,stderr=request.communicate(timeout=15)
        if request.returncode:raise RuntimeError('Delayed native start failed: '+stdout+stderr)
        generation=marker.read_text()
        results.append({'delayed_before_admission':True,'launched_generation':generation,'exit_code':request.returncode})
        if generation!='b':raise RuntimeError('Stale request executed the old definition')
    finally:
        if lease is not None:os.close(lease)
        if request.poll() is None:
            signal.pidfd_send_signal(process,signal.SIGKILL)
            request.wait(timeout=5)
        os.close(process)
        cleanup=subprocess.run(['/sbin/rc-service',name,'zap'],capture_output=True,text=True,timeout=15)
        if cleanup.returncode:raise RuntimeError('Cannot clear delayed service bookkeeping: '+cleanup.stderr)
        definition.unlink()
        replacement.unlink(missing_ok=True)
        lock.unlink(missing_ok=True)
print(json.dumps(results))
'''


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--base-image", type=Path, required=True)
    parser.add_argument("--base-sha256", required=True)
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    options = parser.parse_args()
    digest = hashlib.file_digest(options.base_image.open("rb"), "sha256").hexdigest()
    if digest != options.base_sha256:
        raise ValueError("Alpine base image identity differs from the expected artifact")
    report = {"state": "preparing", "base_sha256": digest}

    def save():
        options.output.write_text(json.dumps(report, indent=2) + "\n")

    save()
    try:
        compiler = shutil.which("musl-gcc")
        if compiler is None:
            raise RuntimeError("musl-gcc is required; install musl-tools on the Linux probe host")
        with tempfile.TemporaryDirectory(prefix="zup-admission-hook-") as temporary:
            directory = Path(temporary)
            source = directory / "hook.c"
            binary = directory / "hook.so"
            source.write_text(DELAY_HOOK)
            subprocess.run([compiler, "-shared", "-fPIC", "-O2", "-o", str(binary), str(source), "-ldl"], check=True, capture_output=True, text=True, timeout=30)
            hook = binary.read_bytes()
        encoded_hook = base64.b64encode(hook).decode()
        hook_digest = hashlib.sha256(hook).hexdigest()
        report["delay_hook_sha256"] = hook_digest
        with supervisors.Guest(options.base_image, options.cache) as guest:
            report["guest_directory"] = str(guest.root)
            guest.run("apk", "add", "--no-cache", "openrc=0.63.2-r0", timeout=180)
            report["packages"] = guest.run("apk", "list", "--installed", "openrc").stdout
            report["kernel"] = guest.run("uname", "-a").stdout.strip()
            try:
                result = guest.run("python3", "-c", EXPERIMENT, encoded_hook, hook_digest, timeout=90, check=False)
            except subprocess.TimeoutExpired:
                raise RuntimeError("Native admission experiment timed out") from None
            if result.returncode:
                raise RuntimeError("Native admission experiment failed: " + result.stderr[-4096:])
            report["cases"] = json.loads(result.stdout)
            report["state"] = "completed"
            report["scope"] = "native admission and lock inode behavior; no crash or power-loss proof"
            save()
    except BaseException as error:
        report.update(state="failed", failure=f"{type(error).__name__}: {error}")
        save()
        raise
    print(json.dumps(report, indent=2), flush=True)


if __name__ == "__main__":
    main()
