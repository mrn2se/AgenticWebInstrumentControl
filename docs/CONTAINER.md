# AWIC in a container

The [Containerfile](../Containerfile) builds one image with everything AWIC needs: the `awic`
server, the web UI, and the FX2 firmware for both instruments. Nothing else has to be installed on
the host apart from podman and the udev rules — no Rust, no Node, no firmware package. Running
the release binary is in the [README](../README.md), building from source in
[DEVELOPMENT.md](DEVELOPMENT.md#7-building-and-running-from-source).

The scripts and make targets use **rootless podman**. Docker reads the same Containerfile; the
run flags below translate one to one, but only podman is tested.

## What is in the image

| Stage | Base | Produces |
|---|---|---|
| build | `rust:1.98-slim-trixie` | `/usr/local/bin/awic`, and the TypeScript wire types for the next stage |
| web | `node:22-trixie-slim` | the web UI, at `/usr/share/awic/dist` |
| runtime | `debian:trixie-slim` | the two above, plus `sigrok-firmware-fx2lafw`, `ca-certificates` and `fonts-dejavu-core` (labels in PNG views) |

Exposes **8080** (web UI, WebSocket, `/mcp`, `/api/render`) and **5025** (SCPI). The entrypoint is
`awic`, so anything after the image name is a server argument.

## Build

```sh
make image                                   # podman build -t localhost/awic:dev .
make image IMAGE=localhost/awic:test         # another tag; the run targets take IMAGE too
podman build --build-arg CARGO_BUILD_JOBS=8 -t localhost/awic:dev .
```

The Rust stage builds with 4 parallel jobs by default: more peaked past 14 GB and was OOM-killed on
the bench host. Raise it with `CARGO_BUILD_JOBS` on a bigger machine.

## Host setup (once)

The container reaches the instruments through the host's `/dev/bus/usb`, which is root-owned. The
udev rules give the `plugdev` group the instruments, the hubs and the hubs' port-power attributes:

```sh
make udev                     # sudo tools/udev/install-udev.sh
sudo usermod -aG plugdev $USER   # then log out and back in, and replug the instruments
```

Hosts where udev rules cannot be installed use the rootful fallback below instead.

## Run

```sh
make run                                      # build if needed, then tools/run.sh
make run ARGS="--bind 0.0.0.0:8080"           # server arguments after the image
AWIC_RECORDER__MEMORY_MB=512 make run         # AWIC_<SECTION>__<KEY> variables are passed in
make run-rootful                              # fallback without udev rules (sudo, --privileged)
```

Then open <http://127.0.0.1:8080/>. The container is named `awic` and removed when it stops.

`tools/run.sh` runs, from the repository root:

```bash
podman run --rm --name awic \
  --net=host \                              # hot-plug uevents are network-namespace scoped
  --group-add keep-groups \                 # carries plugdev into the container
  --security-opt apparmor=unconfined \      # Ubuntu's AppArmor blocks USB access otherwise
  -v /dev/bus/usb:/dev/bus/usb \            # the whole directory: device nodes change on re-enumeration
  -v /sys/devices:/sys/devices \            # writable: port power through sysfs
  -v /dev:/host/dev \                       # serial ports, tracked as they come and go
  -v ./config:/etc/awic:ro \                # configuration and firmware overrides
  -v ./state:/var/lib/awic \                # session state: USB port locks
  localhost/awic:dev [server arguments]
```

Why each flag is there is in SPECIFICATION.md §1.3–1.4. In short: a per-device `--device` mount
goes stale on the first power cycle or firmware upload, because the device comes back with a new
device number; the bind mounts follow it. Rootless, the server still has only your own rights on
the host.

`tools/run-rootful.sh` runs the same with `sudo podman run --privileged`, copying the image into
root's image store first if it is not there.

## Configuration

| | In the container |
|---|---|
| Config file | `config/awic.toml` on the host, mounted at `/etc/awic/awic.toml` (read-only) |
| Environment | every `AWIC_<SECTION>__<KEY>` variable set when calling `make run` is passed in, except `AWIC_SERVER__STATIC_DIR` and `AWIC_SERVER__STATE_DIR`, which name host paths |
| Arguments | `make run ARGS="--set recorder.memory_mb=512 --bind 0.0.0.0:8080"` |
| Web UI | fixed at `/usr/share/awic/dist` in the image, whatever the config says |
| State | `state/` on the host, at `/var/lib/awic` |
| Firmware | `.fw` files in `config/firmware/` on the host override the image's own; the log says which one was used |

The default `bind` is `127.0.0.1:8080`. To reach AWIC from the LAN bind `0.0.0.0:8080`, and set
`[auth] token` unless the network is trusted: anyone who reaches the port can switch USB power.

## Connecting an agent

The MCP endpoint is `http://<host>:8080/mcp`, the same as without a container; the web UI's MCP
tab shows the settings to copy. A client that only speaks stdio runs the proxy inside the
container:

```json
{
  "mcpServers": {
    "awic": {
      "command": "podman",
      "args": ["exec", "-i", "awic", "awic", "mcp-stdio"],
      "env": { "AWIC_MCP_TOKEN": "<token if set>" }
    }
  }
}
```

## Other containers

These need no Node or Python on the host either:

| Target | Runs |
|---|---|
| `make web-install` | `npm ci` for the frontend in `node:22-trixie-slim` |
| `make web-build` | the frontend build into `web/dist` |
| `make e2e` | the Playwright UI smoke test (`web/e2e/`) against a running server; `E2E_URL=...` for another one |
| `make visa` | the pyvisa conformance check ([tools/visa](../tools/visa/README.md)) against a running server; `VISA_ADDR=host:5025` for another one |

## Troubleshooting

| Symptom | Cause |
|---|---|
| The UI loads but lists no instruments | the udev rules are not installed, you are not in `plugdev` (log out and in), or the instruments were attached before the rules (replug them) |
| A port switches off but the device stays listed | `/sys/devices` is not writable, so AWIC fell back to a raw hub transfer; use `tools/run.sh` or the rootful fallback |
| No hot-plug updates | the container is not on `--net=host`, so netlink uevents never arrive; run with `--net=host`, or set `[watcher] mode = "polling"` (about 1 s latency) |
| `podman: image not known` under `make run-rootful` | the image is built rootless; the script copies it across, which needs `sudo` |
| A serial port is missing in the terminal | the `/dev:/host/dev` mount is missing, or the port's tty node is not in `plugdev`'s reach |
