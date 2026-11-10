//! NC-AG domain types (spec: specs/nc-ag-orchestration.md section 3).
//!
//! These are the wire shapes too: every struct is `camelCase` on the JSON edge so the
//! dashboard and API clients read one vocabulary. Status enums serialize as stable
//! snake_case strings and are stored verbatim in SQLite.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ============================================================
// project / request / requirement
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectConfig {
    pub max_concurrent_agents: usize,
    pub max_agents_per_task: u32,
    pub max_retries: u32,
    pub max_review_cycles: u32,
    pub project_budget_usd: f64,
    pub task_token_budget: u64,
    pub require_plan_approval: bool,
    /// Material decisions require an explicit user decision instead of an autonomous pick.
    pub require_material_approval: bool,
    pub stall_after_secs: u64,
    pub heartbeat_timeout_secs: u64,
    pub lease_ttl_secs: u64,
    /// Commands agents may run (prefix match on argv[0..2]); everything else is denied.
    pub allowed_commands: Vec<String>,
    pub default_decision_class: String,
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            max_concurrent_agents: 4,
            max_agents_per_task: 3,
            max_retries: 2,
            max_review_cycles: 2,
            project_budget_usd: 25.0,
            task_token_budget: 250_000,
            require_plan_approval: false,
            require_material_approval: true,
            stall_after_secs: 300,
            heartbeat_timeout_secs: 90,
            lease_ttl_secs: 900,
            allowed_commands: Self::default_commands(),
            default_decision_class: "technical_recommendation".to_string(),
        }
    }
}

