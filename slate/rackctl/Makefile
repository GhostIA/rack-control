# rackctl: orchestrator for the Slate 7 (OpenWrt, arm64).
GO      ?= go
BIN     := bin/rackctl
GOFLAGS := -trimpath -ldflags='-s -w'

.PHONY: all host test e2e clean

# Release build for the Slate 7.
all:
	CGO_ENABLED=0 GOOS=linux GOARCH=arm64 $(GO) build $(GOFLAGS) -o $(BIN) .

# Build for the machine you are on (dry runs, local experiments).
host:
	CGO_ENABLED=0 $(GO) build $(GOFLAGS) -o bin/rackctl-host .

test:
	$(GO) vet ./... && $(GO) test ./...

# Docker end-to-end test (needs a running Docker daemon; see e2e/README section in README.md).
e2e:
	$(GO) test -tags e2e -count=1 -v -timeout 15m ./e2e/

clean:
	rm -rf bin e2e/work
