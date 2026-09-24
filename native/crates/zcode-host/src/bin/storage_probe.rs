//! storage probe 二进制：建库 + 写入种子数据（供跨语言 parity 测试调用）。
//!
//! 这是**测试辅助入口**，不是产品路径：它把 `zcode_host::storage` 在真实 sqlite
//! 上跑一遍，让 TS 侧（node:sqlite）随后打开同一个文件验证 schema 与数据。
//!
//! 用法：storage-probe <db-path>

use std::path::PathBuf;

use zcode_host::storage::{self, TaskRow};

fn main() {
    let Some(path) = std::env::args().nth(1).map(PathBuf::from) else {
        eprintln!("usage: storage-probe <db-path>");
        std::process::exit(2);
    };
    let connection = match storage::open(&path) {
        Ok(connection) => connection,
        Err(error) => {
            eprintln!("storage-probe open failed: {error}");
            std::process::exit(1);
        }
    };

    for (task_id, title, updated_at) in [
        ("task-old", "written by rust", 100_i64),
        ("task-new", "written by rust", 200_i64),
    ] {
        let row = TaskRow {
            workspace_key: "ws".to_string(),
            workspace_path: "F:/ws".to_string(),
            workspace_identity: Some("ws".to_string()),
            task_id: task_id.to_string(),
            title: title.to_string(),
            task_status: Some("completed".to_string()),
            mode: "build".to_string(),
            created_at: 1,
            updated_at,
            meta_json: "{}".to_string(),
        };
        if let Err(error) = row.insert(&connection) {
            eprintln!("storage-probe insert failed: {error}");
            std::process::exit(1);
        }
    }

    let listed = match storage::list_tasks(&connection, "ws") {
        Ok(rows) => rows,
        Err(error) => {
            eprintln!("storage-probe list failed: {error}");
            std::process::exit(1);
        }
    };
    eprintln!("storage-probe: wrote {} tasks", listed.len());
}
