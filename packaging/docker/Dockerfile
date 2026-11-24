# syntax=docker/dockerfile:1.29.0@sha256:572cebbc2b15e6f84c077742952674889cb63cd5e9b80abdffcbe587b6f19183

# Trackside container images. Build from the repository root:
#
#   docker buildx build -f packaging/docker/Dockerfile --target standard .
#
# Targets:
#   slim      static binary on distroless static, no ffmpeg (direct play only)
#   standard  slim plus a static ffmpeg build; the default and the `latest` tag
#   acquire   distroless cc (glibc) plus ffmpeg, yt-dlp and Deno; the `-acquire` tags
#
# Every base image and download is pinned by version and digest or checksum. To update one,
# change the tag and the digest together; docs/ops/releasing.md has the commands.

ARG RUST_VERSION=1.100.0
ARG ZIGBUILD_IMAGE=ghcr.io/rust-cross/cargo-zigbuild:0.24.1@sha256:3abe26afb5537ea225c223c6790c1bf056271ef51ea52d2c4e343c569c8f129a
ARG ALPINE_IMAGE=docker.io/library/alpine:3.24.4@sha256:f59abe656b692d901e184be3445e084786602108599d2a51d229c0f5a48c71bf
ARG STATIC_BASE=gcr.io/distroless/static-debian13:nonroot@sha256:8ce6dd87e9823c5b48dcfb2c8604714ccb351ce59607327bcc4957448da279b5
# Debian 13 distroless cc ships zlib1g, which the yt-dlp build links; debian12 did not.
ARG CC_BASE=gcr.io/distroless/cc-debian13:nonroot@sha256:532c7a94f9e96ba9db75eb9388f4227751131c7cb0a411d1a8521af4096135de
# ffmpeg 9.1, static musl build from github.com/wader/static-ffmpeg. GPL v3 build with no
# nonfree libraries (no libfdk_aac); Trackside's AAC profiles use ffmpeg's native encoder.
ARG FFMPEG_IMAGE=docker.io/mwader/static-ffmpeg:9.1@sha256:dd09509d8c2152bfa601c8c20e3dd166bde906d37951b9f7e8bbd922267f5ac9
ARG DENO_IMAGE=docker.io/denoland/deno:bin-2.9.8@sha256:b507583d3c68ba69f3ed2ce5969ae5b8b70f43421c781279c7aea06435263543
ARG YTDLP_VERSION=2026.10.26
ARG YTDLP_SHA256_AMD64=f34053415136e55073dd64ba5326d161ecc23ce0e98c081d8752867b37809450
ARG YTDLP_SHA256_ARM64=74708e707e8fa8177c8347e58152337edf320149cd7b225defd453f7cde47822

ARG VERSION=dev
ARG REVISION=unknown

# ---------------------------------------------------------------------------------------
# Build: cross-compiles on the build machine's own architecture, so arm64 images need no
# emulation. cargo-zigbuild links musl statically with zig as the C toolchain (SQLite and
# ring have C parts).

FROM --platform=$BUILDPLATFORM ${ZIGBUILD_IMAGE} AS build
ARG RUST_VERSION
ARG TARGETARCH
ENV RUSTUP_TOOLCHAIN=${RUST_VERSION} \
    CARGO_TERM_COLOR=never \
    CARGO_INCREMENTAL=0
RUN rustup toolchain install "${RUST_VERSION}" --profile minimal \
      --target x86_64-unknown-linux-musl,aarch64-unknown-linux-musl
