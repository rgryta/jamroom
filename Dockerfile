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

# The files the runtime image needs besides the program: certificates, the C library and zlib the
# standalone yt-dlp is linked against, and an empty /tmp.
FROM alpine:3.22 AS rootfs
RUN apk add --no-cache ca-certificates \
 && mkdir -p /rootfs/etc/ssl/certs /rootfs/usr/local/bin /rootfs/lib /rootfs/tmp \
 && chmod 1777 /rootfs/tmp \
 && cp /etc/ssl/certs/ca-certificates.crt /rootfs/etc/ssl/certs/ \
 && cp -a /lib/ld-musl-*.so.1 /lib/libc.musl-*.so.1 /usr/lib/libz.so.1* /rootfs/lib/ \
 && wget -qO /rootfs/usr/local/bin/yt-dlp https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_musllinux \
 && chmod 755 /rootfs/usr/local/bin/yt-dlp

# Runtime: nothing but the program, yt-dlp (standalone, python bundled) and a static ffmpeg, which
# repairs the DASH audio yt-dlp downloads and measures tempo. No shell, no package manager.
FROM scratch
COPY --from=rootfs /rootfs/ /
COPY --from=mwader/static-ffmpeg:7.1.1 /ffmpeg /usr/local/bin/ffmpeg
COPY --from=test /tested /usr/local/share/tested
COPY --from=build /src/target/release/jamroom /usr/local/bin/jamroom

# 3001:3000 owns the music library on the NAS; compose can override this.
USER 3001:3000
# yt-dlp copies itself into the data volume so it can update itself there; an old script copy
# from the python image is left alone. JAM_PYTHON is empty because yt-dlp runs by itself.
ENV PATH=/usr/local/bin JAM_DATA=/data JAM_LIBRARY=/music JAM_PYTHON= JAM_YTDLP=/data/bin/yt-dlp-standalone
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s CMD ["/usr/local/bin/jamroom", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/jamroom"]
