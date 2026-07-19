# Multi-stage cargo build → distroless (nonroot).
ARG RUST_VERSION=1.97.1
ARG DEBIAN_RELEASE=trixie

FROM rust:${RUST_VERSION}-${DEBIAN_RELEASE} AS build
WORKDIR /build
# Cache the dependency layer: first build only the manifest + a dummy bin, then
# the real sources.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
 && cargo build --release --locked \
 && rm -rf src
COPY src ./src
# `touch` is MANDATORY: the copied sources carry the (older) checkout mtime, while
# the dummy binary from the dep-cache layer is newer → otherwise cargo considers the
# binary up to date and does NOT rebuild, shipping the empty `fn main(){}` dummy (the
# container would exit 0 immediately with no logs). `touch` forces the real rebuild.
RUN touch src/main.rs && cargo build --release --locked

# The runtime Debian MUST match the build Debian (glibc): build = rust:*-trixie
# (Debian 13, glibc 2.39) → distroless cc-debian13, otherwise `GLIBC_2.39 not found`
# at startup. distroless/cc ships ca-certificates (used for the outbound HTTPS to
# the IONOS Cloud API).
FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=build /build/target/release/nlb-reconciler /nlb-reconciler
ENTRYPOINT ["/nlb-reconciler"]
