//! 配置类 admitted 命令：一条命令 = 一份声明式 spec，共用同一条执行序列。
//!
//! 序列：mutate 状态 → 推进 revision → （按需）落盘 meta → 投递投影 → 收口 →
//! （按需）尝试提升队首。四条命令原本各自抄一遍这四步，共 200 行；现在只有 spec
//! 不同，执行序列一份，可测试、可扩展（新增配置命令只加一个 `ConfigCommand`）。

use std::sync::Arc;

use serde_json::Value;
use zcode_core::inbox::Admitted;
use zcode_core::protocol::CommandFinal;

use crate::commands::{payload_bool, payload_str, settle_admitted};
use crate::session::Session;
use crate::HostServer;

/// 一条配置命令的全部可变点，其余流程由 [`HostServer::run_config_command`] 持有。
pub(crate) struct ConfigCommand {
    /// 命令类型，用于日志与 fail-closed reasonCode。
    pub name: &'static str,
    /// 把 payload 落到 session 状态。不负责 revision、落盘或投影。
    pub mutate: fn(&Session, &Value),
    /// 状态变更后要投递的投影帧。
    pub project: fn(&Session) -> Vec<String>,
    /// 是否把 session meta 落盘：只有随 `SessionMeta` 持久化的字段需要。
    pub persist_meta: bool,
    /// 收口后是否尝试提升队首（返回 true 才 drain）。
    pub drain_when: Option<fn(&Session) -> bool>,
}

/// `setAutoDrain`：授权位。恢复授权时同时清除 stop/error 留下的暂停原因，
/// 并复刻 TS 的 ready hook——置 true 且空闲时立即按 sendQueuedNow 路径提升。
pub(crate) static SET_AUTO_DRAIN: ConfigCommand = ConfigCommand {
    name: "setAutoDrain",
    mutate: |session, payload| {
        let auto_drain = payload_bool(payload, "autoDrain");
        session.set_queue_auto_drain(auto_drain);
        if auto_drain {
            session.set_queue_pause_reason(None);
        }
    },
    project: |session| {
        let mut lines = session.project_queue_update();
        lines.extend(session.project_input_routing_update());
        lines
    },
    persist_meta: false,
    drain_when: Some(Session::queue_auto_drain),
};

/// `setFollowupMode`：queue / guide。决定 busy 时新输入是排队还是引导，落 meta。
pub(crate) static SET_FOLLOWUP_MODE: ConfigCommand = ConfigCommand {
    name: "setFollowupMode",
    mutate: |session, payload| {
        session.set_followup_mode(payload_str(payload, "mode", "queue"));
    },
    project: Session::project_config_update,
    persist_meta: true,
    drain_when: None,
};

/// `switchModelConfig`：provider / model / thought 三元组，UI 侧的模型档位切换。
pub(crate) static SWITCH_MODEL_CONFIG: ConfigCommand = ConfigCommand {
    name: "switchModelConfig",
    mutate: |session, payload| {
        session.set_model_config(
            payload_str(payload, "provider", ""),
            payload_str(payload, "model", ""),
            payload_str(payload, "thought", ""),
        );
    },
    project: Session::project_config_update,
    persist_meta: true,
    drain_when: None,
};

/// `switchCollaborationMode`：build / edit / plan / yolo（auto 不在可切换面）。
pub(crate) static SWITCH_COLLABORATION_MODE: ConfigCommand = ConfigCommand {
    name: "switchCollaborationMode",
    mutate: |session, payload| {
        session.set_collaboration_mode(payload_str(payload, "mode", "build"));
    },
    project: Session::project_config_update,
    persist_meta: true,
    drain_when: None,
};

impl HostServer {
    /// 配置命令的唯一执行序列。
    pub(crate) async fn run_config_command(
        self: &Arc<Self>,
        spec: &'static ConfigCommand,
        session: Arc<Session>,
        admitted: Admitted,
    ) {
        let payload = admitted.envelope.payload.clone();
        (spec.mutate)(&session, &payload);
        session.bump_revision();
        if spec.persist_meta {
            self.persist_meta(&session);
        }
        let lines = (spec.project)(&session);
        let drain = spec.drain_when.is_some_and(|probe| probe(&session));
        settle_admitted(
            &session,
            admitted,
            CommandFinal::accepted(),
            lines,
            &self.outbox,
        );
        if drain {
            // 与 TS 的 ready hook 同义：idle + autoDrain 时立即提升队首。
            self.drain_queue(session).await;
        }
    }
}

/// 配置命令的注册表：**路由事实的唯一来源**。`commands::plan` 按 `name` 查表，
/// 因此加一条配置命令只需要写一个 spec 并登记，不必再改 dispatcher 的 match。
pub(crate) static CONFIG_COMMANDS: [&ConfigCommand; 4] = [
    &SET_AUTO_DRAIN,
    &SET_FOLLOWUP_MODE,
    &SWITCH_MODEL_CONFIG,
    &SWITCH_COLLABORATION_MODE,
];

pub(crate) fn spec_for(command_type: &str) -> Option<&'static ConfigCommand> {
    CONFIG_COMMANDS
        .into_iter()
        .find(|spec| spec.name == command_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 注册表必须自洽：命令名唯一，且每条声明的命令都能被 `spec_for` 路由到。
    /// 漏登记 = 命令落到 fail-closed 分支，客户端只会看到 unsupported。
    #[test]
    fn registry_routes_every_declared_config_command() {
        for name in [
            "setAutoDrain",
            "setFollowupMode",
            "switchModelConfig",
            "switchCollaborationMode",
        ] {
            let spec =
                spec_for(name).unwrap_or_else(|| panic!("unregistered config command: {name}"));
            assert_eq!(spec.name, name);
        }
        assert!(spec_for("sendText").is_none());
        assert!(spec_for("notACommand").is_none());
        let mut names: Vec<&str> = CONFIG_COMMANDS.iter().map(|spec| spec.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "duplicate command name in registry");
    }

    /// 只有持久化到 `SessionMeta` 的字段才允许 `persist_meta`：autoDrain 是队列授权位，
    /// 事实归 transcript 投影而不是 meta，写成 true 会让两处状态分叉。
    #[test]
    fn only_meta_backed_fields_persist() {
        assert!(!SET_AUTO_DRAIN.persist_meta);
        assert!(SET_AUTO_DRAIN.drain_when.is_some());
        for spec in [
            &SET_FOLLOWUP_MODE,
            &SWITCH_MODEL_CONFIG,
            &SWITCH_COLLABORATION_MODE,
        ] {
            assert!(
                spec.persist_meta,
                "config field of {} must persist",
                spec.name
            );
            assert!(spec.drain_when.is_none(), "{} must not drain", spec.name);
        }
    }
}
