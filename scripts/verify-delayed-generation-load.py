"""Verify delayed native library loading across a real installer upgrade."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import traceback
import uuid

RUNNER = r'''
use std::{env, fs, io};
#[cfg(windows)]
unsafe fn load(path: &std::path::Path) -> u32 {
    use std::{ffi::c_void, os::windows::ffi::OsStrExt};
    #[link(name="kernel32")]
    extern "system" {
        fn LoadLibraryW(path: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    }
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let module = LoadLibraryW(path.as_ptr());
    assert!(!module.is_null());
    let entry = GetProcAddress(module, b"fixture_value\0".as_ptr());
    assert!(!entry.is_null());
    std::mem::transmute::<*mut c_void, extern "C" fn() -> u32>(entry)()
}
#[cfg(target_os="linux")]
unsafe fn load(path: &std::path::Path) -> u32 {
    use std::{ffi::{c_void, c_char, CString}, os::unix::ffi::OsStrExt};
    #[link(name="dl")]
    extern "C" {
        fn dlopen(path: *const c_char, flags: i32) -> *mut c_void;
        fn dlsym(module: *mut c_void, name: *const c_char) -> *mut c_void;
    }
    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let module = dlopen(path.as_ptr(), 2);
    assert!(!module.is_null());
    let entry = dlsym(module, b"fixture_value\0".as_ptr().cast());
    assert!(!entry.is_null());
    std::mem::transmute::<*mut c_void, extern "C" fn() -> u32>(entry)()
}
fn main() {
    let executable = env::current_exe().unwrap();
    let root = executable.parent().unwrap();
    println!("ready");
    io::stdin().read_line(&mut String::new()).unwrap();
    let library = root.join(if cfg!(windows) { "fixture.dll" } else { "libfixture.so" });
    println!("{}:{}", fs::read_to_string(root.join("asset.txt")).unwrap(), unsafe { load(&library) });
}
'''


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--zup", type=Path, required=True)
    parser.add_argument("--runtime", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    options = parser.parse_args()
    zup, runtime = options.zup.resolve(strict=True), options.runtime.resolve(strict=True)
    base = Path(os.environ["LOCALAPPDATA"]) if os.name == "nt" else None
    root = Path(tempfile.mkdtemp(prefix="zup-delayed-load-", dir=base))
    report = {"state": "failed", "fixture": str(root), "checks": []}
    running = None
    succeeded = False

    def run(*arguments, parse=False):
        result = subprocess.run([str(value) for value in arguments], cwd=root, capture_output=True, text=True, timeout=240)
        if result.returncode:
            raise RuntimeError(f"command failed: {result.stdout[-3000:]} {result.stderr[-3000:]}")
        return json.loads(result.stdout) if parse else result.stdout

    try:
        dist = root / "dist"
        dist.mkdir()
        (root / "runner.rs").write_text(RUNNER, encoding="utf8")
        executable = "application.exe" if os.name == "nt" else "application"
        library = "fixture.dll" if os.name == "nt" else "libfixture.so"
        target = "windows-x64" if os.name == "nt" else "linux-x64"
        run("rustc", "--edition=2021", "runner.rs", "-o", dist / executable)
        application = "com.zup.delayed-load-" + uuid.uuid4().hex
        run(zup, "init", "--name", application, "--app-id", application, "--source", "dist", "--target", target, "--scope", "user", "--frontend", "headless", "--main", executable, "--yes")
        manifest = root / "zup.toml"
        authored = manifest.read_text(encoding="utf8")
        setups = []
        for number in [1, 2]:
            (root / "library.rs").write_text(f'#[no_mangle] pub extern "C" fn fixture_value() -> u32 {{ {number} }}\n', encoding="utf8")
            run("rustc", "--edition=2021", "--crate-type=cdylib", "library.rs", "-o", dist / library)
            (dist / "asset.txt").write_text(f"generation-{number}", encoding="utf8")
            manifest.write_text(authored.replace('version = "0.1.0"', f'version = "0.{number}.0"'), encoding="utf8")
            setup = root / (f"setup-{number}" + (".exe" if os.name == "nt" else ""))
            run(zup, "build", "--runtime", runtime, "--output", setup)
            setups.append(setup)
        state, install = root / "state", root / "application"

        def invoke(setup, action, *extra):
            return run(setup, action, *extra, "--scope", "user", "--state-root", state, "--output", "json", *([] if action == "inspect" else ["--non-interactive", "--yes"]), parse=True)

        invoke(setups[0], "install", "--install-directory", install)
        first = invoke(setups[0], "inspect")
        old = install / ".zup/deployments" / str(first["current"]["materialization"])
        running = subprocess.Popen([str(old / executable)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, creationflags=subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0)
        assert running.stdout.readline().strip() == "ready"
        invoke(setups[1], "upgrade")
        second = invoke(setups[1], "inspect")
        active = install / ".zup/deployments" / str(second["current"]["materialization"])
        assert active != old
        output, error = running.communicate("load\n", timeout=15)
        assert running.returncode == 0, error
        assert output.strip() == "generation-1:1", output
        newer = subprocess.run([str(active / executable)], input="load\n", capture_output=True, text=True, timeout=15)
        assert newer.returncode == 0, newer.stderr
        assert newer.stdout.splitlines() == ["ready", "generation-2:2"], newer.stdout
        report["checks"] = ["running A loads its own native library and adjacent asset after activation of B", "new B loads B library and asset", "both execute directly without a Zup launcher or process"]
        invoke(setups[1], "uninstall")
        report["state"] = "passed"
        succeeded = True
    except Exception as error:
        report["error"] = str(error)
        report["traceback"] = traceback.format_exc()
    finally:
        if running is not None and running.poll() is None:
            running.kill()
            running.communicate(timeout=15)
        options.output.parent.mkdir(parents=True, exist_ok=True)
        options.output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf8")
        if succeeded:
            expected = base.resolve() if base else Path(tempfile.gettempdir()).resolve()
            if root.is_symlink() or root.resolve().parent != expected or not root.name.startswith("zup-delayed-load-"):
                raise RuntimeError("disposable fixture boundary changed")
            shutil.rmtree(root)
    print(json.dumps({"state": report["state"], "report": str(options.output), "error": report.get("error")}))
    return 0 if succeeded else 1


if __name__ == "__main__":
    raise SystemExit(main())
