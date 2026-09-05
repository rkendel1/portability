use clap::{Parser, Subcommand, ValueEnum};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

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
        #[arg(long, value_enum, default_value_t = StateProvider::FeltDB)]
        state_provider: StateProvider,
        #[arg(long, value_name = "NAME")]
        secret: Vec<String>,
    },
    Start {
        manifest: Option<PathBuf>,
        #[arg(long, value_name = "DIR")]
        state: Option<PathBuf>,
        #[arg(long, value_enum, default_value_t = StateProvider::FeltDB)]
        state_provider: StateProvider,
        #[arg(long, value_name = "NAME")]
        secret: Vec<String>,
    },
    Status {
        manifest: Option<PathBuf>,
    },
    Stop {
        manifest: Option<PathBuf>,
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
            secret,
        } => resolve_secrets(&secret).and_then(|secrets| match manifest {
            Some(manifest) => app_runtime::run_manifest_with_state_provider_and_secrets(
                &manifest,
                state.as_deref(),
                state_provider.into(),
                &secrets,
            ),
            None => app_runtime::run_with_state_provider_and_secrets(
                Path::new("."),
                state.as_deref(),
                state_provider.into(),
                &secrets,
            ),
        }),
        Command::Start {
            manifest,
            state,
            state_provider,
            secret,
        } => start(
            manifest.as_deref(),
            state.as_deref(),
            state_provider,
            &secret,
        ),
        Command::Status { manifest } => status(manifest.as_deref()),
        Command::Stop { manifest } => stop(manifest.as_deref()),
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

fn start(
    manifest: Option<&Path>,
    state: Option<&Path>,
    state_provider: StateProvider,
    secret: &[String],
) -> Result<(), String> {
    let manifest_path = manifest.unwrap_or(Path::new("target/app.manifest.json"));
    let secrets = resolve_secrets(secret)?;
    let prepared = app_runtime::prepare_start_with_secrets(
        manifest_path,
        Path::new("."),
        state,
        state_provider.into(),
        &secrets,
    )?;
    let mut command =
        std::process::Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
    command.arg("run");
    if let Some(manifest) = manifest {
        command.arg(manifest);
    }
    if let Some(state) = state {
        command.arg("--state").arg(state);
    }
    command
        .arg("--state-provider")
        .arg(state_provider.to_string());
    for name in secret {
        command.arg("--secret").arg(name);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    wait_for_started(&mut child, &prepared.endpoint)?;
    match app_runtime::record_started(prepared, child.id()) {
        Ok(record) => {
            println!(
                "Started {}\nApplication ID: {}\nPID: {}\nEndpoint: {}",
                record.name, record.application_id, record.pid, record.endpoint
            );
            Ok(())
        }
        Err(error) => {
            child.kill().ok();
            Err(error)
        }
    }
}

fn resolve_secrets(names: &[String]) -> Result<app_runtime::RuntimeSecrets, String> {
    let mut secrets = app_runtime::RuntimeSecrets::new();
    for name in names {
        let value = std::env::var(name)
            .map_err(|_| format!("secret {name} requested but environment variable is not set"))?;
        secrets.insert(name.clone(), value);
    }
    Ok(secrets)
}

fn wait_for_started(child: &mut std::process::Child, endpoint: &str) -> Result<(), String> {
    let stdout = child.stdout.take().ok_or("cannot read app start output")?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
        tx.send(result).ok();
    });
    match rx.recv_timeout(Duration::from_secs(60)) {
        Ok(Ok(line)) if line.contains(&format!("listening on {endpoint}")) => Ok(()),
        Ok(Ok(line)) => {
            child.kill().ok();
            Err(format!("unexpected app start output: {line}"))
        }
        Ok(Err(error)) => {
            child.kill().ok();
            Err(format!("failed to read app start output: {error}"))
        }
        Err(_) => {
            child.kill().ok();
            Err(format!("timed out waiting for app to listen on {endpoint}"))
        }
    }
}

fn status(manifest: Option<&Path>) -> Result<(), String> {
    let manifest_path = manifest.unwrap_or(Path::new("target/app.manifest.json"));
    match app_runtime::status(
        manifest_path,
        Path::new("."),
        None,
        app_runtime::StateProviderKind::FeltDB,
    )? {
        app_runtime::ApplicationStatus::Running(record) => {
            print_record_status("running", &record);
        }
        app_runtime::ApplicationStatus::Stopped(prepared) => {
            println!(
                "Application: {}\nApplication ID: {}\nStatus:       stopped\nEndpoint:     {}\nState:        {}\nState scope:  {}\nArtifact:     verified",
                prepared.name,
                prepared.application_id,
                prepared.endpoint,
                prepared.state_provider.label(),
                prepared.application_id
            );
        }
    }
    Ok(())
}

fn stop(manifest: Option<&Path>) -> Result<(), String> {
    let manifest_path = manifest.unwrap_or(Path::new("target/app.manifest.json"));
    let record = app_runtime::stop(manifest_path)?;
    print_record_status("stopped", &record);
    Ok(())
}

fn print_record_status(status: &str, record: &app_runtime::RuntimeRecord) {
    println!(
        "Application: {}\nApplication ID: {}\nStatus:       {}\nEndpoint:     {}\nState:        {}\nState scope:  {}\nArtifact:     verified\nPID:          {}\nStarted at:   {}\nArtifact path: {}",
        record.name,
        record.application_id,
        status,
        record.endpoint,
        record.state_provider.label(),
        record.application_id,
        record.pid,
        record.started_at,
        record.artifact_path.display()
    );
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
