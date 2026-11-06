//! `GET /api/server-info` payload — mirror of `ServerRemoteInfo` in
//! `packages/shared/src/server-remote.ts` (`protocolVersion` is literal `1`).

use serde_json::{json, Value};

use crate::ServerOptions;

pub const SERVER_REMOTE_PROTOCOL_VERSION: u32 = 1;

pub fn server_info(options: &ServerOptions, version: &str, auth_required: bool) -> Value {
    let workspaces: Vec<Value> = options
        .workspaces
        .iter()
        .map(|workspace| {
            let mut value = json!({ "path": workspace.path });
            if let Some(label) = &workspace.label {
                value["label"] = json!(label);
            }
            if let Some(identity) = &workspace.workspace_identity {
                value["workspaceIdentity"] = json!(identity);
            }
            value
        })
        .collect();

    let mut info = json!({
        "serverId": options.server_id,
        "version": version,
        "protocolVersion": SERVER_REMOTE_PROTOCOL_VERSION,
        "authRequired": auth_required,
        "workspaces": workspaces,
        "capabilities": {
            "desktopContinuous": true,
            "websocketRpc": true,
            "processResourceTelemetry": true,
            "remoteAccess": options.remote.is_some(),
        },
    });
    if let Some(name) = &options.name {
        info["name"] = json!(name);
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ServerOptions, WorkspaceInfo};

    #[test]
    fn workspace_fields_are_omitted_when_absent_and_never_null() {
        let options = ServerOptions {
            workspaces: vec![
                WorkspaceInfo::new("G:/workspace"),
                WorkspaceInfo::with_identity("G:/other", Some("identity-1".to_string())),
            ],
            ..ServerOptions::default()
        };
        let info = server_info(&options, "3.14.0", false);
        assert_eq!(info["protocolVersion"], json!(1));
        assert_eq!(info["capabilities"]["websocketRpc"], json!(true));
        assert_eq!(info["workspaces"][0]["path"], json!("G:/workspace"));
        assert!(info["workspaces"][0].get("workspaceIdentity").is_none());
        assert_eq!(
            info["workspaces"][1]["workspaceIdentity"],
            json!("identity-1")
        );
        assert_eq!(info["authRequired"], json!(false));
    }
}
