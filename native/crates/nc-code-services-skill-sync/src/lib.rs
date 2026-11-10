//! `skill-sync` channel — local skill directory operations with path awareness.
//!
//! Provides contract methods for local candidate listing, export/import of a simple
//! archive shape, and honest "unsupported" for remote-only operations. Uses
//! `AppPaths` for home directory resolution.

/// Handoff skeleton (pure data/types): serde persistence structs, port trait, Send/Sync markers.
pub mod handoff;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::{AppPaths, ServiceArgs, ServiceError};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::fs;

pub struct SkillSyncService {
    paths: AppPaths,
}

impl SkillSyncService {
    pub fn new(paths: AppPaths) -> Arc<Self> {
        Arc::new(Self { paths })
    }

    fn skills_dir(&self) -> PathBuf {
        self.paths.home_dir().join(".zcode").join("skills")
    }

    fn agents_skills_dir(&self) -> PathBuf {
        self.paths.home_dir().join(".agents").join("skills")
    }

    fn candidate_id(path: &str) -> String {
        let mut h = Sha256::new();
        h.update(path.as_bytes());
        format!("{:x}", h.finalize())
    }

    async fn collect_candidates_in_root(&self, root: &Path) -> Vec<Value> {
        let mut items = Vec::new();
        let mut rd = match fs::read_dir(root).await {
            Ok(r) => r,
            Err(_) => return items,
        };
        while let Ok(Some(e)) = rd.next_entry().await {
            let Ok(ft) = e.file_type().await else {
                continue;
            };
            if !ft.is_dir() {
                continue;
            }
            let dir_name = e.file_name().to_string_lossy().to_string();
            let skill_md = e.path().join("SKILL.md");
            let (name, description) = match fs::read_to_string(&skill_md).await {
                Ok(content) => parse_skill_frontmatter(&content, &dir_name),
                Err(_) => (dir_name.clone(), String::new()),
            };
            let size = dir_size(&e.path()).await.unwrap_or(0);
            let path_str = skill_md.to_string_lossy().to_string();
            items.push(json!({
                "id": Self::candidate_id(&path_str),
                "name": name,
                "directoryName": dir_name,
                "description": description,
                "path": path_str,
                "sizeBytes": size,
            }));
        }
        items.sort_by(|a, b| {
            a["directoryName"]
                .as_str()
                .unwrap_or("")
                .cmp(b["directoryName"].as_str().unwrap_or(""))
        });
        items
    }

    async fn list_local(&self) -> Result<Value, ServiceError> {
        let zcode = self.collect_candidates_in_root(&self.skills_dir()).await;
        let agents = self
            .collect_candidates_in_root(&self.agents_skills_dir())
            .await;
        let mut seen = std::collections::HashSet::new();
        let mut candidates = Vec::new();
        for c in zcode.into_iter().chain(agents) {
            let key = c["name"].as_str().unwrap_or("").trim().to_lowercase();
            if seen.insert(key) {
                candidates.push(c);
            }
        }
        candidates.sort_by(|a, b| {
            a["directoryName"]
                .as_str()
                .unwrap_or("")
                .cmp(b["directoryName"].as_str().unwrap_or(""))
        });
        Ok(json!({
            "candidates": candidates,
            "maxArchiveBytes": 20 * 1024 * 1024u64,
        }))
    }

    async fn export_archive(&self, args: &ServiceArgs) -> Result<Value, ServiceError> {
        let params = args.json(0).ok().flatten().unwrap_or(Value::Null);
        let ids = params
            .get("skillIds")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let listing = self.list_local().await?;
        let candidates = listing["candidates"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let by_id: std::collections::BTreeMap<String, Value> = candidates
            .into_iter()
            .filter_map(|c| {
                let id = c
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())?;
                Some((id, c))
            })
            .collect();
        let mut selected = Vec::new();
        let mut items = Vec::new();
        for idv in ids {
            let id = idv.as_str().unwrap_or("");
            let Some(c) = by_id.get(id) else {
                return Err(ServiceError::new(format!(
                    "skill sync candidate not found: {id}"
                )));
            };
            selected.push(
                json!({"id": c["id"], "name": c["name"], "directoryName": c["directoryName"]}),
            );
            let skill_md = c["path"].as_str().unwrap_or("");
            let dir = PathBuf::from(skill_md)
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_default();
            let data = encode_dir_base64(&dir).await?;
            items.push(json!({"directoryName": c["directoryName"], "files": data}));
        }
        Ok(json!({
            "archive": json!({"format":"zcode-skills-simple-v1","items": items}),
            "archiveBytes": 0,
            "skills": selected,
        }))
    }

