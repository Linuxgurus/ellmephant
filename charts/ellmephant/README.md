# ellmephant Helm chart

Deploy [ellmephant](../../README.md) — single-binary, SQLite-backed long-term memory for AI
agents — as an MCP server over Streamable HTTP.

The chart is one Deployment, one Service, a store PVC, and (optionally) a separate weights-cache
PVC. No sidecars, no operators.

## Install

```sh
helm install ellmephant ./charts/ellmephant --namespace ellmephant --create-namespace
```

[`chart-release.yml`](../../.github/workflows/chart-release.yml) packages the chart and attaches it
to a GitHub Release tagged `ellmephant-<version>` — release assets only, so there is no Helm repo
index and nothing to `helm repo add`. Install straight from the asset URL:

```sh
helm install ellmephant \
  https://github.com/jdblack/ellmephant/releases/download/ellmephant-0.1.0/ellmephant-0.1.0.tgz
```

Or from an OCI mirror, if one exists (see [`DEVELOPMENT.md`](../../DEVELOPMENT.md) for the Harbor
recipe):

```sh
helm install ellmephant oci://harbor.vn.linuxguru.net/library/charts/ellmephant --version 0.1.0
```

Then point an MCP client at the Service, path `/mcp`:

```
http://ellmephant.ellmephant.svc:8888/mcp
```

## What happens on first start

1. The `init-store` container runs `ellmephant doctor`, creating the schema in `<store>/memory.db`.
2. The `mcp` container binds `0.0.0.0:8888` and serves MCP.
3. The embedder (~1.2 GB, `Qwen/Qwen3-Embedding-0.6B`) downloads **on first use** — the first
   embedding is slow; later ones are not. Piping logs at startup shows the encoder loading.

Two ways to avoid the slow first start:

- Set `models.fetch.enabled=true` to fetch the weights in an init container (needs egress to the
  Hugging Face Hub), or
- Pre-populate a cache elsewhere and mount it: `models.existingClaim` (or `models.hostPath`) with
  `models.readOnly=true`.

## Storage

- **`persistence`** — the source of truth (`memory.db`). Without it every memory dies with the
  pod. Keep it.
- **`models`** — the weights cache. Off by default, in which case it lives under
  `<store>/models` on the data volume. Turn it on to park the fat, mostly read-only cache on a
  cheaper class, or to share one warm cache across stores.

`replicaCount` is pinned to 1 on purpose: SQLite has a single writer, so a second pod would
contend for the file. The strategy is `Recreate` for the same reason.

## Configuration

`config.toml` is rendered from `.Values.config.toml` (or an existing ConfigMap via
`config.existingConfigMap`) and mounted at `<store>/config.toml`. Flags and environment override
it, and the file **rejects unknown keys**, so only real settings belong there. The defaults the
chart ships mirror [`src/config.rs`](../../src/config.rs).

Common knobs:

| Value | Default | Effect |
|---|---|---|
| `mcp.port` / `service.port` | `8888` | MCP port |
| `offline` | `false` | Never open a socket; a cold cache becomes a hard error |
| `huggingFaceToken.secretName` | `""` | Optional token for a private mirror / rate limits |
| `logging.rustLog` | `ellmephant=info,rmcp=info,warn` | `RUST_LOG` |
| `env` / `envFrom` | `[]` | Extra env, appended after the built-ins |

## Probes

