# Server image built from prebuilt static musl binaries (no Rust compile in Docker).
# Expects dist/linux/amd64/sdc-server and dist/linux/arm64/sdc-server
# (CI and `task docker` create them).
FROM gcr.io/distroless/static:nonroot
ARG TARGETARCH
COPY dist/linux/${TARGETARCH}/sdc-server /usr/local/bin/sdc-server
ENV SDC_CONFIG=/config/config.lua \
    SDC_LISTEN=0.0.0.0:7700
EXPOSE 7700
USER nonroot
ENTRYPOINT ["/usr/local/bin/sdc-server"]