    async fn import_archive(&self, args: &ServiceArgs) -> Result<Value, ServiceError> {
        let params = args.required_json(0, "params")?;
        if params
            .get("overwrite")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return Err(ServiceError::new("skill sync overwrite is not supported"));
        }
        let archive = params.get("archive").cloned().unwrap_or(Value::Null);
        let items = archive
            .get("items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let root = self.skills_dir();
        fs::create_dir_all(&root)
            .await
            .map_err(|e| ServiceError::new(e.to_string()))?;
        let mut results = Vec::new();
        for item in items {
            let dir_name = item
                .get("directoryName")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if dir_name.is_empty()
                || dir_name.contains("..")
                || dir_name.contains('/')
                || dir_name.contains('\\')
            {
                results.push(json!({"name": dir_name, "directoryName": dir_name, "status":"failed","error":"invalid directory name"}));
                continue;
            }
            let target = root.join(&dir_name);
            if target.exists() {
                results.push(json!({"name": dir_name, "directoryName": dir_name, "status":"skipped","path": target.to_string_lossy()}));
                continue;
            }
            match decode_dir_base64(&target, item.get("files")).await {
                Ok(()) => results.push(json!({"name": dir_name, "directoryName": dir_name, "status":"synced","path": target.to_string_lossy()})),
                Err(e) => results.push(json!({"name": dir_name, "directoryName": dir_name, "status":"failed","error": e.to_string()})),
            }
        }
        Ok(json!({ "results": results }))
    }

    async fn list_remote_statuses(&self, args: &ServiceArgs) -> Result<Value, ServiceError> {
        let params = args.json(0).ok().flatten().unwrap_or(Value::Null);
        let directory_names = params
            .get("directoryNames")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let root = self.skills_dir();
        let statuses: Vec<Value> = directory_names
            .into_iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .map(|dn| {
                let path = root.join(&dn);
                json!({"directoryName": dn, "exists": path.exists(), "path": path.to_string_lossy()})
            })
            .collect();
        Ok(json!({ "statuses": statuses }))
    }

    async fn check_write_access(&self) -> Result<Value, ServiceError> {
        let dir = self.skills_dir();
        fs::create_dir_all(&dir)
            .await
            .map_err(|e| ServiceError::new(e.to_string()))?;
        let probe = dir.join(format!(".zcode-skill-probe-{}", std::process::id()));
        match fs::write(&probe, b"probe").await {
            Ok(()) => {
                let _ = fs::remove_file(&probe).await;
                Ok(json!({"writable": true, "path": dir.to_string_lossy()}))
            }
            Err(e) => Ok(
                json!({"writable": false, "path": dir.to_string_lossy(), "error": e.to_string()}),
            ),
        }
    }
}

fn parse_skill_frontmatter(content: &str, fallback: &str) -> (String, String) {
    let mut name = fallback.to_string();
    let mut desc = String::new();
    if let Some(rest) = content.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let fm = &rest[..end];
            for line in fm.lines() {
                if let Some(v) = line.strip_prefix("name:") {
                    let v = v.trim().trim_matches('"').trim_matches('\'');
                    if !v.is_empty() {
                        name = v.to_string();
                    }
                } else if let Some(v) = line.strip_prefix("description:") {
                    desc = v.trim().trim_matches('"').trim_matches('\'').to_string();
                }
            }
        }
    }
    (name, desc)
}

async fn dir_size(dir: &Path) -> Result<u64, std::io::Error> {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let mut rd = match fs::read_dir(&d).await {
            Ok(r) => r,
            Err(_) => continue,
        };
        while let Ok(Some(e)) = rd.next_entry().await {
            let Ok(m) = e.metadata().await else { continue };
            if m.is_dir() {
                stack.push(e.path());
            } else {
                total += m.len();
            }
        }
    }
    Ok(total)
}

async fn encode_dir_base64(dir: &Path) -> Result<Value, ServiceError> {
    use base64::Engine;
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let mut rd = match fs::read_dir(&d).await {
            Ok(r) => r,
            Err(_) => continue,
        };
        while let Ok(Some(e)) = rd.next_entry().await {
            let Ok(ft) = e.file_type().await else {
                continue;
            };
            let path = e.path();
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            if ft.is_file() {
                if let Ok(bytes) = fs::read(&path).await {
                    let rel = path
                        .strip_prefix(dir)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    files.push(json!({"path": rel, "data": base64::engine::general_purpose::STANDARD.encode(bytes)}));
                }
            }
        }
    }
    Ok(Value::Array(files))
}

