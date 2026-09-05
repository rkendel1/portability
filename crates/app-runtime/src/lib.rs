use app_capabilities::{
    CapabilityError, ConfigCapability, FeltDBStateProvider, NetworkCapability, SecretCapability,
    StorageCapability,
};
use app_manifest::{ApplicationId, Manifest, sha256};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use wasmtime::{Caller, Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};

#[derive(Clone)]
struct HostState {
    application_id: String,
    network: NetworkCapability,
    storage: Option<StorageCapability>,
    secrets: SecretCapability,
    config: ConfigCapability,
    limits: StoreLimits,
    execution_fuel: Option<u64>,
    requests: RequestLimiter,
    execution_units: Arc<AtomicUsize>,
    log_path: Option<PathBuf>,
}

pub fn run(project: &Path) -> Result<(), String> {
    run_with_state(project, None)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StateProviderKind {
    Filesystem,
    FeltDB,
}

pub type RuntimeSecrets = BTreeMap<String, String>;
pub type RuntimeConfig = BTreeMap<String, String>;

#[derive(Clone)]
struct RequestLimiter {
    max: usize,
    active: Arc<AtomicUsize>,
}

struct RequestPermit {
    active: Arc<AtomicUsize>,
}

impl RequestLimiter {
    fn new(max: usize) -> Self {
        Self {
            max,
            active: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn enter(&self) -> Result<RequestPermit, String> {
        let previous = self.active.fetch_add(1, Ordering::SeqCst);
        if previous >= self.max {
            self.active.fetch_sub(1, Ordering::SeqCst);
            Err("request concurrency limit exceeded".into())
        } else {
            Ok(RequestPermit {
                active: Arc::clone(&self.active),
            })
        }
    }
}

impl Drop for RequestPermit {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

impl StateProviderKind {
    pub fn label(self) -> &'static str {
        match self {
            StateProviderKind::Filesystem => "filesystem",
            StateProviderKind::FeltDB => "FeltDB",
        }
    }
}

#[derive(Debug)]
pub struct PreparedApplication {
    pub application_id: ApplicationId,
    pub name: String,
    pub endpoint: String,
    pub state_provider: StateProviderKind,
    pub state_location: Option<PathBuf>,
    pub artifact_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeRecord {
    pub application_id: String,
    pub name: String,
    pub pid: u32,
    pub endpoint: String,
    pub state_provider: StateProviderKind,
    pub state_location: Option<PathBuf>,
    pub artifact_path: PathBuf,
    pub started_at: u64,
}

#[derive(Debug)]
pub enum ApplicationStatus {
    Running(RuntimeRecord),
    Stopped(PreparedApplication),
}

pub fn run_with_state(project: &Path, state: Option<&Path>) -> Result<(), String> {
    run_with_state_provider(project, state, StateProviderKind::FeltDB)
}

pub fn run_with_state_provider(
    project: &Path,
    state: Option<&Path>,
    provider: StateProviderKind,
) -> Result<(), String> {
    run_with_state_provider_and_secrets(project, state, provider, &RuntimeSecrets::new())
}

pub fn run_with_state_provider_and_secrets(
    project: &Path,
    state: Option<&Path>,
    provider: StateProviderKind,
    secrets: &RuntimeSecrets,
) -> Result<(), String> {
    run_from_manifest(
        &project.join("target/app.manifest.json"),
        project,
        state,
        provider,
        secrets,
    )
}

pub fn run_manifest(manifest_path: &Path, state: Option<&Path>) -> Result<(), String> {
    run_manifest_with_state_provider(manifest_path, state, StateProviderKind::FeltDB)
}

pub fn run_manifest_with_state_provider(
    manifest_path: &Path,
    state: Option<&Path>,
    provider: StateProviderKind,
) -> Result<(), String> {
    run_manifest_with_state_provider_and_secrets(
        manifest_path,
        state,
        provider,
        &RuntimeSecrets::new(),
    )
}

pub fn run_manifest_with_state_provider_and_secrets(
    manifest_path: &Path,
    state: Option<&Path>,
    provider: StateProviderKind,
    secrets: &RuntimeSecrets,
) -> Result<(), String> {
    run_from_manifest(manifest_path, Path::new("."), state, provider, secrets)
}

pub fn invoke_appport_operation(
    appport_manifest_path: &Path,
    request_path: &Path,
    state: Option<&Path>,
    provider: StateProviderKind,
    secrets: &RuntimeSecrets,
) -> Result<Value, String> {
    let appport = serde_json::from_reader(fs::File::open(appport_manifest_path).map_err(|e| {
        format!(
            "cannot read AppPort manifest {}: {e}",
            appport_manifest_path.display()
        )
    })?)
    .map_err(|e| format!("invalid AppPort manifest: {e}"))?;
    let request: Value = serde_json::from_reader(fs::File::open(request_path).map_err(|e| {
        format!(
            "cannot read AppPort request {}: {e}",
            request_path.display()
        )
    })?)
    .map_err(|e| format!("invalid AppPort request: {e}"))?;
    invoke_appport_operation_value(
        appport_manifest_path,
        &appport,
        &request,
        state,
        provider,
        secrets,
    )
}

pub fn invoke_appport_operation_value(
    appport_manifest_path: &Path,
    appport: &Value,
    request: &Value,
    state: Option<&Path>,
    provider: StateProviderKind,
    secrets: &RuntimeSecrets,
) -> Result<Value, String> {
    let request_id = request
        .get("requestId")
        .and_then(Value::as_str)
        .ok_or("AppPort request requires requestId")?;
    if request.get("type").and_then(Value::as_str) != Some("request") {
        return Err("AppPort operation invocation requires a request envelope".into());
    }
    let capability = request
        .get("capability")
        .and_then(Value::as_object)
        .ok_or("AppPort request requires capability")?;
    let operation_name = capability
        .get("name")
        .and_then(Value::as_str)
        .ok_or("AppPort request capability.name must be a string")?;
    let operation_version = capability
        .get("version")
        .and_then(Value::as_u64)
        .ok_or("AppPort request capability.version must be an integer")?;
    let manifest_dir = appport_manifest_path.parent().ok_or_else(|| {
        format!(
            "AppPort manifest path has no parent: {}",
            appport_manifest_path.display()
        )
    })?;
    let artifact_file = app_manifest::appport_artifact_file(appport)?;
    let artifact_path = manifest_dir.join(&artifact_file);
    let wasm = fs::read(&artifact_path).map_err(|e| {
        format!(
            "cannot read AppPort artifact {}: {e}",
            artifact_path.display()
        )
    })?;
    let manifest =
        Manifest::from_appport_operation(appport, operation_name, operation_version, &wasm)?;
    let application_id = manifest.application_id(&wasm)?;
    let host_state = host_state(
        manifest_dir,
        state,
        provider,
        application_id.as_str(),
        &manifest,
        secrets,
    )?;
    let engine = engine(host_state.execution_fuel.is_some())?;
    let module = Module::new(&engine, &wasm).map_err(|e| e.to_string())?;
    invoke(&engine, &module, host_state)?;
    let mut response = json!({
        "protocol": request
            .get("protocol")
            .or_else(|| appport.get("protocol"))
            .cloned()
            .unwrap_or_else(|| json!("appport/1")),
        "type": "response",
        "requestId": request_id,
        "ok": true,
        "output": null
    });
    if let Some(trace_id) = request.get("traceId").cloned() {
        response["traceId"] = trace_id;
    }
    Ok(response)
}

fn run_from_manifest(
    manifest_path: &Path,
    default_state_base: &Path,
    state: Option<&Path>,
    provider: StateProviderKind,
    secrets: &RuntimeSecrets,
) -> Result<(), String> {
    let runtime = runtime_application(manifest_path, default_state_base, state, provider, secrets)?;
    let manifest = runtime.manifest;
    let wasm = runtime.wasm;
    let host_state = runtime.host_state;
    let engine = engine(host_state.execution_fuel.is_some())?;
    let module = Module::new(&engine, wasm).map_err(|e| e.to_string())?;
    let http = manifest.http.ok_or("no HTTP endpoint declared")?;
    let listener = TcpListener::bind(("127.0.0.1", http.listen)).map_err(|e| e.to_string())?;
    let listen = format!(
        "{} listening on http://127.0.0.1:{}",
        manifest.name, http.listen
    );
    log_line(&host_state.log_path, &listen)?;
    println!("{listen}");
    for stream in listener.incoming() {
        let mut stream = stream.map_err(|e| e.to_string())?;
        let mut request = [0; 1024];
        stream.read(&mut request).map_err(|e| e.to_string())?;
        if let Err(error) = invoke(&engine, &module, host_state.clone()) {
            log_line(&host_state.log_path, &format!("ERROR {error}"))?;
            stream.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain\r\nContent-Length: 21\r\nConnection: close\r\n\r\nWASM execution failed").map_err(|e| e.to_string())?;
            continue;
        }
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 15\r\nConnection: close\r\n\r\nHello from WASM").map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn prepare_start(
    manifest_path: &Path,
    default_state_base: &Path,
    state: Option<&Path>,
    provider: StateProviderKind,
) -> Result<PreparedApplication, String> {
    prepare_start_with_secrets(
        manifest_path,
        default_state_base,
        state,
        provider,
        &RuntimeSecrets::new(),
    )
}

pub fn prepare_start_with_secrets(
    manifest_path: &Path,
    default_state_base: &Path,
    state: Option<&Path>,
    provider: StateProviderKind,
    secrets: &RuntimeSecrets,
) -> Result<PreparedApplication, String> {
    let runtime = runtime_application(manifest_path, default_state_base, state, provider, secrets)?;
    reject_if_running(&runtime.prepared.application_id)?;
    Ok(runtime.prepared)
}

pub fn record_started(prepared: PreparedApplication, pid: u32) -> Result<RuntimeRecord, String> {
    let record = RuntimeRecord {
        application_id: prepared.application_id.to_string(),
        name: prepared.name,
        pid,
        endpoint: prepared.endpoint,
        state_provider: prepared.state_provider,
        state_location: prepared.state_location,
        artifact_path: prepared.artifact_path,
        started_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_secs(),
    };
    let path = record_path(&record.application_id)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(
        &path,
        format!(
            "{}\n",
            serde_json::to_string_pretty(&record).map_err(|e| e.to_string())?
        ),
    )
    .map_err(|e| e.to_string())?;
    Ok(record)
}

pub fn logs(manifest_path: &Path) -> Result<String, String> {
    let (manifest, wasm) = load_manifest_artifact(manifest_path)?;
    let application_id = manifest.application_id(&wasm)?;
    let path = log_path(application_id.as_str())?;
    match fs::read_to_string(&path) {
        Ok(logs) => Ok(logs),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(format!("no logs for application {application_id}"))
        }
        Err(error) => Err(error.to_string()),
    }
}

pub fn log_path_for_application_id(application_id: &ApplicationId) -> Result<PathBuf, String> {
    log_path(application_id.as_str())
}

pub fn status(
    manifest_path: &Path,
    default_state_base: &Path,
    state: Option<&Path>,
    provider: StateProviderKind,
) -> Result<ApplicationStatus, String> {
    let prepared = prepare_status(manifest_path, default_state_base, state, provider)?;
    match read_record(prepared.application_id.as_str())? {
        Some(record) if process_running(record.pid) => Ok(ApplicationStatus::Running(record)),
        Some(_) => {
            remove_record(prepared.application_id.as_str())?;
            Ok(ApplicationStatus::Stopped(prepared))
        }
        None => Ok(ApplicationStatus::Stopped(prepared)),
    }
}

pub fn stop(manifest_path: &Path) -> Result<RuntimeRecord, String> {
    let (manifest, wasm) = load_manifest_artifact(manifest_path)?;
    let application_id = manifest.application_id(&wasm)?;
    let Some(record) = read_record(application_id.as_str())? else {
        return Err(format!("application {application_id} is not running"));
    };
    if !process_running(record.pid) {
        remove_record(application_id.as_str())?;
        return Err(format!("application {application_id} is not running"));
    }
    terminate(record.pid)?;
    remove_record(application_id.as_str())?;
    Ok(record)
}

struct RuntimeApplication {
    manifest: Manifest,
    wasm: Vec<u8>,
    host_state: HostState,
    prepared: PreparedApplication,
}

fn runtime_application(
    manifest_path: &Path,
    default_state_base: &Path,
    state: Option<&Path>,
    provider: StateProviderKind,
    secrets: &RuntimeSecrets,
) -> Result<RuntimeApplication, String> {
    let (manifest, wasm) = load_manifest_artifact(manifest_path)?;
    let application_id = manifest.application_id(&wasm)?;
    let state_location = manifest
        .storage
        .as_ref()
        .map(|storage| {
            state_root(
                default_state_base,
                state,
                provider,
                &storage.path,
                application_id.as_str(),
            )
        })
        .transpose()?;
    let host_state = host_state(
        default_state_base,
        state,
        provider,
        application_id.as_str(),
        &manifest,
        secrets,
    )?;
    let listen = manifest
        .http
        .as_ref()
        .ok_or("no HTTP endpoint declared")?
        .listen;
    let name = manifest.name.clone();
    Ok(RuntimeApplication {
        manifest,
        wasm,
        host_state,
        prepared: PreparedApplication {
            application_id,
            name,
            endpoint: format!("http://127.0.0.1:{listen}"),
            state_provider: provider,
            state_location,
            artifact_path: artifact_path(manifest_path)?,
        },
    })
}

fn prepare_status(
    manifest_path: &Path,
    default_state_base: &Path,
    state: Option<&Path>,
    provider: StateProviderKind,
) -> Result<PreparedApplication, String> {
    let (manifest, wasm) = load_manifest_artifact(manifest_path)?;
    let application_id = manifest.application_id(&wasm)?;
    let state_location = manifest
        .storage
        .as_ref()
        .map(|storage| {
            state_root(
                default_state_base,
                state,
                provider,
                &storage.path,
                application_id.as_str(),
            )
        })
        .transpose()?;
    let listen = manifest
        .http
        .as_ref()
        .ok_or("no HTTP endpoint declared")?
        .listen;
    Ok(PreparedApplication {
        application_id,
        name: manifest.name,
        endpoint: format!("http://127.0.0.1:{listen}"),
        state_provider: provider,
        state_location,
        artifact_path: artifact_path(manifest_path)?,
    })
}

fn load_manifest_artifact(manifest_path: &Path) -> Result<(Manifest, Vec<u8>), String> {
    let manifest = Manifest::load(manifest_path)?;
    let artifact_path = artifact_path_for(manifest_path, &manifest)?;
    let wasm = fs::read(&artifact_path)
        .map_err(|e| format!("cannot read artifact {}: {e}", artifact_path.display()))?;
    if sha256(&wasm) != manifest.artifact.sha256 {
        return Err("artifact hash mismatch".into());
    }
    Ok((manifest, wasm))
}

fn artifact_path(manifest_path: &Path) -> Result<PathBuf, String> {
    let manifest = Manifest::load(manifest_path)?;
    artifact_path_for(manifest_path, &manifest)
}

fn artifact_path_for(manifest_path: &Path, manifest: &Manifest) -> Result<PathBuf, String> {
    let manifest_dir = manifest_path
        .parent()
        .ok_or_else(|| format!("manifest path has no parent: {}", manifest_path.display()))?;
    let artifact_path = manifest_dir.join(&manifest.artifact.file);
    Ok(artifact_path
        .canonicalize()
        .unwrap_or_else(|_| artifact_path.to_path_buf()))
}

fn host_state(
    default_state_base: &Path,
    state: Option<&Path>,
    provider: StateProviderKind,
    application_id: &str,
    manifest: &Manifest,
    secrets: &RuntimeSecrets,
) -> Result<HostState, String> {
    let storage = match (&manifest.storage, manifest.capabilities.filesystem) {
        (Some(storage), true) => {
            let state_root = state_root(
                default_state_base,
                state,
                provider,
                &storage.path,
                application_id,
            )?;
            let storage = match provider {
                StateProviderKind::Filesystem => {
                    StorageCapability::new(&storage.mount, state_root)?
                }
                StateProviderKind::FeltDB => StorageCapability::with_provider(
                    &storage.mount,
                    FeltDBStateProvider::new(state_root, application_id)?,
                )?,
            };
            Some(storage)
        }
        (None, false) => None,
        (None, true) => return Err("filesystem capability requires storage declaration".into()),
        (Some(_), false) => return Err("storage declaration requires filesystem capability".into()),
    };
    let config = runtime_config(&manifest.config.allowed);
    let (store_limits, execution_fuel, request_limit) =
        runtime_limits(manifest.resources.as_ref())?;
    let destinations = manifest
        .network
        .as_ref()
        .map(|network| network.outbound.as_slice())
        .unwrap_or(&[]);
    Ok(HostState {
        application_id: application_id.to_string(),
        network: NetworkCapability::with_destinations(manifest.capabilities.network, destinations),
        storage,
        secrets: SecretCapability::new(&manifest.secrets.required, secrets.clone())
            .map_err(|error| error.to_string())?,
        config: ConfigCapability::new(&manifest.config.allowed, config)
            .map_err(|error| error.to_string())?,
        limits: store_limits,
        execution_fuel,
        requests: RequestLimiter::new(request_limit),
        execution_units: Arc::new(AtomicUsize::new(0)),
        log_path: std::env::var_os("APP_RUNTIME_LOG")
            .map(PathBuf::from)
            .or_else(|| log_path(application_id).ok()),
    })
}

fn runtime_config(allowed: &[String]) -> RuntimeConfig {
    allowed
        .iter()
        .filter_map(|name| std::env::var(name).ok().map(|value| (name.clone(), value)))
        .collect()
}

fn runtime_limits(
    resources: Option<&app_manifest::Resources>,
) -> Result<(StoreLimits, Option<u64>, usize), String> {
    let mut limits = StoreLimitsBuilder::new().instances(1);
    let mut fuel = None;
    let mut request_limit = 1;
    if let Some(resources) = resources {
        if resources.memory_mb == 0 {
            return Err("resources.memory_mb must be greater than zero".into());
        }
        if resources.timeout_ms == 0 {
            return Err("resources.timeout_ms must be greater than zero".into());
        }
        if resources.max_concurrent_requests == 0 {
            return Err("resources.max_concurrent_requests must be greater than zero".into());
        }
        let memory_bytes = resources
            .memory_mb
            .checked_mul(1024)
            .and_then(|mb| mb.checked_mul(1024))
            .ok_or_else(|| "resources.memory_mb is too large".to_string())?
            as usize;
        limits = limits.memory_size(memory_bytes).trap_on_grow_failure(true);
        fuel = Some(
            resources
                .timeout_ms
                .checked_mul(1000)
                .ok_or_else(|| "resources.timeout_ms is too large".to_string())?,
        );
        request_limit = resources.max_concurrent_requests as usize;
    }
    Ok((limits.build(), fuel, request_limit))
}

fn state_root(
    default_state_base: &Path,
    state: Option<&Path>,
    provider: StateProviderKind,
    storage_path: &str,
    application_id: &str,
) -> Result<PathBuf, String> {
    if let Some(state) = state {
        return Ok(state.to_path_buf());
    }
    match provider {
        StateProviderKind::Filesystem => Ok(default_state_base.join(storage_path)),
        StateProviderKind::FeltDB => default_feltdb_state_root(application_id),
    }
}

fn default_feltdb_state_root(application_id: &str) -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "cannot determine home directory for default FeltDB state".to_string())?;
    Ok(feltdb_state_root(&home, application_id))
}

fn feltdb_state_root(home: &Path, application_id: &str) -> PathBuf {
    let mut root = home.join(".appboundry").join("state");
    for segment in application_id.split(':') {
        if !segment.is_empty() {
            root.push(segment);
        }
    }
    root
}

fn reject_if_running(application_id: &ApplicationId) -> Result<(), String> {
    if let Some(record) = read_record(application_id.as_str())? {
        if process_running(record.pid) {
            return Err(format!(
                "application {application_id} already running (pid {})",
                record.pid
            ));
        }
        remove_record(application_id.as_str())?;
    }
    Ok(())
}

fn read_record(application_id: &str) -> Result<Option<RuntimeRecord>, String> {
    let path = record_path(application_id)?;
    if !path.exists() {
        return Ok(None);
    }
    let record = serde_json::from_reader(fs::File::open(&path).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    Ok(Some(record))
}

fn remove_record(application_id: &str) -> Result<(), String> {
    let path = record_path(application_id)?;
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn record_path(application_id: &str) -> Result<PathBuf, String> {
    let mut path = runtime_root()?;
    for segment in application_id.split(':') {
        if !segment.is_empty() {
            path.push(segment);
        }
    }
    path.push("runtime.json");
    Ok(path)
}

fn runtime_root() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            "cannot determine home directory for AppBoundry runtime state".to_string()
        })?;
    Ok(home.join(".appboundry").join("runtime"))
}

fn log_path(application_id: &str) -> Result<PathBuf, String> {
    let mut path = runtime_root()?;
    for segment in application_id.split(':') {
        if !segment.is_empty() {
            path.push(segment);
        }
    }
    path.push("runtime.log");
    Ok(path)
}

fn log_line(path: &Option<PathBuf>, line: &str) -> Result<(), String> {
    let Some(path) = path else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    writeln!(file, "{line}").map_err(|e| e.to_string())
}

fn process_running(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn terminate(pid: u32) -> Result<(), String> {
    Command::new("kill")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| e.to_string())
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                Err(format!("failed to terminate pid {pid}"))
            }
        })?;
    let deadline = SystemTime::now() + Duration::from_secs(5);
    while SystemTime::now() < deadline {
        if !process_running(pid) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| e.to_string())?;
    Ok(())
}

