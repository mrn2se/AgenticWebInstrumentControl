#!/bin/bash
# Rootful fallback (Q-01 option C), for hosts where the udev rules cannot be
# installed: --privileged gives the container raw USB and sysfs access.
set -euo pipefail

IMAGE="${1:-localhost/awic:dev}"
[ $# -gt 0 ] && shift   # the rest are server arguments, e.g. --bind 0.0.0.0:8080

# Settings given as AWIC_<SECTION>__<KEY> variables (e.g. AWIC_SERVER__BIND)
# reach the server inside the container too. The two directory settings are
# left out: they name host paths here, and the container has its own.
env_args=()
for name in $(compgen -e); do
  case "$name" in
    AWIC_SERVER__STATIC_DIR|AWIC_SERVER__STATE_DIR) ;;
    AWIC_*__*) env_args+=(-e "$name=${!name}") ;;
  esac
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
mkdir -p "$ROOT/state"   # session state (e.g. USB port locks), written by the server

# Rootful podman keeps its own image store: an image built rootless (as
# `make image` does) is not visible to `sudo podman` until copied across.
if ! sudo podman image exists "$IMAGE"; then
  echo "copying $IMAGE into root's image store..."
  podman save "$IMAGE" | sudo podman load
fi

# --net=host: kernel hot-plug events are network-namespace scoped (SPEC §4.3).
exec sudo podman run --rm --name awic \
  --privileged \
  --net=host \
  -v /dev/bus/usb:/dev/bus/usb \
  -v /dev:/host/dev \
  -v "$ROOT/config:/etc/awic:ro" \
  -v "$ROOT/state:/var/lib/awic" \
  ${env_args[@]+"${env_args[@]}"} \
  "$IMAGE" "$@"
