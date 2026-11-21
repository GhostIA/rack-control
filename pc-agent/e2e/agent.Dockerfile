# Builds the agent natively for the Docker host's arch (musl via Alpine) for
# the e2e run only. The release x86_64 binary comes from `make`.
FROM rust:1.85-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock rack-agent.service config.example.toml ./
COPY polkit polkit
COPY src src
RUN cargo build --release --locked

FROM alpine:3.20
COPY --from=build /src/target/release/rack-agent /usr/local/bin/rack-agent
USER 65534:65534
ENTRYPOINT ["/usr/local/bin/rack-agent"]
