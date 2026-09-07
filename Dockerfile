# syntax=docker/dockerfile:1
FROM debian:bookworm-slim AS ffmpeg-build
ARG FFMPEG_VERSION=8.1.2
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential ca-certificates curl xz-utils nasm pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
# Build with OpenSSL explicitly: DSI needs OPENSSL_CONF / SECLEVEL=1.
RUN curl --fail --show-error --location --retry 3 \
      "https://ffmpeg.org/releases/ffmpeg-${FFMPEG_VERSION}.tar.xz" -o ffmpeg.tar.xz \
    && tar -xf ffmpeg.tar.xz --strip-components=1 \
    && ./configure --prefix=/opt/ffmpeg --disable-autodetect \
      --enable-openssl --enable-version3 --enable-shared --disable-static \
      --disable-doc --disable-debug --disable-ffplay \
    && make -j"$(nproc)" && make install

FROM rust:1.92.0-bookworm AS app-build
RUN apt-get update && apt-get install -y --no-install-recommends \
    clang libclang-dev pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
COPY --from=ffmpeg-build /opt/ffmpeg /opt/ffmpeg
ENV PKG_CONFIG_PATH=/opt/ffmpeg/lib/pkgconfig
ENV LD_LIBRARY_PATH=/opt/ffmpeg/lib
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY static ./static
COPY config ./config
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl libssl3 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=ffmpeg-build /opt/ffmpeg/bin /opt/ffmpeg/bin
COPY --from=ffmpeg-build /opt/ffmpeg/lib /opt/ffmpeg/lib
COPY --from=app-build /build/target/release/ferrumview /usr/local/bin/ferrumview
ENV PATH=/opt/ffmpeg/bin:$PATH
ENV LD_LIBRARY_PATH=/opt/ffmpeg/lib
ENV FERRUMVIEW_BIND=0.0.0.0:6523
WORKDIR /app
EXPOSE 6523
# The application's existing Ctrl+C handler also cleans up child processes.
STOPSIGNAL SIGINT
HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD curl --fail --silent http://127.0.0.1:6523/api/config > /dev/null || exit 1
ENTRYPOINT ["ferrumview"]
