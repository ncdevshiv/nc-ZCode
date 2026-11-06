//! `terminal` channel — real pseudoterminals for the frontend terminal panel.
//!
//! Surface (from `native/fixtures/service-channels.json`, generated from
//! `packages/services/src/terminal/terminal.ts`):
//! - RPC `create` / `write` / `resize` / `dispose` (plus the `disposeAll` entry point the
//!   TypeScript service exposes on the same object for host shutdown);
//! - dynamic events `onDynamicData` (UTF-8 output string, tag 1) and `onDynamicExit`
//!   (exit code, tag 4), both keyed by the terminal id passed to `listen`.
//!
//! State ownership: this service alone owns the terminal map. Ids are `String(nextId++)`
//! starting at `0`, exactly like the TypeScript counter. A terminal leaves the map either
//! when the child exits (the waiter thread calls `reap`) or when `dispose` removes it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure, EventSubscription};
use nc_code_rpc::WireValue;
use nc_code_service_core::{AppPaths, ServiceArgs, ServiceError};
use serde_json::{Map, Value};

use crate::profile::{
    resolve_terminal_font_profile, windows_pty_info, FontProfile, TerminalProfileSettings,
};
use crate::pty::{PtyTerminal, SpawnRequest};
use crate::shell::{
    inherited_environment, resolve_terminal_cwd, resolve_terminal_environment,
    resolve_terminal_shell, Platform,
};

const TERM_NAME: &str = "xterm-256color";

struct TerminalEntry {
    terminal: PtyTerminal,
}

pub struct TerminalService {
    paths: AppPaths,
    terminals: Arc<Mutex<HashMap<String, TerminalEntry>>>,
    next_id: Arc<AtomicU64>,
    reaped: Arc<AtomicU64>,
    platform: Platform,
}

