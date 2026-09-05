use app_manifest::Manifest;
use app_spec::AppSpec;
use std::fs;
use std::path::Path;
use std::process::Command;

pub fn build(project: &Path) -> Result<Manifest, String> {
    let spec = AppSpec::load(&project.join("app.toml"))?;
    let builder: Box<dyn ApplicationBuilder> = match spec.build.language.as_str() {
        "rust" => Box::new(RustWasmBuilder),
        "go" => Box::new(GoWasmBuilder),
        language => return Err(format!("unsupported build language: {language}")),
    };
    builder.build(project, &spec)
}

trait ApplicationBuilder {
    fn build(&self, project: &Path, spec: &AppSpec) -> Result<Manifest, String>;
}

struct RustWasmBuilder;

impl ApplicationBuilder for RustWasmBuilder {
    fn build(&self, project: &Path, spec: &AppSpec) -> Result<Manifest, String> {
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
        write_artifacts(project, spec, &wasm)
    }
}

struct GoWasmBuilder;

impl ApplicationBuilder for GoWasmBuilder {
    fn build(&self, project: &Path, spec: &AppSpec) -> Result<Manifest, String> {
        let target = project.join("target");
        fs::create_dir_all(&target).map_err(|e| e.to_string())?;
        let output = target.join("app.wasm");
        let status = Command::new("go")
            .current_dir(project)
            .env("GOOS", "wasip1")
            .env("GOARCH", "wasm")
            .args(["build", "-buildmode=c-shared", "-o"])
            .arg(&output)
            .arg(".")
            .status()
            .map_err(|e| format!("failed to start go: {e}"))?;
        if !status.success() {
            return Err("Go to WASM compilation failed".into());
        }
        let wasm = fs::read(&output)
            .map_err(|e| format!("cannot find compiled artifact {}: {e}", output.display()))?;
        write_artifacts(project, spec, &wasm)
    }
}

fn write_artifacts(project: &Path, spec: &AppSpec, wasm: &[u8]) -> Result<Manifest, String> {
    let target = project.join("target");
    fs::create_dir_all(&target).map_err(|e| e.to_string())?;
    fs::write(target.join("app.wasm"), wasm).map_err(|e| e.to_string())?;
    let manifest = Manifest::from_spec(spec, wasm);
    manifest.write(&target.join("app.manifest.json"))?;
    Ok(manifest)
}
