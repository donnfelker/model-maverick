# syntax=docker/dockerfile:1
#
# The two deploy images (bead 9.4; master plan §4 "Deployment"; spec §14.2):
#
#   docker build -f docker/Dockerfile --target engine -t maverick .
#   docker build -f docker/Dockerfile --target proxy  -t maverick-proxy .
#
# - Both images come from one builder stage, so a second build reuses its layers.
# - cargo-chef cooks the dependencies in a layer of their own. A source change rebuilds
#   only the workspace crates.
# - Alpine's Rust targets musl, and Rust links musl binaries statically. The builder
#   refuses a binary with an ELF interpreter. The final images are distroless/static: no
#   libc, no shell, and the nonroot user.
# - TLS is rustls only. The builder has no OpenSSL headers, so a dependency that needs
#   OpenSSL fails the build.
# - The checked queries compile from .sqlx/ (SQLX_OFFLINE), with no database.
#
# The engine image runs every process shape through its arguments: `serve` (default),
# `serve --api-only`, `serve --worker-only`, and `migrate`.
#
# The release pipeline (scripts/release-artifacts.sh) passes MAVERICK_RELEASE_DATE and
# MAVERICK_RELEASE_COMMIT as build args, so `maverick --version` reports them. A build
# without them reports `dev` and `unknown`.

ARG RUST_VERSION=1.98

FROM rust:${RUST_VERSION}-alpine AS chef
# aws-lc-sys, the rustls crypto provider, compiles its C sources.
RUN apk add --no-cache musl-dev gcc make perl
ARG CARGO_CHEF_VERSION=0.1.78
RUN cargo install cargo-chef --locked --version "${CARGO_CHEF_VERSION}"
WORKDIR /src

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json \
      --package maverick --package maverick-proxy
COPY . .
ENV SQLX_OFFLINE=true
ARG MAVERICK_RELEASE_DATE=""
ARG MAVERICK_RELEASE_COMMIT=""
# An empty build arg is unset, so the binary keeps its `dev` and `unknown` defaults.
RUN if [ -z "${MAVERICK_RELEASE_DATE}" ]; then unset MAVERICK_RELEASE_DATE; fi \
 && if [ -z "${MAVERICK_RELEASE_COMMIT}" ]; then unset MAVERICK_RELEASE_COMMIT; fi \
 && cargo build --release --locked --package maverick --package maverick-proxy \
      --bin maverick --bin maverick-proxy \
 && for bin in maverick maverick-proxy; do \
      if readelf -l "target/release/${bin}" | grep -q INTERP; then \
        echo "${bin} is dynamically linked" >&2; exit 1; \
      fi; \
    done

FROM gcr.io/distroless/static-debian12:nonroot AS engine
COPY --from=builder /src/target/release/maverick /usr/local/bin/maverick
# 8080: the public listener (kamal-proxy routes it). 8081: the internal listener
# (never routed; loopback or the private network only).
EXPOSE 8080 8081
ENTRYPOINT ["/usr/local/bin/maverick"]
CMD ["serve"]

FROM gcr.io/distroless/static-debian12:nonroot AS proxy
COPY --from=builder /src/target/release/maverick-proxy /usr/local/bin/maverick-proxy
EXPOSE 8090
ENTRYPOINT ["/usr/local/bin/maverick-proxy"]
