# ionos-mks-nlb-node-reconciler

[![ci](https://github.com/jernoxit/ionos-mks-nlb-node-reconciler/actions/workflows/ci.yml/badge.svg)](https://github.com/jernoxit/ionos-mks-nlb-node-reconciler/actions/workflows/ci.yml)
[![release](https://github.com/jernoxit/ionos-mks-nlb-node-reconciler/actions/workflows/release.yml/badge.svg)](https://github.com/jernoxit/ionos-mks-nlb-node-reconciler/actions/workflows/release.yml)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](./LICENSE)

A tiny Kubernetes controller that keeps the `targets[]` of an **IONOS Network Load
Balancer**'s forwarding rules in sync with your cluster's **Ready nodes** — using
each node's *private* target-LAN IP.

## The problem it solves

On a **public IONOS Managed Kubernetes (MKS)** cluster there is no cloud-controller
that wires a real load balancer's backend pool to your nodes:

- IONOS MKS's built-in `Service type=LoadBalancer` does **not** put a real load
  balancer in front of the cluster — it just assigns a static public IP to *one*
  node ([IONOS docs](https://docs.ionos.com/cloud/containers/managed-kubernetes/use-cases/horizontal-scaling)).
  To spread traffic across nodes you need a real **IONOS Network Load Balancer**.
- A real IONOS NLB's forwarding rules take **node IPs** as targets. But the node's
  Kubernetes `InternalIP` on a public cluster is its **public** IP, while the NLB
  needs the node's **private target-LAN** IP. That private IP is only discoverable
  by enumerating the VDC servers via the IONOS Cloud API (server name == k8s node
  name, the NIC on your target LAN → its private DHCP IP).
- MKS **replaces nodes on its weekly managed maintenance**, so those private IPs
  drift. Wiring the targets by hand (as the [SUSE Rancher-on-IONOS guide](https://www.suse.com/c/deploy-suse-rancher-prime-on-ionos-cloud/)
  does) breaks on the next maintenance window.

On AWS/GCP/Azure the cloud-controller-manager closes this loop automatically. IONOS
leaves it open. This controller closes it: it watches your nodes and continuously
reconciles the NLB forwarding-rule targets to the current, Ready nodes' private IPs.

> **Note on terminology:** IONOS sometimes calls the "static public IP on one node"
> MKS trick a "Network Load Balancer" too. This tool is for the **real IONOS Managed
> Network Load Balancer** product (forwarding rules + a target pool), not that
> single-node ingress mechanism.

## How it works

1. Enumerates the VDC servers (`GET /datacenters/{dc}/servers?depth=5`) and maps
   each server (== node) name → its private IP on the configured target LAN.
2. Lists the cluster's Ready nodes and resolves each to its private IP.
3. For each configured forwarding rule, `PATCH`es `targets[]` to those IPs
   (weight 1, PROXY protocol v2, health-check on).

It re-runs on every node event (kube-rs watch), on a periodic resync, and once in
full on startup (self-healing after a crash). It is **single-replica by design** —
no CRD, no leader election.

**Empty-target guard:** if *no* Ready node resolves to a private IP (a transient API
hiccup, or mid node-replacement), it **skips** the PATCH rather than clearing the
NLB and taking your edge down. Partial shrinks (N→M, M>0) are allowed as normal
churn; only the jump to zero is treated as pathological. A liveness probe returns
`503` if no reconcile has succeeded for ~3 resync cycles, so the kubelet restarts a
wedged pod.

## Prerequisites

- An IONOS Managed Kubernetes cluster (public node pool) whose nodes sit on a LAN.
- A **real IONOS Network Load Balancer** already provisioned (e.g. via Terraform/
  OpenTofu) with the forwarding rules you want managed. This controller owns **only**
  the rules' `targets[]` — keep whatever provisions the NLB from fighting over them
  (e.g. Terraform `lifecycle { ignore_changes = [targets] }`).
- An IONOS API token. Prefer a **least-privilege** user with edit access to that VDC
  only — no contract/capability privileges are needed.

You will need these IDs (all obtainable from the IONOS DCD or the Cloud API, or as
outputs of your Terraform):

| Value | Where |
| --- | --- |
| `datacenterId` | the VDC (datacenter) UUID holding the worker VMs + NLB |
| `nlbId` | the Network Load Balancer UUID |
| `targetLanId` | the LAN id (integer) whose NIC carries the private node IP |
| `ruleIds` | the forwarding-rule UUIDs to manage |

## Install (Helm)

The chart is published as an OCI artifact to GHCR:

```sh
helm install nlb-reconciler \
  oci://ghcr.io/jernoxit/charts/ionos-mks-nlb-node-reconciler \
  --namespace kube-system \
  --set config.datacenterId=<DC_UUID> \
  --set config.nlbId=<NLB_UUID> \
  --set config.targetLanId=<LAN_ID> \
  --set 'config.ruleIds={<RULE_UUID_1>,<RULE_UUID_2>}' \
  --set token.value=<IONOS_TOKEN>          # or: --set token.existingSecret=my-secret
```

For GitOps, put the values in a file and reference an existing Secret for the token
(created out-of-band via SOPS / sealed-secrets), rather than `token.value`.

### Key values

| Value | Default | Description |
| --- | --- | --- |
| `image.repository` / `image.tag` | `ghcr.io/jernoxit/ionos-mks-nlb-node-reconciler` / chart appVersion | container image |
| `config.apiUrl` | `https://api.ionos.com/cloudapi/v6` | IONOS Cloud API base URL |
| `config.datacenterId` | — (required) | VDC UUID |
| `config.nlbId` | — (required) | NLB UUID |
| `config.targetLanId` | — (required) | target LAN id (as a string) |
| `config.ruleIds` | `[]` (required) | forwarding-rule UUIDs to manage |
| `config.resyncSeconds` | `180` | full-resync interval |
| `token.existingSecret` / `token.value` | — | provide one; token needs edit access to the VDC |
| `rbac.create` | `true` | create the `list/watch nodes` ClusterRole + binding |
| `resources` | 25m/32Mi → 200m/128Mi | requests/limits |

See [`charts/ionos-mks-nlb-node-reconciler/values.yaml`](./charts/ionos-mks-nlb-node-reconciler/values.yaml) for all values.

### RBAC

The controller only needs to `list`/`watch` cluster-scoped **nodes**. The chart
creates a `ClusterRole` + `ClusterRoleBinding` for that (`rbac.create=true`).

## Configuration (raw / without Helm)

The binary is configured entirely via environment variables:

| Env | Required | Default |
| --- | --- | --- |
| `IONOS_TOKEN` | ✅ | — |
| `DATACENTER_ID` | ✅ | — |
| `NLB_ID` | ✅ | — |
| `TARGET_LAN_ID` | ✅ | — |
| `RULE_IDS` (comma-separated) | ✅ | — |
| `IONOS_API_URL` | | `https://api.ionos.com/cloudapi/v6` |
| `RESYNC_SECONDS` | | `180` |
| `HEALTH_PORT` | | `8080` |
| `RUST_LOG` | | `info` |

## Security

- Distroless (`gcr.io/distroless/cc-debian13:nonroot`) — no shell, minimal surface.
- Runs non-root, `readOnlyRootFilesystem`, `allowPrivilegeEscalation: false`, all
  capabilities dropped, `seccompProfile: RuntimeDefault`.
- Outbound TLS to the IONOS API via rustls (bundled CA certs from the distroless
  image).
- No inbound traffic except the liveness probe on the health port.

## Container image

```
ghcr.io/jernoxit/ionos-mks-nlb-node-reconciler:<version>
```

## Build from source

```sh
cargo build --release          # binary at target/release/nlb-reconciler
docker build -t nlb-reconciler .
```

## Releasing

Push a semver tag; the [`release`](./.github/workflows/release.yml) workflow builds
and pushes the image and the Helm chart to GHCR and creates a GitHub Release with
auto-generated notes (so each version's diff is visible):

```sh
git tag v0.1.0 && git push origin v0.1.0
```

> After the **first** release, set the two GHCR packages
> (`ionos-mks-nlb-node-reconciler` and `charts/ionos-mks-nlb-node-reconciler`) to
> **Public** in the repository's package settings — GHCR packages are private by
> default and `GITHUB_TOKEN` cannot flip that automatically.

## License

[Apache-2.0](./LICENSE).
