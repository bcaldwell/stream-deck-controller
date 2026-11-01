# Server image built from prebuilt static musl binaries (no Rust compile in Docker).
# Expects dist/linux/amd64/sdc-server and dist/linux/arm64/sdc-server, plus the static
# client binaries dist/clients/sdc-client-{aarch64,x86_64}-linux that the server hands out
# to clients (CI and `task docker` create them). The client binaries are the same in both
# image arches: they are for the machines running the Stream Decks, not for this image.
FROM gcr.io/distroless/static:nonroot
ARG TARGETARCH
COPY dist/linux/${TARGETARCH}/sdc-server /usr/local/bin/sdc-server
COPY dist/clients/ /usr/local/share/sdc/clients/
ENV SDC_CONFIG=/config/config.lua \
    SDC_LISTEN=0.0.0.0:7700 \
    SDC_CLIENT_DIR=/usr/local/share/sdc/clients
EXPOSE 7700
USER nonroot
ENTRYPOINT ["/usr/local/bin/sdc-server"]
