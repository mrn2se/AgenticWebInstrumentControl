# Two-stage build for AWIC.
#
# Stage order matters: web/src/proto/types.ts is GENERATED from the Rust wire
# types by ts-rs, so the Rust stage must run BEFORE the web stage and hand the
# generated file across. Building web first would fail on a fresh clone.

# ---- Stage 1: Rust backend (also emits the TypeScript wire types) ----
FROM rust:1.98-slim-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
# .cargo/config.toml sets TS_RS_EXPORT_DIR; without it ts-rs writes the types
# somewhere the web stage never looks.
COPY .cargo/ ./.cargo
COPY crates/ ./crates
# ts-rs writes into web/src/proto/, so that tree must exist here.
COPY web/ ./web
# Fail the build here, loudly, if the wire types were not generated — rather
# than let the web stage pick up nothing (or, before .containerignore, a
# stale host copy).
# Parallelism is capped, not left to rustc's default of one job per core:
# linking several release binaries at once peaked past what a 14 GB bench
# host had free and the build was OOM-killed twice (2026-09-16). Override
# with --build-arg CARGO_BUILD_JOBS=<n> on a bigger machine.
ARG CARGO_BUILD_JOBS=4
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}
RUN cargo build --release \
 && cargo test --release -p awic-proto export_bindings \
 && test -s web/src/proto/types.ts

# ---- Stage 2: Frontend ----
FROM node:26-trixie-slim AS web
WORKDIR /web
COPY web/ .
# Generated wire types from the Rust stage — not in source control.
COPY --from=build /src/web/src/proto/types.ts ./src/proto/types.ts
RUN npm ci && npm run build

# ---- Stage 3: Runtime ----
FROM debian:trixie-slim
# ca-certificates for optional firmware auto-download (SPEC §13);
# sigrok-firmware-fx2lafw supplies the FX2 blobs for both instruments;
# fonts-dejavu-core gives `render_view`'s PNG pictures their labels (the
# executable embeds no font and uses the host's).
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
    ca-certificates \
    fonts-dejavu-core \
    sigrok-firmware-fx2lafw && \
    rm -rf /var/lib/apt/lists/*

# Runs as root: USB device access and hub port power control both need write
# access to /dev/bus/usb nodes, which are root-owned on the host. Host-side
# least privilege is handled by udev rules (tools/udev/99-awic.rules), not by a
# container user.
COPY --from=build /src/target/release/awic /usr/local/bin/awic
COPY --from=web   /web/dist                /usr/share/awic/dist
# The frontend's location in this image, stated rather than implied: it wins
# over whatever a mounted config says, since only this path exists here.
ENV AWIC_SERVER__STATIC_DIR=/usr/share/awic/dist
# Session state, where tools/run.sh mounts the host's ./state. The built-in
# default is the user's home (~/.local/state/awic), which is not mounted.
ENV AWIC_SERVER__STATE_DIR=/var/lib/awic

EXPOSE 8080 5025
ENTRYPOINT ["/usr/local/bin/awic"]
