# Multi-stage build: compile → minimal runtime image
#
# Cloud mode (default):  docker run -p 3000:3000 -e OPENAI_API_KEY=... ghcr.io/promptexecution/rust-docs-mcp-b00t:latest
# Single crate (stdio):  docker run -i ghcr.io/promptexecution/rust-docs-mcp-b00t:latest serde
# Single crate (HTTP):   docker run -p 3000:3000 ghcr.io/promptexecution/rust-docs-mcp-b00t:latest serde --http
#
# Cloud mode env vars:
#   OPENAI_API_KEY      Required for generating embeddings (new crates)
#   OPENAI_API_BASE     Optional: custom OpenAI-compatible API base URL
#   EMBEDDING_MODEL     Optional: override embedding model (default: text-embedding-3-small)
#   LLM_MODEL           Optional: override LLM model (default: gpt-4o-mini-2024-07-18)
#   HOST                Bind address (default: 0.0.0.0)
#   PORT                Management API port (default: 3000)
#   CRATE_PORT_START    First ephemeral port for crate SSE servers (default: 3001)
#   CRATE_PRELOAD       Comma-separated crate specs to pre-load on startup
#   PUBLIC_HOST         Public hostname for SSE URLs (default: rust-docs.mcp.b00t.promptexecution.com)

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
ENV PUBLIC_HOST=rust-docs.mcp.b00t.promptexecution.com

# Default: cloud mode (management API + on-demand crate loading)
# Override CMD for single-crate mode: docker run -i ... serde
ENTRYPOINT ["/usr/local/bin/rustdocs_mcp_server"]
CMD ["--cloud", "--port", "3000"]
