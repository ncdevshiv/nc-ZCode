//! Built-in agent profile registry (spec: specs/nc-ag-orchestration.md section 5, A-P) plus
//! runtime registration of new specialisations.
//!
//! A profile is a *capability contract*: persona, expertise, allowed tools,
//! permission ceiling, deliverable shape and escalation rule. The orchestrator may
//! spawn only profiles that exist here or in the project's added profiles.

use std::collections::BTreeMap;

use serde_json::json;

use crate::error::{NcagError, Result};
use crate::types::AgentProfile;

fn profile(
    id: &str,
    role: &str,
    display_name: &str,
    persona: &str,
    expertise: &[&str],
    capabilities: &[&str],
    tools: &[&str],
    permissions: &[&str],
    restrictions: &[&str],
    deliverable: &str,
    review_capable: bool,
) -> AgentProfile {
    AgentProfile {
        id: id.to_string(),
        role: role.to_string(),
        display_name: display_name.to_string(),
        persona: persona.to_string(),
        expertise: expertise.iter().map(|v| v.to_string()).collect(),
        capabilities: capabilities.iter().map(|v| v.to_string()).collect(),
        tools: tools.iter().map(|v| v.to_string()).collect(),
        permissions: permissions.iter().map(|v| v.to_string()).collect(),
        restrictions: restrictions.iter().map(|v| v.to_string()).collect(),
        deliverable: deliverable.to_string(),
        quality_bar: "Acceptance criteria machine-verified; no unverified success claims.".to_string(),
        escalation: "Escalate to the orchestrator when requirements are ambiguous, a contract is missing, or verification cannot pass after one repair attempt.".to_string(),
        review_capable,
    }
}

const READ: &str = "fs.read";
const WRITE: &str = "fs.write";
const SHELL: &str = "shell.exec";

