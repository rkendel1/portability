use clap::{Parser, Subcommand};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "app", about = "Daemonless WASM application runtime")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Init { name: String },
    Build,
    Run { manifest: Option<PathBuf> },
    Inspect { manifest: Option<PathBuf> },
}

fn main() {
    let result = match Cli::parse().command {
        Command::Init { name } => init(&name),
        Command::Build => app_compiler::build(Path::new(".")).map(|m| {
            println!(
                "Compiled {}\nArtifact: sha256:{}",
                m.name, m.artifact.sha256
            )
        }),
        Command::Run { manifest } => match manifest {
            Some(manifest) => app_runtime::run_manifest(&manifest),
            None => app_runtime::run(Path::new(".")),
        },
        Command::Inspect { manifest } => match manifest {
            Some(manifest) => inspect(&manifest),
            None => inspect(Path::new("target/app.manifest.json")),
        },
    };
    if let Err(error) = result {
        eprintln!("ERROR {error}");
        std::process::exit(1);
    }
}

fn init(name: &str) -> Result<(), String> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("invalid application name".into());
    }
    let root = PathBuf::from(name);
    if root.exists() {
        return Err(format!("{} already exists", root.display()));
    }
    fs::create_dir_all(root.join("src")).map_err(|e| e.to_string())?;
    fs::write(root.join("app.toml"), format!("name = \"{name}\"\nversion = \"0.1.0\"\n[build]\nsource = \"src\"\nentry = \"src/main.rs\"\n[runtime]\nkind = \"wasm\"\n[http]\nlisten = 8080\n[capabilities]\nnetwork = false\nfilesystem = true\n[storage]\npath = \".app/data\"\nmount = \"/data\"\n")).map_err(|e| e.to_string())?;
    fs::write(root.join("Cargo.toml"), format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\nautobins = false\n\n[lib]\npath = \"src/main.rs\"\ncrate-type = [\"cdylib\"]\n")).map_err(|e| e.to_string())?;
    fs::write(
        root.join("src/main.rs"),
        "#[unsafe(no_mangle)]\npub extern \"C\" fn handle_request() {}\n",
    )
    .map_err(|e| e.to_string())?;
    println!("Created {}", root.display());
    Ok(())
}

fn inspect(manifest_path: &Path) -> Result<(), String> {
    let manifest = app_manifest::Manifest::load(manifest_path)?;
    println!(
        "Application: {}\nVersion:     {}\nRuntime:     {}\nArtifact:\n  File:      {}\n  SHA256:    {}\n  Size:      {}\nCapabilities:\n  http:      {}\n  network:   {}\n  filesystem: {}\nStorage:\n  mount:     {}\n  path:      {}",
        manifest.name,
        manifest.version,
        manifest.runtime,
        manifest.artifact.file,
        manifest.artifact.sha256,
        manifest.artifact.size,
        manifest
            .http
            .map(|h| format!("listen :{}", h.listen))
            .unwrap_or_else(|| "none".into()),
        if manifest.capabilities.network {
            "allowed"
        } else {
            "denied"
        },
        manifest.capabilities.filesystem,
        manifest
            .storage
            .as_ref()
            .map(|storage| storage.mount.as_str())
            .unwrap_or("none"),
        manifest
            .storage
            .as_ref()
            .map(|storage| storage.path.as_str())
            .unwrap_or("none")
    );
    Ok(())
}
