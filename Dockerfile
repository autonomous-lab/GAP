# GAP node - multi-stage build
#
# Stage 1: compile the Rust binary (musl, statically linked).
FROM rust:1.97-alpine AS builder

# The Alpine CDN is the most common cause of a failed deploy here: one
# transient "temporary error (try again later)" from dl-cdn and the
# whole build dies. Retry, and fall back to a second mirror pinned to
# this image's own Alpine version before giving up.
RUN for attempt in 1 2 3 4 5; do \
        apk add --no-cache musl-dev pkgconfig && exit 0; \
        echo "apk failed (attempt $attempt of 5)"; \
        if [ "$attempt" = "2" ]; then \
            v=$(cut -d. -f1,2 /etc/alpine-release); \
            echo "switching to a fallback mirror for alpine v$v"; \
            printf 'https://mirror.leaseweb.com/alpine/v%s/main\nhttps://mirror.leaseweb.com/alpine/v%s/community\n' "$v" "$v" \
                > /etc/apk/repositories; \
        fi; \
        sleep $((attempt * 3)); \
    done; \
    echo "apk failed after 5 attempts"; exit 1

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
# benches/ must exist before anything runs cargo: Cargo.toml declares a
# [[bench]] target, and cargo refuses to even PARSE the manifest if the
# file is missing. Leaving it out failed the build outright - and worse,
# it silently defeated the dependency cache below, whose errors are
# swallowed, so every build recompiled every dependency from scratch.
COPY benches ./benches
# Warm the dependency cache with a stub so the real build is fast.
RUN mkdir -p src && \
    printf 'fn main() {}\n' > src/main.rs && \
    printf 'pub fn _stub() {}\n' > src/lib.rs && \
    cargo build --release 2>/dev/null; true
# Copy the real sources.
COPY src ./src
COPY examples ./examples
# Docker COPY preserves the mtimes from the build context, and those can
# be OLDER than the stub written above. Cargo decides freshness by
# mtime, concludes nothing changed, and ships the 531 KB stub binary -
# a node that starts, prints nothing and exits 0. Touching the sources
# forces the real build.
RUN find src examples benches -name '*.rs' -exec touch {} + && \
    cargo build --release && \
    # Fail loudly here rather than shipping an empty binary again.
    # GAP_STORAGE is an env-var name read by main.rs, so it is present
    # verbatim in any real build and absent from a stub.
    grep -q "GAP_STORAGE" target/release/gap

# Stage 2: minimal runtime image.
#
# Installs nothing. The previous "apk add ca-certificates tzdata" was
# both a deploy-time liability and unnecessary:
#   - TLS roots are compiled into the binary (ureq uses rustls with
#     webpki-roots), so outbound calls to webhook targets and to the
#     verifier API do not read the system trust store.
#   - tzdata was never used: the node stores and compares Unix
#     timestamps only, never local time.
#   - the healthcheck uses busybox wget, already in the base image.
# With no package manager call, a flaky mirror can no longer break a
# deploy at the last step.
FROM alpine:3.20

WORKDIR /app
COPY --from=builder /build/target/release/gap /usr/local/bin/gap-node

# Data directory for the SQLite database (default storage). Created,
# not declared as a VOLUME: compose bind-mounts ./data here, and a
# VOLUME line makes that mount anonymous on some runtimes.
RUN mkdir -p /data
ENV GAP_ADDR=0.0.0.0:8080 \
    GAP_STORAGE=sqlite \
    GAP_SQLITE_PATH=/data/gap-node.db

EXPOSE 8080

# Hydrating a production ClickHouse projection can take around a minute once
# the event spine contains several million rows. During that bounded bootstrap
# Docker must keep the container in `starting`, not prematurely label it
# `unhealthy`. A 10s interval also notices readiness much sooner than the old
# 30s cadence once the listener is finally open.
HEALTHCHECK --interval=10s --timeout=3s --start-period=120s --retries=6 \
    CMD wget -qO- http://127.0.0.1:8080/health || exit 1

ENTRYPOINT ["gap-node"]