async fn decode_dir_base64(target: &Path, files: Option<&Value>) -> Result<(), ServiceError> {
    use base64::Engine;
    let Some(Value::Array(files)) = files else {
        return Err(ServiceError::new("invalid archive files"));
    };
    fs::create_dir_all(target)
        .await
        .map_err(|e| ServiceError::new(e.to_string()))?;
    for f in files {
        let rel = f.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if rel.is_empty() || rel.contains("..") {
            continue;
        }
        let data = f.get("data").and_then(|v| v.as_str()).unwrap_or("");
        let dest = target.join(rel);
        if let Some(p) = dest.parent() {
            fs::create_dir_all(p)
                .await
                .map_err(|e| ServiceError::new(e.to_string()))?;
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|e| ServiceError::new(e.to_string()))?;
        fs::write(dest, bytes)
            .await
            .map_err(|e| ServiceError::new(e.to_string()))?;
    }
    Ok(())
}

#[async_trait]
impl Channel for SkillSyncService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let args = ServiceArgs::new(args);
        let result = match command {
            "listLocalUserSkillCandidates" => self.list_local().await,
            "listRemoteUserSkillStatuses" => self.list_remote_statuses(&args).await,
            "exportSkillsArchive" => self.export_archive(&args).await,
            "checkRemoteUserSkillWriteAccess" => self.check_write_access().await,
            "importSkillsArchive" => self.import_archive(&args).await,
            other => return Err(ChannelFailure::method_not_found(other)),
        };
        result.map(WireValue::Json).map_err(ChannelFailure::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn lists_skill_dirs_with_frontmatter() {
        let tmp = TempDir::new().unwrap();
        let paths = AppPaths::new(tmp.path().join("home"), tmp.path().join("data"));
        let skill_dir = paths.home_dir().join(".zcode").join("skills").join("demo");
        fs::create_dir_all(&skill_dir).await.unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: Demo\ndescription: hi\n---\nbody",
        )
        .await
        .unwrap();
        let svc = SkillSyncService::new(paths);
        let v = svc.list_local().await.unwrap();
        let c = v["candidates"].as_array().unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0]["name"], "Demo");
    }

    #[tokio::test]
    async fn export_import_round_trip() {
        let tmp = TempDir::new().unwrap();
        let paths = AppPaths::new(tmp.path().join("home"), tmp.path().join("data"));
        let skill_dir = paths.home_dir().join(".zcode").join("skills").join("demo");
        fs::create_dir_all(&skill_dir).await.unwrap();
        fs::write(skill_dir.join("SKILL.md"), "---\nname: Demo\n---\n")
            .await
            .unwrap();
        let svc = SkillSyncService::new(paths.clone());
        let list = svc.list_local().await.unwrap();
        let id = list["candidates"][0]["id"].as_str().unwrap();
        let exp = svc
            .export_archive(&ServiceArgs::new(vec![WireValue::Json(
                json!({"skillIds":[id]}),
            )]))
            .await
            .unwrap();
        let tmp2 = TempDir::new().unwrap();
        let svc2 = SkillSyncService::new(AppPaths::new(
            tmp2.path().join("home"),
            tmp2.path().join("data"),
        ));
        let imp = svc2
            .import_archive(&ServiceArgs::new(vec![WireValue::Json(
                json!({"archive": exp["archive"]}),
            )]))
            .await
            .unwrap();
        assert_eq!(imp["results"][0]["status"], "synced");
        assert!(tmp2
            .path()
            .join("home/.zcode/skills/demo/SKILL.md")
            .exists());
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use nc_code_channels::ClientMode;
    use tempfile::TempDir;

    #[tokio::test]
    async fn remote_status_checks_local_root() {
        let tmp = TempDir::new().unwrap();
        let svc = SkillSyncService::new(AppPaths::new(
            tmp.path().join("home"),
            tmp.path().join("data"),
        ));
        let ctx = ChannelContext::new("skill-sync", ClientMode::DesktopContinuous);
        let r = svc
            .call(
                &ctx,
                "listRemoteUserSkillStatuses",
                vec![WireValue::Json(json!({"directoryNames":["x"]}))],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = r else { panic!() };
        assert_eq!(v["statuses"][0]["exists"], false);
    }
}
