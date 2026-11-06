# -*- coding: utf-8 -*-
# Three final steps: delete legacy stub, add cold read entry to runtime, declare module in lib.rs and register zcode-session.
import io

failures = 0


def patch(path, pairs):
    global failures
    text = io.open(path, encoding="utf-8").read()
    for old, new, label in pairs:
        hits = text.count(old)
        if hits != 1:
            print("MISS(%d) [%s] in %s" % (hits, label, path))
            failures += 1
            continue
        text = text.replace(old, new)
        print("ok [%s]" % label)
    io.open(path, "w", encoding="utf-8", newline="").write(text)


# ── 1. Delete legacy stub (two functions that fake status, have been replaced by build_snapshot) ──
path = "native/crates/zcode-services-agent/src/service.rs"
text = io.open(path, encoding="utf-8").read()
start = text.find("fn legacy_snapshot(workspace_path: &str, session_id: &str) -> Value {")
end = text.find("fn empty_subscription(")
if start == -1 or end == -1 or start >= end:
    print("MISS legacy stub span", start, end)
    failures += 1
else:
    # Delete together with the comment immediately above.
    head = text.rfind("\n\n", 0, start) + 1
    io.open(path, "w", encoding="utf-8", newline="").write(text[:head] + text[end:])
    print("ok [delete legacy stub]")

# ── 2. runtime: cold read entry (host's load_session is the only cold recovery entry, pub is available)──
patch(
    "native/crates/zcode-services-agent/src/runtime.rs",
    [
        (
            """ /// Session running state projection (task list reading; does not trigger cold resume).
    pub fn session_view(&self, session_id: &str) -> Option<Value> {
        self.server.session_view(session_id)
    }""",
            """ /// Session running state projection (task list reading; does not trigger cold resume).
    pub fn session_view(&self, session_id: &str) -> Option<Value> {
        self.server.session_view(session_id)
    }

    /// Cold resume: `v4/conversation/resync/subscribe` has been cold resumed by host.
    /// Here we add one more time to the session with "meta but not loaded" - `load_session` is unique to the host/// Cold recovery entry (zcode-host/src/lib.rs:1091). Returns whether it has been loaded.
    pub fn ensure_loaded(&self, session_id: &str) -> bool {
        if self.server.session_view(session_id).is_some() {
            return true;
        }
        self.server.load_session(session_id).is_some()
    }""",
            "runtime ensure_loaded",
        ),
    ],
)

# ── 3. lib.rs: declare snapshot/session module + register zcode-session ──
patch(
    "native/crates/zcode-services-agent/src/lib.rs",
    [
        (
            """pub mod runtime;
pub mod service;
pub mod task;""",
            """pub mod runtime;
pub mod service;
pub mod session;
pub mod snapshot;
pub mod task;""",
            "declare snapshot/session modules",
        ),
        (
            "pub use service::AgentService;",
            "pub use service::AgentService;\npub use session::SessionService;",
            "export SessionService",
        ),
        (
            """ registry.register("zcode-agent", self.service.clone())?;
        registry.register("zcode-task", self.tasks.clone())?;""",
            """ registry.register("zcode-agent", self.service.clone())?;
        registry.register("zcode-session", self.sessions.clone())?;
        registry.register("zcode-task", self.tasks.clone())?;""",
            "register zcode-session",
        ),
        (
            """pub struct AgentServices {
    pub service: Arc<AgentService>,
    pub tasks: Arc<TaskService>,
}""",
            """pub struct AgentServices {
    pub service: Arc<AgentService>,
    pub sessions: Arc<SessionService>,
    pub tasks: Arc<TaskService>,
}""",
            "AgentServices field",
        ),
        (
            """ let service = AgentService::new(paths.clone(), store);
        let tasks = TaskService::new(paths, Arc::clone(&service));        Self { service, tasks }""",
            """        let service = AgentService::new(paths.clone(), store);
        let sessions = SessionService::new(Arc::clone(&service));
        let tasks = TaskService::new(paths, Arc::clone(&service));
        Self {
            service,
            sessions,
            tasks,
        }""",
            "construct SessionService",
        ),
    ],
)

if failures:
    raise SystemExit("FAILED: %d" % failures)
print("runtime + lib wired")
