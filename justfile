# rust-docs-mcp-server — b00t variant
# Forked from Govcraft/rust-docs-mcp-server
# Compound engineering: plan → work → review → compound
#
# Usage:
#   just build              Build release binary
#   just run                Run in stdio mode (default)
#   just run-http           Run in HTTP/SSE mode (agentic rendering)
#   just test               Run tests
#   just docker-build       Build Docker image
#   just bump               Auto-bump version (Cocogitto)

set shell := ["bash", "-cu"]

# ── Build ──────────────────────────────────────────────────────────────────

build:
    cargo build --release

# ── Run ────────────────────────────────────────────────────────────────────

# Stdio mode (default, for IDE MCP integration)
run crate="serde@^1.0":
    cargo run --release -- "{{crate}}"

# HTTP/SSE mode (b00t agentic rendering)
run-http crate="serde@^1.0" port="3000":
    cargo run --release -- "{{crate}}" --http --port {{port}}

# ── Install ────────────────────────────────────────────────────────────────

install: build
    cp target/release/rustdocs_mcp_server ~/.local/bin/

# ── Test ───────────────────────────────────────────────────────────────────

test:
    cargo test

# ── Clean ──────────────────────────────────────────────────────────────────

clean:
    cargo clean

# ── Docker ─────────────────────────────────────────────────────────────────

docker-build:
    docker build -t promptexecution/rust-docs-mcp-server:latest .

docker-run crate="serde@^1.0":
    docker run -i --rm -e OPENAI_API_KEY promptexecution/rust-docs-mcp-server:latest "{{crate}}"

# ── wrkflw — local CI gate ─────────────────────────────────────────────────
# Run before push to catch compile errors without burning cloud CI minutes.
# Requires: cargo install wrkflw

# Validate workflow YAML schema (instantaneous)
ci-validate:
    wrkflw validate .github/workflows/docker.yml

# Run local-build job locally via emulation (no containers)
ci-local:
    wrkflw run --job local-build --runtime emulation .github/workflows/docker.yml

# Watch: auto-rerun ci-local on file changes (dev loop)
ci-watch:
    wrkflw watch --job local-build .github/workflows/docker.yml

# ── Cocogitto ──────────────────────────────────────────────────────────────

bump:
    cog bump --auto

changelog:
    cog changelog

version:
    @grep '^version' Cargo.toml | head -1
