//! Policy model (port of `cmPolicies`).

use crate::policy_table::POLICIES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyStatus {
    Old,
    Warn,
    New,
}

/// Sparse policy settings for one stack entry.
#[derive(Clone, Debug, Default)]
pub struct PolicyMap {
    // index -> status
    entries: Vec<Option<PolicyStatus>>,
}

impl PolicyMap {
    pub fn get(&self, index: usize) -> Option<PolicyStatus> {
        self.entries.get(index).copied().flatten()
    }

    pub fn set(&mut self, index: usize, status: PolicyStatus) {
        if self.entries.len() <= index {
            self.entries.resize(POLICIES.len(), None);
        }
        self.entries[index] = Some(status);
    }

    pub fn overlay(&mut self, other: &PolicyMap) {
        for (index, status) in other.entries.iter().enumerate() {
            if let Some(status) = status {
                self.set(index, *status);
            }
        }
    }
}

/// Parses `CMPnnnn` into a table index.
/// `stringToId` in cmPolicies.cxx: the id is positional, so the number in
/// the string selects the policy regardless of its declared name (4.1.0
/// declares policy 194 as `CMP194`, which `CMP0194` still resolves to).
pub fn index_of(id: &str) -> Option<usize> {
    if id.len() != 7 {
        return None;
    }
    let digits = id.strip_prefix("CMP")?;
    if digits.ends_with("0000") {
        return Some(0);
    }
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let number: usize = digits.parse().ok()?;
    (number < POLICIES.len()).then_some(number)
}

fn version_text(index: usize) -> String {
    let (major, minor, patch) = POLICIES[index].version;
    format!("{major}.{minor}.{patch}")
}

/// `cmPolicies::GetPolicyWarning`.
pub fn policy_warning(index: usize) -> String {
    let info = &POLICIES[index];
    format!(
        "Policy {} is not set: {}  Run \"cmake --help-policy {}\" for policy details.  Use the cmake_policy command to set the policy and suppress this warning.",
        info.id, info.doc, info.id
    )
}

/// `cmPolicies::GetPolicyDeprecatedWarning`.
pub fn deprecated_warning(index: usize) -> String {
    let info = &POLICIES[index];
    format!(
        "The OLD behavior for policy {} will be removed from a future version of CMake.\nThe cmake-policies(7) manual explains that the OLD behaviors of all policies are deprecated and that a policy should be set to OLD only under specific short-term circumstances.  Projects should be ported to the NEW behavior and not rely on setting a policy to OLD.",
        info.id
    )
}

/// `cmPolicies::GetRemovedPolicyError`.
pub fn removed_error(index: usize) -> String {
    let info = &POLICIES[index];
    format!(
        "Policy {} may not be set to OLD behavior because this version of CMake no longer supports it.  The policy was introduced in CMake version {}, and use of NEW behavior is now required.\nPlease either update your CMakeLists.txt files to conform to the new behavior or use an older version of CMake that still supports the old behavior.  Run cmake --help-policy {} for more information.",
        info.id,
        version_text(index),
        info.id
    )
}

/// Whether policy `index` was introduced after `major.minor.patch`.
pub fn newer_than(index: usize, major: u32, minor: u32, patch: u32) -> bool {
    POLICIES[index].version > (major, minor, patch)
}
