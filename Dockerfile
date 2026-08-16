# Build the frontend and the Linux Tauri executable in one reproducible stage.
ARG NODE_IMAGE=node:22-bookworm
ARG RUNTIME_IMAGE=debian:bookworm-slim
ARG DEBIAN_MIRROR=http://deb.debian.org/debian
ARG DEBIAN_SECURITY_MIRROR=http://deb.debian.org/debian-security

FROM ${NODE_IMAGE} AS builder

ARG DEBIAN_MIRROR
ARG DEBIAN_SECURITY_MIRROR

ENV DEBIAN_FRONTEND=noninteractive \
    RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH

RUN sed -i "s|http://deb.debian.org/debian-security|${DEBIAN_SECURITY_MIRROR}|g; s|http://deb.debian.org/debian|${DEBIAN_MIRROR}|g" /etc/apt/sources.list.d/debian.sources \
    && apt-get -o Acquire::Retries=5 -o Acquire::http::Timeout=30 update \
    && apt-get -o Acquire::Retries=5 -o Acquire::http::Timeout=30 install -y --no-install-recommends \
        build-essential \
        curl \
        file \
        libayatana-appindicator3-dev \
        libgtk-3-dev \
        libssl-dev \
        libwebkit2gtk-4.1-dev \
        librsvg2-dev \
        patchelf \
        pkg-config \
        xdg-utils \
    && rm -rf /var/lib/apt/lists/* \
    && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --no-modify-path

WORKDIR /app
RUN corepack enable

COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
RUN pnpm install --frozen-lockfile

COPY . .
RUN pnpm tauri build --no-bundle

# Keep only the executable and the runtime assets out of the builder image.
FROM ${RUNTIME_IMAGE} AS runtime

ARG DEBIAN_MIRROR
ARG DEBIAN_SECURITY_MIRROR

ENV DEBIAN_FRONTEND=noninteractive \
    WALIAPI_SERVER_HOST=0.0.0.0 \
    WALIAPI_SERVER_PORT=8777 \
    WALIAPI_ENABLE_UI=0 \
    WALIAPI_HIDE_WINDOW=1 \
    XDG_DATA_HOME=/data \
    DISPLAY=:99 \
    XDG_RUNTIME_DIR=/tmp/runtime-waliapi \
    GDK_BACKEND=x11 \
    NO_AT_BRIDGE=1 \
    WEBKIT_DISABLE_COMPOSITING_MODE=1 \
    WEBKIT_DISABLE_DMABUF_RENDERER=1 \
    LIBGL_ALWAYS_SOFTWARE=1 \
    LANG=C.UTF-8 \
    LC_ALL=C.UTF-8

RUN sed -i "s|http://deb.debian.org/debian-security|${DEBIAN_SECURITY_MIRROR}|g; s|http://deb.debian.org/debian|${DEBIAN_MIRROR}|g" /etc/apt/sources.list.d/debian.sources \
    && apt-get -o Acquire::Retries=5 -o Acquire::http::Timeout=30 update \
    && apt-get -o Acquire::Retries=5 -o Acquire::http::Timeout=30 install -y --no-install-recommends \
        ca-certificates \
        curl \
        dbus-x11 \
        fonts-noto-cjk \
        libayatana-appindicator3-1 \
        libgtk-3-0 \
        libssl3 \
        libwebkit2gtk-4.1-0 \
        librsvg2-2 \
        fluxbox \
        novnc \
        websockify \
        x11vnc \
        xauth \
        xvfb \
    && rm -rf /var/lib/apt/lists/* \
    && fc-cache -f \
    && useradd --create-home --uid 10001 waliapi \
    && mkdir -p /data \
    && chown -R waliapi:waliapi /data

COPY --from=builder /app/src-tauri/target/release/waliapi /usr/local/bin/waliapi
COPY docker/entrypoint.sh /usr/local/bin/waliapi-entrypoint
RUN chmod +x /usr/local/bin/waliapi-entrypoint

USER waliapi
WORKDIR /home/waliapi
VOLUME ["/data"]
EXPOSE 8777 5900 6080

HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD curl --fail --silent "http://127.0.0.1:${WALIAPI_SERVER_PORT}/health" || exit 1

ENTRYPOINT ["/usr/local/bin/waliapi-entrypoint"]
CMD ["/usr/local/bin/waliapi"]
