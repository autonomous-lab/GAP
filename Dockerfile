# GAP node — multi-stage build
#
# Stage 1: compile the Rust binary (musl for static linking).
FROM rust:1.97-alpine AS builder

RUN apk add --no-cache musl-dev pkgconfig

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
# Warm the dependency cache with a stub so the real build is fast.
RUN mkdir -p src && \
    printf 'fn main() {}\n' > src/main.rs && \
    printf 'pub fn _stub() {}\n' > src/lib.rs && \
    cargo build --release 2>/dev/null; true
# Copy the real sources (Cargo rebuilds what changed by content hash).
COPY src ./src
COPY examples ./examples
RUN cargo build --release

# Stage 2: minimal runtime image.
FROM alpine:3.20

RUN apk add --no-cache ca-certificates tzdata

WORKDIR /app
COPY --from=builder /build/target/release/gap /usr/local/bin/gap-node

# Data volume for the SQLite database (default storage).
VOLUME ["/data"]
ENV GAP_ADDR=0.0.0.0:8080 \
    GAP_STORAGE=sqlite \
    GAP_SQLITE_PATH=/data/gap-node.db

EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=3s --retries=3 \
    CMD wget -qO- http://127.0.0.1:8080/health || exit 1

ENTRYPOINT ["gap-node"]
