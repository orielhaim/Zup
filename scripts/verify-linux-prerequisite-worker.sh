#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
image="${ZUP_LINUX_PREREQUISITE_TEST_IMAGE:-rust:1.100-slim}"

if ! command -v docker >/dev/null 2>&1; then
  echo "docker is required to run the disposable Linux prerequisite check" >&2
  exit 2
fi

docker run --rm --user 0:0 \
  --mount "type=bind,source=$root,target=/work,readonly" \
  --workdir /work \
  "$image" \
  cargo test --locked --target-dir /tmp/zup-target \
    -p zup-linux --features test-support --test machine \
    machine_install_
