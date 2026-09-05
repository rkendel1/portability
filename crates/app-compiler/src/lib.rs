use app_manifest::Manifest;
use app_spec::AppSpec;
use std::fs;
use std::path::Path;
use std::process::Command;

pub fn build(project: &Path) -> Result<Manifest, String> {
    let spec = AppSpec::load(&project.join("app.toml"))?;
    let target = project.join("target");
    let cargo_target = target.join(".cargo");
    let status = Command::new("cargo")
        .current_dir(project)
        .env("CARGO_TARGET_DIR", &cargo_target)
        .args(["build", "--release", "--target", "wasm32-unknown-unknown"])
        .status()
        .map_err(|e| format!("failed to start cargo: {e}"))?;
    if !status.success() {
        return Err("Rust to WASM compilation failed".into());
    }
    let binary = cargo_target
        .join("wasm32-unknown-unknown/release")
        .join(format!("{}.wasm", spec.name.replace('-', "_")));
    let wasm = fs::read(&binary)
        .map_err(|e| format!("cannot find compiled artifact {}: {e}", binary.display()))?;
    fs::create_dir_all(&target).map_err(|e| e.to_string())?;
    fs::write(target.join("app.wasm"), &wasm).map_err(|e| e.to_string())?;
    let manifest = Manifest::from_spec(&spec, &wasm);
    manifest.write(&target.join("app.manifest.json"))?;
    Ok(manifest)
}
