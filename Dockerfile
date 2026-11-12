# supported versions here: https://hub.docker.com/_/rust
ARG ALPINE_VERSION=3.24

# Use the official Rust image as a stable base for the builder
# Build natively on each architecture; browser TLS includes native C/C++ code.
FROM rust:alpine${ALPINE_VERSION} AS builder
RUN apk add --no-cache musl-dev g++ git make perl file cmake clang clang-dev ninja go binutils
# bindgen loads libclang dynamically, including from Cargo build scripts.
# Alpine's default static CRT prevents dlopen; the runtime supplies musl.
ENV RUSTFLAGS="-C target-feature=-crt-static"
RUN rustup component add rust-src
WORKDIR /redlib
COPY . .
ARG TARGETPLATFORM
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=reg-${TARGETPLATFORM} \
    --mount=type=cache,target=/redlib/target,id=target-${TARGETPLATFORM} \
    cargo build --locked --release --bin redlib && \
    cp target/release/redlib /usr/local/bin/redlib
RUN file /usr/local/bin/redlib

########################
## Release Image
########################
FROM alpine:${ALPINE_VERSION} AS release
RUN apk add --no-cache ca-certificates libstdc++
COPY --from=builder /usr/local/bin/redlib /usr/local/bin/redlib

# Security: Non-root user
RUN adduser --home /nonexistent --no-create-home --disabled-password redlib
USER redlib

EXPOSE 8080
HEALTHCHECK --interval=1m --timeout=3s CMD wget --spider -q http://localhost:8080/settings || exit 1

CMD ["/usr/local/bin/redlib"]
