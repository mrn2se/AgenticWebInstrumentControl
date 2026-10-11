# tools

Helpers around the `awic` server that are not part of it. Each has a make target; run them from
the repository root.

| Path | Make target | Does |
|---|---|---|
| [`run.sh`](run.sh) | `make run` | runs the container image with rootless podman ([docs/CONTAINER.md](../docs/CONTAINER.md)) |
| [`run-rootful.sh`](run-rootful.sh) | `make run-rootful` | the same with `sudo podman --privileged`, for hosts without the udev rules |
| [`udev/`](udev/) | `make udev` | installs `99-awic.rules`: the `plugdev` group gets the instruments, the hubs and their port-power attributes |
| [`visa/`](visa/README.md) | `make visa` | pyvisa conformance and control check of a running server's SCPI port, in a container |
| [`licenses/`](licenses/generate.sh) | `make licenses` | writes `THIRD-PARTY-LICENSES.txt` for the release binary (needs `cargo-about`) |
