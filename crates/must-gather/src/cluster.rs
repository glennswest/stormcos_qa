//! What the cluster API has: discovery and health, every listed resource,
//! every custom resource, and pod logs. Never Secrets or ConfigMaps.
//!
//! The node's own services (fastetcd, the apiserver, rustkube-node, …) are
//! mirror pods `kube-system/<service>-<node>`, so their logs come through
//! `pods/log` like any pod's.

use std::path::Path;

use serde_json::Value;

use crate::api::{Api, items};
use crate::host::log_path;
use crate::{Item, Status};

/// Plain GETs: (file, path).
const RAW: [(&str, &str); 6] = [
    ("version.json", "/version"),
    ("healthz.txt", "/healthz"),
    ("livez.txt", "/livez"),
    ("readyz.txt", "/readyz"),
    ("api.json", "/api"),
    ("apis.json", "/apis"),
];

/// Lists, all namespaces: (file, path).
pub const LISTS: [(&str, &str); 22] = [
    ("nodes", "/api/v1/nodes"),
    ("namespaces", "/api/v1/namespaces"),
    ("pods", "/api/v1/pods"),
    ("events", "/api/v1/events"),
    ("services", "/api/v1/services"),
    ("endpoints", "/api/v1/endpoints"),
    ("persistentvolumes", "/api/v1/persistentvolumes"),
    ("persistentvolumeclaims", "/api/v1/persistentvolumeclaims"),
    ("serviceaccounts", "/api/v1/serviceaccounts"),
    ("deployments", "/apis/apps/v1/deployments"),
    ("daemonsets", "/apis/apps/v1/daemonsets"),
    ("statefulsets", "/apis/apps/v1/statefulsets"),
    ("replicasets", "/apis/apps/v1/replicasets"),
    ("jobs", "/apis/batch/v1/jobs"),
    ("storageclasses", "/apis/storage.k8s.io/v1/storageclasses"),
    ("volumeattachments", "/apis/storage.k8s.io/v1/volumeattachments"),
    ("csidrivers", "/apis/storage.k8s.io/v1/csidrivers"),
    ("csinodes", "/apis/storage.k8s.io/v1/csinodes"),
    ("leases", "/apis/coordination.k8s.io/v1/leases"),
    ("networkpolicies", "/apis/networking.k8s.io/v1/networkpolicies"),
    ("customresourcedefinitions", "/apis/apiextensions.k8s.io/v1/customresourcedefinitions"),
    ("priorityclasses", "/apis/scheduling.k8s.io/v1/priorityclasses"),
];

/// Write the cluster's state under `out/cluster/`, pod logs under
/// `out/logs/`. Returns the nodes list (for the host part) when it could be read.
pub async fn gather(api: &Api, out: &Path, items_out: &mut Vec<Item>) -> Option<Value> {
    let dir = out.join("cluster");
    let _ = std::fs::create_dir_all(dir.join("crs"));
    for (file, path) in RAW {
        match api.get(path).await {
            Ok(g) => {
                let _ = std::fs::write(dir.join(file), &g.text);
                items_out.push(Item::new(format!("cluster/{file}"), if g.ok() { Status::Ok } else { Status::Error }, if g.ok() { String::new() } else { format!("{} {}", g.code, first_line(&g.text)) }));
            }
            Err(e) => items_out.push(Item::new(format!("cluster/{file}"), Status::Error, format!("{e:#}"))),
        }
    }
    let mut nodes = None;
    let mut pods = None;
    let mut crds = None;
    for (name, path) in LISTS {
        let file = format!("cluster/{name}.json");
        match list_to(api, path, &out.join(&file)).await {
            Ok(v) => {
                items_out.push(Item::new(file, Status::Ok, format!("{} items", items(&v).len())));
                match name {
                    "nodes" => nodes = Some(v),
                    "pods" => pods = Some(v),
                    "customresourcedefinitions" => crds = Some(v),
                    _ => {}
                }
            }
            Err(e) => items_out.push(Item::new(file, Status::Error, e)),
        }
    }

    // Every custom resource (VMs, VMIs, CloudImages, placements, stormblock's, …).
    for crd in crds.as_ref().map(items).unwrap_or(&[]) {
        let Some((file, path)) = cr_list(crd) else { continue };
        let file = format!("cluster/crs/{file}");
        match list_to(api, &path, &out.join(&file)).await {
            Ok(v) => items_out.push(Item::new(file, Status::Ok, format!("{} items", items(&v).len()))),
            Err(e) => items_out.push(Item::new(file, Status::Error, e)),
        }
    }

    // Pod logs: kube-system (the node's services) and every pod in trouble.
    for p in pods.as_ref().map(items).unwrap_or(&[]) {
        let (ns, name) = (p["metadata"]["namespace"].as_str().unwrap_or(""), p["metadata"]["name"].as_str().unwrap_or(""));
        if !wants_logs(p) {
            continue;
        }
        for (c, restarts) in containers(p) {
            for previous in [false, true] {
                if previous && restarts == 0 {
                    continue;
                }
                let file = format!("logs/{ns}/{name}/{c}{}.log", if previous { ".previous" } else { "" });
                let target = out.join(&file);
                let _ = std::fs::create_dir_all(target.parent().unwrap());
                match api.get(&log_path(ns, name, &c, previous)).await {
                    Ok(g) if g.ok() => {
                        let _ = std::fs::write(&target, &g.text);
                        items_out.push(Item::new(file, Status::Ok, format!("{} bytes", g.text.len())));
                    }
                    Ok(g) => items_out.push(Item::new(file, Status::Error, format!("{} {}", g.code, first_line(&g.text)))),
                    Err(e) => items_out.push(Item::new(file, Status::Error, format!("{e:#}"))),
                }
            }
        }
    }
    nodes
}

