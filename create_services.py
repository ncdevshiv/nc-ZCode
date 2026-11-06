import pathlib
base = r'G:\ZCode-main\native\crates'
for name in ['hooks','memory','subagents']:
    d = pathlib.Path(base)/f'nc-code-services-{name}'
    d.mkdir(parents=True, exist_ok=True)
    (d/'src').mkdir(exist_ok=True)
    (d/'tests').mkdir(exist_ok=True)
    cargo = f'''[package]
name = "nc-code-services-{name}"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true
publish.workspace = true
description = "{name} service channel"

[dependencies]
async-trait = "0.1"
serde_json.workspace = true
tokio.workspace = true
nc-code-channels = {{ path = "../nc-code-channels" }}
nc-code-rpc = {{ path = "../nc-code-rpc" }}
nc-code-service-core = {{ path = "../nc-code-service-core" }}

[dev-dependencies]
tokio = {{ workspace = true, features = ["test-util"] }}
'''
    (d/'Cargo.toml').write_text(cargo)
    struct_name = name.replace('-', ' ').title().replace(' ', '')
    lib = f'''//! `{name}` channel

use std::sync::Arc;

use async_trait::async_trait;
use nc_code_channels::{{CancelToken, Channel, ChannelContext, ChannelFailure}};
use nc_code_rpc::WireValue;
use nc_code_service_core::{{ServiceArgs, ServiceError}};
use serde_json::json;

pub struct {struct_name}Service;

impl {struct_name}Service {{
    pub fn new() -> Arc<Self> {{
        Arc::new(Self)
    }}
}}

impl Default for {struct_name}Service {{
    fn default() -> Self {{
        Self
    }}
}}

#[async_trait]
impl Channel for {struct_name}Service {{
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {{
        let _args = ServiceArgs::new(args);
        match command {{
            "list" | "get" | "getStatus" => Ok(WireValue::Json(json!([]))),
            other => Err(ChannelFailure::method_not_found(other)),
        }}
    }}
}}

#[cfg(test)]
mod tests {{
    use super::*;
    use nc_code_channels::ClientMode;

    fn ctx() -> ChannelContext {{
        ChannelContext::new("{name}", ClientMode::DesktopContinuous)
    }}

    #[tokio::test]
    async fn basic() {{
        let service = {struct_name}Service::new();
        let _ = service.call(&ctx(), "list", vec![], CancelToken::new()).await;
    }}
}}
'''
    (d/'src/lib.rs').write_text(lib)
print('done')