/// Parameters of `create`, already validated.
#[derive(Clone, Debug, PartialEq)]
pub struct CreateParams {
    pub cols: u16,
    pub rows: u16,
    pub cwd: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WriteParams {
    pub id: String,
    pub data: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResizeParams {
    pub id: String,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DisposeParams {
    pub id: String,
}

impl TerminalService {
    pub fn new(paths: AppPaths) -> Arc<Self> {
        Arc::new(Self {
            paths,
            terminals: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(0)),
            reaped: Arc::new(AtomicU64::new(0)),
            platform: Platform::current(),
        })
    }

    /// Number of live terminals — the counter the TypeScript service reports through
    /// `registerMemoryDiagnosticsProvider("terminal", …)`.
    pub fn open_count(&self) -> usize {
        self.terminals.lock().expect("terminal map").len()
    }

    /// Terminals whose child process has been waited for; `open_count` plus `reaped_count`
    /// is how a leak (killed but never reaped, or reaped but never removed) shows up.
    pub fn reaped_count(&self) -> u64 {
        self.reaped.load(Ordering::Relaxed)
    }

    fn take_id(&self) -> String {
        self.next_id.fetch_add(1, Ordering::Relaxed).to_string()
    }

    fn with_terminal<T>(
        &self,
        id: &str,
        action: impl FnOnce(&TerminalEntry) -> T,
    ) -> Result<T, ServiceError> {
        let terminals = self.terminals.lock().expect("terminal map");
        let entry = terminals
            .get(id)
            .ok_or_else(|| ServiceError::new(format!("Terminal not found: {id}")))?;
        Ok(action(entry))
    }

    async fn create(&self, params: CreateParams) -> Result<Value, ServiceError> {
        let id = self.take_id();
        let paths = self.paths.clone();
        let platform = self.platform;
        let terminals = Arc::clone(&self.terminals);
        let reaped = Arc::clone(&self.reaped);
        // A fast-exiting child finishes its waiter thread before registration; using the same map lock plus this flag
        // guarantees no race window between "exited" and "registered", so no entry is ever left that can never be removed.
        let exited = Arc::new(AtomicBool::new(false));

        // Opening a PTY and spawning the shell block, so the whole start-up sequence runs
        // on the blocking pool; the order matches the TypeScript `create`.
        let (id, shell, font_profile) = tokio::task::spawn_blocking(move || {
            let env = inherited_environment();
            let shell = resolve_terminal_shell(&env, platform).map_err(ServiceError::new)?;
            let cwd = resolve_terminal_cwd(params.cwd.as_deref(), &env, platform, &paths)
                .map_err(ServiceError::new)?;
            let terminal_env = resolve_terminal_environment(&env, platform, paths.home_dir());
            let settings = read_terminal_profile_settings(&paths);
            let font_profile =
                resolve_terminal_font_profile(&settings, &env, platform, paths.home_dir());

            let reap_terminals = Arc::clone(&terminals);
            let reap_reaped = Arc::clone(&reaped);
            let reap_exited = Arc::clone(&exited);
            let terminal = PtyTerminal::spawn(
                SpawnRequest {
                    id: &id,
                    cols: params.cols,
                    rows: params.rows,
                    shell: &shell,
                    cwd: &cwd,
                    env: &terminal_env,
                    term_name: TERM_NAME,
                },
                move |finished_id| {
                    let mut open = reap_terminals.lock().expect("terminal map");
                    open.remove(finished_id);
                    reap_exited.store(true, Ordering::Release);
                    drop(open);
                    reap_reaped.fetch_add(1, Ordering::Relaxed);
                },
            )
            .map_err(|error| {
                ServiceError::new(format!(
                    "Failed to start terminal with shell '{shell}' in '{}': {error}",
                    cwd.display()
                ))
            })?;

            let mut open = terminals.lock().expect("terminal map");
            if !exited.load(Ordering::Acquire) {
                open.insert(id.clone(), TerminalEntry { terminal });
            }
            drop(open);

            Ok::<_, ServiceError>((id, shell, font_profile))
        })
        .await
        .map_err(|error| ServiceError::new(format!("terminal create task failed: {error}")))??;

        Ok(create_result(&id, &shell, &font_profile, self.platform))
    }

    fn write(&self, params: WriteParams) -> Result<(), ServiceError> {
        self.with_terminal(&params.id, |entry| entry.terminal.write(&params.data))?
            .map_err(ServiceError::new)
    }

    fn resize(&self, params: ResizeParams) -> Result<(), ServiceError> {
        self.with_terminal(&params.id, |entry| {
            entry.terminal.resize(params.cols, params.rows)
        })?
        .map_err(ServiceError::new)
    }

    /// `cleanupTerminal`: kill, dispose the emitters, forget the terminal. Missing ids are
    /// a no-op, matching the TypeScript early return.
    fn cleanup(&self, id: &str) {
        let removed = self
            .terminals
            .lock()
            .expect("terminal map")
            .remove(id)
            .map(|entry| entry.terminal);
        if let Some(terminal) = removed {
            terminal.kill();
        }
    }

    pub(crate) fn dispose_all(&self) {
        let drained: Vec<PtyTerminal> = self
            .terminals
            .lock()
            .expect("terminal map")
            .drain()
            .map(|(_, entry)| entry.terminal)
            .collect();
        for terminal in drained {
            terminal.kill();
        }
    }

    fn subscription(&self, id: &str, data: bool) -> Result<EventSubscription, ServiceError> {
        self.with_terminal(id, |entry| {
            if data {
                EventSubscription::from_broadcast(entry.terminal.subscribe_data())
            } else {
                EventSubscription::from_broadcast(entry.terminal.subscribe_exit())
            }
        })
    }

    /// Kernel/ConPTY window size for one terminal — an inspection helper used by the
    /// resize tests, never part of the channel surface.
    pub fn terminal_size(&self, id: &str) -> Option<(u32, u32)> {
        self.with_terminal(id, |entry| entry.terminal.size())
            .ok()
            .flatten()
    }

    /// Child process id of a live terminal, for lifecycle assertions.
    pub fn terminal_process_id(&self, id: &str) -> Option<u32> {
        self.with_terminal(id, |entry| entry.terminal.process_id())
            .ok()
            .flatten()
    }

    /// Platform-resolved shell the next terminal would start; exposed so the composition
    /// root and tests can assert the same detection the channel uses.
    pub fn resolved_shell(&self) -> Result<String, ServiceError> {
        resolve_terminal_shell(&inherited_environment(), self.platform).map_err(ServiceError::new)
    }
}

/// The `create` payload in the TypeScript key order; `undefined` members are omitted so
/// the wire object matches `JSON.stringify` of the original result.
fn create_result(id: &str, shell: &str, profile: &FontProfile, platform: Platform) -> Value {
    let mut object = Map::new();
    object.insert("id".to_string(), Value::String(id.to_string()));
    object.insert("shell".to_string(), Value::String(shell.to_string()));
    object.insert(
        "fontFamily".to_string(),
        Value::String(profile.font_family.clone()),
    );
    if let Some(font_size) = profile.font_size {
        object.insert("fontSize".to_string(), Value::from(font_size));
    }
    if let Some(theme) = &profile.theme {
        object.insert("theme".to_string(), theme.clone());
    }
    object.insert(
        "fontFamilySource".to_string(),
        Value::String(profile.source.to_string()),
    );
    if let Some(windows_pty) = windows_pty_info(platform) {
        object.insert("windowsPty".to_string(), windows_pty);
    }
    Value::Object(object)
}

/// Reads the two profile keys from the persisted settings written by the `setting`
/// channel. Any read/parse failure falls back to the TypeScript `catch` defaults.
pub fn read_terminal_profile_settings(paths: &AppPaths) -> TerminalProfileSettings {
    let raw = match std::fs::read_to_string(paths.settings_file()) {
        Ok(raw) => raw,
        Err(_) => return TerminalProfileSettings::default(),
    };
    let parsed: Value = match serde_json::from_str(&raw) {
        Ok(parsed) => parsed,
        Err(_) => return TerminalProfileSettings::default(),
    };
    TerminalProfileSettings {
        terminal_font_family: parsed
            .get("terminalFontFamily")
            .and_then(Value::as_str)
            .map(str::to_string),
        terminal_inherit_system_profile: parsed
            .get("terminalInheritSystemProfile")
            .and_then(Value::as_bool),
    }
}

// ---------------------------------------------------------------------------
// positional-argument decoding (spec §1: the client always sends `[...args]`)

fn decode_create_params(params: &Value) -> Result<CreateParams, ServiceError> {
    let cols = decode_dimension(params, "cols")?;
    let rows = decode_dimension(params, "rows")?;
    let cwd = match params.get("cwd") {
        None | Some(Value::Null) => None,
        Some(Value::String(cwd)) => Some(cwd.clone()),
        Some(_) => return Err(ServiceError::invalid_params("params.cwd must be a string")),
    };
    Ok(CreateParams { cols, rows, cwd })
}

fn decode_dimension(params: &Value, key: &str) -> Result<u16, ServiceError> {
    let number = params
        .get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| ServiceError::invalid_params(format!("params.{key} is required")))?;
    if !number.is_finite()
        || number < 1.0
        || number > f64::from(u16::MAX)
        || number != number.floor()
    {
        return Err(ServiceError::invalid_params(format!(
            "params.{key} must be an integer between 1 and {}",
            u16::MAX
        )));
    }
    Ok(number as u16)
}

fn decode_write_params(params: &Value) -> Result<WriteParams, ServiceError> {
    Ok(WriteParams {
        id: required_string(params, "id")?,
        data: match params.get("data") {
            Some(Value::String(data)) => data.clone(),
            Some(Value::Null) => String::new(),
            None | Some(_) => {
                return Err(ServiceError::invalid_params("params.data must be a string"))
            }
        },
    })
}

fn decode_resize_params(params: &Value) -> Result<ResizeParams, ServiceError> {
    Ok(ResizeParams {
        id: required_string(params, "id")?,
        cols: decode_dimension(params, "cols")?,
        rows: decode_dimension(params, "rows")?,
    })
}

fn decode_dispose_params(params: &Value) -> Result<DisposeParams, ServiceError> {
    Ok(DisposeParams {
        id: required_string(params, "id")?,
    })
}

fn required_string(params: &Value, key: &str) -> Result<String, ServiceError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ServiceError::invalid_params(format!("params.{key} is required")))
}

