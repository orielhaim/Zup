"""Measure native executable exclusion without changing executable contents."""

import argparse
import contextlib
import ctypes
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import tempfile


FIXTURE = r'''
fn main() {
    println!("ready");
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).unwrap();
}
'''


@contextlib.contextmanager
def write_lease(path):
    if os.name == "nt":
        kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel.CreateFileW.argtypes = [ctypes.c_wchar_p, ctypes.c_uint32,
                                      ctypes.c_uint32, ctypes.c_void_p,
                                      ctypes.c_uint32, ctypes.c_uint32,
                                      ctypes.c_void_p]
        kernel.CreateFileW.restype = ctypes.c_void_p
        kernel.CloseHandle.argtypes = [ctypes.c_void_p]
        kernel.CloseHandle.restype = ctypes.c_int
        handle = kernel.CreateFileW(str(path), 0x40000000, 7, None, 3,
                                    0x00200000, None)
        if handle == ctypes.c_void_p(-1).value:
            raise ctypes.WinError(ctypes.get_last_error())
        try:
            yield handle
        finally:
            if not kernel.CloseHandle(handle):
                raise ctypes.WinError(ctypes.get_last_error())
    else:
        descriptor = os.open(path, os.O_WRONLY | os.O_CLOEXEC | os.O_NOFOLLOW)
        try:
            assert not os.get_inheritable(descriptor)
            yield descriptor
        finally:
            os.close(descriptor)


def error_code(error):
    return {"errno": error.errno, "winerror": getattr(error, "winerror", None)}


def probe():
    with tempfile.TemporaryDirectory(prefix="zup-image-exclusion-") as temporary:
        root = Path(temporary)
        source = root / "fixture.rs"
        source.write_text(FIXTURE, encoding="utf-8")
        image = root / ("application.exe" if os.name == "nt" else "application")
        subprocess.run(["rustc", str(source), "-o", str(image)], check=True)
        before = hashlib.sha256(image.read_bytes()).hexdigest()
        running = subprocess.Popen([str(image)], stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   text=True)
        try:
            assert running.stdout.readline().strip() == "ready"
            try:
                with write_lease(image):
                    raise AssertionError("write lease accepted a running executable image")
            except OSError as error:
                busy = error_code(error)
            running.communicate("exit\n", timeout=15)
            assert running.returncode == 0
        finally:
            if running.poll() is None:
                running.kill()
                running.communicate(timeout=15)

        with write_lease(image):
            try:
                subprocess.run([str(image)], input="exit\n", capture_output=True,
                               text=True, timeout=15, check=True)
            except OSError as error:
                excluded = error_code(error)
            else:
                raise AssertionError("a new process started while the write lease was held")
        completed = subprocess.run([str(image)], input="exit\n", capture_output=True,
                                   text=True, timeout=15, check=True)
        assert completed.stdout.strip() == "ready"
        assert hashlib.sha256(image.read_bytes()).hexdigest() == before
        readonly = "not-probed"
        if os.name != "nt":
            image.chmod(0o555)
            try:
                with write_lease(image):
                    readonly = "available-to-current-authority"
            except OSError as error:
                readonly = error_code(error)
            finally:
                image.chmod(0o755)
        return {
            "platform": platform.platform(),
            "running_image_write_open": busy,
            "new_execution_while_write_open": excluded,
            "execution_after_release": "passed",
            "content_unchanged": True,
            "read_only_image_write_open": readonly,
            "scope": "native image exclusion only; no generation non-use or crash-durability proof",
            "sources": [
                "https://www.man7.org/linux/man-pages/man2/open.2.html",
                "https://www.man7.org/linux/man-pages/man2/execve.2.html",
                "https://learn.microsoft.com/en-us/windows-hardware/drivers/ifs/executable-images",
            ],
        }


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path)
    arguments = parser.parse_args()
    result = json.dumps(probe(), indent=2) + "\n"
    if arguments.output:
        arguments.output.write_text(result, encoding="utf-8")
    print(result, end="")
