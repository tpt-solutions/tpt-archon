FROM rust:slim AS builder
WORKDIR /app
COPY . .
RUN cargo build --release -p out-archon-sql --bin archon-sql

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends libssl3 && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/archon-sql /usr/local/bin/
ENTRYPOINT ["archon-sql"]