The probes are **TCP** against the MCP port. There is no health HTTP route — the only path is
`/mcp`, an MCP handshake — so a port check is the honest signal (and avoids the trap of a probe
that writes to the store, which the image's own `HEALTHCHECK` deliberately avoids too).

## Security

Defaults are locked down: non-root uid/gid `10001`, read-only root filesystem, all capabilities
dropped, `RuntimeDefault` seccomp, and **no** ServiceAccount token (`automount: false`). The only
writable paths are the store volume and an `emptyDir` at `/tmp` (`HOME=/tmp`).

## Exposing it — Gateway API

A classic `networking.k8s.io/v1` Ingress renders when `ingress.enabled=true`, but on a **Gateway
API** cluster (NGINX Gateway Fabric) that is a dead end: the shared gateways only serve HTTPS
through app-declared `ListenerSet`s. So the chart speaks that language directly. `gateway.enabled`
renders the same trio as the in-house `network/gateway/expose` module:

- a **`ListenerSet`** on a shared Gateway, carrying the `cert-manager.io/cluster-issuer`
  annotation on the listener and terminating TLS with `cert-<hostname>`;
- **`ReferenceGrant`s** (ListenerSet + HTTPRoute) — only when the Gateway lives in another
  namespace;
- an **`HTTPRoute`** with the `external-dns.alpha.kubernetes.io/hostname` annotation, one per
  `httpRoutes` entry (an empty list renders a single catch-all route to the Service).

```yaml
gateway:
  enabled: true
  gatewayName: private            # the shared Gateway
  gatewayNamespace: kube-network
  domain: vn.linuxguru.net        # hostname becomes <listenerName>.<domain>
  certIssuer: letsencrypt         # cert-manager ClusterIssuer
  # listenerName: ""              # defaults to the release fullname
```

`httpRoutes` entries accept `name`, `sectionName`, `hostnames`, `backendName`, `backendPort`,
`annotations` and `filters` — enough for the Authentik forward-auth route (`<name>-auth`) the
house grammar uses. Anything the chart does not model still goes through `extraObjects`, which
renders manifests verbatim.

`networkPolicy` is opt-in with free-form `ingress`/`egress` rules, so the cluster's own
conventions (the gateway pod selector `gateway.networking.k8s.io/gateway-name`, node IPs for
kubelet probes, DNS, the Hub) win over anything this chart could guess. Empty list denies that
direction.

## TypingMind MCP connector (optional)

`mcpConnector.enabled=true` adds the connector TypingMind needs to reach an MCP server, cribbed
from the in-house `modules/mcp/typingmind` deployment and hardened the same way:

- its own Deployment (`Recreate` — it holds MCP clients in memory), Service, and bearer-token
  Secret;
- **no** ServiceAccount token, uid/gid 1000, read-only root filesystem, all caps dropped, a
  writable `emptyDir` at `/tmp` with `HOME=/tmp` and `npm_config_cache=/tmp/.npm` for the stdio
  servers it spawns;
- **TCP** probes (every connector route is bearer-gated, so a headerless `/ping` reads 401);
- a `checksum/secret` pod annotation so rotating the token rolls the pod.

The token is required — `authToken.value` (chart-created Secret) or `authToken.secretName`
(existing Secret). With neither, the render **fails** rather than deploy an open connector.

```yaml
mcpConnector:
  enabled: true
  authToken:
    value: "…"                    # or secretName: typingmind-mcp-auth
  gateway:
    enabled: true                 # its own ListenerSet + HTTPRoute, private gateway
    domain: vn.linuxguru.net
    certIssuer: letsencrypt
```

The default image is the upstream npm package (`node:lts-slim` + `npx @typingmind/mcp`). For the
pinned in-house build, set `image.repository: harbor.vn.linuxguru.net/library/typingmind-mcp`,
`image.tag: v1.0.3`, and `command: []` (use the image entrypoint). Set
`mcpConnector.fullnameOverride: typingmind-mcp` to keep the existing Service name.

## Compatibility notes

- **Gateway API / NGINX Gateway Fabric** — first-class via `gateway`; the handler gets a
  `ListenerSet`, not an Ingress. Add a `ProxySettingsPolicy` read timeout externally if a client
  holds a long-lived stream (the house `modules/network/gateway` provides it).
- **cert-manager** — the ListenerSet carries `cert-manager.io/cluster-issuer: <certIssuer>`;
  cert-manager issues cert-`<hostname>` itself, no chart-side cert objects.
- **external-dns** — HTTPRoutes carry the hostname annotation, so records land in BIND with no
  zone edits, as elsewhere.
- **Storage** — `persistence.storageClass` for the small hot store (`longhorn`); park the ~1.2 GB
  weights cache on `models.storageClass` (`seaweedfs-csi`) if you want it off the replicated tier.
  Both blocks take `accessModes`, `existingClaim`, and `annotations`.
- **Harbor** — point `image.repository` at `harbor.vn.linuxguru.net/...` and set
  `imagePullSecrets` if the project is private.
- **Calico NetworkPolicy** — rules are free-form, so they compose with the house
  `network/firewalls/policy` generator instead of fighting it; that generator still owns the
  namespace baseline.
- **Velero** — the chart creates plain PVCs; add `velero.io/*` via `persistence.annotations` /
  `models.annotations` if the shared schedule selects by annotation rather than by label. Label
  or annotate the PVC as the house `storage/backup/schedule` expects.
- **Prometheus** — a SQLite-backed, single-replica MCP server exposes no metrics today; only TCP
  liveness is available. A ServiceMonitor is not shipped because there is nothing yet to scrape.

## Tests

```sh
helm test ellmephant --namespace ellmephant
```

The test pod only checks that the MCP TCP port is open.

## Values

See [`values.yaml`](values.yaml) — every knob is documented inline.
