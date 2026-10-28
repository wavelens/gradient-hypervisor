use anyhow::bail;
use k8s_openapi::api::core::v1::{EnvVar, Volume, VolumeMount};
use serde::{
    Deserialize, Deserializer,
    de::{DeserializeOwned, Error},
};
use std::collections::HashSet;
use std::path::{Component, Path};

pub(crate) const PEERS_VOLUME: &str = "peers";
pub(crate) const STATE_VOLUME: &str = "state";
pub(crate) const PEERS_DIR: &str = "/run/secrets/gradient";
pub(crate) const STATE_DIR: &str = "/var/lib/gradient-worker";
pub(crate) const CONTROLLED_ENV: &[&str] = &[
    "GRADIENT_WORKER_SERVER_URL",
    "GRADIENT_WORKER_ID",
    "GRADIENT_WORKER_PEERS_FILE",
    "GRADIENT_WORKER_BASE_DIR",
    "GRADIENT_WORKER_CAPABILITIES_BUILD",
    "GRADIENT_WORKER_CAPABILITIES_NETWORK",
    "GRADIENT_WORKER_CAPABILITIES_SECRETS",
    "GRADIENT_WORKER_CAPABILITIES_EXEC",
    "GRADIENT_WORKER_CAPABILITIES_EVAL",
    "GRADIENT_WORKER_BUILD_MAX_CONCURRENT",
    "GRADIENT_WORKER_IDLE_TIMEOUT",
    "GRADIENT_WORKER_MAX_LIFETIME",
    "NIX_CONFIG",
];

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct PodOptions {
    #[serde(deserialize_with = "checked")]
    pub volumes: Vec<Volume>,
    #[serde(deserialize_with = "checked")]
    pub volume_mounts: Vec<VolumeMount>,
    #[serde(deserialize_with = "checked")]
    pub env: Vec<EnvVar>,
}

impl PodOptions {
    pub fn validate(&self) -> anyhow::Result<()> {
        let mut volumes = HashSet::new();
        for volume in &self.volumes {
            let name = volume.name.as_str();
            if [PEERS_VOLUME, STATE_VOLUME].contains(&name) {
                bail!("volume {name:?} is controlled by the autoscaler");
            }
            if !volumes.insert(name) {
                bail!("volume {name:?} is defined twice");
            }
        }

        let mut paths = vec![Path::new(PEERS_DIR), Path::new(STATE_DIR)];
        for mount in &self.volume_mounts {
            if !volumes.contains(mount.name.as_str()) {
                bail!("mount {:?} must reference an additional volume", mount.name);
            }
            let path = Path::new(&mount.mount_path);
            if !path.is_absolute() || has_parent(path) {
                bail!(
                    "mount path {:?} must be absolute without '..'",
                    mount.mount_path
                );
            }
            if paths.iter().any(|other| overlaps(path, other)) {
                bail!(
                    "mount path {:?} overlaps another or a reserved mount",
                    mount.mount_path
                );
            }
            paths.push(path);
        }

        let mut names = HashSet::new();
        for env in &self.env {
            let name = env.name.as_str();
            if CONTROLLED_ENV.contains(&name) {
                bail!("environment {name:?} is controlled by the autoscaler");
            }
            if !names.insert(name) {
                bail!("environment {name:?} is defined twice");
            }
        }
        Ok(())
    }
}

fn checked<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    // Generated Kubernetes deserializers otherwise discard misspelled optional fields.
    let value = serde_json::Value::deserialize(deserializer)?;
    let mut unknown = Vec::new();
    let parsed = serde_ignored::deserialize(value, |path| unknown.push(path.to_string()))
        .map_err(D::Error::custom)?;
    if !unknown.is_empty() {
        return Err(D::Error::custom(format!(
            "unknown Kubernetes fields: {}",
            unknown.join(", ")
        )));
    }
    Ok(parsed)
}

fn has_parent(path: &Path) -> bool {
    path.components().any(|part| part == Component::ParentDir)
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use serde_json::{Value, json};

    fn validate(value: Value) -> anyhow::Result<()> {
        serde_json::from_value::<PodOptions>(value)
            .context("parsing pod options")?
            .validate()
    }

    #[test]
    fn names_cannot_conflict_with_generated_fields_or_each_other() {
        for value in [
            json!({"volumes": [{"name": "peers", "emptyDir": {}}]}),
            json!({"volumes": [{"name": "state", "emptyDir": {}}]}),
            json!({"volumes": [
                {"name": "cache", "emptyDir": {}},
                {"name": "cache", "emptyDir": {}}
            ]}),
            json!({"env": [{"name": "NIX_CONFIG", "value": "override"}]}),
            json!({"env": [{"name": "GRADIENT_WORKER_CAPABILITIES_EXEC", "value": "true"}]}),
            json!({"env": [{"name": "EXTRA"}, {"name": "EXTRA"}]}),
            json!({"volumeMounts": [{"name": "peers", "mountPath": "/extra"}]}),
        ] {
            assert!(validate(value).is_err());
        }
    }

    #[test]
    fn mounts_cannot_hide_reserved_paths_or_each_other() {
        for path in [
            "relative",
            "/extra/../run",
            "/",
            "/run",
            PEERS_DIR,
            "/run//secrets/./gradient/peers",
            STATE_DIR,
            "/var/lib/gradient-worker/child",
        ] {
            assert!(
                validate(json!({
                    "volumes": [{"name": "auth", "emptyDir": {}}],
                    "volumeMounts": [{"name": "auth", "mountPath": path}]
                }))
                .is_err()
            );
        }
        for path in ["/creds", "/creds/child"] {
            assert!(
                validate(json!({
                    "volumes": [{"name": "auth", "emptyDir": {}}],
                    "volumeMounts": [
                        {"name": "auth", "mountPath": "/creds"},
                        {"name": "auth", "mountPath": path}
                    ]
                }))
                .is_err()
            );
        }
    }

    #[test]
    fn types_and_top_level_fields_are_checked() {
        for value in [
            json!({"containers": []}),
            json!({"volumes": {}}),
            json!({"volumes": [{"name": "auth", "emptyDir": true}]}),
            json!({"volumeMounts": [{"name": "auth", "mountPath": 1}]}),
            json!({"env": [{"name": "A", "value": 1}]}),
            json!({"volumes": [{"name": "auth", "secret": {"secret_name": "auth"}}]}),
            json!({"volumeMounts": [{"name": "auth", "mountPath": "/creds", "read_only": true}]}),
            json!({"env": [{"name": "A", "value_from": {"secretKeyRef": {"name": "s", "key": "k"}}}]}),
        ] {
            assert!(serde_json::from_value::<PodOptions>(value).is_err());
        }
    }
}
