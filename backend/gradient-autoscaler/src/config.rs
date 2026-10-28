use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use url::Url;

pub mod pod_options;
use pod_options::PodOptions;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    pub public_url: Url,
    pub state_dir: PathBuf,
    #[serde(default = "default_catch_up_interval")]
    pub catch_up_interval: u64,
    #[serde(default)]
    pub connections: Vec<Connection>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "ConnectionFields")]
pub struct Connection {
    pub name: String,
    pub gradient_url: Url,
    pub team: String,
    pub api_key_file: PathBuf,
    pub projects: Option<Vec<String>>,
    pub max_launches: u32,
    pub max_launches_per_kind: u32,
    pub worker_substituters: Vec<String>,
    pub worker_trusted_public_keys: Vec<String>,
    pub worker_capabilities: WorkerCapabilities,
    pub idle_timeout: u64,
    pub max_lifetime: u64,
    pub launcher: LauncherConfig,
}

#[derive(Debug, Clone)]
pub enum LauncherConfig {
    GitHubActions(GitHubActions),
    Kubernetes(Kubernetes),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerCapabilities {
    #[serde(rename(serialize = "enable_build"))]
    pub build: bool,
    #[serde(rename(serialize = "enable_network"))]
    pub network: bool,
    #[serde(rename(serialize = "enable_secrets"))]
    pub secrets: bool,
    #[serde(rename(serialize = "enable_exec"))]
    pub exec: bool,
    #[serde(rename(serialize = "enable_eval"))]
    pub eval: bool,
}

impl Default for WorkerCapabilities {
    fn default() -> Self {
        Self {
            build: true,
            network: true,
            secrets: true,
            exec: false,
            eval: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitHubActions {
    pub token_file: PathBuf,
    pub repository: String,
    #[serde(default = "default_allowed_kinds")]
    pub allowed_kinds: Vec<String>,
    #[serde(default)]
    pub worker_flake: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Kubernetes {
    #[serde(default)]
    pub api_server: Option<Url>,
    #[serde(default = "default_service_account_token")]
    pub token_file: PathBuf,
    #[serde(default)]
    pub ca_file: Option<PathBuf>,
    pub namespace: String,
    pub image: String,
    #[serde(default)]
    pub image_pull_secrets: Vec<String>,
    pub pools: BTreeMap<String, Pool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Pool {
    pub system: String,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default = "default_pool_max_jobs")]
    pub max_jobs: u32,
    #[serde(default)]
    pub cpu: Option<String>,
    #[serde(default)]
    pub memory: Option<String>,
    #[serde(default)]
    pub node_selector: BTreeMap<String, String>,
    #[serde(default)]
    pub tolerations: Vec<serde_json::Value>,
    #[serde(default)]
    pub privileged: bool,
    #[serde(default)]
    pub pod_options: PodOptions,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConnectionFields {
    name: String,
    gradient_url: Url,
    team: String,
    api_key_file: PathBuf,
    #[serde(default)]
    projects: Option<Vec<String>>,
    #[serde(default = "default_max_launches")]
    max_launches: u32,
    #[serde(default = "default_max_launches_per_kind")]
    max_launches_per_kind: u32,
    #[serde(default)]
    worker_substituters: Vec<String>,
    #[serde(default)]
    worker_trusted_public_keys: Vec<String>,
    #[serde(default)]
    worker_capabilities: WorkerCapabilities,
    #[serde(default = "default_idle_timeout")]
    idle_timeout: u64,
    #[serde(default = "default_max_lifetime")]
    max_lifetime: u64,
    #[serde(default)]
    github_actions: Option<GitHubActions>,
    #[serde(default)]
    kubernetes: Option<Kubernetes>,
}

fn default_catch_up_interval() -> u64 {
    300
}

fn default_allowed_kinds() -> Vec<String> {
    vec!["ubuntu-latest".into(), "ubuntu-24.04-arm".into()]
}

fn default_max_launches() -> u32 {
    4
}

fn default_max_launches_per_kind() -> u32 {
    2
}

fn default_idle_timeout() -> u64 {
    120
}

fn default_max_lifetime() -> u64 {
    18000
}

fn default_pool_max_jobs() -> u32 {
    4
}

fn default_service_account_token() -> PathBuf {
    PathBuf::from("/var/run/secrets/kubernetes.io/serviceaccount/token")
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        Self::parse(&raw)
    }

    pub fn parse(raw: &str) -> anyhow::Result<Config> {
        let config: Config = serde_json::from_str(raw).context("parsing config")?;
        let mut names = HashSet::new();
        for connection in &config.connections {
            if !names.insert(connection.name.as_str()) {
                bail!("connection name {:?} is used twice", connection.name);
            }
        }

        Ok(config)
    }
}

impl TryFrom<ConnectionFields> for Connection {
    type Error = anyhow::Error;

    fn try_from(fields: ConnectionFields) -> anyhow::Result<Connection> {
        let name = fields.name;
        if !is_lowercase_name(&name) {
            bail!("connection name {name:?} may only hold a-z, 0-9 and -");
        }

        let launcher = match (fields.github_actions, fields.kubernetes) {
            (Some(github), None) => LauncherConfig::GitHubActions(github.validated(&name)?),
            (None, Some(kubernetes)) => LauncherConfig::Kubernetes(kubernetes.validated(&name)?),
            _ => bail!("connection {name}: set exactly one of githubActions and kubernetes"),
        };
        Ok(Connection {
            name,
            gradient_url: fields.gradient_url,
            team: fields.team,
            api_key_file: fields.api_key_file,
            projects: fields.projects,
            max_launches: fields.max_launches,
            max_launches_per_kind: fields.max_launches_per_kind,
            worker_substituters: fields.worker_substituters,
            worker_trusted_public_keys: fields.worker_trusted_public_keys,
            worker_capabilities: fields.worker_capabilities,
            idle_timeout: fields.idle_timeout,
            max_lifetime: fields.max_lifetime,
            launcher,
        })
    }
}

impl GitHubActions {
    fn validated(self, connection: &str) -> anyhow::Result<GitHubActions> {
        let parts = self.repository.split('/').filter(|part| !part.is_empty());
        if parts.count() != 2 {
            bail!(
                "connection {connection}: repository {:?} is not owner/name",
                self.repository
            );
        }

        Ok(self)
    }

    pub fn owner(&self) -> &str {
        self.repository.split('/').next().unwrap_or_default()
    }
}

impl Kubernetes {
    fn validated(self, connection: &str) -> anyhow::Result<Kubernetes> {
        if self.pools.is_empty() {
            bail!("connection {connection}: kubernetes needs at least one pool");
        }

        if let Some(label) = self.pools.keys().find(|label| !is_lowercase_name(label)) {
            bail!("connection {connection}: pool name {label:?} may only hold a-z, 0-9 and -");
        }

        if let Some((label, _)) = self.pools.iter().find(|(_, pool)| pool.max_jobs == 0) {
            bail!("connection {connection}: pool {label}: maxJobs must be greater than zero");
        }

        for (label, pool) in &self.pools {
            pool.pod_options.validate().map_err(|error| {
                anyhow::anyhow!("connection {connection}: pool {label}: podOptions: {error:#}")
            })?;
        }

        Ok(self)
    }
}

fn is_lowercase_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    const GITHUB: &str = r#""githubActions":{"tokenFile":"/g","repository":"acme/runners"}"#;
    const KUBERNETES: &str = r#""kubernetes":{"namespace":"ci","image":"worker:1","pools":{"x86":{"system":"x86_64-linux"}}}"#;

    fn with_connections(connections: &str) -> String {
        format!(
            r#"{{"listen":"127.0.0.1:8080","publicUrl":"https://scaler.example.com","stateDir":"/tmp/x","connections":[{connections}]}}"#
        )
    }

    fn connection(name: &str, launcher: &str) -> String {
        format!(
            r#"{{"name":"{name}","gradientUrl":"https://gradient.example.com","team":"ops","apiKeyFile":"/k",{launcher}}}"#
        )
    }

    fn error_of(connections: &str) -> String {
        format!(
            "{:#}",
            Config::parse(&with_connections(connections)).unwrap_err()
        )
    }

    #[test]
    fn config_rejects_duplicate_connection_names() {
        let both = format!(
            "{},{}",
            connection("a", GITHUB),
            connection("a", KUBERNETES)
        );
        assert!(error_of(&both).contains("used twice"));
    }

    #[test]
    fn config_rejects_names_outside_the_url_safe_set() {
        assert!(error_of(&connection("Team_A", GITHUB)).contains("may only hold"));
    }

    #[test]
    fn config_rejects_repository_without_owner() {
        let launcher = GITHUB.replace("acme/runners", "runners");
        assert!(error_of(&connection("a", &launcher)).contains("owner/name"));
    }

    #[test]
    fn config_needs_exactly_one_launcher() {
        let both = format!("{GITHUB},{KUBERNETES}");
        for launcher in [both.as_str(), r#""projects":null"#] {
            assert!(error_of(&connection("a", launcher)).contains("exactly one"));
        }
    }

    #[test]
    fn kubernetes_rejects_invalid_pool_configuration() {
        let pools = r#"{"x86":{"system":"x86_64-linux"}}"#;
        for (launcher, message) in [
            (KUBERNETES.replace(pools, "{}"), "at least one pool"),
            (KUBERNETES.replace("\"x86\"", "\"X86\""), "pool name"),
            (
                KUBERNETES.replace(
                    "\"system\":\"x86_64-linux\"",
                    "\"system\":\"x86_64-linux\",\"podOptions\":{\"env\":[{\"name\":\"GRADIENT_WORKER_ID\",\"value\":\"override\"}]}",
                ),
                "controlled by the autoscaler",
            ),
        ] {
            assert!(error_of(&connection("a", &launcher)).contains(message));
        }
    }

    #[test]
    fn partial_worker_capabilities_preserve_unspecified_defaults() {
        for (capabilities, exec) in [
            (r#"{"eval":false,"secrets":false}"#, false),
            (r#"{"eval":false,"secrets":false,"exec":true}"#, true),
        ] {
            let entry = connection("a", KUBERNETES).replace(
                "\"name\":\"a\"",
                &format!("\"name\":\"a\",\"workerCapabilities\":{capabilities}"),
            );
            let config = Config::parse(&with_connections(&entry)).unwrap();
            assert_eq!(
                config.connections[0].worker_capabilities,
                WorkerCapabilities {
                    eval: false,
                    secrets: false,
                    exec,
                    ..Default::default()
                }
            );
        }
    }

    #[test]
    fn omitted_and_empty_worker_capabilities_preserve_defaults() {
        for launcher in [GITHUB, KUBERNETES] {
            let entry = connection("a", launcher);
            for entry in [
                entry.clone(),
                entry.replace("\"name\":\"a\"", "\"name\":\"a\",\"workerCapabilities\":{}"),
            ] {
                let config = Config::parse(&with_connections(&entry)).unwrap();
                assert_eq!(
                    config.connections[0].worker_capabilities,
                    WorkerCapabilities {
                        build: true,
                        network: true,
                        secrets: true,
                        exec: false,
                        eval: true,
                    }
                );
            }
        }
    }

    #[test]
    fn worker_capabilities_reject_unknown_roles_and_non_booleans() {
        for capabilities in [
            r#"{"secret":false}"#,
            r#"{"eval":"false"}"#,
            r#"{"exec":"false"}"#,
            r#"{"enable_build":false}"#,
        ] {
            let entry = connection("a", KUBERNETES).replace(
                "\"name\":\"a\"",
                &format!("\"name\":\"a\",\"workerCapabilities\":{capabilities}"),
            );
            assert!(Config::parse(&with_connections(&entry)).is_err());
        }
    }

    #[test]
    fn kubernetes_rejects_zero_build_slots() {
        let zero = KUBERNETES.replace(
            r#""system":"x86_64-linux""#,
            r#""system":"x86_64-linux","maxJobs":0"#,
        );
        assert!(error_of(&connection("a", &zero)).contains("maxJobs must be greater than zero"));
    }
}
