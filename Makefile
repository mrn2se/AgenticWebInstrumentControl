.PHONY: build test image run run-rootful web-install web-build e2e visa udev clean \
	types backend frontend native run-backend run-lan run-frontend licenses

# One image name for build and run; override with `make run IMAGE=...`.
IMAGE ?= localhost/awic:dev

# Extra server arguments for the run targets, e.g.
# `make run ARGS="--bind 0.0.0.0:8080 --set recorder.memory_mb=512"`.
# AWIC_<SECTION>__<KEY> environment variables work too (see `awic --help`).
ARGS ?=

# Build the Rust backend
build:
	cargo build

# Run tests, linting, and formatting checks
test:
	cargo test
	cargo clippy --all-targets -- -D warnings
	cargo fmt --check

# Build the container image
image:
	podman build -f Containerfile -t $(IMAGE) .

# Run the container with rootless podman
run: image
	tools/run.sh $(IMAGE) $(ARGS)

# Fallback for hosts without the udev rules: rootful podman, which keeps its
# own image store — tools/run-rootful.sh copies the image across.
run-rootful: image
	tools/run-rootful.sh $(IMAGE) $(ARGS)

# ---- Native builds: no container, needs cargo and Node/npm on the host ----
# Same steps as .github/workflows/ci.yml.

# Wire types for the frontend: web/src/proto/types.ts is generated from the
# Rust types by ts-rs and not in source control, so it comes first.
types:
	cargo test --locked -p awic-proto export_bindings
	test -s web/src/proto/types.ts

# Release build of the server binary (target/release/awic)
backend:
	cargo build --release --locked -p awic-server --bin awic

# npm ci only when package-lock.json changed since the last install
web/node_modules/.package-lock.json: web/package-lock.json
	cd web && npm ci

# Build the frontend into web/dist
frontend: types web/node_modules/.package-lock.json
	cd web && npm run build

# One self-contained binary with web/dist compiled in, like the CI release
native: frontend
	cargo build --release --locked -p awic-server --bin awic --features embed-web

# THIRD-PARTY-LICENSES.txt, as CI attaches it to a release: the notices of
# everything compiled into the executable. Needs cargo-about
# (`cargo install --locked cargo-about --features cli`).
licenses: web/node_modules/.package-lock.json
	rustup target add x86_64-unknown-linux-musl
	tools/licenses/generate.sh THIRD-PARTY-LICENSES.txt

# Run the server on the host, serving web/dist at http://127.0.0.1:8080/.
# Uses config/awic.toml with the container paths swapped for local ones;
# device access needs the udev rules (`make udev`).
run-backend: backend frontend
	mkdir -p state
	AWIC_CONFIG=config/awic.toml \
	AWIC_SERVER__STATIC_DIR=$$PWD/web/dist \
	AWIC_SERVER__STATE_DIR=$$PWD/state \
	./target/release/awic $(ARGS)

# Same as run-backend, but listening on all network interfaces (HTTP on
# 0.0.0.0:8080, SCPI on 0.0.0.0:5025). No auth unless [auth] token is set.
run-lan: export AWIC_SERVER__BIND = 0.0.0.0:8080
run-lan: run-backend

# Vite dev server with hot reload at http://127.0.0.1:5173/; proxies /ws,
# /api and /mcp to a running backend (`make run-backend` in another terminal).
run-frontend: types web/node_modules/.package-lock.json
	cd web && npm run dev

# Install frontend dependencies (runs in a container since Node is not on the host)
web-install:
	podman run --rm -v $$PWD/web:/w -w /w node:22-trixie-slim npm ci

# Build the frontend distribution (runs in a container)
web-build:
	podman run --rm -v $$PWD/web:/w -w /w node:22-trixie-slim npm run build

# UI smoke test in headless Chromium against a running server (e.g. `make run`
# in another terminal). Catches layout bugs unit tests cannot see.
E2E_URL ?= http://127.0.0.1:8080/
e2e:
	podman build -q -t localhost/awic-e2e:latest web/e2e >/dev/null
	podman run --rm --net=host -e URL=$(E2E_URL) -v $$PWD/web/e2e:/e2e:ro,Z localhost/awic-e2e:latest node ui-smoke.js

# VISA conformance and control check against a running server (e.g. `make run`
# in another terminal). Proves the SCPI port is driveable by a standard VISA
# client, not only by AWIC's own tools; runs in a container so pyvisa need not
# be on the host.
VISA_ADDR ?= 127.0.0.1:5025
visa:
	podman build -q -t localhost/awic-visa:latest tools/visa >/dev/null
	podman run --rm --net=host -e ADDR=$(VISA_ADDR) -v $$PWD/tools/visa:/visa:ro,Z localhost/awic-visa:latest python visa_check.py

# Install udev rules (requires sudo)
udev:
	sudo tools/udev/install-udev.sh

# Clean build artifacts
clean:
	cargo clean
	rm -rf web/dist
	rm -rf web/node_modules
