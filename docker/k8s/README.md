# Disposable Kubernetes cluster for integration tests

These fixtures support the Kubernetes integration tests. They are independent of
`docker/sshd`; no test in this directory needs SSH or a shell in the workload.

The cluster contains:

- namespace `shop` with restricted Pod Security labels;
- Deployment `web`: three replicas, each with `main` and `sidecar` BusyBox
  containers; `main` writes one log line per second;
- Pod `crashy`: exits with status 1 after five seconds and enters
  `CrashLoopBackOff`, leaving logs for `--previous`;
- Deployment `distroless`: `registry.k8s.io/pause:3.10`, a single static binary
  with no shell;
- ServiceAccounts `reader` and `no-exec`, both bound only to `get`, `list`, and
  `watch` on `pods`, `pods/log`, and `events`. Neither has `pods/exec` or any
  write permission, and neither can read Secrets.

All workload containers run as non-root with a read-only root filesystem, an
`emptyDir` mounted at `/tmp`, dropped capabilities, and a memory limit.

## Prerequisites

- Docker with a running Linux engine;
- `kubectl` compatible with Kubernetes 1.33 or newer;
- `kind` for the default provider (not needed for k3s);
- a POSIX shell for `up.sh` and `down.sh`.

## kind (default)

```sh
docker/k8s/up.sh
```

The script creates cluster `filar-k8s-tests`, applies the manifests, waits for
`web` and `distroless`, and prints the kubeconfig path. `crashy` is deliberately
excluded from readiness waits. Run it again to refresh fixtures and tokens.

```sh
export FILAR_TEST_KUBECONFIG=/absolute/path/printed/by/up.sh
cargo test --workspace k8s -- --ignored
```

Run only the Kubernetes ignored tests when Docker SSH fixtures are not up. New
Kubernetes integration tests use:

```rust
#[ignore = "needs docker/k8s cluster"]
```

Remove the cluster and generated credentials with:

```sh
docker/k8s/down.sh
```

## k3s in Docker (without kind)

```sh
FILAR_K8S_PROVIDER=k3s docker/k8s/up.sh
```

This starts `rancher/k3s:v1.33.5-k3s1` as container `filar-k3s-tests`, publishes
its API on a random loopback port, and writes a corrected kubeconfig. Tear it
down with:

```sh
FILAR_K8S_PROVIDER=k3s docker/k8s/down.sh
```

Names, image, and output path can be overridden with
`FILAR_K8S_CLUSTER_NAME`, `FILAR_K3S_CONTAINER`, `FILAR_K3S_VOLUME`,
`FILAR_K3S_IMAGE`, and `FILAR_TEST_KUBECONFIG`. `KUBECTL` may contain the full
path to kubectl.

## ServiceAccount kubeconfigs and denial checks

`up.sh` creates three files with mode 0600 under the ignored `.run/` directory:
the admin kubeconfig, `reader.kubeconfig`, and `no-exec.kubeconfig`. The latter
two contain 24-hour TokenRequest credentials; never commit or print them.

```sh
kubectl --kubeconfig=docker/k8s/.run/reader.kubeconfig -n shop get pods
kubectl --kubeconfig=docker/k8s/.run/reader.kubeconfig -n shop logs pod/crashy --previous
kubectl --kubeconfig=docker/k8s/.run/no-exec.kubeconfig -n shop auth can-i create pods/exec
kubectl --kubeconfig=docker/k8s/.run/no-exec.kubeconfig -n shop get secrets
```

The first two commands are allowed; the last two return `no`/Forbidden. Use
`no-exec.kubeconfig` for future exec-denial tests.

## Restricted VMs: negative `oom_score_adj`

Some test VMs reject the negative `oom_score_adj` values that kubelet assigns to
pods. The failure appears as an OCI runtime/create error even though the node is
Ready. `runc/oom-score-adj-wrapper.sh` is a test-only wrapper that rewrites only
negative `oomScoreAdj` values to zero in the OCI bundle and then execs the real
runtime.

Install it only in the disposable k3s container or test VM, never on a
production host. In the pinned k3s image, preserve the real runtime as
`/bin/runc.real`, put the wrapper at `/bin/runc`, and make both executable. If
the runtime lives elsewhere, adjust the paths and set `FILAR_RUNC_REAL`
accordingly. The wrapper handles `--bundle PATH`, `--bundle=PATH`, and
`-b PATH`. Restart k3s after replacing the runtime and rerun `up.sh`.

## Airgap images

Preload `rancher/k3s:v1.33.5-k3s1` into Docker before starting. Download the
matching `k3s-airgap-images-amd64.tar.zst` from the k3s GitHub release and pass
its absolute path on first startup:

```sh
FILAR_K8S_PROVIDER=k3s \
FILAR_K3S_AIRGAP_IMAGES=/absolute/path/k3s-airgap-images-amd64.tar.zst \
docker/k8s/up.sh
```

The file is mounted at k3s's airgap image directory before the server starts.
For kind, use `kind load image-archive` after saving the required images with
`docker save`.

When Docker Hub is unavailable, build the fixture BusyBox image from a static
binary instead of pulling it:

```sh
cp /path/to/static/busybox docker/k8s/images/busybox/busybox
docker build -t busybox:1.37.0 docker/k8s/images/busybox
docker save busybox:1.37.0 -o docker/k8s/.run/busybox.tar
kind load image-archive docker/k8s/.run/busybox.tar --name filar-k8s-tests
```

For k3s, import the archive into its containerd before applying the manifests:

```sh
docker exec -i filar-k3s-tests ctr -n k8s.io images import - <docker/k8s/.run/busybox.tar
```

The binary is ignored by Git. `registry.k8s.io/pause:3.10` must likewise be
preloaded when external registries are unreachable.
