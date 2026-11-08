#!/usr/bin/env python3
import argparse
import hashlib
import json
import os
import platform
import shutil
import subprocess
import sys
import tarfile
import urllib.request
from pathlib import Path


def build(options):
    if sys.platform != "linux" or platform.machine() not in ("x86_64", "aarch64"):
        raise RuntimeError("Build the OpenRC controller on native x64 or ARM64 Linux")
    repository = Path(__file__).resolve().parent.parent
    native = repository / "native/openrc"
    upstream = json.loads((native / "upstream.json").read_text())
    patch = native / "pidfd-control.patch"
    if hashlib.sha256(patch.read_bytes()).hexdigest() != upstream["patch_sha256"]:
        raise RuntimeError("OpenRC patch does not match its source lock")
    for command in ("cc", "git", "meson", "ninja", "pkg-config"):
        if shutil.which(command) is None:
            raise RuntimeError(f"Required native build tool is unavailable: {command}")
    output = options.output.resolve()
    output.mkdir(mode=0o700, parents=True, exist_ok=False)
    archive = options.archive
    if archive is None:
        archive = output / "upstream.tar.gz"
        with (
            urllib.request.urlopen(upstream["archive_url"], timeout=60) as response,
            archive.open("wb") as destination,
        ):
            shutil.copyfileobj(response, destination)
    if hashlib.sha256(archive.read_bytes()).hexdigest() != upstream["archive_sha256"]:
        raise RuntimeError("OpenRC source archive checksum mismatch")
    unpacked = output / "source"
    unpacked.mkdir()
    with tarfile.open(archive) as source_archive:
        source_archive.extractall(unpacked, filter="data")
    source = unpacked / ("openrc-" + upstream["commit"])
    if not (source / "meson.build").is_file():
        raise RuntimeError("OpenRC source archive has an unexpected root")
    environment = os.environ.copy()
    for key in ("GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR"):
        environment.pop(key, None)
    environment["GIT_CEILING_DIRECTORIES"] = str(unpacked)
    environment["SOURCE_DATE_EPOCH"] = str(upstream["source_date_epoch"])
    for command in (
        ["git", "apply", "--check", str(patch)],
        ["git", "apply", str(patch)],
        [
            "meson",
            "setup",
            "build",
            "--buildtype=release",
            "--default-library=static",
            "--prefix=/usr",
            "--libdir=lib",
            "--sbindir=sbin",
            "-Dpam=false",
            "-Dselinux=disabled",
            "-Daudit=disabled",
            "-Dnewnet=false",
            "-Dbash-completions=false",
            "-Dzsh-completions=false",
        ],
        [
            "meson",
            "compile",
            "-C",
            "build",
            "-j",
            str(options.jobs),
            "src/supervise-daemon/supervise-daemon",
        ],
    ):
        subprocess.run(command, cwd=source, env=environment, check=True)
    binary = output / "supervise-daemon"
    shutil.copy2(source / "build/src/supervise-daemon/supervise-daemon", binary)
    binary.chmod(0o755)
    features = subprocess.check_output(
        [str(binary), "--control-features"], text=True, timeout=10
    )
    if features.strip() != upstream["control_protocol"]:
        raise RuntimeError(
            "Built OpenRC controller did not verify its required kernel interface"
        )
    shutil.copy2(native / "LICENSE", output / "LICENSE.openrc")
    receipt = {
        "upstream": upstream,
        "architecture": platform.machine(),
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "compiler": subprocess.check_output(
            ["cc", "--version"], text=True
        ).splitlines()[0],
        "meson": subprocess.check_output(["meson", "--version"], text=True).strip(),
    }
    (output / "build.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps({"binary": str(binary), "receipt": str(output / "build.json")}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(
        description="Build the pinned OpenRC pidfd controller without installing it"
    )
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--archive", type=Path)
    parser.add_argument("--jobs", type=int, choices=range(1, 65), default=2)
    build(parser.parse_args())
