"""Check handle-relative containment while changing a parent's reparse tag."""

import ctypes as c
from ctypes import wintypes as w
import json
import os
from pathlib import Path
import struct
import tempfile


def probe():
    if os.name != "nt":
        raise RuntimeError("Windows is required")
    kernel = c.WinDLL("kernel32", use_last_error=True)
    native = c.WinDLL("ntdll")
    kernel.CreateFileW.argtypes = [w.LPCWSTR, w.DWORD, w.DWORD, c.c_void_p,
                                  w.DWORD, w.DWORD, c.c_void_p]
    kernel.CreateFileW.restype = w.HANDLE
    kernel.CloseHandle.argtypes = [w.HANDLE]
    kernel.DeviceIoControl.argtypes = [w.HANDLE, w.DWORD, c.c_void_p, w.DWORD,
                                      c.c_void_p, w.DWORD, c.POINTER(w.DWORD),
                                      c.c_void_p]
    kernel.DeviceIoControl.restype = w.BOOL

    class Unicode(c.Structure):
        _fields_ = [("length", w.USHORT), ("maximum", w.USHORT),
                    ("buffer", c.c_void_p)]

    class Attributes(c.Structure):
        _fields_ = [("length", w.ULONG), ("root", w.HANDLE),
                    ("name", c.POINTER(Unicode)), ("attributes", w.ULONG),
                    ("security", c.c_void_p), ("quality", c.c_void_p)]

    class Status(c.Structure):
        _fields_ = [("status", c.c_void_p), ("information", c.c_size_t)]

    native.NtCreateFile.argtypes = [c.POINTER(w.HANDLE), w.ULONG,
                                   c.POINTER(Attributes), c.POINTER(Status),
                                   c.c_void_p, w.ULONG, w.ULONG, w.ULONG,
                                   w.ULONG, c.c_void_p, w.ULONG]
    native.NtCreateFile.restype = c.c_long

    def opened(path, access, share):
        handle = kernel.CreateFileW(str(path), access, share, None, 3,
                                    0x02200000, None)
        if handle == c.c_void_p(-1).value:
            raise c.WinError(c.get_last_error())
        return handle

    def ioctl(handle, code, value):
        buffer = c.create_string_buffer(value)
        returned = w.DWORD()
        if not kernel.DeviceIoControl(handle, code, buffer, len(value), None,
                                      0, c.byref(returned), None):
            return c.get_last_error()
        return 0

    with tempfile.TemporaryDirectory(prefix="zup-directory-root-") as temporary:
        root = Path(temporary).resolve()
        source, destination = root / "source", root / "destination"
        source.mkdir()
        destination.mkdir()
        (destination / "sentinel").write_bytes(b"unrelated fixture contents")
        held = opened(source, 0x80000004, 3)
        writer = None
        child = w.HANDLE()
        tag_set = False
        try:
            writer = opened(source, 0x40000000, 7)
            substitute = ("\\??\\" + str(destination)).encode("utf-16-le")
            printed = str(destination).encode("utf-16-le")
            paths = substitute + b"\0\0" + printed + b"\0\0"
            reparse = struct.pack("<IHHHHHH", 0xA0000003, 8 + len(paths), 0,
                                  0, len(substitute), len(substitute) + 2,
                                  len(printed)) + paths
            set_error = ioctl(writer, 0x900A4, reparse)
            tag_set = set_error == 0
            name = c.create_unicode_buffer("created-relative")
            unicode = Unicode(len("created-relative") * 2,
                              c.sizeof(name), c.cast(name, c.c_void_p))
            attributes = Attributes(c.sizeof(Attributes), held,
                                    c.pointer(unicode), 0x1040, None, None)
            status = Status()
            result = native.NtCreateFile(c.byref(child), 0x120081,
                                         c.byref(attributes), c.byref(status),
                                         None, 0x80, 7, 2, 0x200021, None, 0)
            if child.value:
                kernel.CloseHandle(child)
                child = w.HANDLE()
            redirected = (destination / "created-relative").exists()
            if tag_set:
                delete_error = ioctl(writer, 0x900AC,
                                     struct.pack("<IHH", 0xA0000003, 0, 0))
                if delete_error:
                    raise c.WinError(delete_error)
                tag_set = False
            physical_child = (source / "created-relative").exists()
            assert (destination / "sentinel").read_bytes() == b"unrelated fixture contents"
            return {
                "parent_share_mode": "read-write; delete excluded",
                "set_parent_mount_point_error": set_error,
                "relative_create_status": f"0x{result & 0xffffffff:08x}",
                "created_in_original_directory": physical_child,
                "created_in_reparse_destination": redirected,
                "scope": "isolated handle-relative NTFS experiment; no durability proof",
            }
        finally:
            if child.value:
                kernel.CloseHandle(child)
            if writer is not None:
                if tag_set:
                    error = ioctl(writer, 0x900AC,
                                  struct.pack("<IHH", 0xA0000003, 0, 0))
                    if error:
                        raise c.WinError(error)
                kernel.CloseHandle(writer)
            kernel.CloseHandle(held)


if __name__ == "__main__":
    result = probe()
    output = Path(__file__).resolve().parent.parent / "target/windows-directory-root-probe.json"
    output.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result, indent=2))