struct ExecutionUnit {
    id: usize,
    application_id: String,
    store: Store<HostState>,
    _permit: RequestPermit,
}

impl ExecutionUnit {
    fn create(engine: &Engine, host_state: HostState) -> Result<Self, String> {
        let permit = host_state.requests.enter()?;
        let id = host_state.execution_units.fetch_add(1, Ordering::SeqCst) + 1;
        let application_id = host_state.application_id.clone();
        log_line(
            &host_state.log_path,
            &format!("execution-unit {application_id}#{id} create"),
        )?;
        let mut unit = Self {
            id,
            application_id,
            store: Store::new(engine, host_state),
            _permit: permit,
        };
        unit.store.limiter(|state| &mut state.limits);
        if let Some(fuel) = unit.store.data().execution_fuel {
            if let Err(error) = unit.store.set_fuel(fuel).map_err(|e| e.to_string()) {
                unit.fail(&error).ok();
                return Err(error);
            }
        }
        Ok(unit)
    }

    fn execute(&mut self, linker: &Linker<HostState>, module: &Module) -> Result<(), String> {
        self.log("execute")?;
        let result: Result<(), String> = (|| {
            let instance = linker
                .instantiate(&mut self.store, module)
                .map_err(runtime_error)?;
            if let Some(initialize) = instance.get_func(&mut self.store, "_initialize") {
                initialize
                    .call(&mut self.store, &[], &mut [])
                    .map_err(runtime_error)?;
            }
            if let Some(handle) = instance.get_func(&mut self.store, "handle_request") {
                handle
                    .call(&mut self.store, &[], &mut [])
                    .map_err(runtime_error)?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.log("complete")?;
                Ok(())
            }
            Err(error) => {
                self.fail(&error).ok();
                Err(error)
            }
        }
    }

    fn fail(&mut self, error: &str) -> Result<(), String> {
        self.log(&format!("fail {error}"))
    }

    fn log(&self, event: &str) -> Result<(), String> {
        log_line(
            &self.store.data().log_path,
            &format!("execution-unit {}#{} {event}", self.application_id, self.id),
        )
    }
}

impl Drop for ExecutionUnit {
    fn drop(&mut self) {
        self.log("dispose").ok();
    }
}

fn invoke(engine: &Engine, module: &Module, host_state: HostState) -> Result<(), String> {
    let mut linker = Linker::new(engine);
    add_host_functions(&mut linker)?;
    let mut unit = ExecutionUnit::create(engine, host_state)?;
    unit.execute(&linker, module)
}

fn runtime_error(error: wasmtime::Error) -> String {
    let error = format!("{error:?}");
    if error.contains("all fuel consumed") {
        "wasm execution timed out".into()
    } else {
        error
    }
}

fn engine(consume_fuel: bool) -> Result<Engine, String> {
    if !consume_fuel {
        return Ok(Engine::default());
    }
    let mut config = Config::new();
    config.consume_fuel(true);
    Engine::new(&config).map_err(|e| e.to_string())
}

fn add_host_functions(linker: &mut Linker<HostState>) -> Result<(), String> {
    add_wasi_functions(linker)?;
    linker
        .func_wrap(
            "app_capabilities",
            "network_connect",
            |caller: Caller<'_, HostState>| -> wasmtime::Result<()> {
                caller
                    .data()
                    .network
                    .connect()
                    .map_err(|error| wasmtime::format_err!("{error}"))
            },
        )
        .map_err(|e| e.to_string())?;
    linker
        .func_wrap(
            "app_capabilities",
            "network_connect_to",
            |mut caller: Caller<'_, HostState>,
             destination_ptr: i32,
             destination_len: i32|
             -> wasmtime::Result<()> {
                let destination = guest_string(&mut caller, destination_ptr, destination_len)?;
                caller
                    .data()
                    .network
                    .connect_to(&destination)
                    .map_err(|error| wasmtime::format_err!("{error}"))
            },
        )
        .map_err(|e| e.to_string())?;
    linker
        .func_wrap(
            "app_capabilities",
            "get_secret",
            |mut caller: Caller<'_, HostState>,
             name_ptr: i32,
             name_len: i32,
             value_ptr: i32,
             value_len: i32|
             -> wasmtime::Result<i32> {
                if value_len < 0 {
                    return Err(wasmtime::format_err!("guest length must be non-negative"));
                }
                let name = guest_string(&mut caller, name_ptr, name_len)?;
                let value = caller
                    .data()
                    .secrets
                    .get(&name)
                    .map_err(|error| wasmtime::format_err!("{error}"))?
                    .as_bytes()
                    .to_vec();
                if value.len() > value_len as usize {
                    return Err(wasmtime::format_err!("secret output buffer too small"));
                }
                write_guest_bytes(&mut caller, value_ptr, &value)?;
                Ok(value.len() as i32)
            },
        )
        .map_err(|e| e.to_string())?;
    linker
        .func_wrap(
            "app_capabilities",
            "get_config",
            |mut caller: Caller<'_, HostState>,
             name_ptr: i32,
             name_len: i32,
             value_ptr: i32,
             value_len: i32|
             -> wasmtime::Result<i32> {
                if value_len < 0 {
                    return Err(wasmtime::format_err!("guest length must be non-negative"));
                }
                let name = guest_string(&mut caller, name_ptr, name_len)?;
                let Some(value) = caller
                    .data()
                    .config
                    .get(&name)
                    .map_err(|error| wasmtime::format_err!("{error}"))?
                    .map(str::as_bytes)
                else {
                    return Ok(-1);
                };
                let value = value.to_vec();
                if value.len() > value_len as usize {
                    return Err(wasmtime::format_err!("config output buffer too small"));
                }
                write_guest_bytes(&mut caller, value_ptr, &value)?;
                Ok(value.len() as i32)
            },
        )
        .map_err(|e| e.to_string())?;
    linker
        .func_wrap(
            "app_capabilities",
            "storage_write",
            |mut caller: Caller<'_, HostState>,
             path_ptr: i32,
             path_len: i32,
             value_ptr: i32,
             value_len: i32|
             -> wasmtime::Result<()> {
                let path = guest_string(&mut caller, path_ptr, path_len)?;
                let value = guest_bytes(&mut caller, value_ptr, value_len)?;
                let storage = caller
                    .data()
                    .storage
                    .as_ref()
                    .ok_or_else(|| denied("filesystem", "write"))?;
                storage
                    .write(&path, &value)
                    .map_err(|error| wasmtime::format_err!("{error}"))
            },
        )
        .map_err(|e| e.to_string())?;
    linker
        .func_wrap(
            "app_capabilities",
            "storage_read_equals",
            |mut caller: Caller<'_, HostState>,
             path_ptr: i32,
             path_len: i32,
             value_ptr: i32,
             value_len: i32|
             -> wasmtime::Result<i32> {
                let path = guest_string(&mut caller, path_ptr, path_len)?;
                let expected = guest_bytes(&mut caller, value_ptr, value_len)?;
                let storage = caller
                    .data()
                    .storage
                    .as_ref()
                    .ok_or_else(|| denied("filesystem", "read"))?;
                let actual = storage
                    .read(&path)
                    .map_err(|error| wasmtime::format_err!("{error}"))?;
                Ok(i32::from(actual == expected))
            },
        )
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn add_wasi_functions(linker: &mut Linker<HostState>) -> Result<(), String> {
    linker
        .func_wrap("wasi_snapshot_preview1", "sched_yield", || -> i32 { 0 })
        .map_err(|e| e.to_string())?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "proc_exit",
            |code: i32| -> wasmtime::Result<()> {
                Err(wasmtime::format_err!("guest exited with status {code}"))
            },
        )
        .map_err(|e| e.to_string())?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "args_get",
            |_argv: i32, _argv_buf: i32| -> i32 { 0 },
        )
        .map_err(|e| e.to_string())?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "args_sizes_get",
            |mut caller: Caller<'_, HostState>,
             argc: i32,
             argv_buf_size: i32|
             -> wasmtime::Result<i32> {
                write_u32(&mut caller, argc, 0)?;
                write_u32(&mut caller, argv_buf_size, 0)?;
                Ok(0)
            },
        )
        .map_err(|e| e.to_string())?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "clock_time_get",
            |mut caller: Caller<'_, HostState>,
             _clock_id: i32,
             _precision: i64,
             time: i32|
             -> wasmtime::Result<i32> {
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|e| wasmtime::format_err!("{e}"))?
                    .as_nanos() as u64;
                write_u64(&mut caller, time, nanos)?;
                Ok(0)
            },
        )
        .map_err(|e| e.to_string())?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "environ_get",
            |_environ: i32, _environ_buf: i32| -> i32 { 0 },
        )
        .map_err(|e| e.to_string())?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "environ_sizes_get",
            |mut caller: Caller<'_, HostState>,
             environ_count: i32,
             environ_buf_size: i32|
             -> wasmtime::Result<i32> {
                write_u32(&mut caller, environ_count, 0)?;
                write_u32(&mut caller, environ_buf_size, 0)?;
                Ok(0)
            },
        )
        .map_err(|e| e.to_string())?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_write",
            |mut caller: Caller<'_, HostState>,
             _fd: i32,
             iovs: i32,
             iovs_len: i32,
             nwritten: i32|
             -> wasmtime::Result<i32> {
                let mut output = Vec::new();
                for index in 0..iovs_len {
                    let ptr = read_u32(&mut caller, iovs + index * 8)? as i32;
                    let len = read_u32(&mut caller, iovs + index * 8 + 4)?;
                    output.extend(guest_bytes(&mut caller, ptr, len as i32)?);
                }
                let written = output.len() as u32;
                let output = String::from_utf8_lossy(&output);
                eprint!("{output}");
                log_line(&caller.data().log_path, &output)
                    .map_err(|error| wasmtime::format_err!("{error}"))?;
                write_u32(&mut caller, nwritten, written)?;
                Ok(0)
            },
        )
        .map_err(|e| e.to_string())?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "random_get",
            |mut caller: Caller<'_, HostState>, buf: i32, len: i32| -> wasmtime::Result<i32> {
                if len < 0 {
                    return Err(wasmtime::format_err!("guest length must be non-negative"));
                }
                write_guest_bytes(&mut caller, buf, &vec![0; len as usize])?;
                Ok(0)
            },
        )
        .map_err(|e| e.to_string())?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "poll_oneoff",
            |mut caller: Caller<'_, HostState>,
             _in: i32,
             _out: i32,
             _nsubscriptions: i32,
             nevents: i32|
             -> wasmtime::Result<i32> {
                write_u32(&mut caller, nevents, 0)?;
                Ok(0)
            },
        )
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn denied(capability: &'static str, operation: &'static str) -> wasmtime::Error {
    wasmtime::format_err!(
        "{}",
        CapabilityError::CapabilityDenied {
            capability,
            operation
        }
    )
}

