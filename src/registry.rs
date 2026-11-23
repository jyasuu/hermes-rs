//! Config compiled for serving: templates registered, retry policies resolved.
//! Shared by the HTTP handler (accepting webhooks) and the outbox workers
//! (delivering them later).

use crate::config::{AuthConfig, Config, DeliveryPolicy, DispatchMode, Target};
use crate::sink::RetryPolicy;
use crate::template::new_handlebars;
use handlebars::Handlebars;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

pub struct CompiledTarget {
    pub label: String,
    pub target: Target,
    /// `Some` when the target overrides the register's template.
    pub own_template: Option<String>,
    /// sync mode: target `retry` > register `retry_config` > `settings`.
    pub retry: RetryPolicy,
    /// async mode: target `retry` > register `retry_config` > `outbox.retry`.
    pub async_retry: RetryPolicy,
}

pub struct CompiledRegister {
    /// Upper-case HTTP method the endpoint accepts.
    pub method: String,
    pub auth: Option<AuthConfig>,
    pub mode: DispatchMode,
    pub delivery: DeliveryPolicy,
    pub default_template: String,
    pub targets: Vec<CompiledTarget>,
}

pub struct Registry {
    pub registers: HashMap<String, CompiledRegister>,
    pub handlebars: Handlebars<'static>,
}

impl Registry {
    /// Compiles every template up front. `Config::validate` must have run.
    /// Panics on a template error; use [`Registry::try_build`] where a failure
    /// must not take the process down (config reload).
    pub fn build(config: &Config) -> Self {
        Self::try_build(config).unwrap_or_else(|e| panic!("{e}"))
    }

    pub fn try_build(config: &Config) -> Result<Self, String> {
        let mut hbs = new_handlebars();
        let mut registers = HashMap::new();
        let sync_default = RetryPolicy::from_settings(&config.settings);
        let async_default = RetryPolicy::from_config(&config.outbox.retry.to_config());

        for (i, reg) in config.registers.iter().enumerate() {
            let reg_policy = reg.retry_config.as_ref().map(RetryPolicy::from_config);

            let default_template = format!("reg{i}");
            hbs.register_template_string(&default_template, &reg.template)
                .map_err(|e| format!("{}: template error: {e}", reg.endpoint))?;

            let mut targets = Vec::new();
            for (j, t) in reg.all_targets().enumerate() {
                let own_template = match &t.template {
                    Some(tpl) => {
                        let name = format!("reg{i}_t{j}");
                        hbs.register_template_string(&name, tpl)
                            .map_err(|e| format!("{}: target '{}' template error: {e}", reg.endpoint, t.label(j)))?;
                        Some(name)
                    }
                    None => None,
                };
                let own = t.retry.as_ref().map(RetryPolicy::from_config);
                targets.push(CompiledTarget {
                    label: t.label(j),
                    target: t.clone(),
                    own_template,
                    retry: own.or(reg_policy).unwrap_or(sync_default),
                    async_retry: own.or(reg_policy).unwrap_or(async_default),
                });
            }

            registers.insert(
                reg.endpoint.clone(),
                CompiledRegister {
                    method: reg.method.to_uppercase(),
                    auth: reg.auth.clone(),
                    mode: reg.mode,
                    delivery: reg.delivery,
                    default_template,
                    targets,
                },
            );
        }

        Ok(Registry { registers, handlebars: hbs })
    }
}

/// The live registry, replaceable at runtime (config reload).
///
/// Readers take a cheap snapshot ([`SharedRegistry::current`]) and keep using
/// it for the whole request, so a reload can never change the rules halfway
/// through a request, and requests in flight are never interrupted.
pub struct SharedRegistry {
    inner: RwLock<Arc<Registry>>,
}

impl SharedRegistry {
    pub fn new(registry: Registry) -> Self {
        Self { inner: RwLock::new(Arc::new(registry)) }
    }

    pub fn current(&self) -> Arc<Registry> {
        self.inner.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn replace(&self, registry: Registry) {
        *self.inner.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(registry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn retry_precedence_differs_between_sync_and_async() {
        let cfg = Config::from_yaml(
            r#"
settings: { retry_attempts: 3 }
outbox: { retry: { attempts: 9 } }
registers:
  - endpoint: /plain
    method: POST
    mode: async
    template: '{}'
    target: { url: "http://a" }
  - endpoint: /reg
    method: POST
    template: '{}'
    retry_config: { attempts: 5 }
    targets:
      - { url: "http://a" }
      - { url: "http://b", retry: { attempts: 2, delay_ms: 77 } }
"#,
        )
        .unwrap();
        let r = Registry::build(&cfg);

        let plain = &r.registers["/plain"].targets[0];
        assert_eq!((plain.retry.attempts, plain.async_retry.attempts), (3, 9));
        assert_eq!(plain.async_retry.max_elapsed, None, "async has no caller waiting");
        assert!(plain.retry.max_elapsed.is_some());

        let t = &r.registers["/reg"].targets;
        assert_eq!((t[0].retry.attempts, t[0].async_retry.attempts), (5, 5), "register beats defaults");
        assert_eq!((t[1].retry.attempts, t[1].async_retry.attempts), (2, 2), "target beats register");
        assert_eq!(t[1].retry.base_delay, Duration::from_millis(77));
        assert_eq!(r.registers["/plain"].mode, DispatchMode::Async);
    }

    #[test]
    fn shared_registry_snapshots_are_stable_across_a_replace() {
        let cfg = |ep: &str| {
            Config::from_yaml(&format!("registers:\n  - {{ endpoint: {ep}, method: POST, template: '{{}}', target: {{ url: 'http://a' }} }}\n")).unwrap()
        };
        let shared = SharedRegistry::new(Registry::build(&cfg("/old")));
        let in_flight = shared.current();
        shared.replace(Registry::build(&cfg("/new")));
        assert!(in_flight.registers.contains_key("/old"), "a request already running keeps its snapshot");
        assert!(!in_flight.registers.contains_key("/new"));
        let after = shared.current();
        assert!(after.registers.contains_key("/new") && !after.registers.contains_key("/old"));
    }

    #[test]
    fn try_build_reports_template_errors_instead_of_panicking() {
        let cfg = Config::from_yaml("registers:\n  - { endpoint: /w, method: POST, template: '{{#if}}', target: { url: 'http://a' } }\n").unwrap();
        let err = Registry::try_build(&cfg).err().expect("must fail");
        assert!(err.contains("/w") && err.contains("template"), "{err}");
    }
}
