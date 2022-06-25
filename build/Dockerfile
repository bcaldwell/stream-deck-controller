FROM python:alpine
ARG TARGETPLATFORM

RUN apk update && apk add -U --no-cache build-base ca-certificates libffi-dev && \
    pip install pyatv

WORKDIR /stream-deck-controller

COPY ${TARGETPLATFORM} /stream-deck-controller/

ENTRYPOINT [ "/stream-deck-controller/server" ]