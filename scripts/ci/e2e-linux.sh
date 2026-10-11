#!/bin/sh
# Full installer lifecycle for the cross-build matrix fixture (user scope).
set -eu
installer="$1"
"$installer" install --scope user
content=$(cat "$HOME/.local/share/Cross Matrix/app.txt")
test "$content" = "hello cross matrix"
"$installer" repair --scope user
"$installer" uninstall --scope user
test ! -e "$HOME/.local/share/Cross Matrix"
echo "lifecycle complete: install, repair, uninstall"
