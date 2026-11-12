# The image is only built if the tests pass: the final stage copies a marker that the test stage
# writes after `cargo test`. Keep the Rust minor version in step with .github/workflows/pr-validation.yml.
# cmake/perl are needed by the TLS crypto library.
FROM rust:1.94-alpine AS deps
RUN apk add --no-cache build-base cmake perl
WORKDIR /src
# Compile the dependencies first so source edits do not rebuild them (release for the binary,
# dev for the tests).
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
 && cargo build --release --locked \
 && cargo test --no-run --locked \
 && rm -rf src target/release/jamroom target/release/deps/jamroom* target/debug/jamroom target/debug/deps/jamroom*

FROM deps AS test
COPY src ./src
COPY web ./web
COPY tests ./tests
RUN cargo test --locked && touch /tested

FROM deps AS build
COPY src ./src
COPY web ./web
RUN cargo build --release --locked

# Runtime: yt-dlp needs python, and ffmpeg repairs the DASH audio it downloads.
FROM alpine:3.22
RUN apk add --no-cache python3 ffmpeg ca-certificates tzdata \
 && wget -qO /usr/local/bin/yt-dlp https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp \
 && chmod 755 /usr/local/bin/yt-dlp
COPY --from=test /tested /usr/local/share/tested
COPY --from=build /src/target/release/jamroom /usr/local/bin/jamroom

# 3001:3000 owns the music library on the NAS; compose can override this.
USER 3001:3000
ENV JAM_DATA=/data JAM_LIBRARY=/music
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s CMD wget -qO- http://127.0.0.1:8080/healthz || exit 1
ENTRYPOINT ["/usr/local/bin/jamroom"]
