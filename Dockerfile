# Build stage: static-ish musl binary. cmake/perl are needed by the TLS crypto library.
FROM rust:1-alpine AS build
RUN apk add --no-cache build-base cmake perl
WORKDIR /src

# Compile the dependencies first so source edits do not rebuild them.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
 && cargo build --release \
 && rm -rf src target/release/jamroom target/release/deps/jamroom*

COPY src ./src
COPY web ./web
RUN cargo build --release

# Runtime: yt-dlp needs python, and ffmpeg repairs the DASH audio it downloads.
FROM alpine:3.22
RUN apk add --no-cache python3 ffmpeg ca-certificates tzdata \
 && wget -qO /usr/local/bin/yt-dlp https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp \
 && chmod 755 /usr/local/bin/yt-dlp
COPY --from=build /src/target/release/jamroom /usr/local/bin/jamroom

# 3001:3000 owns the music library on the NAS; compose can override this.
USER 3001:3000
ENV JAM_DATA=/data JAM_LIBRARY=/music
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s CMD wget -qO- http://127.0.0.1:8080/healthz || exit 1
ENTRYPOINT ["/usr/local/bin/jamroom"]