impl ProjectConfig {
    fn default_commands() -> Vec<String> {
        vec![
            "cargo test".into(),
            "cargo build".into(),
            "cargo check".into(),
            "cargo fmt".into(),
            "cargo clippy".into(),
            "bun test".into(),
            "bun run".into(),
            "bunx tsc".into(),
            "npm test".into(),
            "npm run".into(),
            "node".into(),
            "python".into(),
            "pytest".into(),
            "go test".into(),
            "go build".into(),
            "git status".into(),
            "git diff".into(),
            "git log".into(),
            // Read-only change-set inspection: a verifier could not prove "no other file
            // changed" without these, and the live run failed its acceptance check because
            // `git ls-files` / `git show` were not on the allowlist.
            "git ls-files".into(),
            "git show".into(),
            "ls".into(),
            "cat".into(),
            "echo".into(),
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectStatus {
    Active,
    Paused,
    Completed,
    Archived,
}

impl ProjectStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Archived => "archived",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "active" => Self::Active,
            "paused" => Self::Paused,
            "completed" => Self::Completed,
            "archived" => Self::Archived,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: String,
    pub name: String,
    pub workspace_path: String,
    pub base_branch: String,
    pub base_commit: Option<String>,
    pub status: ProjectStatus,
    pub config: ProjectConfig,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestStatus {
    Received,
    Analyzing,
    Planned,
    AwaitingApproval,
    WaitingProvider,
    Executing,
    Completed,
    Failed,
    Cancelled,
}

impl RequestStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::Analyzing => "analyzing",
            Self::Planned => "planned",
            Self::AwaitingApproval => "awaiting_approval",
            Self::WaitingProvider => "waiting_provider",
            Self::Executing => "executing",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "received" => Self::Received,
            "analyzing" => Self::Analyzing,
            "planned" => Self::Planned,
            "awaiting_approval" => Self::AwaitingApproval,
            "waiting_provider" => Self::WaitingProvider,
            "executing" => Self::Executing,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub id: String,
    pub project_id: String,
    pub instruction: String,
    pub status: RequestStatus,
    pub planner: Option<String>,
    pub analysis: Option<Value>,
    pub plan: Option<Value>,
    pub trace_id: String,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntakeAnalysis {
    pub intent: String,
    pub outcomes: Vec<String>,
    pub constraints: Vec<String>,
    pub technologies: Vec<String>,
    pub ambiguities: Vec<String>,
    pub assumptions: Vec<String>,
    pub contradictions: Vec<String>,
    pub risk_notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Requirement {
    pub id: String,
    pub request_id: String,
    pub code: String,
    pub description: String,
    pub category: String,
    pub priority: i32,
    pub acceptance: Vec<AcceptanceCheck>,
    pub verification: String,
    pub source_excerpt: Option<String>,
    pub status: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceKind {
    CommandSucceeded,
    FileExists,
    FileContains,
    OutputContains,
    ReviewApproved,
    AuditClean,
    TestPassed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcceptanceCheck {
    pub id: String,
    pub description: String,
    pub kind: AcceptanceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<String>,
}

// ============================================================
// tasks
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Created,
    Planned,
    Ready,
    Assigned,
    Running,
    Review,
    Verification,
    Completed,
    Rejected,
    Failed,
    Cancelled,
    Blocked,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Planned => "planned",
            Self::Ready => "ready",
            Self::Assigned => "assigned",
            Self::Running => "running",
            Self::Review => "review",
            Self::Verification => "verification",
            Self::Completed => "completed",
            Self::Rejected => "rejected",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Blocked => "blocked",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "created" => Self::Created,
            "planned" => Self::Planned,
            "ready" => Self::Ready,
            "assigned" => Self::Assigned,
            "running" => Self::Running,
            "review" => Self::Review,
            "verification" => Self::Verification,
            "completed" => Self::Completed,
            "rejected" => Self::Rejected,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "blocked" => Self::Blocked,
            _ => return None,
        })
    }
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Rejected
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: String,
    pub project_id: String,
    pub request_id: String,
    pub requirement_id: Option<String>,
    pub title: String,
    pub description: String,
    pub capability: String,
    pub role: String,
    pub agent_id: Option<String>,
    pub status: TaskStatus,
    pub priority: i32,
    pub depends_on: Vec<String>,
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub acceptance: Vec<AcceptanceCheck>,
    pub resources: Vec<String>,
    pub max_retries: u32,
    pub attempts: u32,
    pub timeout_ms: u64,
    pub deadline_at: Option<i64>,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    pub result: Option<Value>,
    pub blocker: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub request_id: String,
    pub summary: String,
    pub critical_path: Vec<String>,
    pub tasks: Vec<PlannedTask>,
    pub estimated_agents: usize,
    pub estimated_tokens: u64,
    pub assumptions: Vec<String>,
    pub contradictions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlannedTask {
    pub key: String,
    pub title: String,
    pub description: String,
    pub capability: String,
    pub role: String,
    pub priority: i32,
    pub depends_on: Vec<String>,
    pub resources: Vec<String>,
    pub acceptance: Vec<AcceptanceCheck>,
    pub expected_outputs: Vec<String>,
}

// ============================================================
// agent profiles / agents
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentProfile {
    pub id: String,
    pub role: String,
    pub display_name: String,
    pub persona: String,
    pub expertise: Vec<String>,
    pub capabilities: Vec<String>,
    pub tools: Vec<String>,
    pub permissions: Vec<String>,
    pub restrictions: Vec<String>,
    pub deliverable: String,
    pub quality_bar: String,
    pub escalation: String,
    pub review_capable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Created,
    Initializing,
    Starting,
    Idle,
    Assigned,
    Working,
    Waiting,
    Blocked,
    HelpRequested,
    Reviewing,
    HandoffPending,
    Recovering,
    Stopping,
    Stopped,
    Failed,
}

impl AgentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Initializing => "initializing",
            Self::Starting => "starting",
            Self::Idle => "idle",
            Self::Assigned => "assigned",
            Self::Working => "working",
            Self::Waiting => "waiting",
            Self::Blocked => "blocked",
            Self::HelpRequested => "help_requested",
            Self::Reviewing => "reviewing",
            Self::HandoffPending => "handoff_pending",
            Self::Recovering => "recovering",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "created" => Self::Created,
            "initializing" => Self::Initializing,
            "starting" => Self::Starting,
            "idle" => Self::Idle,
            "assigned" => Self::Assigned,
            "working" => Self::Working,
            "waiting" => Self::Waiting,
            "blocked" => Self::Blocked,
            "help_requested" => Self::HelpRequested,
            "reviewing" => Self::Reviewing,
            "handoff_pending" => Self::HandoffPending,
            "recovering" => Self::Recovering,
            "stopping" => Self::Stopping,
            "stopped" => Self::Stopped,
            "failed" => Self::Failed,
            _ => return None,
        })
    }
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Stopped | Self::Failed)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Agent {
    pub id: String,
    pub project_id: String,
    pub task_id: Option<String>,
    pub profile_id: String,
    pub role: String,
    pub status: AgentStatus,
    pub persona: String,
    pub capabilities: Vec<String>,
    pub permissions: Vec<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub workspace: Option<String>,
    pub worktree: Option<String>,
    pub last_heartbeat: Option<i64>,
    pub last_activity: Option<i64>,
    pub progress_marker: Option<String>,
    pub usage_input: u64,
    pub usage_output: u64,
    pub cost_usd: f64,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub ended_at: Option<i64>,
}