WORKDIR /src
COPY . .
RUN --mount=type=cache,id=trackside-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=trackside-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=trackside-target-${TARGETARCH},target=/src/target \
    set -eu; \
    case "${TARGETARCH}" in \
      amd64) target=x86_64-unknown-linux-musl ;; \
      arm64) target=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported architecture: ${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    cargo zigbuild --locked --release -p trackside-server --bin trackside --target "${target}"; \
    install -D -m 0755 "target/${target}/release/trackside" /out/usr/local/bin/trackside

# ---------------------------------------------------------------------------------------
# Filesystem skeleton. Distroless has no shell, so the directories are made here. /data is
# owned by the image user so a new named volume starts out writable. Every RUN step runs on
# the build platform, so a multi-arch build needs no emulation.

FROM --platform=$BUILDPLATFORM ${ALPINE_IMAGE} AS skeleton
RUN mkdir -p /out/data /out/music \
    && chown 65532:65532 /out/data \
    && chmod 0750 /out/data

# ---------------------------------------------------------------------------------------
# yt-dlp, the official release in its one-folder form. The one-file binary unpacks itself
# into /tmp on every run, which fails on a read-only root filesystem and costs a second per
# call; the one-folder build runs in place.

FROM --platform=$BUILDPLATFORM ${ALPINE_IMAGE} AS ytdlp-amd64
ARG YTDLP_VERSION
ARG YTDLP_SHA256_AMD64
ADD --checksum=sha256:${YTDLP_SHA256_AMD64} \
    https://github.com/yt-dlp/yt-dlp/releases/download/${YTDLP_VERSION}/yt-dlp_linux.zip /tmp/yt-dlp.zip

FROM --platform=$BUILDPLATFORM ${ALPINE_IMAGE} AS ytdlp-arm64
ARG YTDLP_VERSION
ARG YTDLP_SHA256_ARM64
ADD --checksum=sha256:${YTDLP_SHA256_ARM64} \
    https://github.com/yt-dlp/yt-dlp/releases/download/${YTDLP_VERSION}/yt-dlp_linux_aarch64.zip /tmp/yt-dlp.zip

FROM ytdlp-${TARGETARCH} AS ytdlp
RUN mkdir -p /out/opt/yt-dlp \
    && unzip -q /tmp/yt-dlp.zip -d /out/opt/yt-dlp \
    && mv /out/opt/yt-dlp/yt-dlp_linux* /out/opt/yt-dlp/yt-dlp \
    && chmod 0755 /out/opt/yt-dlp/yt-dlp

FROM ${FFMPEG_IMAGE} AS ffmpeg
FROM ${DENO_IMAGE} AS deno

# ---------------------------------------------------------------------------------------
# Shared runtime settings are repeated in each target because Dockerfile stages cannot
# inherit from a stage built on a different base.

FROM ${STATIC_BASE} AS slim
ARG VERSION
ARG REVISION
COPY --from=skeleton --chown=65532:65532 /out/data /data
COPY --from=skeleton /out/music /music
COPY --from=build /out/usr/local/bin/trackside /usr/local/bin/trackside
ENV TRACKSIDE_DATA_DIR=/data \
    TRACKSIDE_MUSIC_DIR=/music
USER 65532:65532
VOLUME /data
EXPOSE 4747
HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
  CMD ["/usr/local/bin/trackside", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/trackside"]
CMD ["serve"]
LABEL org.opencontainers.image.title="Trackside" \
      org.opencontainers.image.description="Self-hosted music server for the Tracks app (slim: no ffmpeg)" \
      org.opencontainers.image.source="https://github.com/slukedev/tracks-companion" \
      org.opencontainers.image.url="https://github.com/slukedev/tracks-companion" \
      org.opencontainers.image.documentation="https://github.com/slukedev/tracks-companion/blob/main/README.md" \
      org.opencontainers.image.licenses="GPL-3.0-only" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}"

FROM ${CC_BASE} AS acquire
ARG VERSION
ARG REVISION
COPY --from=skeleton --chown=65532:65532 /out/data /data
COPY --from=skeleton /out/music /music
COPY --from=ffmpeg /ffmpeg /ffprobe /usr/local/bin/
COPY --from=deno /deno /usr/local/bin/deno
COPY --from=ytdlp /out/opt/yt-dlp /opt/yt-dlp
COPY --from=build /out/usr/local/bin/trackside /usr/local/bin/trackside
ENV TRACKSIDE_DATA_DIR=/data \
    TRACKSIDE_MUSIC_DIR=/music \
    PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/opt/yt-dlp \
    DENO_NO_UPDATE_CHECK=1 \
    DENO_DIR=/data/cache/deno
USER 65532:65532
VOLUME /data
EXPOSE 4747
HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
  CMD ["/usr/local/bin/trackside", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/trackside"]
CMD ["serve"]
LABEL org.opencontainers.image.title="Trackside" \
      org.opencontainers.image.description="Self-hosted music server for the Tracks app (with ffmpeg, yt-dlp and Deno)" \
      org.opencontainers.image.source="https://github.com/slukedev/tracks-companion" \
      org.opencontainers.image.url="https://github.com/slukedev/tracks-companion" \
      org.opencontainers.image.documentation="https://github.com/slukedev/tracks-companion/blob/main/README.md" \
      org.opencontainers.image.licenses="GPL-3.0-only" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}"

# Last, so a plain `docker build` produces it.
FROM ${STATIC_BASE} AS standard
ARG VERSION
ARG REVISION
COPY --from=skeleton --chown=65532:65532 /out/data /data
COPY --from=skeleton /out/music /music
COPY --from=ffmpeg /ffmpeg /ffprobe /usr/local/bin/
COPY --from=build /out/usr/local/bin/trackside /usr/local/bin/trackside
ENV TRACKSIDE_DATA_DIR=/data \
    TRACKSIDE_MUSIC_DIR=/music
USER 65532:65532
VOLUME /data
EXPOSE 4747
HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
  CMD ["/usr/local/bin/trackside", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/trackside"]
CMD ["serve"]
LABEL org.opencontainers.image.title="Trackside" \
      org.opencontainers.image.description="Self-hosted music server for the Tracks app" \
      org.opencontainers.image.source="https://github.com/slukedev/tracks-companion" \
      org.opencontainers.image.url="https://github.com/slukedev/tracks-companion" \
      org.opencontainers.image.documentation="https://github.com/slukedev/tracks-companion/blob/main/README.md" \
      org.opencontainers.image.licenses="GPL-3.0-only" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}"
