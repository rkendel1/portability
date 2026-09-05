use clap::{Parser, Subcommand, ValueEnum};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "app", about = "Daemonless WASM application runtime")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Clone, Copy, Debug, ValueEnum)]
enum StateProvider {
    Filesystem,
    #[value(name = "feltdb")]
    FeltDB,
}

impl std::fmt::Display for StateProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StateProvider::Filesystem => f.write_str("filesystem"),
            StateProvider::FeltDB => f.write_str("feltdb"),
        }
    }
}

impl From<StateProvider> for app_runtime::StateProviderKind {
    fn from(provider: StateProvider) -> Self {
        match provider {
            StateProvider::Filesystem => app_runtime::StateProviderKind::Filesystem,
            StateProvider::FeltDB => app_runtime::StateProviderKind::FeltDB,
        }
    }
}

#[derive(Subcommand)]
enum Command {
    Init {
        name: String,
    },
    Build,
    Run {
        manifest: Option<PathBuf>,
        #[arg(long, value_name = "DIR")]
        state: Option<PathBuf>,
        #[arg(long, value_enum, default_value_t = StateProvider::Filesystem)]
        state_provider: StateProvider,
    },
    Inspect {
        manifest: Option<PathBuf>,
    },
}

fn main() {
    let result = match Cli::parse().command {
        Command::Init { name } => init(&name),
        Command::Build => app_compiler::build(Path::new(".")).and_then(|m| {
            let wasm = fs::read(Path::new("target").join(&m.artifact.file))
                .map_err(|e| format!("cannot read built artifact: {e}"))?;
            let application_id = m.application_id(&wasm)?;
            println!(
                "Compiled {}\nApplication ID: {}\nArtifact: sha256:{}",
                m.name, application_id, m.artifact.sha256
            );
            Ok(())
        }),
        Command::Run {
            manifest,
            state,
            state_provider,
        } => match manifest {
            Some(manifest) => app_runtime::run_manifest_with_state_provider(
                &manifest,
                state.as_deref(),
                state_provider.into(),
            ),
            None => app_runtime::run_with_state_provider(
                Path::new("."),
                state.as_deref(),
                state_provider.into(),
            ),
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
    let manifest_dir = manifest_path
        .parent()
        .ok_or_else(|| format!("manifest path has no parent: {}", manifest_path.display()))?;
    let artifact_path = manifest_dir.join(&manifest.artifact.file);
    let wasm = fs::read(&artifact_path)
        .map_err(|e| format!("cannot read artifact {}: {e}", artifact_path.display()))?;
    let application_id = manifest.application_id(&wasm)?;
    let integrity = if app_manifest::sha256(&wasm) == manifest.artifact.sha256 {
        "verified"
    } else {
        "mismatch"
    };
    println!(
        "Application: {}\nVersion:     {}\nRuntime:     {}\nApplication ID:\n  {}\nArtifact:\n  File:      {}\n  SHA256:    {}\n  Size:      {}\n  Integrity: {}\nCapabilities:\n  http:      {}\n  network:   {}\n  filesystem: {}\nStorage:\n  mount:     {}\n  path:      {}",
        manifest.name,
        manifest.version,
        manifest.runtime,
        application_id,
        manifest.artifact.file,
        manifest.artifact.sha256,
        manifest.artifact.size,
        integrity,
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
