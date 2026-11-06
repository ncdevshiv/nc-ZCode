//! Test/dev filesystem fault injection (port of `packages/services/src/fs/fsFaultInjection.ts`).
//!
//! Enabled only when explicitly configured (e.g. E2E). Rules match by operation and path
//! patterns and throw/inject deterministic errors. Not for production behavior.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const ZCODE_E2E_FS_FAULTS_ENV: &str = "ZCODE_E2E_FS_FAULTS";
pub const ZCODE_E2E_FS_FAULTS_ALLOW_ENV: &str = "ZCODE_E2E_FS_FAULTS_ALLOW";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FsFaultOperation {
    Any,
    AppendFile,
    CreateWriteStream,
    Mkdir,
    Open,
    ReadFile,
    Readdir,
    Rename,
    Rm,
    SqliteOpen,
    SqliteRun,
    Stat,
    WriteFile,
}

impl FsFaultOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::AppendFile => "appendFile",
            Self::CreateWriteStream => "createWriteStream",
            Self::Mkdir => "mkdir",
            Self::Open => "open",
            Self::ReadFile => "readFile",
            Self::Readdir => "readdir",
            Self::Rename => "rename",
            Self::Rm => "rm",
            Self::SqliteOpen => "sqliteOpen",
            Self::SqliteRun => "sqliteRun",
            Self::Stat => "stat",
            Self::WriteFile => "writeFile",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "any" => Some(Self::Any),
            "appendFile" => Some(Self::AppendFile),
            "createWriteStream" => Some(Self::CreateWriteStream),
            "mkdir" => Some(Self::Mkdir),
            "open" => Some(Self::Open),
            "readFile" => Some(Self::ReadFile),
            "readdir" => Some(Self::Readdir),
            "rename" => Some(Self::Rename),
            "rm" => Some(Self::Rm),
            "sqliteOpen" => Some(Self::SqliteOpen),
            "sqliteRun" => Some(Self::SqliteRun),
            "stat" => Some(Self::Stat),
            "writeFile" => Some(Self::WriteFile),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsFaultRuleConfig {
    pub id: String,
    pub code: String,
    #[serde(default)]
    pub operations: Vec<String>,
    pub path_includes: Option<String>,
    pub path_ends_with: Option<String>,
    pub path_regex: Option<String>,
    pub max_matches: Option<u64>,
    pub message: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FsFaultCheckInput {
    pub operation: FsFaultOperation,
    pub path: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FsFaultHit {
    pub id: String,
    pub code: String,
    pub operation: String,
    pub path: String,
    pub match_index: u64,
    pub matched_at: String,
}

#[derive(Debug, Clone)]
pub struct InjectedFsFaultError {
    pub code: String,
    pub id: String,
    pub message: String,
    pub operation: String,
    pub path: String,
}

impl std::fmt::Display for InjectedFsFaultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for InjectedFsFaultError {}

#[derive(Debug)]
struct NormalizedRule {
    id: String,
    code: String,
    operations: HashSet<FsFaultOperation>,
    path_includes: Option<String>,
    path_ends_with: Option<String>,
    path_regex: Option<Regex>,
    max_matches: u64,
    message: Option<String>,
    matched_count: u64,
}

fn normalize_path_for_match(path: &str) -> String {
    path.replace('\\', "/")
}

fn normalize_pattern(p: Option<&str>) -> Option<String> {
    p.map(normalize_path_for_match)
}

impl NormalizedRule {
    fn from_config(input: &FsFaultRuleConfig, index: usize) -> Result<Self, String> {
        if input.id.trim().is_empty() {
            return Err(format!(
                "Invalid fs fault rule at index {index}: id must be a non-empty string"
            ));
        }
        if input.code.trim().is_empty() {
            return Err(format!(
                "Invalid fs fault rule at index {index}: code must be a non-empty string"
            ));
        }
        let mut operations = HashSet::new();
        if input.operations.is_empty() {
            operations.insert(FsFaultOperation::Any);
        } else {
            for op in &input.operations {
                let Some(parsed) = FsFaultOperation::parse(op) else {
                    return Err(format!(
                        "Invalid fs fault rule at index {index}: unsupported operation {op}"
                    ));
                };
                operations.insert(parsed);
            }
        }
        let max_matches = input.max_matches.unwrap_or(1);
        let path_regex = if let Some(raw) = input.path_regex.as_deref() {
            Some(Regex::new(raw).map_err(|e| {
                format!("Invalid fs fault rule at index {index}: pathRegex is invalid: {e}")
            })?)
        } else {
            None
        };
        Ok(Self {
            id: input.id.trim().to_string(),
            code: input.code.trim().to_string(),
            operations,
            path_includes: normalize_pattern(input.path_includes.as_deref()),
            path_ends_with: normalize_pattern(input.path_ends_with.as_deref()),
            path_regex,
            max_matches,
            message: input.message.clone(),
            matched_count: 0,
        })
    }

    fn matches_op(&self, op: FsFaultOperation) -> bool {
        self.operations.contains(&FsFaultOperation::Any) || self.operations.contains(&op)
    }
    fn has_remaining(&self) -> bool {
        self.max_matches == 0 || self.matched_count < self.max_matches
    }
    fn matches_path(&self, path: &str) -> bool {
        let np = normalize_path_for_match(path);
        if let Some(ref inc) = self.path_includes {
            if !np.contains(inc) {
                return false;
            }
        }
        if let Some(ref end) = self.path_ends_with {
            if !np.ends_with(end) {
                return false;
            }
        }
        if let Some(ref re) = self.path_regex {
            if !re.is_match(&np) {
                return false;
            }
        }
        true
    }
}

#[derive(Clone, Default, Debug)]
pub struct FsFaultInjector {
    inner: Arc<Mutex<InjectorInner>>,
}

#[derive(Default, Debug)]
struct InjectorInner {
    rules: Vec<NormalizedRule>,
    hits: Vec<FsFaultHit>,
}

impl FsFaultInjector {
    pub fn new(rules: &[FsFaultRuleConfig]) -> Result<Self, String> {
        let mut normalized = Vec::with_capacity(rules.len());
        for (i, r) in rules.iter().enumerate() {
            normalized.push(NormalizedRule::from_config(r, i)?);
        }
        Ok(Self {
            inner: Arc::new(Mutex::new(InjectorInner {
                rules: normalized,
                hits: Vec::new(),
            })),
        })
    }

    pub fn disabled() -> Self {
        Self::default()
    }

    pub fn is_enabled(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        !inner.rules.is_empty()
    }

    pub fn maybe_throw(&self, input: &FsFaultCheckInput) -> Result<(), InjectedFsFaultError> {
        let mut inner = self.inner.lock().unwrap();
        let now = now_iso_like();
        let mut matched: Option<(usize, u64, String, String, Option<String>)> = None;
        for (idx, rule) in inner.rules.iter().enumerate() {
            if !rule.has_remaining()
                || !rule.matches_op(input.operation)
                || !rule.matches_path(&input.path)
            {
                continue;
            }
            matched = Some((
                idx,
                rule.matched_count + 1,
                rule.id.clone(),
                rule.code.clone(),
                rule.message.clone(),
            ));
            break;
        }
        if let Some((idx, match_index, id, code, message)) = matched {
            let rule = &mut inner.rules[idx];
            rule.matched_count = match_index;
            inner.hits.push(FsFaultHit {
                id: id.clone(),
                code: code.clone(),
                operation: input.operation.as_str().to_string(),
                path: input.path.clone(),
                match_index,
                matched_at: now,
            });
            return Err(InjectedFsFaultError {
                code: code.clone(),
                id: id.clone(),
                message: message.unwrap_or_else(|| {
                    format!(
                        "Injected fs fault {} for {}: {}",
                        code,
                        input.operation.as_str(),
                        input.path
                    )
                }),
                operation: input.operation.as_str().to_string(),
                path: input.path.clone(),
            });
        }
        Ok(())
    }

    pub fn get_hits(&self) -> Vec<FsFaultHit> {
        self.inner.lock().unwrap().hits.clone()
    }

    pub fn reset(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.hits.clear();
        for r in inner.rules.iter_mut() {
            r.matched_count = 0;
        }
    }
}

fn now_iso_like() -> String {
    // Deterministic enough for tests/diagnostics without extra deps
    use std::time::{SystemTime, UNIX_EPOCH};
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{ms}")
}

pub fn parse_fs_fault_rules_from_env_value(raw: &str) -> Result<Vec<FsFaultRuleConfig>, String> {
    let parsed: Value =
        serde_json::from_str(raw).map_err(|e| format!("Invalid {ZCODE_E2E_FS_FAULTS_ENV}: {e}"))?;
    let arr = parsed
        .as_array()
        .ok_or_else(|| format!("Invalid {ZCODE_E2E_FS_FAULTS_ENV}: expected a JSON array"))?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, v) in arr.iter().enumerate() {
        if !v.is_object() {
            return Err(format!(
                "Invalid fs fault rule at index {i}: rule must be an object"
            ));
        }
        let rule: FsFaultRuleConfig = serde_json::from_value(v.clone())
            .map_err(|e| format!("Invalid fs fault rule at index {i}: {e}"))?;
        out.push(rule);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injector_matches_operation_and_path() {
        let injector = FsFaultInjector::new(&[FsFaultRuleConfig {
            id: "r1".into(),
            code: "EACCES".into(),
            operations: vec!["writeFile".into()],
            path_includes: Some("secret".into()),
            max_matches: Some(1),
            ..Default::default()
        }])
        .unwrap();
        assert!(injector.is_enabled());
        let err = injector
            .maybe_throw(&FsFaultCheckInput {
                operation: FsFaultOperation::WriteFile,
                path: "C:\\x\\secret.txt".into(),
            })
            .unwrap_err();
        assert_eq!(err.code, "EACCES");
        assert_eq!(injector.get_hits().len(), 1);
        // second call should not throw (max_matches 1)
        injector
            .maybe_throw(&FsFaultCheckInput {
                operation: FsFaultOperation::WriteFile,
                path: "C:\\x\\secret.txt".into(),
            })
            .unwrap();
    }

    #[test]
    fn parse_env_rules() {
        let rules = parse_fs_fault_rules_from_env_value(r#"[{"id":"a","code":"EPERM"}]"#).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, "a");
    }

    #[test]
    fn rejects_invalid_rule() {
        let err = FsFaultInjector::new(&[FsFaultRuleConfig {
            id: "".into(),
            code: "E".into(),
            ..Default::default()
        }])
        .unwrap_err();
        assert!(err.contains("id"));
    }
}
