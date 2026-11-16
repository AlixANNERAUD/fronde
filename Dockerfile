# syntax=docker/dockerfile:1.7

# The build uses the toolchain pinned in mise.toml (Rust, the Dioxus CLI, Tailwind), like CI.
FROM debian:trixie-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2 AS builder

# Updating mise means updating the checksums below (SHASUMS256.txt of its release).
ARG MISE_VERSION=2026.10.18
ARG TARGETARCH

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates curl git build-essential pkg-config \
    && rm -rf /var/lib/apt/lists/*

# The mise binary is checked against the release's checksums before it runs.
RUN case "$TARGETARCH" in \
        amd64) arch=x64; sha256=e97c38973336aa2ff350fea50f47b412b266fe263b102b7200ff1e9b118c6942 ;; \
        arm64) arch=arm64; sha256=3a3c72376687ba21a588d0f5a2c88c76f483fb6cdece7b0fc43d5e3132b08a52 ;; \
        *) echo "unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && curl -fsSL --retry 5 --retry-all-errors \
        "https://github.com/jdx/mise/releases/download/v${MISE_VERSION}/mise-v${MISE_VERSION}-linux-${arch}" \
        -o /usr/local/bin/mise \
    && echo "$sha256  /usr/local/bin/mise" | sha256sum --check --strict \
    && chmod +x /usr/local/bin/mise

ENV MISE_ENV=ci \
    MISE_DATA_DIR=/opt/mise \
    MISE_YES=1 \
    MISE_HTTP_RETRIES=5 \
    PATH=/opt/mise/shims:$PATH \
    CARGO_HOME=/opt/cargo \
    CARGO_TERM_COLOR=never \
    CARGO_INCREMENTAL=0 \
    DX_HOME=/opt/dx

WORKDIR /src

# Tools first, so they are cached until mise.toml changes.
COPY mise.toml mise.ci.toml ./
RUN mise trust --all && mise install

# dx downloads the tools it builds the web interface with (wasm-bindgen, wasm-opt, esbuild) into
# DX_HOME, cached like Cargo's downloads.
COPY . .
RUN --mount=type=cache,target=/opt/cargo/registry \
    --mount=type=cache,target=/opt/cargo/git \
    --mount=type=cache,target=/opt/dx,id=fronde-dx-${TARGETARCH} \
    --mount=type=cache,target=/src/target,id=fronde-target-${TARGETARCH} \
    --mount=type=cache,target=/src/components/echo/target,id=fronde-echo-${TARGETARCH} \
    --mount=type=cache,target=/src/components/calendar/target,id=fronde-calendar-${TARGETARCH} \
    mise run build \
    && mise run components \
    && mkdir -p /out/components \
    && cp target/release/fronde /out/fronde \
    && cp -r target/dx/fronde-web/release/web/public /out/site \
    && cp components/dist/calendar.wasm components/dist/calendar.toml /out/components/

# The Claude ACP adapter needs Node.js 22 or later, newer than Debian's. It brings its own Claude
# Code.
FROM node:22-trixie-slim@sha256:68c76cfad89921b927eae28ba21407475e29d0ce450afcdbe70c1532db042b64 AS agent

# The adapter and its dependencies are locked in agent/package-lock.json, as for `mise run agent`.
WORKDIR /opt/agent
COPY agent/package.json agent/package-lock.json ./
RUN npm ci --omit=dev

FROM debian:trixie-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2

LABEL org.opencontainers.image.title="fronde" \
      org.opencontainers.image.description="A personal agent: a web chat with Claude, and memory, calendar and tools over MCP." \
      org.opencontainers.image.source="https://github.com/AlixANNERAUD/fronde"

# git versions the memory; the agent reaches Claude over https. tini runs as PID 1: it passes
# docker stop's SIGTERM on to Fronde, which a PID 1 without a handler ignores, and reaps the
# agent's processes left behind when an agent is stopped.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git tini \
    && rm -rf /var/lib/apt/lists/*

COPY --from=agent /usr/local/bin/node /usr/local/bin/node
COPY --from=agent /opt/agent /opt/agent
COPY --from=builder /out/fronde /app/fronde
COPY --from=builder /out/site /app/site
# Tool components, each with its manifest beside it; a [[sources]] entry enables one.
COPY --from=builder /out/components /app/components

# HOME is on the volume so the agent's Claude sign-in and settings (~/.claude) survive restarts.
ENV PATH=/opt/agent/node_modules/.bin:$PATH \
    FRONDE_SITE_ROOT=/app/site \
    HOME=/data/home

# The data directory must exist in the image, owned by Fronde's user; a new named volume mounted
# on it takes this ownership.
RUN mkdir -p /data/home /config && chown -R 65532:65532 /data

USER 65532:65532
VOLUME /data
EXPOSE 8484

HEALTHCHECK --interval=30s --timeout=6s --start-period=20s --retries=3 \
    CMD ["/app/fronde", "healthcheck", "--config", "/config/fronde.toml"]

ENTRYPOINT ["tini", "--", "/app/fronde"]
CMD ["serve", "--config", "/config/fronde.toml"]