fn read_u32(caller: &mut Caller<'_, HostState>, ptr: i32) -> wasmtime::Result<u32> {
    let bytes = guest_bytes(caller, ptr, 4)?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

fn write_u32(caller: &mut Caller<'_, HostState>, ptr: i32, value: u32) -> wasmtime::Result<()> {
    write_guest_bytes(caller, ptr, &value.to_le_bytes())
}

fn write_u64(caller: &mut Caller<'_, HostState>, ptr: i32, value: u64) -> wasmtime::Result<()> {
    write_guest_bytes(caller, ptr, &value.to_le_bytes())
}

fn write_guest_bytes(
    caller: &mut Caller<'_, HostState>,
    ptr: i32,
    bytes: &[u8],
) -> wasmtime::Result<()> {
    if ptr < 0 {
        return Err(wasmtime::format_err!("guest pointer must be non-negative"));
    }
    let memory = caller
        .get_export("memory")
        .and_then(|export| export.into_memory())
        .ok_or_else(|| wasmtime::format_err!("guest memory export not found"))?;
    memory
        .write(caller, ptr as usize, bytes)
        .map_err(|e| wasmtime::format_err!("{e}"))
}

fn guest_string(
    caller: &mut Caller<'_, HostState>,
    ptr: i32,
    len: i32,
) -> wasmtime::Result<String> {
    String::from_utf8(guest_bytes(caller, ptr, len)?).map_err(|e| wasmtime::format_err!("{e}"))
}

fn guest_bytes(
    caller: &mut Caller<'_, HostState>,
    ptr: i32,
    len: i32,
) -> wasmtime::Result<Vec<u8>> {
    if ptr < 0 || len < 0 {
        return Err(wasmtime::format_err!(
            "guest pointer and length must be non-negative"
        ));
    }
    let memory = caller
        .get_export("memory")
        .and_then(|export| export.into_memory())
        .ok_or_else(|| wasmtime::format_err!("guest memory export not found"))?;
    let mut bytes = vec![0; len as usize];
    memory
        .read(caller, ptr as usize, &mut bytes)
        .map_err(|e| wasmtime::format_err!("{e}"))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn module(engine: &Engine, wat: &str) -> Module {
        Module::new(engine, wat).unwrap()
    }

    fn host_state(network: bool, storage: Option<StorageCapability>) -> HostState {
        HostState {
            application_id: "test-application-id".into(),
            network: NetworkCapability::new(network),
            storage,
            secrets: SecretCapability::new(&[], RuntimeSecrets::new()).unwrap(),
            config: ConfigCapability::new(&[], RuntimeConfig::new()).unwrap(),
            limits: StoreLimitsBuilder::new().instances(1).build(),
            execution_fuel: None,
            requests: RequestLimiter::new(1),
            execution_units: Arc::new(AtomicUsize::new(0)),
            log_path: None,
        }
    }

    fn host_state_with_secrets(
        storage: Option<StorageCapability>,
        required: &[&str],
        values: &[(&str, &str)],
    ) -> HostState {
        let required = required
            .iter()
            .map(|name| name.to_string())
            .collect::<Vec<_>>();
        let values = values
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        HostState {
            application_id: "test-application-id".into(),
            network: NetworkCapability::new(false),
            storage,
            secrets: SecretCapability::new(&required, values).unwrap(),
            config: ConfigCapability::new(&[], RuntimeConfig::new()).unwrap(),
            limits: StoreLimitsBuilder::new().instances(1).build(),
            execution_fuel: None,
            requests: RequestLimiter::new(1),
            execution_units: Arc::new(AtomicUsize::new(0)),
            log_path: None,
        }
    }

    fn host_state_with_config(allowed: &[&str], values: &[(&str, &str)]) -> HostState {
        let allowed = allowed
            .iter()
            .map(|name| name.to_string())
            .collect::<Vec<_>>();
        let values = values
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        HostState {
            application_id: "test-application-id".into(),
            network: NetworkCapability::new(false),
            storage: None,
            secrets: SecretCapability::new(&[], RuntimeSecrets::new()).unwrap(),
            config: ConfigCapability::new(&allowed, values).unwrap(),
            limits: StoreLimitsBuilder::new().instances(1).build(),
            execution_fuel: None,
            requests: RequestLimiter::new(1),
            execution_units: Arc::new(AtomicUsize::new(0)),
            log_path: None,
        }
    }

    fn host_state_with_limits(
        memory_mb: u64,
        timeout_ms: u64,
        max_concurrent_requests: u32,
    ) -> HostState {
        let resources = app_manifest::Resources {
            memory_mb,
            timeout_ms,
            max_concurrent_requests,
        };
        let (limits, fuel, request_limit) = runtime_limits(Some(&resources)).unwrap();
        HostState {
            application_id: "test-application-id".into(),
            network: NetworkCapability::new(false),
            storage: None,
            secrets: SecretCapability::new(&[], RuntimeSecrets::new()).unwrap(),
            config: ConfigCapability::new(&[], RuntimeConfig::new()).unwrap(),
            limits,
            execution_fuel: fuel,
            requests: RequestLimiter::new(request_limit),
            execution_units: Arc::new(AtomicUsize::new(0)),
            log_path: None,
        }
    }

    fn manifest() -> Manifest {
        Manifest {
            name: "hello".into(),
            version: "0.1.0".into(),
            appport: None,
            runtime: "wasm".into(),
            artifact: app_manifest::Artifact {
                file: "app.wasm".into(),
                sha256: sha256(b"wasm"),
                size: 4,
            },
            http: Some(app_manifest::HttpCapability { listen: 8080 }),
            capabilities: app_manifest::ManifestCapabilities {
                network: false,
                filesystem: true,
            },
            storage: Some(app_manifest::Storage {
                mount: "/data".into(),
                path: ".app/data".into(),
            }),
            secrets: app_manifest::Secrets::default(),
            config: app_manifest::Config::default(),
            network: None,
            resources: None,
        }
    }

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("app-runtime-{name}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn manifest_artifact_is_loaded_relative_to_manifest_path() {
        let tempdir = temp_path("portable-artifact");
        let artifact = b"portable wasm";
        fs::write(tempdir.join("app.wasm"), artifact).unwrap();
        fs::write(
            tempdir.join("app.manifest.json"),
            format!(
                r#"{{
  "name": "hello",
  "version": "0.1.0",
  "runtime": "wasm",
  "artifact": {{
    "file": "app.wasm",
    "sha256": "{}",
    "size": {}
  }},
  "http": {{
    "listen": 8080
  }},
  "capabilities": {{
    "network": false,
    "filesystem": true
  }},
  "storage": {{
    "mount": "/data",
    "path": ".app/data"
  }}
}}
"#,
                sha256(artifact),
                artifact.len()
            ),
        )
        .unwrap();

        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(std::env::temp_dir()).unwrap();
        let result = load_manifest_artifact(&tempdir.join("app.manifest.json"));
        std::env::set_current_dir(original_dir).unwrap();

        let (manifest, wasm) = result.unwrap();
        assert_eq!(manifest.artifact.file, "app.wasm");
        assert_eq!(wasm, artifact);
    }

    #[test]
    fn manifest_artifact_hash_mismatch_stops_execution() {
        let tempdir = temp_path("hash-mismatch");
        fs::write(tempdir.join("app.wasm"), b"modified").unwrap();
        fs::write(
            tempdir.join("app.manifest.json"),
            format!(
                r#"{{
  "name": "hello",
  "version": "0.1.0",
  "runtime": "wasm",
  "artifact": {{
    "file": "app.wasm",
    "sha256": "{}",
    "size": 8
  }},
  "http": {{
    "listen": 8080
  }},
  "capabilities": {{
    "network": false,
    "filesystem": false
  }}
}}
"#,
                sha256(b"original")
            ),
        )
        .unwrap();

        let error = load_manifest_artifact(&tempdir.join("app.manifest.json")).unwrap_err();

        assert_eq!(error, "artifact hash mismatch");
    }

    #[test]
    fn explicit_state_directory_is_storage_root() {
        let project = temp_path("project-default-state");
        let state = temp_path("explicit-state");
        let storage = super::host_state(
            &project,
            Some(&state),
            StateProviderKind::Filesystem,
            "test-application-id",
            &manifest(),
            &RuntimeSecrets::new(),
        )
        .unwrap()
        .storage
        .unwrap();

        storage.write("/data/counter", b"explicit").unwrap();

        assert_eq!(
            fs::read_to_string(state.join("counter")).unwrap(),
            "explicit"
        );
        assert!(!project.join(".app/data/counter").exists());
    }

    #[test]
    fn default_state_directory_is_relative_to_runtime_context() {
        let project = temp_path("default-state");
        let storage = super::host_state(
            &project,
            None,
            StateProviderKind::Filesystem,
            "test-application-id",
            &manifest(),
            &RuntimeSecrets::new(),
        )
        .unwrap()
        .storage
        .unwrap();

        storage.write("/data/counter", b"default").unwrap();

        assert_eq!(
            fs::read_to_string(project.join(".app/data/counter")).unwrap(),
            "default"
        );
    }

    #[test]
    fn default_feltdb_state_directory_uses_application_id_under_home() {
        let home = temp_path("feltdb-home");

        assert_eq!(
            super::feltdb_state_root(&home, "sha256:abc123"),
            home.join(".appboundry")
                .join("state")
                .join("sha256")
                .join("abc123")
        );
    }

    #[test]
    fn network_false_denies_network_operation_at_host_boundary() {
        let engine = Engine::default();
        let module = module(
            &engine,
            r#"
            (module
              (import "app_capabilities" "network_connect" (func $network_connect))
              (func (export "handle_request")
                call $network_connect))
            "#,
        );

        let error = invoke(&engine, &module, host_state(false, None)).unwrap_err();

        assert!(
            error.contains("CapabilityDenied { capability: \"network\", operation: \"connect\" }"),
            "{error}"
        );
    }

    #[test]
    fn network_true_allows_network_operation_at_host_boundary() {
        let engine = Engine::default();
        let module = module(
            &engine,
            r#"
            (module
              (import "app_capabilities" "network_connect" (func $network_connect))
              (func (export "handle_request")
                call $network_connect))
            "#,
        );

        invoke(&engine, &module, host_state(true, None)).unwrap();
    }

    #[test]
    fn declared_network_destination_is_allowed_at_host_boundary() {
        let engine = Engine::default();
        let module = network_destination_module(&engine, "api.example.com");
        let mut host_state = host_state(false, None);
        host_state.network =
            NetworkCapability::with_destinations(true, &["api.example.com".into()]);

        invoke(&engine, &module, host_state).unwrap();
    }

    #[test]
    fn undeclared_network_destination_is_denied_at_host_boundary() {
        let engine = Engine::default();
        let module = network_destination_module(&engine, "other.example.com");
        let mut host_state = host_state(false, None);
        host_state.network =
            NetworkCapability::with_destinations(true, &["api.example.com".into()]);

        let error = invoke(&engine, &module, host_state).unwrap_err();

        assert!(
            error.contains("CapabilityDenied { capability: \"network\", operation: \"connect\" }"),
            "{error}"
        );
    }

    #[test]
    fn filesystem_false_denies_storage_operation_at_host_boundary() {
        let engine = Engine::default();
        let module = storage_write_module(&engine, "/data/counter", "value");

        let error = invoke(&engine, &module, host_state(false, None)).unwrap_err();

        assert!(
            error.contains("CapabilityDenied { capability: \"filesystem\", operation: \"write\" }"),
            "{error}"
        );
    }

    #[test]
    fn filesystem_true_allows_valid_storage_write() {
        let tempdir = temp_path("valid-write");
        let storage = StorageCapability::new("/data", &tempdir).unwrap();
        let engine = Engine::default();
        let module = storage_write_module(&engine, "/data/foo/bar.txt", "value");

        invoke(&engine, &module, host_state(false, Some(storage))).unwrap();

        assert_eq!(
            fs::read_to_string(tempdir.join("foo/bar.txt")).unwrap(),
            "value"
        );
    }

    #[test]
    fn storage_traversal_is_denied_at_host_boundary() {
        let tempdir = temp_path("traversal");
        let storage = StorageCapability::new("/data", &tempdir).unwrap();
        let engine = Engine::default();

        for path in ["/data/../secret", "/data/a/../../secret"] {
            let module = storage_write_module(&engine, path, "x");
            let error =
                invoke(&engine, &module, host_state(false, Some(storage.clone()))).unwrap_err();
            assert!(
                error.contains(
                    "CapabilityDenied { capability: \"filesystem\", operation: \"write\" }"
                ),
                "{error}"
            );
        }
    }

    #[test]
    fn storage_write_persists_across_runtime_restarts() {
        let tempdir = temp_path("persistence");
        let engine = Engine::default();
        let write = storage_write_module(&engine, "/data/counter", "persisted");
        let read = storage_read_equals_module(&engine, "/data/counter", "persisted");

        invoke(
            &engine,
            &write,
            host_state(
                false,
                Some(StorageCapability::new("/data", &tempdir).unwrap()),
            ),
        )
        .unwrap();
        invoke(
            &engine,
            &read,
            host_state(
                false,
                Some(StorageCapability::new("/data", &tempdir).unwrap()),
            ),
        )
        .unwrap();
    }

    #[test]
    fn disposable_execution_units_preserve_application_state_after_success_and_failure() {
        let tempdir = temp_path("recyclable-state");
        let log_path = temp_path("recyclable-logs").join("runtime.log");
        let application_id = "sha256:recyclable-test";
        let engine = Engine::default();
        let write = storage_write_module(&engine, "/data/counter", "42");
        let read = storage_read_equals_module(&engine, "/data/counter", "42");
        let crash = module(
            &engine,
            r#"
            (module
              (func (export "handle_request")
                unreachable))
            "#,
        );

        let mut healthy_state = host_state(
            false,
            Some(StorageCapability::new("/data", &tempdir).unwrap()),
        );
        healthy_state.application_id = application_id.into();
        healthy_state.log_path = Some(log_path.clone());

        invoke(&engine, &write, healthy_state.clone()).unwrap();
        assert_eq!(fs::read_to_string(tempdir.join("counter")).unwrap(), "42");
        invoke(&engine, &read, healthy_state.clone()).unwrap();
        assert_eq!(healthy_state.application_id, application_id);

        let mut recovery_state = host_state(
            false,
            Some(StorageCapability::new("/data", &tempdir).unwrap()),
        );
        recovery_state.application_id = application_id.into();
        recovery_state.log_path = Some(log_path.clone());

        let error = invoke(&engine, &crash, recovery_state.clone()).unwrap_err();
        assert!(error.contains("unreachable"), "{error}");
        invoke(&engine, &read, recovery_state.clone()).unwrap();
        assert_eq!(fs::read_to_string(tempdir.join("counter")).unwrap(), "42");
        assert_eq!(recovery_state.application_id, application_id);

        let logs = fs::read_to_string(log_path).unwrap();
        for expected in [
            "execution-unit sha256:recyclable-test#1 create",
            "execution-unit sha256:recyclable-test#1 execute",
            "execution-unit sha256:recyclable-test#1 complete",
            "execution-unit sha256:recyclable-test#1 dispose",
            "execution-unit sha256:recyclable-test#2 create",
            "execution-unit sha256:recyclable-test#2 execute",
            "execution-unit sha256:recyclable-test#2 complete",
            "execution-unit sha256:recyclable-test#2 dispose",
            "execution-unit sha256:recyclable-test#1 fail",
        ] {
            assert!(logs.contains(expected), "{expected}\n{logs}");
        }
    }

    #[test]
    fn declared_secret_is_available_at_host_boundary() {
        let engine = Engine::default();
        let module = secret_equals_module(&engine, "OPENAI_API_KEY", "test-secret-value");

        invoke(
            &engine,
            &module,
            host_state_with_secrets(
                None,
                &["OPENAI_API_KEY"],
                &[("OPENAI_API_KEY", "test-secret-value")],
            ),
        )
        .unwrap();
    }

    #[test]
    fn undeclared_secret_is_denied_at_host_boundary() {
        let engine = Engine::default();
        let module = secret_equals_module(&engine, "OTHER_SECRET", "value");

        let error = invoke(
            &engine,
            &module,
            host_state_with_secrets(None, &["OPENAI_API_KEY"], &[("OPENAI_API_KEY", "value")]),
        )
        .unwrap_err();

        assert!(
            error.contains("CapabilityDenied { capability: \"secret\", operation: \"read\" }"),
            "{error}"
        );
    }

    #[test]
    fn runtime_requires_declared_secret_values() {
        let mut manifest = manifest();
        manifest.secrets.required.push("OPENAI_API_KEY".into());

        let error = match super::host_state(
            &temp_path("secret-project"),
            None,
            StateProviderKind::Filesystem,
            "test-application-id",
            &manifest,
            &RuntimeSecrets::new(),
        ) {
            Ok(_) => panic!("host state should require declared secret values"),
            Err(error) => error,
        };

        assert_eq!(error, "required secret 'OPENAI_API_KEY' was not provided");
    }

    #[test]
    fn declared_config_is_available_at_host_boundary() {
        let engine = Engine::default();
        let module = config_equals_module(&engine, "LOG_LEVEL", "info");

        invoke(
            &engine,
            &module,
            host_state_with_config(&["LOG_LEVEL"], &[("LOG_LEVEL", "info")]),
        )
        .unwrap();
    }

    #[test]
    fn undeclared_config_is_denied_at_host_boundary() {
        let engine = Engine::default();
        let module = config_equals_module(&engine, "API_BASE_URL", "https://api.example.com");

        let error = invoke(
            &engine,
            &module,
            host_state_with_config(&["LOG_LEVEL"], &[]),
        )
        .unwrap_err();

        assert!(
            error.contains("CapabilityDenied { capability: \"config\", operation: \"read\" }"),
            "{error}"
        );
    }

    #[test]
    fn execution_timeout_is_deterministic() {
        let engine = engine(true).unwrap();
        let module = module(
            &engine,
            r#"
            (module
              (func (export "handle_request")
                (loop $forever
                  br $forever)))
            "#,
        );

        let error = invoke(&engine, &module, host_state_with_limits(1, 1, 1)).unwrap_err();

        assert_eq!(error, "wasm execution timed out");
    }

    #[test]
    fn execution_timeout_applies_across_host_capability_calls() {
        let engine = engine(true).unwrap();
        let module = module(
            &engine,
            r#"
            (module
              (import "app_capabilities" "network_connect" (func $network_connect))
              (func (export "handle_request")
                (loop $forever
                  call $network_connect
                  br $forever)))
            "#,
        );
        let mut host_state = host_state_with_limits(1, 1, 1);
        host_state.network = NetworkCapability::new(true);

        let error = invoke(&engine, &module, host_state).unwrap_err();

        assert_eq!(error, "wasm execution timed out");
    }

    #[test]
    fn memory_limit_is_enforced_at_instantiation() {
        let engine = engine(true).unwrap();
        let module = module(
            &engine,
            r#"
            (module
              (memory (export "memory") 17)
              (func (export "handle_request")))
            "#,
        );

        let error = invoke(&engine, &module, host_state_with_limits(1, 1000, 1)).unwrap_err();

        assert!(
            error.contains("forcing trap when growing memory"),
            "{error}"
        );
    }

    #[test]
    fn memory_limit_blocks_growth_through_host_capabilities() {
        let engine = engine(true).unwrap();
        let module = storage_write_module(&engine, "/data/counter", "value");
        let mut host_state = host_state_with_limits(1, 1000, 1);
        host_state.storage =
            Some(StorageCapability::new("/data", temp_path("limited-state")).unwrap());

        invoke(&engine, &module, host_state).unwrap();
    }

    #[test]
    fn request_concurrency_limit_is_enforced_before_execution() {
        let engine = engine(true).unwrap();
        let module = module(&engine, r#"(module (func (export "handle_request")))"#);
        let host_state = host_state_with_limits(1, 1000, 1);
        let _active = host_state.requests.enter().unwrap();

        let error = invoke(&engine, &module, host_state).unwrap_err();

        assert_eq!(error, "request concurrency limit exceeded");
    }

    #[test]
    fn runtime_log_path_uses_application_id_scope() {
        let home = temp_path("logs-home");
        let previous_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home);
        }

        let path = super::log_path("sha256:abc123").unwrap();

        if let Some(previous_home) = previous_home {
            unsafe {
                std::env::set_var("HOME", previous_home);
            }
        } else {
            unsafe {
                std::env::remove_var("HOME");
            }
        }
        assert_eq!(
            path,
            home.join(".appboundry")
                .join("runtime")
                .join("sha256")
                .join("abc123")
                .join("runtime.log")
        );
    }

    fn storage_write_module(engine: &Engine, path: &str, value: &str) -> Module {
        module(
            engine,
            &format!(
                r#"
                (module
                  (import "app_capabilities" "storage_write"
                    (func $storage_write (param i32 i32 i32 i32)))
                  (memory (export "memory") 1)
                  (data (i32.const 0) "{path}")
                  (data (i32.const 64) "{value}")
                  (func (export "handle_request")
                    i32.const 0
                    i32.const {path_len}
                    i32.const 64
                    i32.const {value_len}
                    call $storage_write))
                "#,
                path_len = path.len(),
                value_len = value.len()
            ),
        )
    }

    fn network_destination_module(engine: &Engine, destination: &str) -> Module {
        module(
            engine,
            &format!(
                r#"
                (module
                  (import "app_capabilities" "network_connect_to"
                    (func $network_connect_to (param i32 i32)))
                  (memory (export "memory") 1)
                  (data (i32.const 0) "{destination}")
                  (func (export "handle_request")
                    i32.const 0
                    i32.const {destination_len}
                    call $network_connect_to))
                "#,
                destination_len = destination.len()
            ),
        )
    }

    fn secret_equals_module(engine: &Engine, name: &str, expected: &str) -> Module {
        let checks = (0..expected.len())
            .map(|offset| {
                format!(
                    r#"
                    i32.const {}
                    i32.load8_u
                    i32.const {}
                    i32.load8_u
                    i32.ne
                    if
                      unreachable
                    end"#,
                    64 + offset,
                    128 + offset
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        module(
            engine,
            &format!(
                r#"
                (module
                  (import "app_capabilities" "get_secret"
                    (func $get_secret (param i32 i32 i32 i32) (result i32)))
                  (memory (export "memory") 1)
                  (data (i32.const 0) "{name}")
                  (data (i32.const 128) "{expected}")
                  (func (export "handle_request")
                    i32.const 0
                    i32.const {name_len}
                    i32.const 64
                    i32.const 128
                    call $get_secret
                    i32.const {expected_len}
                    i32.ne
                    if
                      unreachable
                    end
                    {checks}))
                "#,
                name_len = name.len(),
                expected = expected,
                expected_len = expected.len(),
                checks = checks
            ),
        )
    }

    fn config_equals_module(engine: &Engine, name: &str, expected: &str) -> Module {
        let checks = (0..expected.len())
            .map(|offset| {
                format!(
                    r#"
                    i32.const {}
                    i32.load8_u
                    i32.const {}
                    i32.load8_u
                    i32.ne
                    if
                      unreachable
                    end"#,
                    64 + offset,
                    128 + offset
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        module(
            engine,
            &format!(
                r#"
                (module
                  (import "app_capabilities" "get_config"
                    (func $get_config (param i32 i32 i32 i32) (result i32)))
                  (memory (export "memory") 1)
                  (data (i32.const 0) "{name}")
                  (data (i32.const 128) "{expected}")
                  (func (export "handle_request")
                    i32.const 0
                    i32.const {name_len}
                    i32.const 64
                    i32.const 128
                    call $get_config
                    i32.const {expected_len}
                    i32.ne
                    if
                      unreachable
                    end
                    {checks}))
                "#,
                name_len = name.len(),
                expected = expected,
                expected_len = expected.len(),
                checks = checks
            ),
        )
    }

    fn storage_read_equals_module(engine: &Engine, path: &str, value: &str) -> Module {
        module(
            engine,
            &format!(
                r#"
                (module
                  (import "app_capabilities" "storage_read_equals"
                    (func $storage_read_equals (param i32 i32 i32 i32) (result i32)))
                  (memory (export "memory") 1)
                  (data (i32.const 0) "{path}")
                  (data (i32.const 64) "{value}")
                  (func (export "handle_request")
                    i32.const 0
                    i32.const {path_len}
                    i32.const 64
                    i32.const {value_len}
                    call $storage_read_equals
                    i32.const 1
                    i32.ne
                    if
                      unreachable
                    end))
                "#,
                path_len = path.len(),
                value_len = value.len()
            ),
        )
    }
}
