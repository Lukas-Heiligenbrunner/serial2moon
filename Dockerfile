# Build serial2moon and ship a slim runtime image (used by `docker compose build` for
# local/dev use). CI uses Dockerfile.pack with pre-cross-compiled binaries instead.
FROM rust:1.95-slim AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release && strip target/release/serial2moon

FROM debian:bookworm-slim
COPY --from=build /app/target/release/serial2moon /usr/local/bin/serial2moon
ENTRYPOINT ["serial2moon"]
