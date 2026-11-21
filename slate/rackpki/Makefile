# rackpki runs on the dev Mac (host build), never on the rack.
.PHONY: all build test vet e2e clean

all: build

build:
	go build -trimpath -ldflags='-s -w' -o bin/rackpki .

vet:
	go vet ./...
	go vet -tags e2e ./...

test: vet
	go test ./...

# Needs Docker. Runs eclipse-mosquitto:2.0.20 on an isolated compose network.
e2e:
	go test -tags e2e -count=1 -v ./e2e/

clean:
	rm -rf bin
