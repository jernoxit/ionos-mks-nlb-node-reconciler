# Multi-stage cargo build → distroless (nonroot).
ARG RUST_VERSION=1.97.1
ARG DEBIAN_RELEASE=trixie

FROM rust:${RUST_VERSION}-${DEBIAN_RELEASE} AS build
# sccache: compile cache backed by the GitHub Actions cache. The runner's cache
# endpoint + token arrive as BuildKit secrets (see ci.yml/release.yml); local
# builds without those secrets skip the wrapper entirely.
# ACTIONS_CACHE_SERVICE_V2=on is MANDATORY inside the container: without it the
# ghac backend talks to the cache v1 API (shut down in April 2025) and every
# write fails silently — reads then degrade to permanent misses.
ARG SCCACHE_VERSION=0.17.0
RUN curl -fsSL "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
    | tar -xz --strip-components=1 -C /usr/local/bin --wildcards '*/sccache' \
 && chmod +x /usr/local/bin/sccache
WORKDIR /build
# Cache the dependency layer: first build only the manifest + a dummy bin, then
# the real sources.
COPY Cargo.toml Cargo.lock ./
RUN --mount=type=secret,id=actions_results_url --mount=type=secret,id=actions_runtime_token \
    set -e; \
    if [ -s /run/secrets/actions_results_url ]; then \
      export ACTIONS_RESULTS_URL="$(cat /run/secrets/actions_results_url)" \
             ACTIONS_RUNTIME_TOKEN="$(cat /run/secrets/actions_runtime_token)" \
             SCCACHE_GHA_ENABLED=true RUSTC_WRAPPER=sccache \
             ACTIONS_CACHE_SERVICE_V2=on; \
    fi; \
    mkdir src && echo 'fn main() {}' > src/main.rs; \
    cargo build --release --locked; \
    rm -rf src; \
    # --stop-server instead of --show-stats: sccache uploads asynchronously;
    # without the drain the daemon dies with the RUN step and the writes are
    # lost (observed upstream: 1479 compiles → ~10 cache entries → 0% hits).
    # stop-server blocks until the upload queue is empty and prints the stats.
    sccache --stop-server || true
COPY src ./src
# `touch` is MANDATORY: the copied sources carry the (older) checkout mtime, while
# the dummy binary from the dep-cache layer is newer → otherwise cargo considers the
# binary up to date and does NOT rebuild, shipping the empty `fn main(){}` dummy (the
# container would exit 0 immediately with no logs). `touch` forces the real rebuild.
RUN --mount=type=secret,id=actions_results_url --mount=type=secret,id=actions_runtime_token \
    set -e; \
    if [ -s /run/secrets/actions_results_url ]; then \
      export ACTIONS_RESULTS_URL="$(cat /run/secrets/actions_results_url)" \
             ACTIONS_RUNTIME_TOKEN="$(cat /run/secrets/actions_runtime_token)" \
             SCCACHE_GHA_ENABLED=true RUSTC_WRAPPER=sccache \
             ACTIONS_CACHE_SERVICE_V2=on; \
    fi; \
    touch src/main.rs; \
    cargo build --release --locked; \
    sccache --stop-server || true

# The runtime Debian MUST match the build Debian (glibc): build = rust:*-trixie
# (Debian 13, glibc 2.39) → distroless cc-debian13, otherwise `GLIBC_2.39 not found`
# at startup. distroless/cc ships ca-certificates (used for the outbound HTTPS to
# the IONOS Cloud API).
FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=build /build/target/release/nlb-reconciler /nlb-reconciler
ENTRYPOINT ["/nlb-reconciler"]
