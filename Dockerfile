FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p jarvis

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home /data jarvis \
    && mkdir -p /data /config && chown jarvis /data
COPY --from=build /src/target/release/jarvis /usr/local/bin/jarvis
USER jarvis
VOLUME /data
EXPOSE 8484
ENTRYPOINT ["jarvis", "serve", "--config", "/config/jarvis.toml"]
