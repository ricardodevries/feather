# syntax=docker/dockerfile:1.7

ARG RUST_VERSION=1.96.0

FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION}-bookworm AS source
WORKDIR /src
RUN apt-get update \
    && apt-get install --yes --no-install-recommends libudev-dev pkg-config \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY config.example.toml README.md LICENSE.md ./
COPY assets ./assets
COPY docs ./docs
COPY packaging ./packaging
COPY scripts ./scripts

FROM source AS test
RUN cargo fmt --all -- --check
RUN cargo test --locked --workspace --all-targets --all-features
RUN cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
RUN sh -n scripts/build-amd64.sh scripts/install-server.sh scripts/uninstall-server.sh

FROM source AS release
RUN dpkg --add-architecture amd64 \
    && apt-get update \
    && apt-get install --yes --no-install-recommends \
        gcc-x86-64-linux-gnu \
        libc6-dev-amd64-cross \
        libudev-dev:amd64 \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*
RUN rustup target add x86_64-unknown-linux-gnu
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc
ENV PKG_CONFIG_ALLOW_CROSS=1
ENV PKG_CONFIG_PATH=/usr/lib/x86_64-linux-gnu/pkgconfig
RUN cargo build --locked --workspace --release --target x86_64-unknown-linux-gnu

FROM --platform=linux/amd64 debian:bookworm-slim AS smoke
RUN apt-get update \
    && apt-get install --yes --no-install-recommends libudev1 \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*
COPY --from=release /src/target/x86_64-unknown-linux-gnu/release/feather /usr/local/bin/feather
COPY --from=release /src/target/x86_64-unknown-linux-gnu/release/featherd /usr/local/bin/featherd
COPY config.example.toml /tmp/config.example.toml
RUN feather --version
RUN featherd --version
RUN feather config check /tmp/config.example.toml

FROM scratch AS artifact
COPY --from=release /src/target/x86_64-unknown-linux-gnu/release/feather /feather
COPY --from=release /src/target/x86_64-unknown-linux-gnu/release/featherd /featherd
COPY config.example.toml /config.example.toml
COPY packaging /packaging
COPY scripts/install-server.sh /install-server.sh
COPY scripts/uninstall-server.sh /uninstall-server.sh
COPY README.md LICENSE.md /
COPY assets /assets
COPY docs /docs