async fn list_to(api: &Api, path: &str, file: &Path) -> Result<Value, String> {
    match api.list(path).await {
        Ok(Ok(v)) => {
            let _ = std::fs::create_dir_all(file.parent().unwrap());
            let _ = std::fs::write(file, serde_json::to_string_pretty(&v).unwrap_or_default());
            Ok(v)
        }
        Ok(Err(g)) => Err(format!("{} {}", g.code, first_line(&g.text))),
        Err(e) => Err(format!("{e:#}")),
    }
}

/// (file, list path) for a CRD's objects in every namespace, at its storage
/// (else first served) version. Secrets-like kinds are never listed.
pub fn cr_list(crd: &Value) -> Option<(String, String)> {
    let group = crd["spec"]["group"].as_str()?;
    let plural = crd["spec"]["names"]["plural"].as_str()?;
    let versions = crd["spec"]["versions"].as_array()?;
    let v = versions
        .iter()
        .find(|v| v["storage"] == true)
        .or_else(|| versions.iter().find(|v| v["served"] == true))?["name"]
        .as_str()?;
    if plural.contains("secret") {
        return None;
    }
    Some((format!("{plural}.{group}.json"), format!("/apis/{group}/{v}/{plural}")))
}

/// kube-system's pods, and any pod that is not Running and Ready (or done),
/// or has restarted.
pub fn wants_logs(p: &Value) -> bool {
    if p["metadata"]["namespace"] == "kube-system" {
        return true;
    }
    let st = &p["status"];
    let restarted = containers(p).iter().any(|(_, r)| *r > 0);
    let ready = st["conditions"].as_array().is_some_and(|cs| cs.iter().any(|c| c["type"] == "Ready" && c["status"] == "True"));
    match st["phase"].as_str() {
        Some("Succeeded") => restarted,
        Some("Running") => !ready || restarted,
        _ => true,
    }
}

/// Every container (init first) with its restart count.
pub fn containers(p: &Value) -> Vec<(String, u64)> {
    let restarts = |c: &str| {
        ["initContainerStatuses", "containerStatuses"]
            .iter()
            .flat_map(|k| p["status"][*k].as_array().cloned().unwrap_or_default())
            .find(|s| s["name"] == c)
            .and_then(|s| s["restartCount"].as_u64())
            .unwrap_or(0)
    };
    ["initContainers", "containers"]
        .iter()
        .flat_map(|k| p["spec"][*k].as_array().cloned().unwrap_or_default())
        .filter_map(|c| c["name"].as_str().map(|n| (n.to_string(), restarts(n))))
        .collect()
}

pub fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn never_secrets_or_configmaps() {
        assert!(LISTS.iter().all(|(n, p)| !n.contains("secret") && !p.contains("secret") && !p.contains("configmap")));
    }

    #[test]
    fn a_crd_lists_at_its_storage_version() {
        let crd = json!({"spec": {"group": "kubevirt.io", "names": {"plural": "virtualmachineinstances"},
            "versions": [{"name": "v1alpha3", "served": true}, {"name": "v1", "served": true, "storage": true}]}});
        assert_eq!(cr_list(&crd).unwrap(), ("virtualmachineinstances.kubevirt.io.json".into(), "/apis/kubevirt.io/v1/virtualmachineinstances".into()));
        let sealed = json!({"spec": {"group": "bitnami.com", "names": {"plural": "sealedsecrets"}, "versions": [{"name": "v1", "storage": true}]}});
        assert!(cr_list(&sealed).is_none());
    }

    #[test]
    fn logs_of_kube_system_and_pods_in_trouble() {
        let pod = |ns: &str, phase: &str, ready: bool, restarts: u64| {
            json!({"metadata": {"namespace": ns, "name": "p"},
                   "spec": {"initContainers": [{"name": "init"}], "containers": [{"name": "c"}]},
                   "status": {"phase": phase, "conditions": [{"type": "Ready", "status": if ready { "True" } else { "False" }}],
                              "containerStatuses": [{"name": "c", "restartCount": restarts}]}})
        };
        assert!(wants_logs(&pod("kube-system", "Running", true, 0)));
        assert!(!wants_logs(&pod("apps", "Running", true, 0)));
        assert!(wants_logs(&pod("apps", "Running", false, 0)));
        assert!(wants_logs(&pod("apps", "Running", true, 2)));
        assert!(wants_logs(&pod("apps", "Pending", false, 0)));
        assert!(!wants_logs(&pod("apps", "Succeeded", false, 0)));
        assert_eq!(containers(&pod("apps", "Running", true, 2)), vec![("init".to_string(), 0), ("c".to_string(), 2)]);
    }
}