// ============================================================
// messaging
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Information,
    TaskUpdate,
    HelpRequest,
    HelpResponse,
    Blocker,
    ResourceRequest,
    ResourceRelease,
    ContractProposal,
    ContractChange,
    ArchitectureProposal,
    ArchitectureConflict,
    ReviewRequest,
    ReviewFinding,
    Handoff,
    Escalation,
    TaskCompletion,
    Failure,
    SystemEvent,
}

impl MessageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Information => "information",
            Self::TaskUpdate => "task_update",
            Self::HelpRequest => "help_request",
            Self::HelpResponse => "help_response",
            Self::Blocker => "blocker",
            Self::ResourceRequest => "resource_request",
            Self::ResourceRelease => "resource_release",
            Self::ContractProposal => "contract_proposal",
            Self::ContractChange => "contract_change",
            Self::ArchitectureProposal => "architecture_proposal",
            Self::ArchitectureConflict => "architecture_conflict",
            Self::ReviewRequest => "review_request",
            Self::ReviewFinding => "review_finding",
            Self::Handoff => "handoff",
            Self::Escalation => "escalation",
            Self::TaskCompletion => "task_completion",
            Self::Failure => "failure",
            Self::SystemEvent => "system_event",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "information" => Self::Information,
            "task_update" => Self::TaskUpdate,
            "help_request" => Self::HelpRequest,
            "help_response" => Self::HelpResponse,
            "blocker" => Self::Blocker,
            "resource_request" => Self::ResourceRequest,
            "resource_release" => Self::ResourceRelease,
            "contract_proposal" => Self::ContractProposal,
            "contract_change" => Self::ContractChange,
            "architecture_proposal" => Self::ArchitectureProposal,
            "architecture_conflict" => Self::ArchitectureConflict,
            "review_request" => Self::ReviewRequest,
            "review_finding" => Self::ReviewFinding,
            "handoff" => Self::Handoff,
            "escalation" => Self::Escalation,
            "task_completion" => Self::TaskCompletion,
            "failure" => Self::Failure,
            "system_event" => Self::SystemEvent,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: String,
    pub project_id: String,
    pub task_id: Option<String>,
    pub kind: MessageKind,
    pub sender: String,
    pub recipient: String,
    pub channel: String,
    pub subject: String,
    pub body: String,
    pub refs: Vec<String>,
    pub correlation_id: String,
    pub priority: i32,
    pub requires_ack: bool,
    pub ack_state: String,
    pub delivery_state: String,
    pub dedupe_key: Option<String>,
    pub created_at: i64,
}

// ============================================================
// leases
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseMode {
    SharedRead,
    ExclusiveWrite,
}

impl LeaseMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SharedRead => "shared_read",
            Self::ExclusiveWrite => "exclusive_write",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "shared_read" => Self::SharedRead,
            "exclusive_write" => Self::ExclusiveWrite,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lease {
    pub id: String,
    pub project_id: String,
    pub resource: String,
    pub mode: LeaseMode,
    pub owner_agent: Option<String>,
    pub owner_task: String,
    pub fencing_token: u64,
    pub state: String,
    pub acquired_at: i64,
    pub expires_at: i64,
    pub released_at: Option<i64>,
    pub release_reason: Option<String>,
}

// ============================================================
// decisions / evidence / integration / usage
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionClass {
    Autonomous,
    TechnicalRecommendation,
    Material,
    UserReserved,
}

