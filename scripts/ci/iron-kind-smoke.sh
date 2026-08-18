#!/bin/sh
set -eu

KIND_VERSION="v0.32.0"
KIND_NODE_IMAGE="kindest/node:v1.36.1@sha256:3489c7674813ba5d8b1a9977baea8a6e553784dab7b84759d1014dbd78f7ebd5"
IRON_IMAGE="ironsh/iron-proxy:0.50.0-rc.3"
IRON_RUNTIME_IMAGE="ironsh/iron-proxy:subhub-smoke"
SMOKE_IMAGE="subhub-iron-smoke:local"

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/../.." && pwd)
cluster_name=${KIND_CLUSTER_NAME:-"subhub-iron-smoke-$$"}
keep_cluster=${KEEP_KIND_CLUSTER:-0}
work_dir=$(mktemp -d)
kubeconfig="$work_dir/kubeconfig"
cluster_created=0

kubectl_smoke() {
    kubectl --kubeconfig "$kubeconfig" "$@"
}

cleanup() {
    status=$?
    if [ "$cluster_created" -eq 1 ]; then
        if [ "$status" -ne 0 ]; then
            kubectl_smoke describe job/iron-kind-smoke >&2 || true
            kubectl_smoke logs \
                job/iron-kind-smoke --all-containers=true --prefix=true >&2 || true
        fi
        if [ "$keep_cluster" = "1" ]; then
            printf 'Kind cluster retained: %s\n' "$cluster_name"
            printf 'Kubeconfig retained: %s\n' "$kubeconfig"
        else
            "$kind_bin" delete cluster \
                --name "$cluster_name" \
                --kubeconfig "$kubeconfig" \
                >/dev/null || true
        fi
    fi
    if [ "$cluster_created" -ne 1 ] || [ "$keep_cluster" != "1" ]; then
        rm -rf "$work_dir"
    fi
    exit "$status"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

for command_name in curl docker kubectl sha256sum; do
    if ! command -v "$command_name" >/dev/null 2>&1; then
        printf 'error: required command not found: %s\n' "$command_name" >&2
        exit 1
    fi
done

case "$(uname -m)" in
    x86_64)
        kind_arch=amd64
        kind_sha256=50030de23cf40a18505f20426f6a8506bedf13c6e509244bd1fa9463721b0f54
        iron_digest=sha256:24a84457ce1430b051e6dc04375fe329a5130cab4364a022b3acee3b6e1a357d
        ;;
    aarch64 | arm64)
        kind_arch=arm64
        kind_sha256=b92cd615e97585de8ddade28ed5cd7feb4248d717c233eea5b03c37298900f5d
        iron_digest=sha256:cabb96afd67ee89ca77ca634f8a34de2eb1d1b398ce097f08f8cfd97ef7906c9
        ;;
    *)
        printf 'error: Kind %s is not pinned for architecture %s\n' \
            "$KIND_VERSION" "$(uname -m)" >&2
        exit 1
        ;;
esac

kind_bin="$work_dir/kind"
curl --fail --location --silent --show-error \
    "https://github.com/kubernetes-sigs/kind/releases/download/$KIND_VERSION/kind-linux-$kind_arch" \
    --output "$kind_bin"
printf '%s  %s\n' "$kind_sha256" "$kind_bin" | sha256sum --check --status
chmod +x "$kind_bin"

printf 'Building Subhub smoke image...\n'
docker build \
    --file "$script_dir/iron-kind/Dockerfile" \
    --tag "$SMOKE_IMAGE" \
    "$repo_root"

printf 'Pulling pinned Iron image...\n'
docker pull "$IRON_IMAGE@$iron_digest"
docker tag "$IRON_IMAGE@$iron_digest" "$IRON_RUNTIME_IMAGE"

printf 'Creating Kind cluster %s...\n' "$cluster_name"
"$kind_bin" create cluster \
    --name "$cluster_name" \
    --image "$KIND_NODE_IMAGE" \
    --kubeconfig "$kubeconfig" \
    --wait 120s
cluster_created=1

"$kind_bin" load docker-image \
    --name "$cluster_name" \
    "$SMOKE_IMAGE" \
    "$IRON_RUNTIME_IMAGE"

kubectl_smoke apply \
    --filename "$script_dir/iron-kind/smoke.yaml"

if ! kubectl_smoke wait \
    --for=condition=complete \
    --timeout=240s \
    job/iron-kind-smoke; then
    exit 1
fi

kubectl_smoke logs \
    job/iron-kind-smoke --container=probe
printf 'Iron Kind smoke test passed.\n'
