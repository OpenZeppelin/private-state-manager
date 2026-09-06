# Build stage
# For reproducible builds across machines, specify --platform:
#   docker build --platform linux/amd64 ...
FROM rust:1.98.0-bookworm as base-builder

# Install protobuf compiler (pinned to upstream 3.21.12; the Debian
# packaging revision floats so point-release rebuilds don't break the build)
RUN apt-get update && apt-get install -y \
    "protobuf-compiler=3.21.12-*" \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Set environment variables for reproducible builds
ENV SOURCE_DATE_EPOCH=0
ENV RUSTFLAGS="--remap-path-prefix /app=. --remap-path-prefix $HOME=~"
ARG GUARDIAN_SERVER_FEATURES=postgres

# Copy workspace manifests
COPY Cargo.toml Cargo.lock ./
COPY rust-toolchain.toml ./

COPY crates ./crates
COPY benchmarks ./benchmarks
COPY examples ./examples

# Build for release (only server)
FROM base-builder as server-builder

# build.rs reads this to stamp the git commit; the build context has no .git to fall back on.
ARG GUARDIAN_GIT_SHA

RUN if [ -n "$GUARDIAN_SERVER_FEATURES" ]; then \
      cargo build --release --package guardian-server --bin server --features "$GUARDIAN_SERVER_FEATURES"; \
    else \
      cargo build --release --package guardian-server --bin server; \
    fi

FROM base-builder as benchmark-builder

RUN cargo build --release --package guardian-prod-benchmarks --bin guardian-prod-benchmarks

# Runtime stage
FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171 as benchmark-runner

RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=benchmark-builder /app/target/release/guardian-prod-benchmarks /app/guardian-prod-benchmarks
COPY --from=benchmark-builder /app/crates/contracts/masm /app/crates/contracts/masm

ENTRYPOINT ["/app/guardian-prod-benchmarks"]

# Runtime stage
FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171 as server-runner

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libpq5 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy the binary from builder
COPY --from=server-builder /app/target/release/server /app/server

# Expose HTTP and gRPC ports
EXPOSE 3000 50051

CMD ["/app/server"]
