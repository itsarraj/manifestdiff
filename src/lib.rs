//! Diffs two Kubernetes Deployment manifests and flags the changes that
//! actually matter operationally: a removed env var, a swapped image
//! tag, replicas scaled down (especially to zero), a dropped volume
//! mount, a lost or altered liveness/readiness probe. A plain
//! line-based `diff` on the YAML would also show every label reordering
//! and comment change; this only speaks up about the shape changes
//! above, and stays silent for everything else (a label-only edit
//! produces zero findings).

use std::collections::BTreeMap;

use serde::Deserialize;

// ---------------------------------------------------------------------
// A deliberately small slice of the real Deployment schema — just the
// fields this tool's checks actually look at. Anything else in a real
// manifest (`strategy`, `serviceAccountName`, resource limits, ...) is
// simply ignored by `serde`'s default "unknown fields are fine" mode
// rather than causing a parse error.
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize, Clone)]
pub struct Manifest {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    #[serde(default)]
    pub metadata: Metadata,
    pub spec: DeploymentSpec,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct Metadata {
    pub name: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DeploymentSpec {
    pub replicas: Option<i64>,
    pub template: PodTemplate,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PodTemplate {
    #[serde(default)]
    pub metadata: Metadata,
    pub spec: PodSpec,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PodSpec {
    #[serde(default)]
    pub containers: Vec<Container>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Container {
    pub name: String,
    pub image: String,
    #[serde(default)]
    pub env: Vec<EnvVar>,
    #[serde(rename = "volumeMounts", default)]
    pub volume_mounts: Vec<VolumeMount>,
    #[serde(rename = "livenessProbe", default)]
    pub liveness_probe: Option<serde_yaml::Value>,
    #[serde(rename = "readinessProbe", default)]
    pub readiness_probe: Option<serde_yaml::Value>,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub struct EnvVar {
    pub name: String,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(rename = "valueFrom", default)]
    pub value_from: Option<serde_yaml::Value>,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub struct VolumeMount {
    pub name: String,
    #[serde(rename = "mountPath")]
    pub mount_path: String,
}

pub fn parse_manifest(yaml: &str) -> anyhow::Result<Manifest> {
    Ok(serde_yaml::from_str(yaml)?)
}

// ---------------------------------------------------------------------
// Image reference parsing
// ---------------------------------------------------------------------

/// Splits an image reference into `(repository, tag)`. Only the segment
/// after the *last* `/` is checked for a `:`, so a registry with an
/// explicit port (`registry.example.com:5000/team/app:1.2.3`) doesn't
/// get its port digits mistaken for a tag.
pub fn parse_image(image: &str) -> (String, Option<String>) {
    let last_segment_start = image.rfind('/').map(|i| i + 1).unwrap_or(0);
    let (prefix, last_segment) = image.split_at(last_segment_start);
    match last_segment.rfind(':') {
        Some(colon) => {
            let (name, tag) = last_segment.split_at(colon);
            (format!("{prefix}{name}"), Some(tag[1..].to_string()))
        }
        None => (image.to_string(), None),
    }
}

// ---------------------------------------------------------------------
// Findings
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Risky,
    Breaking,
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub severity: Severity,
    pub message: String,
}

fn finding(severity: Severity, message: impl Into<String>) -> Finding {
    Finding {
        severity,
        message: message.into(),
    }
}

/// Compares `before` and `after` and returns every finding, in a stable
/// order (replicas, then per-container checks in the order containers
/// appear in `before`, then containers added in `after`). Deliberately
/// says nothing about labels, annotations, container ordering, or any
/// field this tool doesn't model — see the README for the full list of
/// what's out of scope.
pub fn diff_manifests(before: &Manifest, after: &Manifest) -> Vec<Finding> {
    let mut findings = Vec::new();

    if before.kind != after.kind || before.api_version != after.api_version {
        findings.push(finding(
            Severity::Breaking,
            format!(
                "resource type changed from {}/{} to {}/{}",
                before.api_version, before.kind, after.api_version, after.kind
            ),
        ));
    }

    check_replicas(before, after, &mut findings);

    let before_containers = &before.spec.template.spec.containers;
    let after_containers = &after.spec.template.spec.containers;

    for b in before_containers {
        match after_containers.iter().find(|a| a.name == b.name) {
            None => findings.push(finding(
                Severity::Breaking,
                format!("container \"{}\" was removed", b.name),
            )),
            Some(a) => diff_container(b, a, &mut findings),
        }
    }
    for a in after_containers {
        if !before_containers.iter().any(|b| b.name == a.name) {
            findings.push(finding(
                Severity::Info,
                format!("container \"{}\" was added", a.name),
            ));
        }
    }

    findings
}

fn check_replicas(before: &Manifest, after: &Manifest, findings: &mut Vec<Finding>) {
    // Kubernetes defaults an omitted `replicas` to 1.
    let before_replicas = before.spec.replicas.unwrap_or(1);
    let after_replicas = after.spec.replicas.unwrap_or(1);
    if after_replicas < before_replicas {
        if after_replicas == 0 {
            findings.push(finding(
                Severity::Breaking,
                format!("replicas reduced from {before_replicas} to 0 — this scales the deployment to nothing"),
            ));
        } else {
            findings.push(finding(
                Severity::Risky,
                format!("replicas reduced from {before_replicas} to {after_replicas}"),
            ));
        }
    }
}

fn diff_container(before: &Container, after: &Container, findings: &mut Vec<Finding>) {
    let name = &before.name;

    if before.image != after.image {
        let (before_repo, before_tag) = parse_image(&before.image);
        let (after_repo, after_tag) = parse_image(&after.image);
        if before_repo != after_repo {
            findings.push(finding(
                Severity::Risky,
                format!(
                    "container \"{name}\": image changed from \"{}\" to \"{}\"",
                    before.image, after.image
                ),
            ));
        } else {
            findings.push(finding(
                Severity::Risky,
                format!(
                    "container \"{name}\": image tag changed from {} to {}",
                    before_tag.as_deref().unwrap_or("<none>"),
                    after_tag.as_deref().unwrap_or("<none>")
                ),
            ));
        }
    }

    let before_env: BTreeMap<&str, &EnvVar> =
        before.env.iter().map(|e| (e.name.as_str(), e)).collect();
    let after_env: BTreeMap<&str, &EnvVar> =
        after.env.iter().map(|e| (e.name.as_str(), e)).collect();
    for (env_name, before_var) in &before_env {
        match after_env.get(env_name) {
            None => findings.push(finding(
                Severity::Breaking,
                format!("container \"{name}\": env var \"{env_name}\" was removed"),
            )),
            Some(after_var) => {
                if before_var.value != after_var.value
                    || before_var.value_from != after_var.value_from
                {
                    findings.push(finding(
                        Severity::Risky,
                        format!("container \"{name}\": env var \"{env_name}\" value changed"),
                    ));
                }
            }
        }
    }
    for env_name in after_env.keys() {
        if !before_env.contains_key(env_name) {
            findings.push(finding(
                Severity::Info,
                format!("container \"{name}\": env var \"{env_name}\" was added"),
            ));
        }
    }

    let before_mounts: BTreeMap<&str, &VolumeMount> = before
        .volume_mounts
        .iter()
        .map(|m| (m.name.as_str(), m))
        .collect();
    let after_mounts: BTreeMap<&str, &VolumeMount> = after
        .volume_mounts
        .iter()
        .map(|m| (m.name.as_str(), m))
        .collect();
    for (mount_name, before_mount) in &before_mounts {
        match after_mounts.get(mount_name) {
            None => findings.push(finding(
                Severity::Breaking,
                format!("container \"{name}\": volumeMount \"{mount_name}\" was removed"),
            )),
            Some(after_mount) => {
                if before_mount.mount_path != after_mount.mount_path {
                    findings.push(finding(
                        Severity::Risky,
                        format!(
                            "container \"{name}\": volumeMount \"{mount_name}\" path changed from {} to {}",
                            before_mount.mount_path, after_mount.mount_path
                        ),
                    ));
                }
            }
        }
    }
    for mount_name in after_mounts.keys() {
        if !before_mounts.contains_key(mount_name) {
            findings.push(finding(
                Severity::Info,
                format!("container \"{name}\": volumeMount \"{mount_name}\" was added"),
            ));
        }
    }

    diff_probe(
        name,
        "livenessProbe",
        &before.liveness_probe,
        &after.liveness_probe,
        findings,
    );
    diff_probe(
        name,
        "readinessProbe",
        &before.readiness_probe,
        &after.readiness_probe,
        findings,
    );
}

fn diff_probe(
    container_name: &str,
    probe_name: &str,
    before: &Option<serde_yaml::Value>,
    after: &Option<serde_yaml::Value>,
    findings: &mut Vec<Finding>,
) {
    match (before, after) {
        (Some(_), None) => findings.push(finding(
            Severity::Breaking,
            format!("container \"{container_name}\": {probe_name} was removed"),
        )),
        (None, Some(_)) => findings.push(finding(
            Severity::Info,
            format!("container \"{container_name}\": {probe_name} was added"),
        )),
        (Some(b), Some(a)) if b != a => findings.push(finding(
            Severity::Risky,
            format!("container \"{container_name}\": {probe_name} configuration changed"),
        )),
        _ => {}
    }
}

pub fn worst_severity(findings: &[Finding]) -> Option<Severity> {
    findings.iter().map(|f| f.severity).max()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_yaml(extra_container_fields: &str, replicas: &str, image_tag: &str) -> String {
        format!(
            r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: my-app
  labels:
    app: my-app
spec:
  replicas: {replicas}
  selector:
    matchLabels:
      app: my-app
  template:
    metadata:
      labels:
        app: my-app
    spec:
      containers:
        - name: app
          image: myrepo/my-app:{image_tag}
          env:
            - name: LOG_LEVEL
              value: info
            - name: DATABASE_URL
              valueFrom:
                secretKeyRef:
                  name: db-secret
                  key: url
          volumeMounts:
            - name: config-volume
              mountPath: /etc/config
          livenessProbe:
            httpGet:
              path: /healthz
              port: 8080
            initialDelaySeconds: 10
          readinessProbe:
            httpGet:
              path: /ready
              port: 8080
{extra_container_fields}
"#
        )
    }

    #[test]
    fn parse_image_handles_bare_tag() {
        assert_eq!(
            parse_image("nginx:1.21"),
            ("nginx".to_string(), Some("1.21".to_string()))
        );
    }

    #[test]
    fn parse_image_handles_no_tag() {
        assert_eq!(parse_image("nginx"), ("nginx".to_string(), None));
    }

    #[test]
    fn parse_image_handles_registry_with_port_and_no_tag() {
        assert_eq!(
            parse_image("registry.example.com:5000/team/app"),
            ("registry.example.com:5000/team/app".to_string(), None)
        );
    }

    #[test]
    fn parse_image_handles_registry_with_port_and_tag() {
        assert_eq!(
            parse_image("registry.example.com:5000/team/app:2.0.1"),
            (
                "registry.example.com:5000/team/app".to_string(),
                Some("2.0.1".to_string())
            )
        );
    }

    #[test]
    fn identical_manifests_produce_no_findings() {
        let yaml = base_yaml("", "3", "1.0.0");
        let before = parse_manifest(&yaml).unwrap();
        let after = parse_manifest(&yaml).unwrap();
        let findings = diff_manifests(&before, &after);
        assert!(
            findings.is_empty(),
            "expected no findings, got {findings:?}"
        );
    }

    #[test]
    fn label_only_change_is_not_flagged() {
        let before = parse_manifest(&base_yaml("", "3", "1.0.0")).unwrap();
        let mut after_yaml = base_yaml("", "3", "1.0.0");
        after_yaml = after_yaml.replace("app: my-app", "app: my-app-renamed-label");
        let after = parse_manifest(&after_yaml).unwrap();
        let findings = diff_manifests(&before, &after);
        assert!(
            findings.is_empty(),
            "label-only changes must not be flagged, got {findings:?}"
        );
    }

    #[test]
    fn detects_removed_env_var() {
        let before = parse_manifest(&base_yaml("", "3", "1.0.0")).unwrap();
        let after_yaml = base_yaml("", "3", "1.0.0").replace(
            "          env:\n            - name: LOG_LEVEL\n              value: info\n            - name: DATABASE_URL\n              valueFrom:\n                secretKeyRef:\n                  name: db-secret\n                  key: url\n",
            "          env:\n            - name: LOG_LEVEL\n              value: info\n",
        );
        let after = parse_manifest(&after_yaml).unwrap();
        let findings = diff_manifests(&before, &after);
        assert!(findings.iter().any(|f| f.severity == Severity::Breaking
            && f.message.contains("DATABASE_URL")
            && f.message.contains("removed")));
    }

    #[test]
    fn detects_changed_image_tag() {
        let before = parse_manifest(&base_yaml("", "3", "1.0.0")).unwrap();
        let after = parse_manifest(&base_yaml("", "3", "2.0.0")).unwrap();
        let findings = diff_manifests(&before, &after);
        assert!(findings.iter().any(|f| f.severity == Severity::Risky
            && f.message.contains("image tag changed from 1.0.0 to 2.0.0")));
    }

    #[test]
    fn detects_reduced_replicas_as_risky() {
        let before = parse_manifest(&base_yaml("", "3", "1.0.0")).unwrap();
        let after = parse_manifest(&base_yaml("", "2", "1.0.0")).unwrap();
        let findings = diff_manifests(&before, &after);
        assert!(findings
            .iter()
            .any(|f| f.severity == Severity::Risky
                && f.message.contains("replicas reduced from 3 to 2")));
    }

    #[test]
    fn detects_replicas_to_zero_as_breaking() {
        let before = parse_manifest(&base_yaml("", "3", "1.0.0")).unwrap();
        let after = parse_manifest(&base_yaml("", "0", "1.0.0")).unwrap();
        let findings = diff_manifests(&before, &after);
        assert!(findings.iter().any(|f| f.severity == Severity::Breaking
            && f.message.contains("replicas reduced from 3 to 0")));
    }

    #[test]
    fn increased_replicas_is_not_flagged() {
        let before = parse_manifest(&base_yaml("", "2", "1.0.0")).unwrap();
        let after = parse_manifest(&base_yaml("", "5", "1.0.0")).unwrap();
        let findings = diff_manifests(&before, &after);
        assert!(!findings.iter().any(|f| f.message.contains("replicas")));
    }

    #[test]
    fn detects_removed_volume_mount() {
        let before = parse_manifest(&base_yaml("", "3", "1.0.0")).unwrap();
        let after_yaml = base_yaml("", "3", "1.0.0").replace(
            "          volumeMounts:\n            - name: config-volume\n              mountPath: /etc/config\n",
            "",
        );
        let after = parse_manifest(&after_yaml).unwrap();
        let findings = diff_manifests(&before, &after);
        assert!(findings.iter().any(|f| f.severity == Severity::Breaking
            && f.message.contains("config-volume")
            && f.message.contains("removed")));
    }

    #[test]
    fn detects_removed_liveness_probe() {
        let before = parse_manifest(&base_yaml("", "3", "1.0.0")).unwrap();
        let after_yaml = base_yaml("", "3", "1.0.0").replace(
            "          livenessProbe:\n            httpGet:\n              path: /healthz\n              port: 8080\n            initialDelaySeconds: 10\n",
            "",
        );
        let after = parse_manifest(&after_yaml).unwrap();
        let findings = diff_manifests(&before, &after);
        assert!(findings.iter().any(|f| f.severity == Severity::Breaking
            && f.message.contains("livenessProbe")
            && f.message.contains("removed")));
    }

    #[test]
    fn detects_changed_readiness_probe_as_risky_not_breaking() {
        let before = parse_manifest(&base_yaml("", "3", "1.0.0")).unwrap();
        let after_yaml = base_yaml("", "3", "1.0.0").replace("path: /ready", "path: /readyz");
        let after = parse_manifest(&after_yaml).unwrap();
        let findings = diff_manifests(&before, &after);
        assert!(findings.iter().any(|f| f.severity == Severity::Risky
            && f.message.contains("readinessProbe")
            && f.message.contains("changed")));
    }

    #[test]
    fn detects_added_container_as_info_only() {
        let before = parse_manifest(&base_yaml("", "3", "1.0.0")).unwrap();
        let extra = "        - name: sidecar\n          image: sidecar:1.0\n";
        let after = parse_manifest(&base_yaml(extra, "3", "1.0.0")).unwrap();
        let findings = diff_manifests(&before, &after);
        let sidecar_finding = findings
            .iter()
            .find(|f| f.message.contains("sidecar"))
            .unwrap();
        assert_eq!(sidecar_finding.severity, Severity::Info);
        assert_eq!(
            worst_severity(&findings),
            Some(Severity::Info),
            "adding a container alone should never raise the overall severity above Info"
        );
    }

    #[test]
    fn detects_removed_container_as_breaking() {
        let extra = "        - name: sidecar\n          image: sidecar:1.0\n";
        let before = parse_manifest(&base_yaml(extra, "3", "1.0.0")).unwrap();
        let after = parse_manifest(&base_yaml("", "3", "1.0.0")).unwrap();
        let findings = diff_manifests(&before, &after);
        assert!(findings.iter().any(|f| f.severity == Severity::Breaking
            && f.message.contains("sidecar")
            && f.message.contains("removed")));
    }

    #[test]
    fn worst_severity_picks_breaking_over_risky_and_info() {
        assert_eq!(
            worst_severity(&[
                finding(Severity::Info, "a"),
                finding(Severity::Risky, "b"),
                finding(Severity::Breaking, "c"),
            ]),
            Some(Severity::Breaking)
        );
        assert_eq!(worst_severity(&[]), None);
    }

    #[test]
    fn env_var_value_change_is_risky_not_breaking() {
        let before = parse_manifest(&base_yaml("", "3", "1.0.0")).unwrap();
        let after_yaml = base_yaml("", "3", "1.0.0").replace("value: info", "value: debug");
        let after = parse_manifest(&after_yaml).unwrap();
        let findings = diff_manifests(&before, &after);
        assert!(findings.iter().any(|f| f.severity == Severity::Risky
            && f.message.contains("LOG_LEVEL")
            && f.message.contains("changed")));
    }

    #[test]
    fn resource_kind_change_is_flagged_breaking() {
        let before = parse_manifest(&base_yaml("", "3", "1.0.0")).unwrap();
        let after_yaml =
            base_yaml("", "3", "1.0.0").replace("kind: Deployment", "kind: StatefulSet");
        let after = parse_manifest(&after_yaml).unwrap();
        let findings = diff_manifests(&before, &after);
        assert!(findings.iter().any(
            |f| f.severity == Severity::Breaking && f.message.contains("resource type changed")
        ));
    }
}