impl DecisionClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Autonomous => "autonomous",
            Self::TechnicalRecommendation => "technical_recommendation",
            Self::Material => "material",
            Self::UserReserved => "user_reserved",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "autonomous" => Self::Autonomous,
            "technical_recommendation" => Self::TechnicalRecommendation,
            "material" => Self::Material,
            "user_reserved" => Self::UserReserved,
            _ => return None,
        })
    }
    pub fn needs_user(self) -> bool {
        matches!(self, Self::Material | Self::UserReserved)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionStatus {
    Open,
    Resolved,
    Overridden,
    Superseded,
}

impl DecisionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Resolved => "resolved",
            Self::Overridden => "overridden",
            Self::Superseded => "superseded",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "open" => Self::Open,
            "resolved" => Self::Resolved,
            "overridden" => Self::Overridden,
            "superseded" => Self::Superseded,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DecisionOption {
    pub id: String,
    pub label: String,
    pub pros: Vec<String>,
    pub cons: Vec<String>,
    pub risks: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Decision {
    pub id: String,
    pub project_id: String,
    pub request_id: Option<String>,
    pub task_id: Option<String>,
    pub class: DecisionClass,
    pub status: DecisionStatus,
    pub topic: String,
    /// A: what the user asked for.
    pub requested: String,
    /// B/D/F: what NC-AG believes, why, and the evidence.
    pub recommendation: String,
    pub evidence: Vec<String>,
    /// E: the comparison of both approaches (pros/cons/risks per side).
    pub options: Vec<DecisionOption>,
    pub risks: Vec<String>,
    pub resolution: Option<String>,
    pub rationale: Option<String>,
    pub decided_by: Option<String>,
    pub created_at: i64,
    pub resolved_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    Command,
    FileExists,
    FileContains,
    OutputContains,
    Test,
    Review,
    Audit,
    Integration,
    Build,
}

impl EvidenceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::FileExists => "file_exists",
            Self::FileContains => "file_contains",
            Self::OutputContains => "output_contains",
            Self::Test => "test",
            Self::Review => "review",
            Self::Audit => "audit",
            Self::Integration => "integration",
            Self::Build => "build",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "command" => Self::Command,
            "file_exists" => Self::FileExists,
            "file_contains" => Self::FileContains,
            "output_contains" => Self::OutputContains,
            "test" => Self::Test,
            "review" => Self::Review,
            "audit" => Self::Audit,
            "integration" => Self::Integration,
            "build" => Self::Build,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Evidence {
    pub id: String,
    pub project_id: String,
    pub task_id: String,
    pub kind: EvidenceKind,
    pub criterion: String,
    pub command: Option<String>,
    pub result: String,
    pub detail: String,
    pub artifact: Option<String>,
    pub produced_by: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Integration {
    pub id: String,
    pub project_id: String,
    pub task_id: String,
    pub base_commit: Option<String>,
    pub commit_sha: Option<String>,
    pub changed: Vec<String>,
    pub conflicts: Vec<String>,
    pub tests: Vec<String>,
    pub outcome: String,
    pub detail: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRow {
    pub project_id: String,
    pub task_id: Option<String>,
    pub agent_id: Option<String>,
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub created_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub calls: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub id: i64,
    pub project_id: Option<String>,
    pub task_id: Option<String>,
    pub agent_id: Option<String>,
    pub kind: String,
    pub payload: Value,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditEntry {
    pub id: i64,
    pub project_id: Option<String>,
    pub actor: String,
    pub action: String,
    pub detail: Value,
    pub prev_hash: String,
    pub hash: String,
    pub created_at: i64,
}

// ============================================================
// agent run results / delivery
// ============================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSubmission {
    pub summary: String,
    pub files_changed: Vec<String>,
    pub commands_run: Vec<String>,
    pub limitations: Vec<String>,
    pub acceptance_notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskOutcome {
    Completed,
    Rejected,
    Failed,
    Cancelled,
    WaitingProvider,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Delivery {
    pub request_id: String,
    pub project_id: String,
    pub status: String,
    pub summary: String,
    pub tasks: Vec<Value>,
    pub evidence: Vec<Value>,
    pub reviews: Vec<Value>,
    pub integrations: Vec<Value>,
    pub usage: ProjectUsage,
    pub limitations: Vec<String>,
    pub files_changed: Vec<String>,
    pub decisions: Vec<Value>,
    /// Requirement -> status -> tasks, for traceability in the final report.
    pub requirements: Vec<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Informational,
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Informational => "informational",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}
