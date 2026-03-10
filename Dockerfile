# syntax=docker/dockerfile:1

FROM rust:1.85-bookworm

RUN apt-get update && apt-get install -y \
    vim \
    curl \
    git \
    && rm -rf /var/lib/apt/lists/*

RUN cargo install cargo-watch --locked

WORKDIR /app

# Copy only Cargo.toml first
COPY Cargo.toml .

# Create dummy src files so cargo fetch can parse the manifest
# Real source is copied below — this layer is just for dep caching
RUN mkdir -p src && \
    echo "fn main() {}" > src/main.rs && \
    echo "" > src/lib.rs

# Fetch dependencies — this layer is cached unless Cargo.toml changes
RUN cargo fetch

# Now copy the real source code
COPY src ./src

CMD ["cargo", "test"]