# relay-mqtt: SBC4 (armv7l) release build + checks.
#   make            cross-build with cargo-zigbuild (brew install zig; cargo install cargo-zigbuild;
#                   rustup target add armv7-unknown-linux-musleabihf)
#   make docker     same build inside a musl cross image, no network, no host toolchain
#   make check      fmt + clippy + unit/socket tests (host)
#   make e2e        docker compose e2e (broker + fake controller + sidecar), see e2e/
TARGET      := armv7-unknown-linux-musleabihf
BIN         := target/$(TARGET)/release/relay-mqtt
CROSS_IMAGE ?= ghcr.io/rust-cross/rust-musl-cross:armv7-musleabihf
VENDOR_CFG  := --config 'source.crates-io.replace-with="vendored"' --config 'source.vendored.directory="/src/target/vendor"'

.PHONY: all build docker vendor check e2e clean

all: build

build:
	cargo zigbuild --release --locked --target $(TARGET) --bin relay-mqtt

vendor:
	cargo vendor --locked --versioned-dirs target/vendor > /dev/null

docker: vendor
	docker run --rm --network none -v "$(CURDIR)":/src -w /src $(CROSS_IMAGE) cargo build --release --locked --offline $(VENDOR_CFG) --target $(TARGET) --bin relay-mqtt

check:
	cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test

e2e:
	cargo run --release --manifest-path e2e/Cargo.toml --bin e2e

clean:
	cargo clean
