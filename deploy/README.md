# Running Subhub in a cluster

Subhub can serve beyond loopback when started with `--allow-remote` and an
explicit client token (`SUBHUB_CLIENT_TOKEN` or `--client-token`). The
`Dockerfile` at the repo root builds an image whose default command does
exactly that on `0.0.0.0:7842`, with the Linux credential store on a `/data`
volume.

## Build and publish

```sh
docker build -t ghcr.io/maddiaa0/subhub:latest .
docker push ghcr.io/maddiaa0/subhub:latest
```

Or let the `docker.yml` GitHub Actions workflow publish to GHCR on pushes to
`main` and version tags. For a private image, give the cluster an
`imagePullSecret` for ghcr.io.

## Deploy

```sh
kubectl create namespace subhub
kubectl create secret generic subhub-client-token -n subhub \
  --from-literal=token="$(openssl rand -hex 32)"
kubectl apply -f deploy/kubernetes.yaml
```

The pod will crash-loop with `no credentials saved` until the volume is
seeded — that is expected on first deploy.

## Seed credentials

`subhub add` needs the `claude`/`codex` CLIs and a browser, so capture
accounts on a workstation, then copy the store onto the PVC:

```sh
# on the workstation: accounts already saved via `subhub add`
kubectl scale deploy/subhub -n subhub --replicas=0
kubectl run subhub-seed -n subhub --image=busybox --restart=Never \
  --overrides='{"spec":{"securityContext":{"runAsUser":10001,"fsGroup":10001},"containers":[{"name":"subhub-seed","image":"busybox","command":["sleep","3600"],"volumeMounts":[{"name":"data","mountPath":"/data"}]}],"volumes":[{"name":"data","persistentVolumeClaim":{"claimName":"subhub-data"}}]}}'
kubectl cp ~/.config/subhub subhub/subhub-seed:/data/subhub
kubectl delete pod subhub-seed -n subhub
kubectl scale deploy/subhub -n subhub --replicas=1
```

**One owner per refresh-token family.** The gateway rotates Claude refresh
tokens; two gateways sharing an account (your laptop's and the cluster's)
will race and strand the account with `invalid_grant`. Either move accounts
to the cluster and stop the local gateway, or dedicate separate accounts to
the cluster. To keep the local experience, point the local CLIs at the
cluster gateway instead of running a second one.

## Verify

```sh
TOKEN=$(kubectl get secret subhub-client-token -n subhub -o jsonpath='{.data.token}' | base64 -d)
kubectl run curl-test -n subhub --rm -it --image=curlimages/curl --restart=Never -- \
  curl -s -H "Authorization: Bearer $TOKEN" http://subhub:7842/_subhub/status
```

## Pointing clients at it

Claude Code (Anthropic Messages API surface, any path):

```sh
export ANTHROPIC_BASE_URL=http://subhub.subhub.svc.cluster.local:7842
export ANTHROPIC_AUTH_TOKEN=<client token>
```

Codex (Responses API surface, under the `/openai` prefix):

```toml
# ~/.codex/config.toml
[model_providers.subhub]
name = "subhub"
base_url = "http://subhub.subhub.svc.cluster.local:7842/openai"
wire_api = "responses"
```

with the client token supplied as the provider's API key. The gateway accepts
the token as either `Authorization: Bearer …` or `x-api-key: …`.

Note that the gateway only speaks the Anthropic Messages API and the Codex
Responses API. Generic OpenAI *chat-completions* clients are not translated
and will not work through it.

## Omnigent wiring

Runner Pods are configured through `sandbox.host_config` — no Omnigent code
changes needed. Add the client token to the runner Secret (for example
`SUBHUB_TOKEN` in `omnigent-creds`), then:

```yaml
sandbox:
  host_config:
    providers:
      subhub:
        kind: gateway
        default: [anthropic]
        anthropic:
          base_url: http://subhub.subhub.svc.cluster.local:7842
          api_key_ref: env:SUBHUB_TOKEN
        openai: # codex harnesses only (Responses API)
          base_url: http://subhub.subhub.svc.cluster.local:7842/openai
          api_key_ref: env:SUBHUB_TOKEN
          wire_api: responses
```
