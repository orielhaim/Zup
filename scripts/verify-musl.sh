#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
case "$(uname -m)" in
  x86_64) target=linux-x64-musl ;;
  aarch64) target=linux-arm64-musl ;;
  *) echo 'musl verification requires x64 or ARM64' >&2; exit 1 ;;
esac
cargo build -p zup
cargo run -p zup-xtask -- toolchain build --profile dev
cargo test -p zup-binary -p zup-bootstrap -p zup-plan -p zup-plugin-contract --lib
rustup target add wasm32-unknown-unknown
cargo run -p zup -- plugin build --project examples/plugins/configure --output target/configure.wasm
profile="${CARGO_TARGET_DIR:-target}/debug"
fixture="$(pwd)/target/musl-lifecycle"
mkdir -p "$fixture/dist" "$fixture/home"
printf '#!/bin/sh\nexit 0\n' > "$fixture/dist/tool"
cp target/configure.wasm "$fixture/configure.wasm"
cat > "$fixture/zup.toml" <<EOF
schema = 1
frontend = "headless"
[app]
id = "com.example.musl-verification"
name = "Musl Verification"
version = "1.0.0"
[build.targets.default]
target = "$target"
source = { directory = "dist" }
[install]
scope = "user"
[install.directory]
user = "\${location.programs}/musl-verification"
[[files]]
id = "tool"
source = "tool"
placement = { kind = "fixed", directory = "\${install}" }
executable = true
[[plugins]]
id = "configure"
source = "configure.wasm"
[[prerequisites]]
id = "tree"
name = "tree"
requirement = { kind = "native_package", id = "tree", minimum_version = "2.0" }
package = { type = "repository" }
EOF
"$profile/zup" build --manifest "$fixture/zup.toml" --runtime "$profile/toolchain/0.0.1/zup-setup-headless-$target" --output "$fixture/setup" --force
export HOME="$fixture/home"
export XDG_STATE_HOME="$HOME/.local/state"
"$fixture/setup" install --non-interactive --yes
installed="$HOME/.local/lib/zup/apps/musl-verification/tool"
test -x "$installed"
test -f "$HOME/.local/lib/zup/apps/musl-verification/configure.txt"
"$fixture/setup" repair --non-interactive --yes
"$fixture/setup" uninstall --non-interactive --yes
test ! -e "$installed"
apk info --exists tree
"$profile/zup-setup-console" --help >/dev/null
