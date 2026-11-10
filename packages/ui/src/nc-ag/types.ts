/**
 * NC-AG wire types (mirror of `native/crates/nc-code-orchestrator/src/types.rs`).
 * These are the API shapes, not a second model: the dashboard reads exactly what
 * `/api/nc-ag/*` returns.
 */

export type ProjectStatus = "active" | "paused" | "completed" | "archived";
export type RequestStatus =
  | "received"
  | "analyzing"
  | "planned"
  | "awaiting_approval"
  | "waiting_provider"
  | "executing"
  | "completed"
  | "failed"
  | "cancelled";
export type TaskStatus =
  | "created"
  | "planned"
  | "ready"
  | "assigned"
  | "running"
  | "review"
  | "verification"
  | "completed"
  | "rejected"
  | "failed"
  | "cancelled"
  | "blocked";
export type AgentStatus =
  | "created"
  | "initializing"
  | "starting"
  | "idle"
  | "assigned"
  | "working"
  | "waiting"
  | "blocked"
  | "help_requested"
  | "reviewing"
  | "handoff_pending"
  | "recovering"
  | "stopping"
  | "stopped"
  | "failed";
export type DecisionStatus = "open" | "resolved" | "overridden" | "superseded";
export type DecisionClass = "autonomous" | "technical_recommendation" | "material" | "user_reserved";

export interface AcceptanceCheck {
  id: string;
  description: string;
  kind: string;
  command?: string;
  target?: string;
  expect?: string;
}

export interface NcagProject {
  id: string;
  name: string;
  workspacePath: string;
  baseBranch: string;
  baseCommit: string | null;
  status: ProjectStatus;
  config: {
    maxConcurrentAgents: number;
    projectBudgetUsd: number;
    taskTokenBudget: number;
    maxReviewCycles: number;
    requirePlanApproval: boolean;
    allowedCommands: string[];
  };
  createdAt: number;
  updatedAt: number;
}

export interface NcagTask {
  id: string;
  projectId: string;
  requestId: string;
  title: string;
  description: string;
  capability: string;
  role: string;
  agentId: string | null;
  status: TaskStatus;
  priority: number;
  dependsOn: string[];
  acceptance: AcceptanceCheck[];
  resources: string[];
  attempts: number;
  maxRetries: number;
  worktree: string | null;
  branch: string | null;
  result: Record<string, unknown> | null;
  blocker: string | null;
  createdAt: number;
  updatedAt: number;
}

export interface NcagAgent {
  id: string;
  projectId: string;
  taskId: string | null;
  profileId: string;
  role: string;
  status: AgentStatus;
  persona: string;
  capabilities: string[];
  permissions: string[];
  model: string | null;
  provider: string | null;
  worktree: string | null;
  lastHeartbeat: number | null;
  usageInput: number;
  usageOutput: number;
  costUsd: number;
  error: string | null;
  createdAt: number;
}

export interface NcagMessage {
  id: string;
  projectId: string;
  taskId: string | null;
  kind: string;
  sender: string;
  recipient: string;
  channel: string;
  subject: string;
  body: string;
  refs: string[];
  correlationId: string;
  priority: number;
  requiresAck: boolean;
  ackState: string;
  deliveryState: string;
  createdAt: number;
}

export interface NcagDecisionOption {
  id: string;
  label: string;
  pros: string[];
  cons: string[];
  risks: string[];
}

export interface NcagDecision {
  id: string;
  projectId: string;
  requestId: string | null;
  taskId: string | null;
  class: DecisionClass;
  status: DecisionStatus;
  topic: string;
  requested: string;
  recommendation: string;
  evidence: string[];
  options: NcagDecisionOption[];
  risks: string[];
  resolution: string | null;
  rationale: string | null;
  decidedBy: string | null;
  createdAt: number;
  resolvedAt: number | null;
}

export interface NcagLease {
  id: string;
  projectId: string;
  resource: string;
  mode: "shared_read" | "exclusive_write";
  ownerAgent: string | null;
  ownerTask: string;
  fencingToken: number;
  state: string;
  acquiredAt: number;
  expiresAt: number;
}

export interface NcagEvidence {
  id: string;
  taskId: string;
  kind: string;
  criterion: string;
  command: string | null;
  result: string;
  detail: string;
  producedBy: string;
  createdAt: number;
}

export interface NcagIntegration {
  id: string;
  taskId: string;
  commitSha: string | null;
  changed: string[];
  conflicts: string[];
  outcome: string;
  detail: string;
  createdAt: number;
}

export interface NcagEvent {
  id: number;
  projectId: string | null;
  taskId: string | null;
  agentId: string | null;
  kind: string;
  payload: Record<string, unknown>;
  createdAt: number;
}

export interface NcagRequest {
  id: string;
  projectId: string;
  instruction: string;
  status: RequestStatus;
  planner: string | null;
  analysis: Record<string, unknown> | null;
  plan: {
    summary: string;
    criticalPath: string[];
    tasks: { key: string; title: string; capability: string; role: string; dependsOn: string[] }[];
    estimatedAgents: number;
    estimatedTokens: number;
    assumptions: string[];
    contradictions: string[];
  } | null;
  traceId: string;
  error: string | null;
  createdAt: number;
}

export interface NcagDelivery {
  requestId: string;
  status: string;
  summary: string;
  tasks: Record<string, unknown>[];
  evidence: Record<string, unknown>[];
  reviews: Record<string, unknown>[];
  integrations: Record<string, unknown>[];
  usage: { inputTokens: number; outputTokens: number; costUsd: number; calls: number };
  limitations: string[];
  filesChanged: string[];
  decisions: NcagDecision[];
}

export interface NcagProfile {
  id: string;
  role: string;
  displayName: string;
  persona: string;
  capabilities: string[];
  permissions: string[];
  deliverable: string;
  reviewCapable: boolean;
}

export interface NcagDashboardProject {
  project: NcagProject;
  counts: {
    running: number;
    idle: number;
    blocked: number;
    failed: number;
    tasksActive: number;
    tasksCompleted: number;
    pendingReviews: number;
  };
  usage: { inputTokens: number; outputTokens: number; costUsd: number; calls: number };
  activeLeases: number;
}

export interface NcagDashboard {
  projects: NcagDashboardProject[];
  openDecisions: number;
  inFlight: number;
  recentEvents: NcagEvent[];
  auditVerified: boolean;
}

export interface NcagHealth {
  status: string;
  auditChainVerified: boolean;
  inFlight: number;
  provider: { configured: boolean; detail?: string; note?: string };
  database: string;
}
