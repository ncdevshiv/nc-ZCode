-- NC-AG schema v1 (specs/nc-ag-orchestration.md §3).
-- One writer: the orchestrator crate's Store. WAL + busy_timeout (see db.rs).

CREATE TABLE IF NOT EXISTS projects (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  workspace_path TEXT NOT NULL,
  base_branch TEXT NOT NULL,
  base_commit TEXT,
  status TEXT NOT NULL,
  config_json TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS requests (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL,
  instruction TEXT NOT NULL,
  status TEXT NOT NULL,
  planner TEXT,
  analysis_json TEXT,
  plan_json TEXT,
  trace_id TEXT NOT NULL,
  error TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS requests_project ON requests(project_id, created_at);

CREATE TABLE IF NOT EXISTS requirements (
  id TEXT PRIMARY KEY,
  request_id TEXT NOT NULL,
  code TEXT NOT NULL,
  description TEXT NOT NULL,
  category TEXT NOT NULL,
  priority INTEGER NOT NULL,
  acceptance_json TEXT NOT NULL,
  verification TEXT NOT NULL,
  source_excerpt TEXT,
  status TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS requirements_request ON requirements(request_id);

CREATE TABLE IF NOT EXISTS tasks (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL,
  request_id TEXT NOT NULL,
  requirement_id TEXT,
  title TEXT NOT NULL,
  description TEXT NOT NULL,
  capability TEXT NOT NULL,
  role TEXT NOT NULL,
  agent_id TEXT,
  status TEXT NOT NULL,
  priority INTEGER NOT NULL,
  depends_on_json TEXT NOT NULL,
  inputs_json TEXT NOT NULL,
  outputs_json TEXT NOT NULL,
  acceptance_json TEXT NOT NULL,
  resources_json TEXT NOT NULL,
  max_retries INTEGER NOT NULL,
  attempts INTEGER NOT NULL,
  timeout_ms INTEGER NOT NULL,
  deadline_at INTEGER,
  worktree TEXT,
  branch TEXT,
  result_json TEXT,
  blocker TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  started_at INTEGER,
  ended_at INTEGER
);
CREATE INDEX IF NOT EXISTS tasks_project ON tasks(project_id, status);
CREATE INDEX IF NOT EXISTS tasks_request ON tasks(request_id);

CREATE TABLE IF NOT EXISTS agents (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL,
  task_id TEXT,
  profile_id TEXT NOT NULL,
  role TEXT NOT NULL,
  status TEXT NOT NULL,
  persona TEXT NOT NULL,
  capabilities_json TEXT NOT NULL,
  permissions_json TEXT NOT NULL,
  model TEXT,
  provider TEXT,
  workspace TEXT,
  worktree TEXT,
  last_heartbeat INTEGER,
  last_activity INTEGER,
  progress_marker TEXT,
  usage_input INTEGER NOT NULL DEFAULT 0,
  usage_output INTEGER NOT NULL DEFAULT 0,
  cost_usd REAL NOT NULL DEFAULT 0,
  error TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  ended_at INTEGER
);
CREATE INDEX IF NOT EXISTS agents_project ON agents(project_id, status);

CREATE TABLE IF NOT EXISTS messages (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL,
  task_id TEXT,
  kind TEXT NOT NULL,
  sender TEXT NOT NULL,
  recipient TEXT NOT NULL,
  channel TEXT NOT NULL,
  subject TEXT NOT NULL,
  body TEXT NOT NULL,
  refs_json TEXT NOT NULL,
  correlation_id TEXT NOT NULL,
  priority INTEGER NOT NULL,
  requires_ack INTEGER NOT NULL,
  ack_state TEXT NOT NULL,
  delivery_state TEXT NOT NULL,
  dedupe_key TEXT,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS messages_project ON messages(project_id, created_at);
CREATE UNIQUE INDEX IF NOT EXISTS messages_dedupe ON messages(dedupe_key) WHERE dedupe_key IS NOT NULL;

CREATE TABLE IF NOT EXISTS leases (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL,
  resource TEXT NOT NULL,
  mode TEXT NOT NULL,
  owner_agent TEXT,
  owner_task TEXT NOT NULL,
  fencing_token INTEGER NOT NULL,
  state TEXT NOT NULL,
  acquired_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  released_at INTEGER,
  release_reason TEXT
);
-- Shared reads may coexist on one resource; only an exclusive write is exclusive.
CREATE UNIQUE INDEX IF NOT EXISTS leases_exclusive ON leases(project_id, resource)
  WHERE state = 'active' AND mode = 'exclusive_write';

CREATE TABLE IF NOT EXISTS decisions (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL,
  request_id TEXT,
  task_id TEXT,
  class TEXT NOT NULL,
  status TEXT NOT NULL,
  topic TEXT NOT NULL,
  requested TEXT NOT NULL,
  recommendation TEXT NOT NULL,
  evidence_json TEXT NOT NULL,
  options_json TEXT NOT NULL,
  risks_json TEXT NOT NULL,
  resolution TEXT,
  rationale TEXT,
  decided_by TEXT,
  created_at INTEGER NOT NULL,
  resolved_at INTEGER
);
CREATE INDEX IF NOT EXISTS decisions_project ON decisions(project_id, status);

CREATE TABLE IF NOT EXISTS evidence (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL,
  task_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  criterion TEXT NOT NULL,
  command TEXT,
  result TEXT NOT NULL,
  detail TEXT NOT NULL,
  artifact TEXT,
  produced_by TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS evidence_task ON evidence(task_id, kind);

CREATE TABLE IF NOT EXISTS integrations (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL,
  task_id TEXT NOT NULL,
  base_commit TEXT,
  commit_sha TEXT,
  changed_json TEXT NOT NULL,
  conflicts_json TEXT NOT NULL,
  tests_json TEXT NOT NULL,
  outcome TEXT NOT NULL,
  detail TEXT NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS usage (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL,
  task_id TEXT,
  agent_id TEXT,
  provider TEXT NOT NULL,
  model TEXT NOT NULL,
  input_tokens INTEGER NOT NULL,
  output_tokens INTEGER NOT NULL,
  cost_usd REAL NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS usage_project ON usage(project_id, created_at);

CREATE TABLE IF NOT EXISTS events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  project_id TEXT,
  task_id TEXT,
  agent_id TEXT,
  kind TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS events_project ON events(project_id, id);

CREATE TABLE IF NOT EXISTS audit (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  project_id TEXT,
  actor TEXT NOT NULL,
  action TEXT NOT NULL,
  detail_json TEXT NOT NULL,
  prev_hash TEXT NOT NULL,
  hash TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
