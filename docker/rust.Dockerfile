# All six Rust images (migrator, gateway, control-plane, geo-engine,
# location-consumer, safety-consumer) build from this one file.
#
#   docker build -f docker/rust.Dockerfile \
#     --build-arg PACKAGE=atlas-gateway \
#     --build-arg BIN=atlas-gateway \
#     [--build-arg EXTRA_APT=cmake] .
#
# Base is pinned to the version in rust-toolchain.toml; the per-service
# Dockerfiles this replaced had drifted to 1.95 while the toolchain file
# said 1.97, so images were built by a compiler CI never used.

FROM rust:1.97 AS build
ARG PACKAGE
ARG BIN
# Extra build tools, e.g. "cmake" for the crates whose rdkafka dependency
# builds librdkafka from source. Multi-value: quoted, space-separated.
ARG EXTRA_APT=""
WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends \
        protobuf-compiler \
        ${EXTRA_APT} \
    && rm -rf /var/lib/apt/lists/*
# The whole workspace: Cargo member paths are relative to the workspace
# root, and build.rs reads from /proto at the repo root.
COPY . .
RUN cargo build --release -p "${PACKAGE}"

FROM debian:bookworm-slim AS runtime
ARG BIN
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
# Non-root by default; the k8s manifests set runAsUser explicitly anyway,
# but a docker run of these images should not be root either.
    && useradd --uid 10001 --system --no-create-home atlas
COPY --from=build "/src/target/release/${BIN}" /usr/local/bin/atlas-entry
USER 10001
ENTRYPOINT ["/usr/local/bin/atlas-entry"]
