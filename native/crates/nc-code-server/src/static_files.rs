//! Static asset serving — mirror of `packages/server/src/http.ts`
//! (`resolveStaticFile` / `staticContentType` / cache headers).
//!
//! Security: the decoded path is validated lexically (`..` / absolute / prefix
//! components rejected) before joining the static root; `/api/*` and `/ws*` never fall
//! back to `index.html`, so a mistyped API path stays a 404 instead of returning HTML.

use std::path::{Component, Path, PathBuf};

use axum::body::Body;
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use percent_encoding::percent_decode_str;

const STATIC_MIME_TYPES: &[(&str, &str)] = &[
    (".css", "text/css; charset=utf-8"),
    (".gif", "image/gif"),
    (".html", "text/html; charset=utf-8"),
    (".ico", "image/x-icon"),
    (".jpg", "image/jpeg"),
    (".jpeg", "image/jpeg"),
    (".js", "text/javascript; charset=utf-8"),
    (".json", "application/json; charset=utf-8"),
    (".map", "application/json; charset=utf-8"),
    (".png", "image/png"),
    (".svg", "image/svg+xml"),
    (".txt", "text/plain; charset=utf-8"),
    (".wasm", "application/wasm"),
    (".webp", "image/webp"),
    (".woff", "font/woff"),
    (".woff2", "font/woff2"),
];

pub async fn serve_static(
    static_dir: &Path,
    spa_fallback: bool,
    method: &Method,
    pathname: &str,
) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(resolved) = resolve_static_file(static_dir, pathname, spa_fallback).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(bytes) = tokio::fs::read(&resolved.path).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let content_type = static_content_type(&resolved.path);
    let cache_control = if resolved.is_index {
        "no-cache"
    } else {
        "public, max-age=31536000, immutable"
    };
    let mut response = if method == Method::HEAD {
        Response::new(Body::empty())
    } else {
        Response::new(Body::from(bytes))
    };
    *response.status_mut() = StatusCode::OK;
    if let Ok(value) = HeaderValue::from_str(content_type) {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    if let Ok(value) = HeaderValue::from_str(cache_control) {
        response.headers_mut().insert(header::CACHE_CONTROL, value);
    }
    response
}

struct ResolvedStaticFile {
    path: PathBuf,
    is_index: bool,
}

async fn resolve_static_file(
    static_dir: &Path,
    pathname: &str,
    spa_fallback: bool,
) -> Option<ResolvedStaticFile> {
    let normalized = if pathname == "/" {
        "/index.html"
    } else {
        pathname
    };
    let decoded = percent_decode_str(normalized).decode_utf8().ok()?;
    let relative = decoded.trim_start_matches('/');
    let candidate = safe_join(static_dir, relative)?;

    match tokio::fs::metadata(&candidate).await {
        Ok(metadata) if metadata.is_dir() => {
            let index = candidate.join("index.html");
            let index_metadata = tokio::fs::metadata(&index).await.ok()?;
            if index_metadata.is_file() {
                return Some(ResolvedStaticFile {
                    path: index,
                    is_index: true,
                });
            }
        }
        Ok(metadata) if metadata.is_file() => {
            let is_index = candidate
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == "index.html");
            return Some(ResolvedStaticFile {
                path: candidate,
                is_index,
            });
        }
        _ => {}
    }

    if !spa_fallback || !is_static_fallback_allowed(pathname) {
        return None;
    }
    let index = static_dir.join("index.html");
    let metadata = tokio::fs::metadata(&index).await.ok()?;
    metadata.is_file().then_some(ResolvedStaticFile {
        path: index,
        is_index: true,
    })
}

fn safe_join(root: &Path, relative: &str) -> Option<PathBuf> {
    let path = Path::new(relative);
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(root.join(path))
}

pub fn is_static_fallback_allowed(pathname: &str) -> bool {
    !(pathname == "/ws" || pathname.starts_with("/ws/") || pathname.starts_with("/api/"))
}

fn static_content_type(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| format!(".{}", extension.to_lowercase()));
    extension
        .as_deref()
        .and_then(|extension| {
            STATIC_MIME_TYPES
                .iter()
                .find(|(candidate, _)| *candidate == extension)
                .map(|(_, mime)| *mime)
        })
        .unwrap_or("application/octet-stream")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traversal_and_absolute_paths_are_rejected() {
        let root = Path::new("root");
        assert!(safe_join(root, "../secret").is_none());
        assert!(safe_join(root, "a/../../secret").is_none());
        assert!(safe_join(root, "/etc/passwd").is_none());
        assert_eq!(
            safe_join(root, "assets/app.js"),
            Some(root.join("assets/app.js"))
        );
    }

    #[test]
    fn mime_matches_the_typescript_table() {
        assert_eq!(
            static_content_type(Path::new("a/app.js")),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            static_content_type(Path::new("a/app.CSS")),
            "text/css; charset=utf-8"
        );
        assert_eq!(
            static_content_type(Path::new("a/unknown.bin")),
            "application/octet-stream"
        );
    }

    #[test]
    fn api_and_ws_paths_never_use_spa_fallback() {
        assert!(!is_static_fallback_allowed("/api/server-info"));
        assert!(!is_static_fallback_allowed("/ws"));
        assert!(!is_static_fallback_allowed("/ws/host"));
        assert!(is_static_fallback_allowed("/tasks/123"));
    }
}
