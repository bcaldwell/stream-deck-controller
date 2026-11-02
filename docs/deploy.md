# Deployment

The server is one static binary in a distroless image. The same image carries the static client binaries
(`sdc-client-aarch64-linux`, `sdc-client-x86_64-linux`) that it hands out to decks at `/client/<target>`, so deploying a
new image also updates every Pi.

## Docker image

Images are published by CI to `ghcr.io/bcaldwell/stream-deck-controller`, tagged with the commit SHA (what a deployment
should pin), the branch name, and the version on `v*` tags. To build locally:

```sh
nix develop -c task docker          # static musl binaries via cargo-zigbuild, then docker build -> stream-deck-controller:dev
```

Run it with your config mounted:

```sh
docker run --rm -p 7700:7700 \
  -e HA_URL=https://homeassistant.local:8123 -e HA_TOKEN=... \
  -e SDC_CLIENT_TOKEN=$(openssl rand -hex 32) \
  -v "$PWD:/config:ro" stream-deck-controller:dev
```

The image defaults to `SDC_CONFIG=/config/config.lua`, `SDC_LISTEN=0.0.0.0:7700` and
`SDC_CLIENT_DIR=/usr/local/share/sdc/clients`, runs as a non-root user, and exposes port 7700 for `/ws`, `/plugin/ws`,
`/deck`, `/client/*` and `/healthz`. The `render` subcommand works in the same image
(`docker run ... stream-deck-controller:dev render --config ...`).

## Environment variables

| Variable | Meaning |
|----------|---------|
| `SDC_CONFIG` | Lua config path (default `/config/config.lua` in the image) |
| `HA_URL`, `HA_TOKEN` | Home Assistant base URL and a long-lived access token |
| `SDC_LISTEN` | Listen address (default `0.0.0.0:7700`) |
| `SDC_CLIENT_TOKEN` | Shared secret for `/ws`, `/plugin/ws` and `/client/*`. Unset = open to everyone (warning logged); do not do that behind a public ingress |
| `SDC_DECK_AUTH` | `none` disables the SSO-header check on `/deck` (local development only) |
| `SDC_CLIENT_DIR` | Directory of client binaries to distribute; missing = no self-update |
| `SDC_ASSETS_S3_ENDPOINT`, `_BUCKET`, `_REGION`, `_ACCESS_KEY_ID`, `_SECRET_ACCESS_KEY` | Optional S3 bucket for icons and images |
| `RUST_LOG` | Log filter (`sdc_server=debug` logs render timings) |

The full table is in [crates/server/README.md](../crates/server/README.md#environment). Create the Home Assistant token
under your profile page in Home Assistant ("Long-lived access tokens").

## Kubernetes

`deploy/k8s/` is a kustomization with a namespace, a config map for `config.lua`, a one-replica `Deployment`
(strategy `Recreate`, readiness probe on `/healthz`, small resource limits) and a `Service`. The Service is a
`LoadBalancer` so that clients on the LAN can reach port 7700; use `NodePort` if you have no load balancer.

```sh
kubectl apply -f deploy/k8s/namespace.yaml
kubectl -n stream-deck create secret generic sdc-secrets \
  --from-literal=HA_TOKEN=<long-lived-token> \
  --from-literal=SDC_CLIENT_TOKEN=$(openssl rand -hex 32)
kubectl -n stream-deck create configmap sdc-config --from-file=config.lua=config.lua   # your config
kubectl apply -k deploy/k8s
```

Edit `HA_URL` in `deployment.yaml` first. Keep the config map out of the kustomization if you generate it from your
real file (see the comment in `configmap.yaml`). The S3 variables and secret keys are optional; remove them if you do
not use a bucket. The secret is created out of band; `secret.example.yaml` only shows its shape.

Saving a changed config map updates the mounted file and the server hot-reloads it, so layout changes do not need a
restart.

For the `/deck` page, put an SSO proxy in front and route only `/deck` through it, see
[Browser deck](browser.md#authentication). Pis and the plugin connect to `/ws` and `/plugin/ws` with the bearer token
and do not go through the SSO.

## CI and automatic deploys

`.github/workflows/ci.yml` on every push:

1. **check**: `cargo fmt`, `clippy -D warnings`, tests.
2. **build**: static musl server and client binaries for aarch64 and x86_64 with cargo-zigbuild.
3. **elgato plugin**: builds and packs the plugin (an artifact of every run).
4. **docker image**: assembles the multi-arch image from the prebuilt binaries (no Rust compile inside Docker, no QEMU)
   and pushes it to GHCR on non-PR runs.
5. **deploy** (only on `main`, and only if a `K8S_CONFIG_TOKEN` secret exists): dispatches an `update-image` workflow in
   a separate GitOps repository with the new image tag, which commits it and lets ArgoCD roll the Deployment. The
   tag is the commit SHA.
6. **cleanup**: prunes old image versions, keeping enough SHAs for a rollback.

On `v*` tags a GitHub Release is created with `sdc-{server,client}-{aarch64,x86_64}-linux`, checksums and the
Elgato plugin.

The deployment is the update mechanism for clients too: once the new server is running, each connected Pi sees that its
binary hash differs, downloads the new one and restarts (see [Raspberry Pi deck](pi.md#updates)).
