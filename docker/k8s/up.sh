#!/bin/sh
# Start the disposable filar Kubernetes test cluster and apply its fixtures.
set -eu
umask 077

DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
RUN_DIR="$DIR/.run"
PROVIDER=${FILAR_K8S_PROVIDER:-kind}
CLUSTER_NAME=${FILAR_K8S_CLUSTER_NAME:-filar-k8s-tests}
K3S_CONTAINER=${FILAR_K3S_CONTAINER:-filar-k3s-tests}
K3S_VOLUME=${FILAR_K3S_VOLUME:-filar-k3s-tests-data}
K3S_IMAGE=${FILAR_K3S_IMAGE:-rancher/k3s:v1.33.5-k3s1}
KUBECONFIG_PATH=${FILAR_TEST_KUBECONFIG:-$RUN_DIR/$PROVIDER.kubeconfig}
KUBECTL=${KUBECTL:-kubectl}

require() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "required command not found: $1" >&2
        exit 1
    }
}

require docker
require "$KUBECTL"
mkdir -p "$RUN_DIR"

case $PROVIDER in
    kind)
        require kind
        if ! kind get clusters | grep -Fx "$CLUSTER_NAME" >/dev/null 2>&1; then
            kind create cluster --name "$CLUSTER_NAME" --wait 5m
        fi
        kind get kubeconfig --name "$CLUSTER_NAME" >"$KUBECONFIG_PATH"
        ;;
    k3s)
        if ! docker container inspect "$K3S_CONTAINER" >/dev/null 2>&1; then
            if [ -n "${FILAR_K3S_AIRGAP_IMAGES:-}" ]; then
                docker run -d \
                    --name "$K3S_CONTAINER" \
                    --hostname "$K3S_CONTAINER" \
                    --privileged \
                    --restart=unless-stopped \
                    --publish 127.0.0.1::6443 \
                    --volume "$K3S_VOLUME:/var/lib/rancher/k3s" \
                    --volume "$FILAR_K3S_AIRGAP_IMAGES:/var/lib/rancher/k3s/agent/images/k3s-airgap-images.tar.zst:ro" \
                    --tmpfs /run \
                    --tmpfs /var/run \
                    "$K3S_IMAGE" server --disable=traefik
            else
                docker run -d \
                    --name "$K3S_CONTAINER" \
                    --hostname "$K3S_CONTAINER" \
                    --privileged \
                    --restart=unless-stopped \
                    --publish 127.0.0.1::6443 \
                    --volume "$K3S_VOLUME:/var/lib/rancher/k3s" \
                    --tmpfs /run \
                    --tmpfs /var/run \
                    "$K3S_IMAGE" server --disable=traefik
            fi
        elif [ "$(docker inspect -f '{{.State.Running}}' "$K3S_CONTAINER")" != true ]; then
            docker start "$K3S_CONTAINER" >/dev/null
        fi

        attempts=0
        until docker exec "$K3S_CONTAINER" test -s /etc/rancher/k3s/k3s.yaml; do
            attempts=$((attempts + 1))
            if [ "$attempts" -ge 150 ]; then
                echo "k3s did not write its kubeconfig within 5 minutes" >&2
                docker logs --tail 100 "$K3S_CONTAINER" >&2
                exit 1
            fi
            sleep 2
        done
        docker cp "$K3S_CONTAINER:/etc/rancher/k3s/k3s.yaml" "$KUBECONFIG_PATH.tmp"
        port=$(docker port "$K3S_CONTAINER" 6443/tcp | head -n 1 | sed 's/.*://')
        sed "s|https://127.0.0.1:6443|https://127.0.0.1:$port|" \
            "$KUBECONFIG_PATH.tmp" >"$KUBECONFIG_PATH"
        rm -f "$KUBECONFIG_PATH.tmp"
        ;;
    *)
        echo "FILAR_K8S_PROVIDER must be kind or k3s (got: $PROVIDER)" >&2
        exit 2
        ;;
esac
chmod 600 "$KUBECONFIG_PATH"

export KUBECONFIG="$KUBECONFIG_PATH"
"$KUBECTL" wait --for=condition=Ready nodes --all --timeout=5m
"$KUBECTL" apply -f "$DIR/manifests"
"$KUBECTL" -n shop rollout status deployment/web --timeout=5m
"$KUBECTL" -n shop rollout status deployment/distroless --timeout=5m
"$KUBECTL" -n shop rollout status deployment/nowrite --timeout=5m

make_sa_kubeconfig() {
    account=$1
    output=$2
    token=$("$KUBECTL" -n shop create token "$account" --duration=24h)
    server=$("$KUBECTL" config view --raw --minify -o jsonpath='{.clusters[0].cluster.server}')
    ca=$("$KUBECTL" config view --raw --minify -o jsonpath='{.clusters[0].cluster.certificate-authority-data}')
    cat >"$output" <<EOF
apiVersion: v1
kind: Config
clusters:
  - name: filar-test
    cluster:
      server: $server
      certificate-authority-data: $ca
users:
  - name: $account
    user:
      token: $token
contexts:
  - name: $account
    context:
      cluster: filar-test
      namespace: shop
      user: $account
current-context: $account
EOF
    chmod 600 "$output"
}

make_sa_kubeconfig reader "$RUN_DIR/reader.kubeconfig"
make_sa_kubeconfig no-exec "$RUN_DIR/no-exec.kubeconfig"

printf 'Kubernetes test fixtures are ready (crashy intentionally keeps restarting).\n'
printf 'FILAR_TEST_KUBECONFIG=%s\n' "$KUBECONFIG_PATH"
printf 'Reader kubeconfig: %s\n' "$RUN_DIR/reader.kubeconfig"
printf 'No-exec kubeconfig: %s\n' "$RUN_DIR/no-exec.kubeconfig"