/// Dynamic events are subscribed with the terminal id as the listen argument.
fn decode_dynamic_event_id(arg: &WireValue) -> Option<String> {
    match arg {
        WireValue::String(id) => Some(id.clone()),
        WireValue::Json(Value::String(id)) => Some(id.clone()),
        _ => None,
    }
}

#[async_trait]
impl Channel for TerminalService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let args = ServiceArgs::new(args);
        match command {
            "create" => {
                let params = decode_create_params(&args.required_json(0, "params")?)?;
                Ok(WireValue::Json(self.create(params).await?))
            }
            "write" => {
                let params = decode_write_params(&args.required_json(0, "params")?)?;
                self.write(params)?;
                Ok(WireValue::Undefined)
            }
            "resize" => {
                let params = decode_resize_params(&args.required_json(0, "params")?)?;
                self.resize(params)?;
                Ok(WireValue::Undefined)
            }
            "dispose" => {
                let params = decode_dispose_params(&args.required_json(0, "params")?)?;
                self.cleanup(&params.id);
                Ok(WireValue::Undefined)
            }
            "disposeAll" => {
                self.dispose_all();
                Ok(WireValue::Undefined)
            }
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }

    async fn listen(
        &self,
        _ctx: &ChannelContext,
        event: &str,
        arg: WireValue,
    ) -> Result<EventSubscription, ChannelFailure> {
        let data = match event {
            "onDynamicData" => true,
            "onDynamicExit" => false,
            other => return Err(ChannelFailure::event_not_found(other)),
        };
        // The TypeScript service throws `Terminal not found` from `getTerminal(id)`; the
        // channel runtime turns that into a dropped listener, same as the TS server.
        let id = decode_dynamic_event_id(&arg)
            .ok_or_else(|| ServiceError::invalid_params("terminal id is required"))?;
        Ok(self.subscription(&id, data)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn service() -> Arc<TerminalService> {
        TerminalService::new(AppPaths::new(std::env::temp_dir(), std::env::temp_dir()))
    }

    async fn call(
        service: &Arc<TerminalService>,
        command: &str,
        args: Vec<WireValue>,
    ) -> Result<WireValue, ChannelFailure> {
        let ctx = ChannelContext::new("conn-test", nc_code_channels::ClientMode::DesktopContinuous);
        service.call(&ctx, command, args, CancelToken::new()).await
    }

    #[test]
    fn create_params_decode_cols_rows_and_optional_cwd() {
        let params =
            decode_create_params(&json!({"cols": 120, "rows": 32, "cwd": "G:/x"})).unwrap();
        assert_eq!(
            params,
            CreateParams {
                cols: 120,
                rows: 32,
                cwd: Some("G:/x".to_string())
            }
        );
        let params = decode_create_params(&json!({"cols": 80, "rows": 24})).unwrap();
        assert_eq!(params.cwd, None);
        let params = decode_create_params(&json!({"cols": 80, "rows": 24, "cwd": null})).unwrap();
        assert_eq!(params.cwd, None);
    }

    #[test]
    fn create_params_reject_bad_geometry() {
        for payload in [
            json!({"rows": 24}),
            json!({"cols": 80}),
            json!({"cols": 0, "rows": 24}),
            json!({"cols": -1, "rows": 24}),
            json!({"cols": 80.5, "rows": 24}),
            json!({"cols": 70000, "rows": 24}),
            json!({"cols": "80", "rows": 24}),
            json!({"cols": 80, "rows": 24, "cwd": 7}),
        ] {
            let error = decode_create_params(&payload).unwrap_err();
            assert_eq!(error.name, "ZodError", "{payload}");
        }
    }

    #[test]
    fn write_resize_dispose_decode_their_ids() {
        assert_eq!(
            decode_write_params(&json!({"id": "3", "data": "ls\r"})).unwrap(),
            WriteParams {
                id: "3".to_string(),
                data: "ls\r".to_string()
            }
        );
        assert_eq!(
            decode_resize_params(&json!({"id": "3", "cols": 90, "rows": 30})).unwrap(),
            ResizeParams {
                id: "3".to_string(),
                cols: 90,
                rows: 30
            }
        );
        assert_eq!(
            decode_dispose_params(&json!({"id": "3"})).unwrap(),
            DisposeParams {
                id: "3".to_string()
            }
        );
        // TS: in write(params: {id, data}) the data field is required (terminalService.ts:415);
        // a missing data must not be treated as an empty write (undefined passed into node-pty throws), so reject it explicitly here.
        assert!(decode_write_params(&json!({"id": "3"})).is_err());
        for payload in [json!({"data": "x"}), json!({"id": 1, "data": "x"})] {
            assert_eq!(
                decode_write_params(&payload).unwrap_err().name,
                "ZodError",
                "{payload}"
            );
        }
    }

    #[test]
    fn dynamic_event_ids_accept_string_and_json_string() {
        assert_eq!(
            decode_dynamic_event_id(&WireValue::String("7".into())),
            Some("7".to_string())
        );
        assert_eq!(
            decode_dynamic_event_id(&WireValue::Json(json!("7"))),
            Some("7".to_string())
        );
        assert_eq!(decode_dynamic_event_id(&WireValue::Undefined), None);
        assert_eq!(decode_dynamic_event_id(&WireValue::Json(json!(7))), None);
    }

    #[tokio::test]
    async fn unknown_method_and_event_are_reported() {
        let service = service();
        let error = call(&service, "nope", vec![WireValue::Undefined])
            .await
            .expect_err("unknown method");
        assert_eq!(
            error.payload().expect("payload").message,
            "Method not found: nope"
        );

        let ctx = ChannelContext::new("conn-test", nc_code_channels::ClientMode::DesktopContinuous);
        let error = match service
            .listen(&ctx, "onDynamicSomething", WireValue::String("0".into()))
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("listening to an unknown event must fail"),
        };
        assert_eq!(
            error.payload().expect("payload").message,
            "Event not found: onDynamicSomething"
        );
    }

    #[tokio::test]
    async fn missing_params_object_is_a_schema_error() {
        let service = service();
        let error = call(&service, "write", vec![WireValue::Undefined])
            .await
            .expect_err("params required");
        assert_eq!(error.payload().expect("payload").name, "ZodError");
    }

    #[tokio::test]
    async fn write_resize_and_listen_report_a_missing_terminal() {
        let service = service();
        for (command, payload) in [
            ("write", json!({"id": "42", "data": "x"})),
            ("resize", json!({"id": "42", "cols": 80, "rows": 24})),
        ] {
            let error = call(&service, command, vec![WireValue::Json(payload)])
                .await
                .expect_err("terminal missing");
            let payload = error.payload().expect("payload");
            assert_eq!(payload.name, "Error");
            assert_eq!(payload.message, "Terminal not found: 42");
        }

        let ctx = ChannelContext::new("conn-test", nc_code_channels::ClientMode::DesktopContinuous);
        let error = match service
            .listen(&ctx, "onDynamicData", WireValue::String("42".into()))
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("listening for an unknown terminal must fail"),
        };
        assert_eq!(
            error.payload().expect("payload").message,
            "Terminal not found: 42"
        );
    }

    #[tokio::test]
    async fn dispose_and_dispose_all_tolerate_unknown_ids() {
        let service = service();
        let value = call(
            &service,
            "dispose",
            vec![WireValue::Json(json!({"id": "nope"}))],
        )
        .await
        .expect("dispose is a no-op");
        assert_eq!(value, WireValue::Undefined);
        let value = call(&service, "disposeAll", vec![])
            .await
            .expect("disposeAll on an empty map");
        assert_eq!(value, WireValue::Undefined);
        assert_eq!(service.open_count(), 0);
    }

    #[test]
    fn create_result_uses_the_typescript_key_order() {
        let profile = FontProfile {
            font_family: "Menlo".to_string(),
            font_size: Some(11.0),
            theme: Some(json!({"background": "#000000"})),
            source: "system",
        };
        let value = create_result("7", "/bin/zsh", &profile, Platform::Linux);
        let keys: Vec<&String> = value.as_object().expect("object").keys().collect();
        assert_eq!(
            keys,
            vec![
                "id",
                "shell",
                "fontFamily",
                "fontSize",
                "theme",
                "fontFamilySource"
            ]
        );
        assert_eq!(value["id"], json!("7"));
        assert_eq!(value["fontFamilySource"], json!("system"));

        // A fallback profile has no fontSize/theme, so those keys stay absent, and
        // windowsPty only exists on win32.
        let profile = FontProfile {
            font_family: "monospace".to_string(),
            font_size: None,
            theme: None,
            source: "fallback",
        };
        let value = create_result("0", "cmd.exe", &profile, Platform::Windows);
        let keys: Vec<&String> = value.as_object().expect("object").keys().collect();
        assert_eq!(
            keys,
            vec![
                "id",
                "shell",
                "fontFamily",
                "fontFamilySource",
                "windowsPty"
            ]
        );
        assert!(value.get("windowsPty").is_some());
        assert_eq!(value["windowsPty"]["backend"], json!("conpty"));
        let value = create_result("0", "sh", &profile, Platform::Linux);
        assert!(value.get("windowsPty").is_none());
    }

    #[test]
    fn profile_settings_fall_back_when_the_settings_file_is_unreadable() {
        let paths = AppPaths::new(
            std::env::temp_dir().join("zcode-terminal-no-such-home"),
            std::env::temp_dir().join("zcode-terminal-no-such-home"),
        );
        let settings = read_terminal_profile_settings(&paths);
        assert_eq!(settings, TerminalProfileSettings::default());
    }

    #[test]
    fn profile_settings_are_read_from_the_persisted_setting_file() {
        let unique = format!("zcode-terminal-settings-{}", std::process::id());
        let home = std::env::temp_dir().join(&unique);
        let paths = AppPaths::new(&home, &home);
        std::fs::create_dir_all(paths.settings_dir()).expect("settings dir");
        std::fs::write(
            paths.settings_file(),
            json!({"terminalFontFamily": "  Fira Code  ", "terminalInheritSystemProfile": false})
                .to_string(),
        )
        .expect("settings");
        let settings = read_terminal_profile_settings(&paths);
        assert_eq!(
            settings,
            TerminalProfileSettings {
                terminal_font_family: Some("  Fira Code  ".to_string()),
                terminal_inherit_system_profile: Some(false),
            }
        );
        let profile = resolve_terminal_font_profile(
            &settings,
            &inherited_environment(),
            Platform::current(),
            paths.home_dir(),
        );
        assert_eq!(profile.source, "custom");
        assert!(profile.font_family.starts_with("Fira Code, "));
        std::fs::remove_dir_all(&home).ok();
    }
}
