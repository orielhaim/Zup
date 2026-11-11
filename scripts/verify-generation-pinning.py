"""Probe native generation pinning; this does not test the deployment engine."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


FIXTURE = r'''
use std::{io, fs, env};
fn main() {
    let executable = env::current_exe().unwrap();
    println!("ready");
    let mut input = String::new();
    io::stdin().read_line(&mut input).unwrap();
    let resource = executable.parent().unwrap().join("resource.txt");
    println!("{}", fs::read_to_string(resource).unwrap());
}
'''


def probe():
    with tempfile.TemporaryDirectory(prefix="zup-generation-probe-") as temporary:
        root = Path(temporary)
        source = root / "fixture.rs"
        source.write_text(FIXTURE, encoding="utf-8")
        executable_name = "application.exe" if os.name == "nt" else "application"
        a, b = root / "a", root / "b"
        a.mkdir()
        b.mkdir()
        subprocess.run(
            ["rustc", str(source), "-o", str(a / executable_name)], check=True
        )
        (a / "resource.txt").write_text("generation-a", encoding="utf-8")
        with subprocess.Popen(
            [str(a / executable_name)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        ) as running:
            try:
                assert running.stdout.readline().strip() == "ready"
                shutil.copy2(a / executable_name, b / executable_name)
                (b / "resource.txt").write_text("generation-b", encoding="utf-8")
                locked_replacement = "not-probed"
                if os.name == "nt":
                    try:
                        with (a / executable_name).open("r+b") as image:
                            image.write(b"invalid")
                    except PermissionError:
                        locked_replacement = "refused"
                    else:
                        raise AssertionError("running executable accepted an overwrite")
                output, error = running.communicate("load\n", timeout=15)
                assert running.returncode == 0, error
                assert output.strip() == "generation-a", output
                newer = subprocess.run(
                    [str(b / executable_name)], input="load\n", text=True,
                    capture_output=True, check=True, timeout=15,
                )
                assert newer.stdout.splitlines() == ["ready", "generation-b"]
                return {
                    "platform": os.name,
                    "running_generation_delayed_resource": "generation-a",
                    "new_generation_resource": "generation-b",
                    "running_image_overwrite": locked_replacement,
                    "scope": "native pinned-path experiment, not engine validation",
                }
            finally:
                if running.poll() is None:
                    running.kill()
                    running.communicate(timeout=15)


if __name__ == "__main__":
    print(json.dumps(probe(), indent=2))