pub fn builtin() -> Vec<AgentProfile> {
    vec![
        profile(
            "generalist",
            "general",
            "Generalist",
            "Pragmatic, resourceful, knows when to ask for a specialist.",
            &["research", "analysis", "implementation"],
            &["research", "small-implementation", "analysis"],
            &[READ, WRITE],
            &[READ, WRITE],
            &["Do not claim specialist outcomes outside your scope."],
            "A written summary, the files changed and the commands that substantiate it.",
            false,
        ),
        profile(
            "architect",
            "architecture",
            "System Architect",
            "Strategic, structured and conservative about architectural risk; states trade-offs explicitly.",
            &["system-design", "module-boundaries", "interfaces"],
            &["architecture", "contract-design", "interface-review"],
            &[READ, WRITE],
            &[READ, WRITE],
            &["Do not rewrite working systems to match personal preference."],
            "An architecture note with module boundaries, interfaces, dependencies and rejected alternatives.",
            true,
        ),
        profile(
            "backend",
            "backend",
            "Backend Engineer",
            "Implementation-focused and correctness-obsessed; prefers explicit error handling.",
            &["services", "business-logic", "auth", "db-integration"],
            &["backend", "service-implementation", "unit-tests"],
            &[READ, WRITE, SHELL],
            &[READ, WRITE, SHELL],
            &["Do not alter public API contracts without an approved decision."],
            "Working code plus the test command output proving it.",
            false,
        ),
        profile(
            "frontend",
            "frontend",
            "Frontend Engineer",
            "User-experience oriented; cares about accessibility and loading states.",
            &["ui", "state-management", "accessibility"],
            &["frontend", "component-implementation", "ui-tests"],
            &[READ, WRITE, SHELL],
            &[READ, WRITE, SHELL],
            &["Do not invent backend behaviour; read the contract."],
            "Implemented components with rendered-state evidence.",
            false,
        ),
        profile(
            "api",
            "api",
            "API Engineer",
            "Precise about schemas, versioning and compatibility.",
            &["api-contracts", "schemas", "versioning"],
            &["api", "contract-implementation", "schema-validation"],
            &[READ, WRITE, SHELL],
            &[READ, WRITE, SHELL],
            &["Do not break existing consumers silently."],
            "Endpoint contract, request/response schema and compatibility notes.",
            false,
        ),
        profile(
            "database",
            "database",
            "Database Engineer",
            "Defensive about data integrity and migrations.",
            &["schema-design", "migrations", "indexing", "recovery"],
            &["database", "migration", "query-optimisation"],
            &[READ, WRITE, SHELL],
            &[READ, WRITE, SHELL],
            &["Do not drop or silently rewrite user data."],
            "Migration files, rollback plan and integrity checks.",
            false,
        ),
        profile(
            "devops",
            "devops",
            "DevOps & Infrastructure Engineer",
            "Reliability-minded; treats a manual step as a defect.",
            &["build", "deployment", "ci-cd", "containers"],
            &["build", "deployment", "ci"],
            &[READ, WRITE, SHELL],
            &[READ, WRITE, SHELL],
            &["Do not touch production credentials."],
            "Reproducible build/deploy definition and a verified local run.",
            false,
        ),
        profile(
            "docs",
            "documentation",
            "Documentation Engineer",
            "Precise and clarity-focused; documentation must match the code.",
            &["architecture-docs", "api-docs", "runbooks"],
            &["documentation"],
            &[READ, WRITE],
            &[READ, WRITE],
            &["Do not document behaviour that is not implemented."],
            "Documents that reflect the actual implementation, with limitations listed.",
            false,
        ),
        profile(
            "qa",
            "qa",
            "QA Engineer",
            "Systematic; a test that cannot fail is not a test.",
            &["test-planning", "regression", "negative-testing"],
            &["testing", "verification"],
            &[READ, WRITE, SHELL],
            &[READ, WRITE, SHELL],
            &["Do not weaken assertions to make a suite pass."],
            "Test plan, executed results and coverage of failure paths.",
            true,
        ),
        profile(
            "e2e",
            "e2e-test",
            "E2E Test Engineer",
            "Thinks in user journeys and failure paths.",
            &["e2e-architecture", "browser-automation", "api-workflows"],
            &["e2e-testing", "verification"],
            &[READ, WRITE, SHELL],
            &[READ, WRITE, SHELL],
            &["Do not mask flaky outcomes; report reproducibility."],
            "E2E scenarios with evidence per scenario and known flakiness.",
            true,
        ),
        profile(
            "security",
            "security",
            "Security Auditor",
            "Skeptical, evidence-driven and risk-focused.",
            &["authn-authz", "secret-management", "dependency-risk"],
            &["security-audit", "verification"],
            &[READ, SHELL],
            &[READ, SHELL],
            &["Read-only: never modifies the code being audited."],
            "Findings with severity, evidence, impact, correction and verification method.",
            true,
        ),
        profile(
            "reviewer",
            "review",
            "Code Reviewer",
            "Direct but constructive; correctness before style.",
            &["code-quality", "error-handling", "regression-risk"],
            &["code-review", "verification"],
            &[READ, SHELL],
            &[READ, SHELL],
            &["Read-only: findings only, never edits."],
            "Approval or rejection with per-finding severity and evidence.",
            true,
        ),
        profile(
            "critiquer",
            "critique",
            "Critiquer",
            "Challenges assumptions with evidence; never argumentative for its own sake.",
            &["risk-analysis", "alternatives", "hidden-dependencies"],
            &["critique", "plan-review"],
            &[READ],
            &[READ],
            &["Must state what evidence would change its mind."],
            "Weaknesses found, scenarios missed and a concrete alternative for each.",
            true,
        ),
        profile(
            "integrator",
            "integration",
            "Integration & Merge Engineer",
            "Treats a textual merge as a hypothesis, not a proof.",
            &["merge-conflicts", "semantic-conflicts", "integration-tests"],
            &["integration", "merge"],
            &[READ, SHELL, "git.merge"],
            &[READ, SHELL, "git.merge"],
            &["Never merges a task whose verification evidence is missing."],
            "Integration report: files, conflicts, tests and the resulting commit.",
            false,
        ),
        profile(
            "performance",
            "performance",
            "Performance Engineer",
            "Measures before changing anything.",
            &["profiling", "load-testing", "resource-use"],
            &["performance", "benchmark"],
            &[READ, WRITE, SHELL],
            &[READ, WRITE, SHELL],
            &["Do not trade correctness for speed."],
            "Measurements before/after and the bottleneck identified.",
            true,
        ),
        profile(
            "analyst",
            "requirements",
            "Product & Requirements Analyst",
            "Turns vague intent into testable acceptance criteria.",
            &["requirements", "user-journeys", "traceability"],
            &["requirements-analysis", "acceptance-criteria"],
            &[READ, WRITE],
            &[READ, WRITE],
            &["Do not invent user preferences; mark assumptions."],
            "Normalised requirements with acceptance criteria and contradictions found.",
            true,
        ),
    ]
}

