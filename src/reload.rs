//! Hot configuration reload (`SIGHUP`).
//!
//! The new file is loaded, validated and fully compiled **before** anything is
//! touched; only then is the live registry swapped (a pointer replacement).
//! A bad edit therefore leaves the old configuration serving, untouched.

use crate::config::{Config, WebhookRegister};
use crate::outbox::Outbox;
use crate::registry::{Registry, SharedRegistry};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReloadSummary {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
    /// Settings that differ from what is running but only take effect after a restart.
    pub restart_required: Vec<&'static str>,
    /// Queued async jobs whose target no longer exists in the new configuration.
    pub orphaned_jobs: Vec<String>,
}

impl ReloadSummary {
    pub fn is_noop(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// What a register looks like, for change detection only (contains secrets:
/// never log it).
fn fingerprint(reg: &WebhookRegister) -> String {
    serde_yaml::to_string(reg).unwrap_or_else(|_| format!("{reg:?}"))
}

/// Settings bound to running machinery. They are applied only by a restart.
pub fn restart_required_changes(running: &Config, new: &Config) -> Vec<&'static str> {
    let mut out = Vec::new();
    let (a, b) = (&running.settings, &new.settings);
    if a.enable_metrics != b.enable_metrics {
        out.push("settings.enable_metrics");
    }
    if a.metrics_token != b.metrics_token {
        out.push("settings.metrics_token");
    }
    let cb = |c: &Config| serde_yaml::to_string(&c.settings.circuit_breaker).unwrap_or_default();
    if cb(running) != cb(new) {
        out.push("settings.circuit_breaker");
    }
    let (x, y) = (&running.outbox, &new.outbox);
    if running.outbox_path() != new.outbox_path() || x.path != y.path {
        out.push("outbox.path");
    }
    for (name, changed) in [
        ("outbox.workers", x.workers != y.workers),
        ("outbox.lease_secs", x.lease_secs != y.lease_secs),
        ("outbox.poll_interval_ms", x.poll_interval_ms != y.poll_interval_ms),
        ("outbox.max_pending", x.max_pending != y.max_pending),
        ("outbox.delivered_retention_secs", x.delivered_retention_secs != y.delivered_retention_secs),
        ("outbox.dead_retention_secs", x.dead_retention_secs != y.dead_retention_secs),
    ] {
        if changed {
            out.push(name);
        }
    }
    out
}

/// Pure planning step: validate and compile `new`, and describe the change.
/// Nothing is applied here.
pub fn plan(running: &Config, new: &Config, outbox_running: bool) -> Result<(Registry, ReloadSummary), String> {
    new.validate()?;
    if new.has_async_registers() && !outbox_running {
        return Err(
            "the new configuration uses `mode: async` but the outbox was not started (no async register at \
             startup); restart Hermes to enable it"
                .to_string(),
        );
    }
    let registry = Registry::try_build(new)?;

    let prints = |c: &Config| -> BTreeMap<String, String> {
        c.registers.iter().map(|r| (r.endpoint.clone(), fingerprint(r))).collect()
    };
    let (old_p, new_p) = (prints(running), prints(new));
    let mut s = ReloadSummary::default();
    for (ep, fp) in &new_p {
        match old_p.get(ep) {
            None => s.added.push(ep.clone()),
            Some(old_fp) if old_fp != fp => s.changed.push(ep.clone()),
            Some(_) => {}
        }
    }
    s.removed = old_p.keys().filter(|ep| !new_p.contains_key(*ep)).cloned().collect();
    s.restart_required = restart_required_changes(running, new);
    Ok((registry, s))
}

/// Reloads `path` and swaps the live registry. On `Err` nothing has changed.
///
/// `running` is the configuration currently in effect; on success it becomes
/// the new configuration *except* for the restart-only settings, which keep
/// their running values so the next reload keeps flagging them.
pub async fn reload(
    path: &Path,
    running: &mut Config,
    shared: &SharedRegistry,
    outbox: Option<&Outbox>,
) -> Result<ReloadSummary, String> {
    let new = Config::load(&path.to_path_buf())
        .await
        .map_err(|e| format!("could not load {}: {e}", path.display()))?;
    let (registry, mut summary) = plan(running, &new, outbox.is_some())?;

    // Queued async jobs name their target by (endpoint, position, label). If
    // the edit removed or reshuffled targets, those jobs would be
    // dead-lettered: say so now, while the operator can still react.
    if let Some(ob) = outbox {
        if let Ok(rows) = ob.pending_targets().await {
            for (endpoint, index, label, n) in rows {
                let found = registry
                    .registers
                    .get(&endpoint)
                    .and_then(|r| r.targets.get(index))
                    .is_some_and(|t| t.label == label);
                if !found {
                    summary.orphaned_jobs.push(format!("{endpoint} -> '{label}' ({n} queued job(s))"));
                }
            }
        }
    }

    shared.replace(registry);

    let mut applied = new;
    applied.settings.enable_metrics = running.settings.enable_metrics;
    applied.settings.metrics_token = running.settings.metrics_token.clone();
    applied.settings.circuit_breaker = running.settings.circuit_breaker.clone();
    let (rb, ob) = (running.outbox.clone(), &mut applied.outbox);
    ob.path = rb.path;
    ob.workers = rb.workers;
    ob.lease_secs = rb.lease_secs;
    ob.poll_interval_ms = rb.poll_interval_ms;
    ob.max_pending = rb.max_pending;
    ob.delivered_retention_secs = rb.delivered_retention_secs;
    ob.dead_retention_secs = rb.dead_retention_secs;
    *running = applied;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(yaml: &str) -> Config {
        Config::from_yaml(yaml).unwrap()
    }
    const BASE: &str = "registers:\n  - { endpoint: /a, method: POST, template: '{\"v\":1}', target: { url: 'http://a' } }\n  - { endpoint: /b, method: POST, template: '{}', target: { url: 'http://b' } }\n";

    #[test]
    fn identical_config_is_a_noop() {
        let (_, s) = plan(&cfg(BASE), &cfg(BASE), false).unwrap();
        assert!(s.is_noop() && s.restart_required.is_empty(), "{s:?}");
    }

    #[test]
    fn added_removed_and_changed_endpoints_are_reported() {
        let new = "registers:\n  - { endpoint: /a, method: POST, template: '{\"v\":2}', target: { url: 'http://a' } }\n  - { endpoint: /c, method: POST, template: '{}', target: { url: 'http://c' } }\n";
        let (reg, s) = plan(&cfg(BASE), &cfg(new), false).unwrap();
        assert_eq!((s.added, s.removed, s.changed), (vec!["/c".to_string()], vec!["/b".to_string()], vec!["/a".to_string()]));
        assert!(reg.registers.contains_key("/c") && !reg.registers.contains_key("/b"));
    }

    #[test]
    fn a_rotated_secret_counts_as_a_change() {
        let mk = |secret: &str| cfg(&format!("registers:\n  - {{ endpoint: /a, method: POST, template: '{{}}', target: {{ url: 'http://a' }}, auth: {{ type: hmac, secret: {secret} }} }}\n"));
        let (_, s) = plan(&mk("old"), &mk("new"), false).unwrap();
        assert_eq!(s.changed, vec!["/a".to_string()]);
        let (_, same) = plan(&mk("old"), &mk("old"), false).unwrap();
        assert!(same.is_noop());
    }

    #[test]
    fn invalid_or_uncompilable_config_is_rejected() {
        let bad_tpl = "registers:\n  - { endpoint: /a, method: POST, template: '{{#if}}', target: { url: 'http://a' } }\n";
        assert!(plan(&cfg(BASE), &cfg(bad_tpl), false).is_err());
        let no_target = "registers:\n  - { endpoint: /a, method: POST, template: '{}' }\n";
        assert!(plan(&cfg(BASE), &cfg(no_target), false).err().unwrap().contains("at least one"));
        let dup = "registers:\n  - { endpoint: /a, method: POST, template: '{}', target: { url: 'http://a' } }\n  - { endpoint: /a, method: POST, template: '{}', target: { url: 'http://b' } }\n";
        assert!(plan(&cfg(BASE), &cfg(dup), false).err().unwrap().contains("duplicate"));
    }

    #[test]
    fn enabling_async_without_a_running_outbox_needs_a_restart() {
        let new = "registers:\n  - { endpoint: /a, method: POST, mode: async, template: '{}', target: { url: 'http://a' } }\n";
        let err = plan(&cfg(BASE), &cfg(new), false).err().unwrap();
        assert!(err.contains("restart"), "{err}");
        assert!(plan(&cfg(BASE), &cfg(new), true).is_ok(), "fine when the outbox is already running");
    }

    #[test]
    fn restart_only_settings_are_flagged_not_applied() {
        let new = format!("outbox: {{ workers: 9, max_pending: 5 }}\nsettings: {{ enable_metrics: true, circuit_breaker: {{ failure_threshold: 2 }} }}\n{BASE}");
        let (_, s) = plan(&cfg(BASE), &cfg(&new), false).unwrap();
        for k in ["outbox.workers", "outbox.max_pending", "settings.enable_metrics", "settings.circuit_breaker"] {
            assert!(s.restart_required.contains(&k), "{k} in {:?}", s.restart_required);
        }
        assert!(s.is_noop(), "no endpoint changed");
    }

    #[test]
    fn reloadable_settings_do_not_demand_a_restart() {
        let new = format!("settings: {{ retry_attempts: 7 }}\noutbox: {{ retry: {{ attempts: 3 }} }}\n{BASE}");
        let (_, s) = plan(&cfg(BASE), &cfg(&new), false).unwrap();
        assert!(s.restart_required.is_empty(), "{:?}", s.restart_required);
    }

    #[tokio::test]
    async fn reload_applies_atomically_and_failure_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.yml");
        std::fs::write(&path, BASE).unwrap();
        let mut running = cfg(BASE);
        let shared = SharedRegistry::new(Registry::build(&running));

        // good edit
        std::fs::write(&path, format!("{BASE}  - {{ endpoint: /new, method: POST, template: '{{}}', target: {{ url: 'http://n' }} }}\n")).unwrap();
        let s = reload(&path, &mut running, &shared, None).await.unwrap();
        assert_eq!(s.added, vec!["/new".to_string()]);
        assert!(shared.current().registers.contains_key("/new"));
        assert_eq!(running.registers.len(), 3);

        // bad edit: broken template -> Err, registry and running config untouched
        std::fs::write(&path, "registers:\n  - { endpoint: /x, method: POST, template: '{{#if}}', target: { url: 'http://x' } }\n").unwrap();
        assert!(reload(&path, &mut running, &shared, None).await.is_err());
        assert!(shared.current().registers.contains_key("/new") && !shared.current().registers.contains_key("/x"));
        assert_eq!(running.registers.len(), 3);

        // unparsable YAML and a missing file also leave everything as it was
        std::fs::write(&path, "registers: [").unwrap();
        assert!(reload(&path, &mut running, &shared, None).await.is_err());
        std::fs::remove_file(&path).unwrap();
        assert!(reload(&path, &mut running, &shared, None).await.unwrap_err().contains("could not load"));
        assert!(shared.current().registers.contains_key("/new"));
    }

    #[tokio::test]
    async fn restart_only_values_stay_at_their_running_value_so_they_keep_being_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.yml");
        std::fs::write(&path, format!("outbox: {{ workers: 9 }}\n{BASE}")).unwrap();
        let mut running = cfg(BASE); // started with the default 4 workers
        let shared = SharedRegistry::new(Registry::build(&running));
        let first = reload(&path, &mut running, &shared, None).await.unwrap();
        assert!(first.restart_required.contains(&"outbox.workers"));
        assert_eq!(running.outbox.workers, 4, "still what is actually running");
        let second = reload(&path, &mut running, &shared, None).await.unwrap();
        assert!(second.restart_required.contains(&"outbox.workers"), "keeps reminding until restart");
    }
}
