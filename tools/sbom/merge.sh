#!/bin/bash
# Adds the web frontend's npm packages to an executable's Rust SBOM, in place:
# with `embed-web` they are compiled into the executable, so one SBOM should
# describe all of it. The web frontend becomes a component the executable
# depends on, and its packages keep their own dependency entries.
#
# The result keeps the executable SBOM's CycloneDX version (cargo-cyclonedx's
# 1.3): the only fields npm's 1.5 output adds to the file are in its metadata,
# and none of that is copied. It is a new document, so it gets a new serial.
#
# Usage: tools/sbom/merge.sh EXECUTABLE.cdx.json WEB.cdx.json
set -euo pipefail

exe="$1"
web="$2"
tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

jq --slurpfile web "$web" --arg serial "urn:uuid:$(cat /proc/sys/kernel/random/uuid)" '
  $web[0] as $w
  | .metadata.component["bom-ref"] as $root
  | .serialNumber = $serial
  | .components += [$w.metadata.component] + $w.components
  | .dependencies |= map(
      if .ref == $root then .dependsOn += [$w.metadata.component["bom-ref"]] else . end
    )
  | .dependencies += $w.dependencies
' "$exe" > "$tmp"

# Every web package arrived, and no reference collides with a Rust one.
jq -e --slurpfile web "$web" '
  ([.components[]["bom-ref"]] | length == (unique | length))
  and ([.components[]["bom-ref"]] | contains([$web[0].components[]["bom-ref"]]))
  and ([.dependencies[].ref] | length == (unique | length))
' "$tmp" > /dev/null || { echo "merge.sh: merged SBOM $exe is inconsistent" >&2; exit 1; }

mv "$tmp" "$exe"
trap - EXIT
