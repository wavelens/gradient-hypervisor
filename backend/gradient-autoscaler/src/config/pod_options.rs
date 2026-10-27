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
            if !volume_name(name) {
                bail!("volume name {name:?} must be a Kubernetes DNS label");
            }
            if [PEERS_VOLUME, STATE_VOLUME].contains(&name) {
                bail!("volume {name:?} is controlled by the autoscaler");
            }
            if !volumes.insert(name) {
                bail!("volume {name:?} is defined twice");
            }
            // Kubernetes serializes only the selected source fields and the name.
            let fields = serde_json::to_value(volume)?;
            if !fields.as_object().is_some_and(|fields| fields.len() == 2) {
                bail!("volume {name:?} must have exactly one source");
            }
        }

        let mut paths: Vec<&Path> = Vec::new();
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
            for reserved in [PEERS_DIR, STATE_DIR] {
                if overlaps(path, Path::new(reserved)) {
                    bail!(
                        "mount path {:?} overlaps autoscaler state or credentials",
                        mount.mount_path
                    );
                }
            }
            if paths.iter().any(|other| overlaps(path, other)) {
                bail!(
                    "mount path {:?} overlaps another additional mount",
                    mount.mount_path
                );
            }
            paths.push(path);
            if mount.sub_path.is_some() && mount.sub_path_expr.is_some() {
                bail!("mount {:?} sets both subPath and subPathExpr", mount.name);
            }
            for sub_path in [mount.sub_path.as_deref(), mount.sub_path_expr.as_deref()]
                .into_iter()
                .flatten()
            {
                let path = Path::new(sub_path);
                if path.is_absolute() || has_parent(path) {
                    bail!(
                        "mount {:?} subPath must be relative without '..'",
                        mount.name
                    );
                }
            }
        }

        let mut names = HashSet::new();
        for env in &self.env {
            let name = env.name.as_str();
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|byte| (b' '..=b'~').contains(&byte) && byte != b'=')
            {
                bail!("environment name {name:?} must be printable ASCII without '='");
            }
            if CONTROLLED_ENV.contains(&name) {
                bail!("environment {name:?} is controlled by the autoscaler");
            }
            if !names.insert(name) {
                bail!("environment {name:?} is defined twice");
            }
            if let Some(source) = &env.value_from {
                if env.value.is_some() {
                    bail!("environment {name:?} sets both value and valueFrom");
                }
                let fields = serde_json::to_value(source)?;
                if !fields.as_object().is_some_and(|fields| fields.len() == 1) {
                    bail!("environment {name:?} valueFrom must have exactly one source");
                }
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

fn volume_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name.as_bytes()[name.len() - 1].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
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

    fn error(value: Value) -> String {
        format!("{:#}", validate(value).unwrap_err())
    }

    #[test]
    fn standard_volumes_mounts_and_environment_references_are_valid() {
        validate(json!({
            "volumes": [
                {"name": "fetch-auth", "secret": {"secretName": "fetch-auth", "items": [{"key": "netrc", "path": "merged.netrc"}]}},
                {"name": "config", "configMap": {"name": "worker-config"}},
                {"name": "scratch", "emptyDir": {"sizeLimit": "10Gi"}},
                {"name": "cache", "persistentVolumeClaim": {"claimName": "cache"}}
            ],
            "volumeMounts": [
                {"name": "fetch-auth", "mountPath": "/creds", "readOnly": true},
                {"name": "config", "mountPath": "/etc/worker", "readOnly": true},
                {"name": "scratch", "mountPath": "/build"},
                {"name": "cache", "mountPath": "/cache"}
            ],
            "env": [
                {"name": "NIX_REMOTE", "value": "daemon"},
                {"name": "TOKEN", "valueFrom": {"secretKeyRef": {"name": "worker-auth", "key": "token"}}},
                {"name": "SETTING", "valueFrom": {"configMapKeyRef": {"name": "worker-config", "key": "setting"}}},
                {"name": "NODE_NAME", "valueFrom": {"fieldRef": {"fieldPath": "spec.nodeName"}}},
                {"name": "CPU_LIMIT", "valueFrom": {"resourceFieldRef": {"resource": "limits.cpu"}}}
            ]
        })).unwrap();
    }

    #[test]
    fn volume_names_are_unique_and_cannot_replace_generated_volumes() {
        for name in [PEERS_VOLUME, STATE_VOLUME] {
            assert!(
                error(json!({"volumes": [{"name": name, "emptyDir": {}}]})).contains("controlled")
            );
        }
        assert!(
            error(json!({"volumes": [
                {"name": "cache", "emptyDir": {}},
                {"name": "cache", "emptyDir": {}}
            ]}))
            .contains("twice")
        );
    }

    #[test]
    fn volume_requires_a_valid_name_and_exactly_one_source() {
        for name in ["", "-cache", "cache-", "Bad", "cache.name", &"a".repeat(64)] {
            assert!(
                error(json!({"volumes": [{"name": name, "emptyDir": {}}]})).contains("DNS label")
            );
        }
        for volume in [
            json!({"name": "cache"}),
            json!({"name": "cache", "emptyDir": {}, "secret": {"secretName": "auth"}}),
        ] {
            assert!(error(json!({"volumes": [volume]})).contains("exactly one source"));
        }
    }

    #[test]
    fn mounts_cannot_reuse_generated_volumes_or_hide_generated_paths() {
        assert!(
            error(json!({"volumeMounts": [{"name": "peers", "mountPath": "/creds"}]}))
                .contains("additional volume")
        );
        for path in [
            "/",
            "/run",
            PEERS_DIR,
            "/run//secrets/./gradient/peers",
            STATE_DIR,
            "/var/lib/gradient-worker/child",
        ] {
            assert!(
                error(json!({
                    "volumes": [{"name": "auth", "emptyDir": {}}],
                    "volumeMounts": [{"name": "auth", "mountPath": path}]
                }))
                .contains("overlaps autoscaler")
            );
        }
    }

    #[test]
    fn mount_paths_must_be_absolute_and_non_overlapping() {
        for path in ["relative", "/creds/../run"] {
            assert!(
                error(json!({
                    "volumes": [{"name": "auth", "emptyDir": {}}],
                    "volumeMounts": [{"name": "auth", "mountPath": path}]
                }))
                .contains("must be absolute")
            );
        }
        for path in ["/creds", "/creds/", "/creds/child", "/creds//./child"] {
            assert!(
                error(json!({
                    "volumes": [{"name": "auth", "emptyDir": {}}],
                    "volumeMounts": [
                        {"name": "auth", "mountPath": "/creds"},
                        {"name": "auth", "mountPath": path}
                    ]
                }))
                .contains("another additional mount")
            );
        }
    }

    #[test]
    fn subpaths_cannot_escape_a_volume_or_set_both_forms() {
        for fields in [
            json!({"subPath": "/absolute"}),
            json!({"subPath": "../outside"}),
            json!({"subPath": "file", "subPathExpr": "$(FILE)"}),
        ] {
            let mut mount = json!({"name": "auth", "mountPath": "/creds"});
            mount
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            assert!(
                error(json!({
                    "volumes": [{"name": "auth", "emptyDir": {}}],
                    "volumeMounts": [mount]
                }))
                .contains("subPath")
            );
        }
    }

    #[test]
    fn environment_names_and_sources_are_unambiguous() {
        for env in [
            json!({"name": "NIX_CONFIG", "value": "override"}),
            json!({"name": ""}),
            json!({"name": "A=B"}),
            json!({"name": "A\nB"}),
            json!({"name": "TOKEN", "value": "x", "valueFrom": {"secretKeyRef": {"name": "s", "key": "k"}}}),
            json!({"name": "TOKEN", "valueFrom": {}}),
            json!({"name": "TOKEN", "valueFrom": {
                "secretKeyRef": {"name": "s", "key": "k"},
                "fieldRef": {"fieldPath": "spec.nodeName"}
            }}),
        ] {
            assert!(validate(json!({"env": [env]})).is_err());
        }
        assert!(error(json!({"env": [{"name": "A"}, {"name": "A"}]})).contains("twice"));
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
            assert!(validate(value).is_err());
        }
    }
}
