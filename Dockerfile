# Multi-stage build: compile → minimal runtime image
# HTTP/SSE mode: docker run -p 3000:3000 ghcr.io/promptexecution/rust-docs-mcp-b00t:latest
# stdio mode: docker run -i ghcr.io/promptexecution/rust-docs-mcp-b00t:latest stdio

FROM rust:1.87-slim AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev perl make \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Cache deps layer
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main(){}' > src/main.rs \
    && cargo build --release \
    && rm src/main.rs

COPY src ./src
RUN touch src/main.rs && cargo build --release

FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/rustdocs_mcp_server /usr/local/bin/rustdocs_mcp_server

EXPOSE 3000

ENV RUST_LOG=info
ENV PORT=3000
ENV HOST=0.0.0.0

# Default: HTTP/SSE server mode (K8s / b00t-mcp namespace)
# Override CMD for stdio mode: docker run -i ... stdio
ENTRYPOINT ["/usr/local/bin/rustdocs_mcp_server"]
CMD ["--http", "--port", "3000", "--host", "0.0.0.0"]
