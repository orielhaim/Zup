#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
cargo fmt --all --check
rustup target add wasm32-wasip3
cargo xtask fixtures build
cargo clippy -p zup-linux -p zup-bootstrap -p zup-plan -p zup-binary --all-features --all-targets -- -D warnings
cargo xtask toolchain build --profile dev
cargo nextest run -p zup-linux --features test-support --status-level fail
cargo nextest run -p zup-bootstrap -p zup-binary -p zup-plan -p zup-manifest -p zup-toolchain --status-level fail
cargo test --doc -p zup-linux -p zup-bootstrap -p zup-plan -p zup-binary
