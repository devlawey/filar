#!/bin/sh
# Destroy the disposable cluster and generated kubeconfigs.
set -eu

DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PROVIDER=${FILAR_K8S_PROVIDER:-kind}
CLUSTER_NAME=${FILAR_K8S_CLUSTER_NAME:-filar-k8s-tests}
K3S_CONTAINER=${FILAR_K3S_CONTAINER:-filar-k3s-tests}
K3S_VOLUME=${FILAR_K3S_VOLUME:-filar-k3s-tests-data}

case $PROVIDER in
    kind)
        kind delete cluster --name "$CLUSTER_NAME"
        ;;
    k3s)
        docker rm -f "$K3S_CONTAINER" >/dev/null 2>&1 || true
        docker volume rm "$K3S_VOLUME" >/dev/null 2>&1 || true
        ;;
    *)
        echo "FILAR_K8S_PROVIDER must be kind or k3s (got: $PROVIDER)" >&2
        exit 2
        ;;
esac

rm -rf "$DIR/.run"
