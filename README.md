# gradient-hypervisor

Worker deployment tools for [Gradient](https://github.com/wavelens/gradient).

## gradient-autoscaler

Starts temporary Gradient team workers when connected workers cannot handle waiting jobs, and stops them once they are idle. Each connection starts its workers either as GitHub Actions runs or as Kubernetes pods.

- A `worker.demand` webhook from Gradient wakes the service, and a catch-up check runs every `catchUpInterval` seconds.
- A worker starts only for a missing capability: a system, a system feature, or network, secrets and evaluation access that no connected worker eligible for the project offers. A busy but capable worker starts nothing.
- Workers exit after `idleTimeout` seconds without a job or after `maxLifetime` seconds. A worker that has not started yet is stopped again once its demand is gone.
- The service deletes the team worker of a launch after the launch ends, and only deletes workers it registered.
- Two launches of one runner label or pool that end without their worker connecting within 30 minutes pause that label for 10 minutes.

### Kubernetes

- Each pool in `kubernetes.pools` is one kind of worker pod with its system, features, job slots, resources, node selector and tolerations.
- `maxJobs` sets the worker's concurrent build slots. A value of zero is rejected during configuration loading.
- For each launch, the service registers a team worker and creates the pod `gradient-worker-<launch id>` with `restartPolicy: Never`. The worker token goes into a Secret of the same name that the pod owns, so deleting the pod deletes the token.
- The service polls pod phases. A pod that exits, fails or cannot pull its image ends its launch, and the service deletes it.
- The service needs `create`, `get`, `list` and `delete` on pods and `create` on secrets in the worker namespace.
- Builds get the Nix sandbox and `/dev/kvm` when pods run with `privileged`.
- Nix can fall back to unsandboxed builds when a pod can't create namespaces. Most clusters forbid that setup unless pods run with `privileged`.

### GitHub Actions

- The service creates or repairs the Gradient team webhook, the GitHub repository webhook and `.github/workflows/gradient-worker.yml` on the default branch.
- Removing the first line `# managed by gradient-autoscaler v...` from that file hands it to the operator, and the service stops rewriting it.
- Each run exchanges its GitHub OIDC token for a one-time Gradient worker token, so worker tokens never appear in workflow inputs or logs.
- GitHub and GitHub-hosted runners must reach both `publicUrl` and the Gradient server's `/proto` endpoint.
- `ubuntu-latest` and `ubuntu-24.04-arm` are enabled by default. `macos-latest`, larger runners and self-hosted runners are detected but only used once listed in `allowedKinds`.
- Runs start `gradient-worker` from `github:wavelens/gradient/v<server version>#gradient` unless `workerFlake` is set. Point `workerSubstituters` and `workerTrustedPublicKeys` at a cache holding that closure, or every run builds Gradient from source.

### Requirements

- Tasks using the autoscaler need `wait_for_workers`, so evaluations wait for a worker to start instead of aborting after 300 seconds.
- Gradient must reach `publicUrl` for the `worker.demand` webhook.
- The API key belongs to a user with the Admin role in the team.

## Images

| Package | Contents |
| --- | --- |
| `gradient-worker-image` | `gradient-worker`, Gradient's Nix, git, ssh and CA certificates, with a registered store |
| `gradient-autoscaler-image` | `gradient-autoscaler` and CA certificates |

```sh
nix build .#gradient-worker-image
skopeo copy docker-archive:result docker://registry.example.com/gradient-worker:2.0.1
```

The worker image uses the `gradient` flake input. Match it to the server version, or build an image for another Gradient package with `lib.mkWorkerImage { pkgs, gradient, gradient-nix }`.

## Helm

`helm/gradient-autoscaler` deploys the autoscaler into a cluster: a Deployment with a state volume, its config, a Service, an optional Ingress for `/c/`, and a ServiceAccount with a Role in every worker namespace.

```sh
kubectl create secret generic gradient-api-key --from-file=api-key=./api-key
helm install autoscaler ./helm/gradient-autoscaler -f values.yaml
```

`helm/gradient-autoscaler/ci/kubernetes-values.yaml` shows a Kubernetes and a GitHub Actions connection. A connection inside the cluster talks to the API server with its service account; `apiServer`, `tokenFile` and `caFile` are only needed outside it.

## NixOS

```nix
{
  inputs.gradient-hypervisor.url = "github:wavelens/gradient-hypervisor";

  outputs = { nixpkgs, gradient-hypervisor, ... }: {
    nixosConfigurations.host = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        gradient-hypervisor.nixosModules.gradient-autoscaler
        {
          services.gradient-autoscaler = {
            enable = true;
            publicUrl = "https://autoscaler.example.com";
            connections.cluster = {
              gradientUrl = "https://gradient.example.com";
              team = "platform";
              apiKeyFile = "/run/secrets/gradient-api-key";
              kubernetes = {
                apiServer = "https://k8s.example.com:6443";
                tokenFile = "/run/secrets/kubernetes-token";
                namespace = "gradient-workers";
                image = "registry.example.com/gradient-worker:2.0.1";
                pools.x86 = { system = "x86_64-linux"; cpu = "8"; memory = "32Gi"; };
              };
            };
            connections.actions = {
              gradientUrl = "https://gradient.example.com";
              team = "platform";
              apiKeyFile = "/run/secrets/gradient-api-key";
              githubActions = {
                tokenFile = "/run/secrets/github-token";
                repository = "acme/gradient-runners";
              };
            };
          };
        }
      ];
    };
  };
}
```
