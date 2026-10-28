{ pkgs, runCommand, jq, nixosModule }:
let
  evaluated = import (pkgs.path + "/nixos/lib/eval-config.nix") {
    inherit pkgs;
    system = pkgs.stdenv.hostPlatform.system;
    modules = [
      nixosModule
      {
        system.stateVersion = "26.05";
        services.gradient-autoscaler = {
          enable = true;
          publicUrl = "https://autoscaler.example.com";
          connections.platform = {
            gradientUrl = "https://gradient.example.com";
            team = "platform";
            apiKeyFile = "/run/secrets/gradient-api-key";
            kubernetes = {
              tokenFile = "/run/secrets/kubernetes-token";
              namespace = "gradient-workers";
              image = "registry.example.com/gradient-worker:2.0.1";
              pools.x86 = {
                system = "x86_64-linux";
                podOptions = {
                  volumes = [
                    {
                      name = "credentials";
                      secret.secretName = "worker-credentials";
                    }
                  ];
                  volumeMounts = [
                    {
                      name = "credentials";
                      mountPath = "/run/secrets/worker";
                      readOnly = true;
                    }
                  ];
                  env = [
                    {
                      name = "NODE_NAME";
                      valueFrom.fieldRef.fieldPath = "spec.nodeName";
                    }
                  ];
                };
              };
            };
          };
        };
      }
    ];
  };
  configFile = evaluated.config.systemd.services.gradient-autoscaler.environment.GRADIENT_AUTOSCALER_CONFIG;
in
runCommand "gradient-autoscaler-module" { nativeBuildInputs = [ jq ]; } ''
  jq -e '
    .connections[0].kubernetes.pools.x86.podOptions |
    .volumes[0].secret.secretName == "worker-credentials" and
    .volumeMounts[0].readOnly == true and
    .env[0].valueFrom.fieldRef.fieldPath == "spec.nodeName"
  ' ${configFile} >$out
''
