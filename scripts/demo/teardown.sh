#!/usr/bin/env bash
# Stops the demo's daemon (its agents and shells with it) and deletes
# $TAM_DEMO (default /tmp/tam-demo). Your own tam is not touched.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
demo=${TAM_DEMO:-/tmp/tam-demo}
[ -e "$demo" ] || { echo "Nothing to remove: no $demo."; exit 0; }
(
  # shellcheck source=env.sh
  . "$here/env.sh"
  if [ -S "$XDG_RUNTIME_DIR/tam/sock" ] && command -v tam >/dev/null; then
    tam shutdown >/dev/null 2>&1 || true
  fi
)
rm -rf "$demo"
echo "Removed $demo."
