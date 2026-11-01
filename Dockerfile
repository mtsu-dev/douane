# syntax=docker/dockerfile:1
# cargo-chef builds the dependencies in their own layer, so a source change
# reuses them from the build cache instead of compiling every crate again.
FROM lukemathwalker/cargo-chef:0.1.78-rust-1.99.1-slim-trixie@sha256:b22df2bf8abec048f46a6505c9303e2c8c0a8e22d21181f8b803b9ad5593ba99 AS chef
WORKDIR /src

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY crates crates
# The dashboard is compiled in the repository and embedded by the build.
COPY dash/build dash/build
RUN cargo build --release --locked --bin douane --bin douanectl \
    && mkdir -p /out/data \
    && cp target/release/douane target/release/douanectl /out/

# glibc, libgcc and CA certificates, no shell, uid 65532.
FROM gcr.io/distroless/cc-debian13:nonroot@sha256:b7e81cb1af38d9345680e68bcf564d388f96304e25222030fc8c051faf8565b3
COPY --from=build /out/douane /out/douanectl /usr/local/bin/
COPY --from=build --chown=65532:65532 /out/data /data
COPY LICENSE /usr/share/doc/douane/LICENSE
LABEL org.opencontainers.image.title="douane" \
      org.opencontainers.image.description="One self-hosted endpoint for every AI subscription you pay for" \
      org.opencontainers.image.source="https://github.com/mtsu-dev/douane" \
      org.opencontainers.image.licenses="AGPL-3.0-or-later"
WORKDIR /data
EXPOSE 8787
HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD ["/usr/local/bin/douanectl", "health"]
ENTRYPOINT ["/usr/local/bin/douane"]
CMD ["--config", "/data/douane.toml", "--listen", "0.0.0.0:8787"]
