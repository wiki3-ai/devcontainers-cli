//! Per-workspace permission model. Ported in shape from `wiki3-ai/wiki3-app`'s
//! `permissions.rs`. The decisions gate operations that touch the host
//! outside the workspace sandbox: spawning lifecycle commands on the host,
//! mounting paths outside the workspace, and forwarding privileged ports.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    AllowOnce,
    AllowAlways,
    Deny,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub enum Capability {
    /// Run a host-side `initializeCommand` for a workspace.
    HostInitializeCommand,
    /// Mount a host path that is *outside* the workspace folder.
    MountOutsideWorkspace,
    /// Forward a host port below 1024.
    ForwardPrivilegedPort,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PermissionPolicy {
    /// Persistent decisions keyed by `(workspace_id, capability)`.
    #[serde(default)]
    pub decisions: HashMap<String, HashMap<Capability, Decision>>,
}

impl PermissionPolicy {
    pub fn get(&self, workspace_id: &str, capability: &Capability) -> Option<Decision> {
        self.decisions
            .get(workspace_id)
            .and_then(|m| m.get(capability))
            .copied()
    }

    pub fn set(&mut self, workspace_id: &str, capability: Capability, decision: Decision) {
        self.decisions
            .entry(workspace_id.to_string())
            .or_default()
            .insert(capability, decision);
    }

    /// Returns `true` if the operation may proceed without prompting.
    pub fn is_allowed(&self, workspace_id: &str, capability: &Capability) -> bool {
        matches!(
            self.get(workspace_id, capability),
            Some(Decision::AllowAlways) | Some(Decision::AllowOnce)
        )
    }
}