/// The profile registry: built-ins plus profiles added at runtime.
#[derive(Clone)]
pub struct ProfileRegistry {
    profiles: BTreeMap<String, AgentProfile>,
}

impl ProfileRegistry {
    pub fn new() -> Self {
        let mut profiles = BTreeMap::new();
        for profile in builtin() {
            profiles.insert(profile.id.clone(), profile);
        }
        Self { profiles }
    }

    pub fn get(&self, id: &str) -> Option<&AgentProfile> {
        self.profiles.get(id)
    }

    pub fn require(&self, id: &str) -> Result<&AgentProfile> {
        self.get(id)
            .ok_or_else(|| NcagError::not_found(format!("agent profile {id} is not registered")))
    }

    pub fn all(&self) -> Vec<AgentProfile> {
        self.profiles.values().cloned().collect()
    }

    /// Find a profile that has a capability; error names the missing capability.
    pub fn find_capability(&self, capability: &str) -> Option<&AgentProfile> {
        self.profiles
            .values()
            .find(|profile| profile.capabilities.iter().any(|item| item == capability))
    }

    /// Register a new specialisation before it can be spawned (spec section 5.2).
    pub fn register(&mut self, profile: AgentProfile) -> Result<()> {
        if self.profiles.contains_key(&profile.id) {
            return Err(NcagError::conflict(format!(
                "agent profile {} already exists",
                profile.id
            )));
        }
        self.profiles.insert(profile.id.clone(), profile);
        Ok(())
    }

    /// Which profiles can satisfy a set of required capabilities?
    pub fn capable_of(&self, capabilities: &[String]) -> Vec<&AgentProfile> {
        self.profiles
            .values()
            .filter(|profile| {
                capabilities
                    .iter()
                    .all(|needed| profile.capabilities.contains(needed))
            })
            .collect()
    }
}

impl Default for ProfileRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Prefer a profile already assigned to this project before spawning a new one
/// (spec section 5.2: no unnecessary agent proliferation).
pub fn pick_existing(
    existing: &[crate::types::Agent],
    registry: &ProfileRegistry,
    capability: &str,
    busy_agent_ids: &[String],
) -> Option<crate::types::Agent> {
    existing
        .iter()
        .filter(|agent| !busy_agent_ids.contains(&agent.id))
        .filter(|agent| !agent.status.is_terminal())
        .filter(|agent| {
            registry
                .get(&agent.profile_id)
                .map(|profile| profile.capabilities.iter().any(|item| item == capability))
                .unwrap_or(false)
        })
        .min_by_key(|agent| {
            // Idle agents first, then lowest accumulated cost.
            let rank = match agent.status {
                crate::types::AgentStatus::Idle => 0,
                crate::types::AgentStatus::Stopped => 1,
                _ => 2,
            };
            (rank, (agent.cost_usd * 1000.0) as i64)
        })
        .cloned()
}

pub fn profile_json(profile: &AgentProfile) -> serde_json::Value {
    serde_json::to_value(profile).unwrap_or(json!({}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_sixteen_builtin_profiles_are_registered_with_permissions() {
        let registry = ProfileRegistry::new();
        assert_eq!(registry.all().len(), 16);
        for profile in registry.all() {
            assert!(!profile.persona.is_empty(), "{}", profile.id);
            assert!(!profile.capabilities.is_empty(), "{}", profile.id);
            assert!(!profile.deliverable.is_empty(), "{}", profile.id);
            // Writers must be able to read; reviewers must not hold write permission.
            if profile.permissions.contains(&WRITE.to_string()) {
                assert!(profile.permissions.contains(&READ.to_string()));
            }
            if profile.review_capable && profile.role == "review" {
                assert!(!profile.permissions.contains(&WRITE.to_string()));
            }
        }
    }

    #[test]
    fn capability_lookup_and_registration_are_enforced() {
        let mut registry = ProfileRegistry::new();
        assert!(registry.find_capability("backend").is_some());
        let duplicate = registry
            .register(registry.require("backend").unwrap().clone())
            .expect_err("duplicate must fail");
        assert_eq!(duplicate.code, "conflict");
        registry
            .register(profile(
                "ml",
                "ml",
                "ML Engineer",
                "Empirical.",
                &["ml"],
                &["model-training"],
                &[READ, WRITE],
                &[READ, WRITE],
                &[],
                "Trained model + eval numbers.",
                false,
            ))
            .unwrap();
        assert!(registry.find_capability("model-training").is_some());
    }
}
