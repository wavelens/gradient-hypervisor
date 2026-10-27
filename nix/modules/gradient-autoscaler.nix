self:
{ config, lib, pkgs, ... }:
let
  inherit (lib) literalExpression mapAttrsToList mkEnableOption mkIf mkOption optionals types;

  cfg = config.services.gradient-autoscaler;
  credentialsDir = "/run/credentials/gradient-autoscaler.service";
  option = name: "services.gradient-autoscaler.connections.<name>.${name}";

  githubActionsOptions = {
    options = {
      tokenFile = mkOption {
        type = types.str;
        example = "/run/secrets/github-token";
        description = ''
          Path to a file holding a fine-grained GitHub token for {option}`${option "githubActions.repository"}`
          with Actions, Contents, Workflows and Webhooks write access.
        '';
      };

      repository = mkOption {
        type = types.str;
        example = "acme/gradient-runners";
        description = ''
          GitHub repository, as `owner/name`, whose Actions minutes the worker runs spend.
          The service commits {file}`.github/workflows/gradient-worker.yml` to its default branch.
        '';
      };

      allowedKinds = mkOption {
        type = types.listOf types.str;
        default = [ "ubuntu-latest" "ubuntu-24.04-arm" ];
        example = [ "ubuntu-latest" "ubuntu-24.04-arm" "macos-latest" ];
        description = ''
          Runner labels the service may start runs on.
          macOS, larger and self-hosted runners are detected but only used once listed here.
        '';
      };

      workerFlake = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "github:wavelens/gradient/v2.0.0#gradient";
        description = ''
          Flake reference of the package providing {command}`gradient-worker` on the runner.
          `null` picks the release tag matching the version the Gradient server reports.
        '';
      };
    };
  };

  poolOptions = {
    options = {
      system = mkOption {
        type = types.str;
        example = "x86_64-linux";
        description = "Nix system the pods of this pool build for.";
      };

      features = mkOption {
        type = types.listOf types.str;
        default = [ ];
        example = [ "big-parallel" "kvm" ];
        description = ''
          Nix system features the pods of this pool offer.
          `kvm` needs {option}`${option "kubernetes.pools.<pool>.privileged"}` or a device plugin.
        '';
      };

      maxJobs = mkOption {
        type = types.ints.positive;
        default = 4;
        description = "Number of builds a pod of this pool runs at the same time.";
      };

      cpu = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "4";
        description = "CPU request of each pod, in Kubernetes quantity notation.";
      };

      memory = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "16Gi";
        description = "Memory request and limit of each pod, in Kubernetes quantity notation.";
      };

      nodeSelector = mkOption {
        type = types.attrsOf types.str;
        default = { };
        example = { "kubernetes.io/arch" = "amd64"; };
        description = "Node labels a node needs to run pods of this pool.";
      };

      tolerations = mkOption {
        type = types.listOf (types.attrsOf types.anything);
        default = [ ];
        example = [ { key = "builders"; operator = "Exists"; effect = "NoSchedule"; } ];
        description = "Kubernetes tolerations of the pods of this pool.";
      };

      privileged = mkOption {
        type = types.bool;
        default = false;
        description = ''
          Whether the worker container runs privileged.
          It gives builds the Nix sandbox and {file}`/dev/kvm` at the cost of host isolation.
        '';
      };

      podOptions = mkOption {
        default = { };
        type = types.submodule {
          options = {
            volumes = mkOption {
              type = types.listOf (types.attrsOf types.anything);
              default = [ ];
              description = "Additional Kubernetes Volume objects. The names peers and state are reserved.";
            };
            volumeMounts = mkOption {
              type = types.listOf (types.attrsOf types.anything);
              default = [ ];
              description = ''
                Additional Kubernetes VolumeMount objects for the worker container.
                Each mount references an additional volume and must not overlap another mount or the
                autoscaler's state and peer credentials.
              '';
            };
            env = mkOption {
              type = types.listOf (types.attrsOf types.anything);
              default = [ ];
              description = ''
                Additional Kubernetes EnvVar objects for the worker container, including valueFrom.
                Generated identity, capabilities, slots, lifetime settings and NIX_CONFIG cannot be overridden.
              '';
            };
          };
        };
        description = ''
          Additive worker Pod configuration, validated by the autoscaler using Kubernetes API types.
          Configure Secret references, never secret values. This is trusted operator configuration;
          Kubernetes admission policy still controls access to privileged volume types.
        '';
      };
    };
  };

  kubernetesOptions = {
    options = {
      apiServer = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "https://k8s.example.com:6443";
        description = ''
          URL of the Kubernetes API server.
          `null` reads {env}`KUBERNETES_SERVICE_HOST` and {env}`KUBERNETES_SERVICE_PORT` as set inside a pod.
        '';
      };

      tokenFile = mkOption {
        type = types.str;
        example = "/run/secrets/kubernetes-token";
        description = ''
          Path to a file holding a bearer token for a service account that may create, read, list and delete
          pods and create secrets in {option}`${option "kubernetes.namespace"}`.
        '';
      };

      caFile = mkOption {
        type = types.nullOr types.path;
        default = null;
        description = "Path to the CA certificate of the API server. `null` trusts the system certificates.";
      };

      namespace = mkOption {
        type = types.str;
        example = "gradient-workers";
        description = "Namespace the worker pods and their token secrets live in.";
      };

      image = mkOption {
        type = types.str;
        example = "registry.example.com/gradient-worker:2.0.0";
        description = ''
          Container image of the worker pods, for example built from the `gradient-worker-image` package.
          Match its Gradient version to the server.
        '';
      };

      imagePullSecrets = mkOption {
        type = types.listOf types.str;
        default = [ ];
        description = "Names of the secrets in the namespace used to pull {option}`${option "kubernetes.image"}`.";
      };

      pools = mkOption {
        type = types.attrsOf (types.submodule poolOptions);
        example = literalExpression ''
          {
            x86 = { system = "x86_64-linux"; features = [ "big-parallel" ]; cpu = "8"; memory = "32Gi"; };
            arm = { system = "aarch64-linux"; nodeSelector."kubernetes.io/arch" = "arm64"; };
          }
        '';
        description = ''
          Kinds of worker pods the service may start.
          The attribute name names the pool and may only hold `a-z`, `0-9` and `-`.
        '';
      };
    };
  };

  connectionOptions = { name, ... }: {
    options = {
      gradientUrl = mkOption {
        type = types.str;
        example = "https://gradient.example.com";
        description = ''
          Base URL of the Gradient server.
          Workers connect to its `/proto` endpoint, so it must be reachable from them.
        '';
      };

      team = mkOption {
        type = types.str;
        example = "platform";
        description = ''
          Name of the Gradient team whose workers the launches provide.
          Projects receive them through their grant of the team's workers.
        '';
      };

      apiKeyFile = mkOption {
        type = types.str;
        example = "/run/secrets/gradient-api-key";
        description = ''
          Path to a file holding a Gradient API key of a user with the Admin role in {option}`${option "team"}`.
          The service reads it through a systemd credential, so it stays out of the Nix store.
        '';
      };

      projects = mkOption {
        type = types.nullOr (types.listOf types.str);
        default = null;
        example = [ "website" "backend" ];
        description = ''
          Names or IDs of the granting projects whose waiting jobs may start workers.
          `null` counts every project granting the team its workers.
        '';
      };

      maxLaunches = mkOption {
        type = types.ints.positive;
        default = 4;
        description = "Maximum number of workers that are pending or running at the same time.";
      };

      maxLaunchesPerKind = mkOption {
        type = types.ints.positive;
        default = 2;
        description = "Maximum number of workers per runner label or pool that are pending or running at the same time.";
      };

      workerSubstituters = mkOption {
        type = types.listOf types.str;
        default = [ ];
        example = [ "https://gradient.example.com/cache/main" ];
        description = ''
          Binary caches the workers add to their Nix configuration.
          A cache holding the worker closure keeps a GitHub Actions run from building Gradient from source.
        '';
      };

      workerTrustedPublicKeys = mkOption {
        type = types.listOf types.str;
        default = [ ];
        example = [ "gradient.example.com-1:AAAA..." ];
        description = "Public keys that sign the paths in {option}`${option "workerSubstituters"}`.";
      };

      workerCapabilities = mkOption {
        default = { };
        type = types.submodule {
          options = lib.genAttrs [ "build" "network" "secrets" "eval" ] (role: mkEnableOption "the ${role} worker capability" // {
            default = true;
          }) // {
            exec = mkEnableOption "the exec worker capability";
          };
        };
        description = "Capabilities granted to workers.";
      };

      idleTimeout = mkOption {
        type = types.ints.positive;
        default = 120;
        description = "Seconds without an assigned job after which a worker drains and exits.";
      };

      maxLifetime = mkOption {
        type = types.ints.positive;
        default = 18000;
        description = ''
          Seconds after which a worker drains, finishes its running jobs and exits.
          GitHub stops a job after 6 hours, so keep room for installing Nix and fetching the worker.
        '';
      };

      githubActions = mkOption {
        type = types.nullOr (types.submodule githubActionsOptions);
        default = null;
        description = "Starts the workers as GitHub Actions runs. Set either this or {option}`${option "kubernetes"}`.";
      };

      kubernetes = mkOption {
        type = types.nullOr (types.submodule kubernetesOptions);
        default = null;
        description = "Starts the workers as Kubernetes pods. Set either this or {option}`${option "githubActions"}`.";
      };
    };
  };

  launcherConfig = name: connection:
    if connection.githubActions != null then {
      githubActions = {
        inherit (connection.githubActions) repository allowedKinds workerFlake;
        tokenFile = "${credentialsDir}/${name}-github-token";
      };
    } else {
      kubernetes = {
        inherit (connection.kubernetes) apiServer caFile namespace image imagePullSecrets pools;
        tokenFile = "${credentialsDir}/${name}-kubernetes-token";
      };
    };

  configFile = (pkgs.formats.json { }).generate "gradient-autoscaler.json" {
    inherit (cfg) listen publicUrl catchUpInterval;
    stateDir = "/var/lib/gradient-autoscaler";
    connections = mapAttrsToList (name: connection: {
      inherit name;
      inherit (connection)
        gradientUrl team projects maxLaunches maxLaunchesPerKind
        workerSubstituters workerTrustedPublicKeys workerCapabilities idleTimeout maxLifetime;
      apiKeyFile = "${credentialsDir}/${name}-api-key";
    } // launcherConfig name connection) cfg.connections;
  };

  credentials = name: connection:
    [ "${name}-api-key:${connection.apiKeyFile}" ]
    ++ optionals (connection.githubActions != null) [ "${name}-github-token:${connection.githubActions.tokenFile}" ]
    ++ optionals (connection.kubernetes != null) [ "${name}-kubernetes-token:${connection.kubernetes.tokenFile}" ];
in
{
  options.services.gradient-autoscaler = {
    enable = mkEnableOption "the service that starts temporary Gradient team workers as GitHub Actions runs or Kubernetes pods";

    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.gradient-autoscaler;
      defaultText = literalExpression "gradient-hypervisor.packages.\${pkgs.stdenv.hostPlatform.system}.gradient-autoscaler";
      description = "Package providing {command}`gradient-autoscaler`.";
    };

    listen = mkOption {
      type = types.str;
      default = "127.0.0.1:8080";
      example = "[::]:8080";
      description = "Address and port of the HTTP listener for webhooks and the worker token swap.";
    };

    publicUrl = mkOption {
      type = types.str;
      example = "https://autoscaler.example.com";
      description = ''
        URL under which Gradient, GitHub and GitHub-hosted runners reach {option}`services.gradient-autoscaler.listen`.
        Each connection uses the paths `/c/<name>/gradient`, `/c/<name>/github` and `/c/<name>/swap` below it.
      '';
    };

    catchUpInterval = mkOption {
      type = types.ints.positive;
      default = 300;
      description = ''
        Seconds between two catch-up checks.
        A check repairs the setup, closes launches whose end went unnoticed and recomputes the demand.
      '';
    };

    connections = mkOption {
      type = types.attrsOf (types.submodule connectionOptions);
      default = { };
      example = literalExpression ''
        {
          platform = {
            gradientUrl = "https://gradient.example.com";
            team = "platform";
            apiKeyFile = "/run/secrets/gradient-api-key";
            githubActions = {
              tokenFile = "/run/secrets/github-token";
              repository = "acme/gradient-runners";
            };
          };
        }
      '';
      description = ''
        Connections between a Gradient team and the place its workers start.
        The attribute name names the connection and may only hold `a-z`, `0-9` and `-`.
      '';
    };
  };

  config = mkIf cfg.enable {
    assertions = mapAttrsToList (name: connection: {
      assertion = (connection.githubActions == null) != (connection.kubernetes == null);
      message = "services.gradient-autoscaler.connections.${name}: set exactly one of githubActions and kubernetes";
    }) cfg.connections;

    systemd.services.gradient-autoscaler = {
      description = "Gradient autoscaler";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ];
      environment.GRADIENT_AUTOSCALER_CONFIG = configFile;

      serviceConfig = {
        ExecStart = lib.getExe cfg.package;
        DynamicUser = true;
        StateDirectory = "gradient-autoscaler";
        LoadCredential = lib.flatten (mapAttrsToList credentials cfg.connections);
        Restart = "on-failure";
        RestartSec = 5;

        CapabilityBoundingSet = "";
        LockPersonality = true;
        NoNewPrivileges = true;
        PrivateDevices = true;
        PrivateTmp = true;
        ProtectClock = true;
        ProtectControlGroups = true;
        ProtectHome = true;
        ProtectHostname = true;
        ProtectKernelLogs = true;
        ProtectKernelModules = true;
        ProtectKernelTunables = true;
        ProtectSystem = "strict";
        RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" ];
        RestrictNamespaces = true;
        RestrictRealtime = true;
        SystemCallArchitectures = "native";
        SystemCallFilter = [ "@system-service" "~@privileged" ];
      };
    };
  };
}
