#!/bin/sh
# Full installer lifecycle for the cross-build matrix fixture (user scope).
set -eu
installer="$1"
payload="$HOME/.local/share/Cross Matrix/app.txt"
"$installer" install --scope user
if ! cmp fixtures/cross-matrix/payload/app.txt "$payload" >&2; then
    echo "e2e-linux: $payload is not byte-identical to fixtures/cross-matrix/payload/app.txt" >&2
    exit 1
fi
"$installer" repair --scope user
"$installer" uninstall --scope user
test ! -e "$HOME/.local/share/Cross Matrix"
echo "lifecycle complete: install, repair, uninstall"
