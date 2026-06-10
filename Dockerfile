# syntax=docker/dockerfile:1
# Cross-compiles serial2moon for the requested target arch. The builder stage is pinned
# to the native build platform (so cargo runs natively, not under emulation) and emits a
# binary for the target triple; the runtime stage is the target arch. Builder and runtime
# share Debian bookworm's glibc, so there's no version mismatch. Used by buildx for
# multi-arch CI images and by `docker compose build` locally (native).
FROM --platform=$BUILDPLATFORM rust:1-slim-bookworm AS build
WORKDIR /app
ARG TARGETPLATFORM
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY docker/build.sh /usr/local/bin/build.sh
RUN bash /usr/local/bin/build.sh "$TARGETPLATFORM"

FROM debian:bookworm-slim
COPY --from=build /serial2moon /usr/local/bin/serial2moon
ENTRYPOINT ["serial2moon"]
