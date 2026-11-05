#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
cargo fmt --all --check
rustup target add wasm32-unknown-unknown
cargo xtask fixtures build
cargo clippy -p zup-linux -p zup-bootstrap -p zup-plan -p zup-binary --all-features --all-targets -- -D warnings
cargo build -p zup-installer --features console,headless,gui -p zup-linux --features zup-linux/test-support
profile="${CARGO_TARGET_DIR:-$root/target}/debug"
mkdir -p "$profile/toolchain/0.0.1"
for frontend in console headless gui; do
  cp "$profile/zup-setup-$frontend" "$profile/toolchain/0.0.1/zup-setup-$frontend-linux-$(uname -m | sed 's/x86_64/x64/;s/aarch64/arm64/')"
done
cargo nextest run -p zup-linux --features test-support --status-level fail
cargo nextest run -p zup-bootstrap -p zup-binary -p zup-plan -p zup-manifest -p zup-toolchain --status-level fail
cargo test --doc -p zup-linux -p zup-bootstrap -p zup-plan -p zup-binary
