use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Supported API compatibility line for infrastructure plugins.
pub const SUPPORTED_PLUGIN_API_MAJOR: &str = "1";

/// Capability provided by an infrastructure intelligence plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InfraCapability {
    Metrics,
    Infrastructure,
    Autoscaling,
    Pricing,
    Cost,
}

impl InfraCapability {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Metrics => "metrics",
            Self::Infrastructure => "infrastructure",
            Self::Autoscaling => "autoscaling",
            Self::Pricing => "pricing",
            Self::Cost => "cost",
        }
    }
}

/// Runtime availability and health status of a registered plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "detail", rename_all = "snake_case")]
pub enum InfraPluginHealth {
    Healthy,
    Degraded(String),
    Unavailable(String),
}

impl InfraPluginHealth {
    pub fn is_usable(&self) -> bool {
        matches!(self, Self::Healthy | Self::Degraded(_))
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded(_) => "degraded",
            Self::Unavailable(_) => "unavailable",
        }
    }

    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Healthy => None,
            Self::Degraded(msg) | Self::Unavailable(msg) => Some(msg.as_str()),
        }
    }
}

/// Metadata describing an infrastructure intelligence plugin adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InfraPluginDescriptor {
    pub id: String,
    pub version: String,
    pub api_compat: String,
    pub capabilities: Vec<InfraCapability>,
    pub requires_external_infra: bool,
    pub description: String,
}

/// Serializable health summary row for `riffty infra providers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginHealthSummary {
    pub id: String,
    pub version: String,
    pub api_compat: String,
    pub capabilities: Vec<InfraCapability>,
    pub requires_external_infra: bool,
    pub health: InfraPluginHealth,
    pub active_for: Vec<InfraCapability>,
    pub description: String,
}

#[derive(Debug, Clone)]
struct RegisteredPlugin {
    descriptor: InfraPluginDescriptor,
    health: InfraPluginHealth,
}

/// Registry managing infrastructure plugin descriptors, API compatibility, health, and active swaps.
#[derive(Debug, Default)]
pub struct InfraPluginRegistry {
    plugins: Vec<RegisteredPlugin>,
    active: HashMap<InfraCapability, String>,
}

impl InfraPluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Validates that a plugin's `api_compat` string is compatible with `"1.x"`.
    pub fn validate_api_compat(api_compat: &str) -> Result<(), String> {
        let trimmed = api_compat.trim();
        let Some((major, minor)) = trimmed.split_once('.') else {
            return Err(format!(
                "invalid plugin api_compat '{api_compat}': expected '1.x' or '1.<minor>'"
            ));
        };
        if major != SUPPORTED_PLUGIN_API_MAJOR {
            return Err(format!(
                "incompatible plugin api_compat '{api_compat}': supported major version is '{SUPPORTED_PLUGIN_API_MAJOR}.x'"
            ));
        }
        if minor != "x" && minor.parse::<u32>().is_err() {
            return Err(format!(
                "invalid plugin api_compat minor version in '{api_compat}': expected 'x' or integer"
            ));
        }
        Ok(())
    }

    /// Registers a plugin after validating its API compatibility and uniqueness.
    /// Automatically activates the first usable plugin for each capability not yet assigned.
    pub fn register(
        &mut self,
        descriptor: InfraPluginDescriptor,
        health: InfraPluginHealth,
    ) -> Result<(), String> {
        Self::validate_api_compat(&descriptor.api_compat)?;
        if self
            .plugins
            .iter()
            .any(|p| p.descriptor.id == descriptor.id)
        {
            return Err(format!("duplicate infra plugin id '{}'", descriptor.id));
        }

        if health.is_usable() {
            for &cap in &descriptor.capabilities {
                self.active
                    .entry(cap)
                    .or_insert_with(|| descriptor.id.clone());
            }
        }

        self.plugins.push(RegisteredPlugin { descriptor, health });
        Ok(())
    }

    /// Lists all registered plugin descriptors in registration order.
    pub fn list(&self) -> Vec<&InfraPluginDescriptor> {
        self.plugins.iter().map(|p| &p.descriptor).collect()
    }

    /// Resolves the currently active plugin descriptor for the requested capability.
    pub fn resolve_for_capability(&self, cap: InfraCapability) -> Option<&InfraPluginDescriptor> {
        let active_id = self.active.get(&cap)?;
        self.plugins
            .iter()
            .find(|p| &p.descriptor.id == active_id)
            .map(|p| &p.descriptor)
    }

    /// Swaps the active plugin for a capability to another registered, usable plugin that supports it.
    pub fn swap_active(&mut self, cap: InfraCapability, plugin_id: &str) -> Result<(), String> {
        let Some(entry) = self.plugins.iter().find(|p| p.descriptor.id == plugin_id) else {
            return Err(format!("unknown infra plugin '{plugin_id}'"));
        };
        if !entry.descriptor.capabilities.contains(&cap) {
            return Err(format!(
                "infra plugin '{plugin_id}' does not support capability '{}'",
                cap.as_str()
            ));
        }
        if !entry.health.is_usable() {
            return Err(format!(
                "cannot activate infra plugin '{plugin_id}': health is {}",
                entry.health.label()
            ));
        }
        self.active.insert(cap, plugin_id.to_string());
        Ok(())
    }

    /// Returns a structured health & activation summary for all registered plugins.
    pub fn health_summary(&self) -> Vec<PluginHealthSummary> {
        self.plugins
            .iter()
            .map(|entry| {
                let mut active_for: Vec<InfraCapability> = entry
                    .descriptor
                    .capabilities
                    .iter()
                    .copied()
                    .filter(|cap| {
                        self.active
                            .get(cap)
                            .is_some_and(|id| id == &entry.descriptor.id)
                    })
                    .collect();
                active_for.sort_by_key(|c| c.as_str());

                PluginHealthSummary {
                    id: entry.descriptor.id.clone(),
                    version: entry.descriptor.version.clone(),
                    api_compat: entry.descriptor.api_compat.clone(),
                    capabilities: entry.descriptor.capabilities.clone(),
                    requires_external_infra: entry.descriptor.requires_external_infra,
                    health: entry.health.clone(),
                    active_for,
                    description: entry.descriptor.description.clone(),
                }
            })
            .collect()
    }

    /// Returns deduplicated IDs of all currently active providers in registration order.
    pub fn active_provider_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        for entry in &self.plugins {
            let is_active = self.active.values().any(|id| id == &entry.descriptor.id);
            if is_active {
                ids.push(entry.descriptor.id.clone());
            }
        }
        ids
    }
}
