#![allow(dead_code)]

use gradient_autoscaler::config::{Connection, GitHubActions, Kubernetes, LauncherConfig};
use gradient_autoscaler::github::GitHubLauncher;
use gradient_autoscaler::github::client::GitHubClient;
use gradient_autoscaler::gradient::GradientClient;
use gradient_autoscaler::kubernetes::KubernetesLauncher;
use gradient_autoscaler::kubernetes::client::KubernetesClient;
use gradient_autoscaler::launcher::Launcher;
use gradient_autoscaler::service::ConnectionService;
use gradient_autoscaler::state::State;
use serde_json::json;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::NamedTempFile;
use url::Url;

pub const PUBLIC_URL: &str = "https://scaler.example.com";

pub fn github_actions(repository: &str) -> GitHubActions {
    GitHubActions {
        token_file: PathBuf::from("/g"),
        repository: repository.into(),
        allowed_kinds: vec!["ubuntu-latest".into(), "ubuntu-24.04-arm".into()],
        worker_flake: Some("github:wavelens/gradient/v2.0.0#gradient".into()),
    }
}

pub fn kubernetes() -> Kubernetes {
    serde_json::from_value(json!({
        "namespace": "ci",
        "image": "registry.example.com/gradient-worker:2.0.0",
        "pools": {
            "arm": { "system": "aarch64-linux", "memory": "8Gi" },
            "x86": { "system": "x86_64-linux", "features": ["kvm"], "cpu": "4" },
        },
    }))
    .unwrap()
}

pub fn connection(name: &str, gradient_url: &str, launcher: LauncherConfig) -> Connection {
    Connection {
        name: name.into(),
        gradient_url: Url::parse(gradient_url).unwrap(),
        team: "ops".into(),
        api_key_file: PathBuf::from("/k"),
        projects: None,
        max_launches: 4,
        max_launches_per_kind: 2,
        worker_substituters: vec!["https://cache.example.com".into()],
        worker_trusted_public_keys: vec!["cache.example.com-1:abc=".into()],
        worker_capabilities: Default::default(),
        idle_timeout: 120,
        max_lifetime: 21600,
        launcher,
    }
}

pub async fn github_service(server_uri: &str) -> Arc<ConnectionService> {
    let url = Url::parse(server_uri).unwrap();
    let config = github_actions("acme/runners");
    let client = GitHubClient::with_api_url(url.clone(), "ghp".into(), "acme/runners".into());
    Arc::new(ConnectionService::new(
        connection(
            "ops",
            server_uri,
            LauncherConfig::GitHubActions(config.clone()),
        ),
        Url::parse(PUBLIC_URL).unwrap(),
        GradientClient::new(url, "GRADkey".into()),
        Launcher::GitHub(GitHubLauncher::new(config, client)),
        State::memory().await.unwrap(),
    ))
}

pub struct KubernetesService {
    pub svc: Arc<ConnectionService>,
    _token: NamedTempFile,
}

pub async fn kubernetes_service(server_uri: &str) -> KubernetesService {
    kubernetes_service_with_config(server_uri, kubernetes()).await
}

pub async fn kubernetes_service_with_config(
    server_uri: &str,
    config: Kubernetes,
) -> KubernetesService {
    let url = Url::parse(server_uri).unwrap();
    let mut token = NamedTempFile::new().unwrap();
    writeln!(token, "service-account-token").unwrap();
    let client = KubernetesClient::with_api_url(url.clone(), token.path().to_owned(), "ci");
    let svc = Arc::new(ConnectionService::new(
        connection(
            "ops",
            server_uri,
            LauncherConfig::Kubernetes(config.clone()),
        ),
        Url::parse(PUBLIC_URL).unwrap(),
        GradientClient::new(url, "GRADkey".into()),
        Launcher::Kubernetes(KubernetesLauncher::new(config, client)),
        State::memory().await.unwrap(),
    ));
    KubernetesService { svc, _token: token }
}
