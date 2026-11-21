# syntax=docker/dockerfile:1
# Linux (native arch) build of relay-mqtt + the test-only fake-controller and
# e2e-check binaries. Build context is sbc4/relay-mqtt (see compose.yaml).
# Dependencies come from target/vendor (made by the e2e runner), so the
# compile step runs with no network at all.
FROM rust:1.85-slim-bookworm AS build
# paho-mqtt (bundled + vendored-ssl) compiles the Paho C library with cmake and
# OpenSSL with perl/make. Toolchain packages only; the cargo build below
# still runs with no network.
RUN apt-get update
RUN apt-get install -y --no-install-recommends cmake make perl
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY target/vendor ./target/vendor
RUN --network=none cargo build --release --locked --offline \
    --config 'source.crates-io.replace-with="vendored"' \
    --config 'source.vendored.directory="/src/target/vendor"' \
    --bin relay-mqtt --bin fake-controller --bin e2e-check

FROM debian:bookworm-slim
COPY --from=build /src/target/release/relay-mqtt /src/target/release/fake-controller /src/target/release/e2e-check /usr/local/bin/
