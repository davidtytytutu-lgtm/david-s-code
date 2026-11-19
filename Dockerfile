# Build + runtime (garde rustc pour l'exec Rust, + runtimes JS/Lua/Java)
FROM rust:1.78-bookworm AS build
WORKDIR /app
COPY Cargo.toml ./
COPY src ./src
RUN cargo build --release

FROM rust:1.78-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates nodejs lua5.4 default-jdk-headless \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=build /app/target/release/hexforge-server /usr/local/bin/hexforge-server
ENV PORT=10000
EXPOSE 10000
CMD ["hexforge-server"]
