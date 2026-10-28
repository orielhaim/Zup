#!/usr/bin/env python3
"""Measure composed installers against an already-built native toolchain."""

import argparse
import json
import statistics
import struct
import subprocess
import time
from pathlib import Path


def run(*args):
    return subprocess.run(args, check=True, capture_output=True, text=True).stdout


def measure(cli, toolchain, output):
    output.mkdir(parents=True, exist_ok=True)
    components = []
    for path in sorted(toolchain.glob("*.zup-toolchain.json")):
        component = json.loads(path.read_text())
        component["file"] = path.name.removesuffix(".zup-toolchain.json")
        components.append(component)
    preset = toolchain / "zup-preset.zupui"
    raw = preset.read_bytes()
    metadata_size = struct.unpack_from("<Q", raw, 8)[0]
    preset_metadata = json.loads(raw[32 : 32 + metadata_size])
    rows = []
    for component in components:
        if component["kind"] != "runtime":
            continue
        target, frontend = component["target"], component["frontend"]
        directory = output / f"{target}-{frontend}"
        directory.mkdir(exist_ok=True)
        (directory / "app.txt").write_bytes(b"hello installer sizes\n")
        manifest = directory / "zup.toml"
        manifest.write_text(
            '[app]\nid = "com.example.sizes"\nname = "Installer Sizes"\n'
            'version = "1.0.0"\npublisher = "Example"\n'
            f'[build.targets.{target}]\nsource = {{ directory = "." }}\n'
            f'frontend = "{frontend}"\n'
            '[install]\nscope = "user"\n[install.directory]\n'
            'user = "${location.user_data}/Installer Sizes"\n'
            '[[files]]\nsource = "app.txt"\ndestination = "${install}"\n'
        )
        artifact = directory / ("setup.exe" if target.startswith("windows") else "setup")
        started = time.perf_counter()
        run(str(cli), "--toolchain", str(toolchain), "build", "--manifest",
            str(manifest), "--output", str(artifact), "--release-manifest", "none", "--force")
        compose_seconds = time.perf_counter() - started
        inspection = run(str(cli), "artifact", "inspect", str(artifact), "--format", "json")
        (directory / "inspection.json").write_text(inspection)
        row = dict(target=target, frontend=frontend, runtime_bytes=component["size"],
                   payload_bytes=22, installer_bytes=artifact.stat().st_size,
                   compose_seconds=compose_seconds)
        if frontend == "gui":
            row["preset_executable_bytes"] = next(
                binary["size"] for binary in preset_metadata["binaries"]
                if binary["target"] == target
            )
        else:
            samples = []
            for _ in range(7):
                started = time.perf_counter()
                run(str(artifact), "--help")
                samples.append((time.perf_counter() - started) * 1000)
            row["help_startup_median_ms"] = statistics.median(samples)
        if target.startswith("linux"):
            footer = artifact.read_bytes()[-121:]
            row["package_bytes"] = struct.unpack_from("<Q", footer, 33)[0]
            row["embedded_preset_bytes"] = struct.unpack_from("<Q", footer, 73)[0]
            row["carrier_bytes"] = 121
        rows.append(row)
        print(json.dumps(row), flush=True)
    report = dict(components=components, preset_package_bytes=preset.stat().st_size,
                  preset_metadata_bytes=metadata_size, measurements=rows)
    (output / "sizes.json").write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", type=Path, required=True)
    parser.add_argument("--toolchain", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    measure(args.cli.resolve(), args.toolchain.resolve(), args.output.resolve())
