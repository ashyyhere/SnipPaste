FROM rust:1-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
COPY static ./static
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libsqlite3-0 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home snippaste \
    && mkdir -p /data && chown snippaste:snippaste /data
WORKDIR /app
COPY --from=builder /app/target/release/snippaste /usr/local/bin/snippaste
ENV HOST=0.0.0.0 PORT=3000 DATABASE_URL=sqlite:///data/snippaste.db
USER snippaste
EXPOSE 3000
VOLUME ["/data"]
ENTRYPOINT ["/usr/local/bin/snippaste"]
