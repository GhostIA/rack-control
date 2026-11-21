# rack-agent: one static x86_64 musl binary for all five Proxmox hosts.
#   make            -> release build (cargo-zigbuild if installed, else Docker)
#   make test       -> fmt + clippy + unit tests (host, no bus/broker needed)
#   make e2e        -> Docker e2e: mosquitto (mTLS+passwd+ACL) + agent in dry_run
TARGET     := x86_64-unknown-linux-musl
BIN        := target/$(TARGET)/release/rack-agent
MUSL_IMAGE := ghcr.io/rust-cross/rust-musl-cross:x86_64-musl
HAVE_ZIG   := $(shell command -v cargo-zigbuild 2>/dev/null)

.PHONY: all build zigbuild docker-build test e2e clean

all: build

build: $(if $(HAVE_ZIG),zigbuild,docker-build)

zigbuild:
	cargo zigbuild --release --locked --target $(TARGET)

docker-build:
	docker run --rm -v "$(CURDIR)":/home/rust/src -v rack-agent-cargo-registry:/root/.cargo/registry -w /home/rust/src $(MUSL_IMAGE) cargo build --release --locked --target $(TARGET)

test:
	cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test

e2e:
	cargo run --release --locked --manifest-path e2e/Cargo.toml

clean:
	cargo clean
