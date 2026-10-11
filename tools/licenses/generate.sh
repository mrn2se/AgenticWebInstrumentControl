#!/bin/bash
# Writes THIRD-PARTY-LICENSES.txt: the license notices of everything compiled
# into the release executable, which MIT, BSD and Apache-2.0 require to
# accompany every copy, binary ones included. Three parts:
#   1. Rust crates, from `cargo about` for the release target and features.
#   2. The Rust standard library and musl libc, linked statically: the
#      notices the toolchain itself ships (COPYRIGHT-library.html).
#   3. The web UI's runtime npm packages, compiled in by `embed-web`: each
#      package's own LICENSE, plus the bundled-license block plotly.js keeps
#      in its dist file (the Vite build strips it from the output).
#
# Needs cargo-about, python3, and web/node_modules (`npm ci` in web/).
# Usage: tools/licenses/generate.sh [OUTPUT]   (default: THIRD-PARTY-LICENSES.txt)
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(dirname "$(dirname "$HERE")")"
OUT="${1:-THIRD-PARTY-LICENSES.txt}"
TARGET="${TARGET:-x86_64-unknown-linux-musl}"

if [ ! -d "$ROOT/web/node_modules" ]; then
  echo "web/node_modules missing: run npm ci in web/ first" >&2
  exit 1
fi

{
  cat <<'EOF'
THIRD-PARTY SOFTWARE NOTICES AND LICENSES

The AWIC executable contains the third-party software listed below, each
under its own license. AWIC's own code is under the license in its LICENSE
file.

================================================================================
1. Rust crates
================================================================================
EOF
  cargo about generate --fail \
    -c "$HERE/about.toml" \
    --manifest-path "$ROOT/crates/awic-server/Cargo.toml" \
    --features embed-web \
    --target "$TARGET" \
    "$HERE/about.hbs"

  cat <<'EOF'

================================================================================
2. The Rust standard library and musl libc
================================================================================

EOF
  python3 "$HERE/html_to_text.py" "$(rustc --print sysroot)/share/doc/rust/COPYRIGHT-library.html"

  cat <<'EOF'

================================================================================
3. Web UI packages
================================================================================
EOF
  python3 "$HERE/npm_notices.py" "$ROOT/web"
} > "$OUT"

echo "wrote $OUT ($(wc -l < "$OUT") lines)" >&2
