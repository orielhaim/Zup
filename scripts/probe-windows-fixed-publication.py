"""Observe NTFS file identities during replacement and restoration."""

import ctypes
from ctypes import wintypes
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


def probe():
    if os.name != "nt":
        raise RuntimeError("this experiment requires Windows")
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.CreateFileW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD,
                                   ctypes.c_void_p, wintypes.DWORD, wintypes.DWORD,
                                   wintypes.HANDLE]
    kernel.CreateFileW.restype = wintypes.HANDLE
    kernel.GetFileInformationByHandleEx.argtypes = [wintypes.HANDLE, ctypes.c_int,
                                                   ctypes.c_void_p, wintypes.DWORD]
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    kernel.ReplaceFileW.argtypes = [wintypes.LPCWSTR, wintypes.LPCWSTR,
                                    wintypes.LPCWSTR, wintypes.DWORD,
                                    ctypes.c_void_p, ctypes.c_void_p]
    kernel.MoveFileExW.argtypes = [wintypes.LPCWSTR, wintypes.LPCWSTR, wintypes.DWORD]

    class FileId(ctypes.Structure):
        _fields_ = [("volume", ctypes.c_uint64), ("file", ctypes.c_ubyte * 16)]

    def opened(path, sharing, access=0x80000000, directory=False):
        flags = 0x00200000 | (0x02000000 if directory else 0)
        handle = kernel.CreateFileW(str(path), access, sharing, None, 3, flags, None)
        if handle == ctypes.c_void_p(-1).value:
            raise ctypes.WinError(ctypes.get_last_error())
        return handle

    def identity(path):
        handle = opened(path, 7)
        try:
            info = FileId()
            if not kernel.GetFileInformationByHandleEx(handle, 18, ctypes.byref(info),
                                                       ctypes.sizeof(info)):
                raise ctypes.WinError(ctypes.get_last_error())
            return {"volume": info.volume, "file": bytes(info.file).hex()}
        finally:
            kernel.CloseHandle(handle)

    native = ctypes.WinDLL("ntdll")

    class IoStatus(ctypes.Structure):
        _fields_ = [("status", ctypes.c_size_t), ("information", ctypes.c_size_t)]

    class Rename(ctypes.Structure):
        _fields_ = [("replace", ctypes.c_ubyte), ("root", wintypes.HANDLE),
                    ("length", wintypes.DWORD), ("name", ctypes.c_wchar * 1)]

    native.NtSetInformationFile.argtypes = [wintypes.HANDLE, ctypes.POINTER(IoStatus),
                                           ctypes.c_void_p, wintypes.DWORD, wintypes.DWORD]
    native.NtSetInformationFile.restype = ctypes.c_long

    def rename_held(handle, parent, name, information_class=10, flags=0):
        encoded = name.encode("utf-16-le")
        buffer = ctypes.create_string_buffer(Rename.name.offset + len(encoded))
        info = ctypes.cast(buffer, ctypes.POINTER(Rename)).contents
        info.replace, info.root, info.length = 0, parent, len(encoded)
        ctypes.c_uint32.from_buffer(buffer).value = flags
        ctypes.memmove(ctypes.addressof(buffer) + Rename.name.offset, encoded, len(encoded))
        status = IoStatus()
        return native.NtSetInformationFile(handle, ctypes.byref(status), buffer,
                                            len(buffer), information_class) & 0xFFFFFFFF

    with tempfile.TemporaryDirectory(prefix="zup-fixed-probe-") as temporary:
        root = Path(temporary)
        target, staged, backup = root / "target.bin", root / "staged.bin", root / "backup.bin"
        target.write_bytes(b"previous")
        staged.write_bytes(b"replacement")
        old_id, new_id = identity(target), identity(staged)
        shutil.copyfile(target, backup)
        copied_id = identity(backup)
        assert copied_id != old_id
        backup.unlink()
        if not kernel.ReplaceFileW(str(target), str(staged), str(backup), 0, None, None):
            raise ctypes.WinError(ctypes.get_last_error())
        replaced, saved = identity(target), identity(backup)
        assert replaced == new_id and saved == old_id
        assert target.read_bytes() == b"replacement" and backup.read_bytes() == b"previous"
        if not kernel.MoveFileExW(str(backup), str(target), 1 | 8):
            raise ctypes.WinError(ctypes.get_last_error())
        restored = identity(target)
        assert restored == old_id and target.read_bytes() == b"previous"
        staged.write_bytes(b"blocked")
        handle = opened(target, 1)
        try:
            success = kernel.ReplaceFileW(str(target), str(staged), str(backup), 0, None, None)
            error = ctypes.get_last_error()
            assert not success and error == 32
            assert identity(target) == old_id and target.read_bytes() == b"previous"
            assert not backup.exists() and staged.read_bytes() == b"blocked"
        finally:
            kernel.CloseHandle(handle)
        parent = opened(root, 3, directory=True)
        old = opened(target, 1, 0x80000000 | 0x10000)
        replacement = opened(staged, 1, 0x80000000 | 0x10000)
        try:
            assert rename_held(old, parent, "held-backup.bin") == 0
            assert identity(root / "held-backup.bin") == old_id
            target.write_bytes(b"foreign")
            foreign = identity(target)
            collision = rename_held(replacement, parent, "target.bin")
            assert collision == 0xC0000035
            assert target.read_bytes() == b"foreign" and identity(target) == foreign
            target.unlink()
            assert rename_held(replacement, parent, "target.bin") == 0
            assert rename_held(replacement, parent, "retired.bin") == 0
            assert rename_held(old, parent, "target.bin") == 0
            assert identity(target) == old_id
        finally:
            for handle in [replacement, old, parent]:
                kernel.CloseHandle(handle)
        assert target.read_bytes() == b"previous"
        atomic_results = []
        for access_name, access in [("read_delete", 0x80000000 | 0x10000),
                                    ("read_write_delete", 0x80000000 | 0x40000000 | 0x10000)]:
            for sharing in [0, 1, 5]:
                label = f"{access_name}-{sharing}"
                current_path, stage_path = root / f"{label}-target", root / f"{label}-stage"
                backup_name = f"{label}-backup"
                current_path.write_bytes(b"previous")
                stage_path.write_bytes(b"replacement")
                before, prepared = identity(current_path), identity(stage_path)
                parent = opened(root, 3, directory=True)
                old = opened(current_path, sharing, access)
                replacement = opened(stage_path, 7, 0x80000000 | 0x10000)
                try:
                    linked = rename_held(old, parent, backup_name, information_class=11)
                    assert linked == 0, f"native link failed: {linked:08x}"
                    atomic = rename_held(replacement, parent, current_path.name,
                                         information_class=65, flags=3)
                finally:
                    for handle in [replacement, old, parent]:
                        kernel.CloseHandle(handle)
                assert identity(root / backup_name) == before
                assert identity(current_path) == (prepared if atomic == 0 else before)
                if atomic == 0:
                    parent = opened(root, 3, directory=True)
                    previous = opened(root / backup_name, 7, 0x80000000 | 0x10000)
                    current = opened(current_path, 7, 0x80000000 | 0x10000)
                    try:
                        assert rename_held(current, parent, f"{label}-retired", information_class=11) == 0
                        assert rename_held(previous, parent, current_path.name, information_class=65, flags=3) == 0
                    finally:
                        for handle in [current, previous, parent]:
                            kernel.CloseHandle(handle)
                    assert identity(current_path) == before and identity(root / f"{label}-retired") == prepared
                atomic_results.append({"access": access_name, "sharing": sharing,
                                       "status": f"{atomic:08x}", "original_preserved": True})
        guarded_target, foreign_path = root / "guarded-target", root / "foreign"
        guarded_target.write_bytes(b"owned")
        foreign_path.write_bytes(b"foreign")
        guarded_id, foreign_id = identity(guarded_target), identity(foreign_path)
        parent = opened(root, 3, directory=True)
        guard = opened(guarded_target, 5, 0x80000000 | 0x40000000 | 0x10000)
        foreign = opened(foreign_path, 7, 0x80000000 | 0x10000)
        try:
            assert rename_held(guard, parent, "guarded-backup", information_class=11) == 0
            raced = rename_held(foreign, parent, guarded_target.name, information_class=65, flags=3)
            assert raced == 0
        finally:
            for handle in [foreign, guard, parent]:
                kernel.CloseHandle(handle)
        assert identity(guarded_target) == foreign_id
        assert identity(root / "guarded-backup") == guarded_id

        fixture_source = root / "fixture.rs"
        fixture_image = root / "fixture.exe"
        fixture_source.write_text('''
use std::io::{self, Write};
fn main() {
    println!("ready");
    io::stdout().flush().unwrap();
    let mut input = String::new();
    io::stdin().read_line(&mut input).unwrap();
}
''', encoding="utf-8")
        subprocess.run(["rustc", str(fixture_source), "-o", str(fixture_image)], check=True, timeout=60)
        kernel.CloseHandle(opened(fixture_image, 5, 0x80000000 | 0x40000000 | 0x10000))
        with subprocess.Popen([str(fixture_image)], stdin=subprocess.PIPE,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True) as running:
            try:
                assert running.stdout.readline().strip() == "ready"
                try:
                    guard = opened(fixture_image, 5, 0x80000000 | 0x40000000 | 0x10000)
                except OSError as failure:
                    image_guard_error = failure.winerror
                    assert image_guard_error == 32, failure
                else:
                    kernel.CloseHandle(guard)
                    raise AssertionError("write/delete guard admitted the running image")
                _, failure = running.communicate("exit\n", timeout=15)
                assert running.returncode == 0, failure
            finally:
                if running.poll() is None:
                    running.kill()
                    running.communicate(timeout=15)
        return {
            "platform": "windows",
            "copy_preserved_original_identity": copied_id == old_id,
            "replacement_has_staged_identity": replaced == new_id,
            "native_backup_preserved_original_identity": saved == old_id,
            "restoration_preserved_original_identity": restored == old_id,
            "locked_target_error": error,
            "locked_target_preserved": True,
            "held_no_replace_collision_status": f"{collision:08x}",
            "held_rename_preserved_original_identity": True,
            "foreign_destination_preserved": True,
            "hardlink_atomic_rename_results": atomic_results,
            "delete_sharing_permitted_foreign_replacement": raced == 0,
            "running_image_write_delete_guard_error": image_guard_error,
        }


if __name__ == "__main__":
    result = probe()
    output = Path(__file__).resolve().parent.parent / "target" / "fixed-publication-probe.json"
    output.parent.mkdir(exist_ok=True)
    output.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result))
