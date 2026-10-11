#!/bin/bash
# Rootless podman invocation for AWIC (SPECIFICATION.md §1.3).
#
# Needs the udev rules installed once (`make udev`): they give the plugdev
# group access to the instruments, the hubs, and the hubs' port-power
# attributes. Without them the container starts but cannot open the devices.
#
# /sys/devices is bind-mounted writable because port power goes through the
# kernel's per-port `disable` attribute (SPEC §4.2): a rootless container sees
# /sys read-only, and the fallback — a raw hub control transfer — leaves a
# switched-off device enumerated. This grants nothing beyond this user's own
# rights on the host: writes still pass the host's permission checks, and the
# udev rules are what give plugdev those attributes.
#
# /dev is bind-mounted separately (read-write, at /host/dev rather than over
# the container's own /dev) for the terminal module's serial source (SPEC
# §11.2/§11.2.1): the kernel's tty drivers (cdc_acm, ftdi_sio, ...) create
# /dev/ttyACM*, /dev/ttyUSB* nodes there as USB-serial devices come and go,
# exactly like /dev/bus/usb above. Mounting the host's whole /dev at a
# separate path, rather than over the container's own, keeps the container's
# other device nodes (e.g. /dev/null) intact.
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

exec podman run --rm --name awic \
  --net=host \
  --group-add keep-groups \
  --security-opt apparmor=unconfined \
  -v /dev/bus/usb:/dev/bus/usb \
  -v /sys/devices:/sys/devices \
  -v /dev:/host/dev \
  -v "$ROOT/config:/etc/awic:ro" \
  -v "$ROOT/state:/var/lib/awic" \
  ${env_args[@]+"${env_args[@]}"} \
  "$IMAGE" "$@"
