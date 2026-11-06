fn main() {
    // Register the app ACL manifest so the loopback-served web UI can call the
    // shell's commands through an explicit capability (remote origins are denied
    // by default in Tauri 2). Permissions live in `permissions/shell.toml`.
    tauri_build::try_build(
        tauri_build::Attributes::new().app_manifest(tauri_build::AppManifest::new()),
    )
    .expect("failed to run tauri-build");

    // Web static pipeline (spec §5): copy the built `packages/web/dist` into
    // `$OUT_DIR/web` so `cargo run` can serve it from a stable build-output
    // location in addition to the dev checkout path. Packaged apps bundle the
    // same dist via `bundle.resources` in `tauri.conf.json` (from
    // `../../../packages/web/dist` → `web`) and resolve it through the
    // `BaseDirectory::Resource` branch in `backend.rs`.
    let manifest_dir =
        std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let web_dist = manifest_dir.join("../../../packages/web/dist");
    println!(
        "cargo:rerun-if-changed={}",
        web_dist.join("index.html").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("tauri.conf.json").display()
    );
    let dest = out_dir.join("web");
    if web_dist.join("index.html").is_file() {
        if let Err(error) = copy_dir_recursive(&web_dist, &dest) {
            println!(
                "cargo:warning=web dist copy to {} failed: {error}",
                dest.display()
            );
        } else {
            println!("cargo:rustc-env=ZCODE_WEB_DIST_STAGED={}", dest.display());
        }
    } else {
        println!(
            "cargo:warning=packages/web/dist/index.html not found at {}; shell will need ZCODE_WEB_DIST at runtime",
            web_dist.display()
        );
    }
}

fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if kind.is_file() {
            std::fs::copy(&from, &to)?;
        }
        // Symlinks are not followed (web dist has none; stale links fail loudly).
    }
    Ok(())
}
