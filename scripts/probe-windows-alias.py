"""Measure executable-relative lookup after switching a Windows junction."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


def junction(alias, target):
    environment = dict(os.environ, ZUP_PROBE_ALIAS=str(alias), ZUP_PROBE_TARGET=str(target))
    subprocess.run([
        "powershell", "-NoProfile", "-NonInteractive", "-Command",
        "$ErrorActionPreference='Stop'; New-Item -ItemType Junction "
        "-Path $env:ZUP_PROBE_ALIAS -Target $env:ZUP_PROBE_TARGET | Out-Null",
    ], env=environment, check=True, capture_output=True, text=True, timeout=15)


def probe():
    if os.name != "nt":
        raise RuntimeError("this experiment requires Windows")
    with tempfile.TemporaryDirectory(prefix="zup-junction-probe-") as temporary:
        root = Path(temporary).resolve(strict=True)
        a, b, alias = root / "a", root / "b", root / "current"
        a.mkdir()
        b.mkdir()
        source = root / "fixture.rs"
        source.write_text(r'''
use std::{env, fs, io::{self, Write}};
fn main() {
    let path = env::current_exe().unwrap();
    println!("{}", path.display());
    io::stdout().flush().unwrap();
    let mut line = String::new();
    io::stdin().read_line(&mut line).unwrap();
    println!("{}", fs::read_to_string(path.parent().unwrap().join("resource.txt")).unwrap());
}
''', encoding="utf-8")
        subprocess.run(["rustc", str(source), "-o", str(a / "app.exe")], check=True)
        shutil.copy2(a / "app.exe", b / "app.exe")
        for directory, content in [(a, "a"), (b, "b")]:
            (directory / "resource.txt").write_text(content, encoding="utf-8")
        results = []
        try:
            junction(alias, a)
            for launch in [a / "app.exe", alias / "app.exe"]:
                with subprocess.Popen([str(launch)], stdin=subprocess.PIPE,
                                      stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                      text=True) as child:
                    try:
                        reported = child.stdout.readline().strip()
                        if not reported:
                            raise RuntimeError("fixture did not report its executable path")
                        os.rmdir(alias)
                        junction(alias, b)
                        output, error = child.communicate("continue\n", timeout=15)
                        if child.returncode:
                            raise RuntimeError(error)
                        results.append({"launch": str(launch.relative_to(root)),
                                        "reported": str(Path(reported).relative_to(root)),
                                        "resource_after_switch": output.strip()})
                    finally:
                        if child.poll() is None:
                            child.kill()
                            child.communicate(timeout=15)
                os.rmdir(alias)
                junction(alias, a)
        finally:
            if alias.is_junction():
                os.rmdir(alias)
            if root.resolve(strict=True) != root or root.parent != Path(temporary).parent.resolve():
                raise RuntimeError("temporary cleanup boundary changed")
        return {"platform": "windows", "results": results,
                "scope": "current_exe adjacent-resource lookup, not general DLL behavior"}


if __name__ == "__main__":
    result = probe()
    destination = Path(__file__).resolve().parents[1] / "target/generation-alias-probe.json"
    destination.parent.mkdir(exist_ok=True)
    destination.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result, indent=2))
