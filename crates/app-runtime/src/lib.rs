use app_capabilities::{CapabilityError, NetworkCapability, StorageCapability};
use app_manifest::{Manifest, sha256};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use wasmtime::{Caller, Engine, Linker, Module, Store};

#[derive(Clone)]
struct HostState {
    network: NetworkCapability,
    storage: Option<StorageCapability>,
}

pub fn run(project: &Path) -> Result<(), String> {
    run_with_state(project, None)
}

pub fn run_with_state(project: &Path, state: Option<&Path>) -> Result<(), String> {
    run_from_manifest(&project.join("target/app.manifest.json"), project, state)
}

pub fn run_manifest(manifest_path: &Path, state: Option<&Path>) -> Result<(), String> {
    run_from_manifest(manifest_path, Path::new("."), state)
}

fn run_from_manifest(
    manifest_path: &Path,
    default_state_base: &Path,
    state: Option<&Path>,
) -> Result<(), String> {
    let (manifest, wasm) = load_manifest_artifact(manifest_path)?;
    let host_state = host_state(default_state_base, state, &manifest)?;
    let engine = Engine::default();
    let module = Module::new(&engine, wasm).map_err(|e| e.to_string())?;
    let http = manifest.http.ok_or("no HTTP endpoint declared")?;
    let listener = TcpListener::bind(("127.0.0.1", http.listen)).map_err(|e| e.to_string())?;
    println!(
        "{} listening on http://127.0.0.1:{}",
        manifest.name, http.listen
    );
    for stream in listener.incoming() {
        let mut stream = stream.map_err(|e| e.to_string())?;
        let mut request = [0; 1024];
        stream.read(&mut request).map_err(|e| e.to_string())?;
        invoke(&engine, &module, host_state.clone())?;
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 15\r\nConnection: close\r\n\r\nHello from WASM").map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn load_manifest_artifact(manifest_path: &Path) -> Result<(Manifest, Vec<u8>), String> {
    let manifest = Manifest::load(manifest_path)?;
    let manifest_dir = manifest_path
        .parent()
        .ok_or_else(|| format!("manifest path has no parent: {}", manifest_path.display()))?;
    let artifact_path = manifest_dir.join(&manifest.artifact.file);
    let wasm = fs::read(&artifact_path)
        .map_err(|e| format!("cannot read artifact {}: {e}", artifact_path.display()))?;
    if sha256(&wasm) != manifest.artifact.sha256 {
        return Err("artifact hash mismatch".into());
    }
    Ok((manifest, wasm))
}

fn host_state(
    default_state_base: &Path,
    state: Option<&Path>,
    manifest: &Manifest,
) -> Result<HostState, String> {
    let storage = match (&manifest.storage, manifest.capabilities.filesystem) {
        (Some(storage), true) => {
            let state_root = state
                .map(PathBuf::from)
                .unwrap_or_else(|| default_state_base.join(&storage.path));
            Some(StorageCapability::new(&storage.mount, state_root)?)
        }
        (None, false) => None,
        (None, true) => return Err("filesystem capability requires storage declaration".into()),
        (Some(_), false) => return Err("storage declaration requires filesystem capability".into()),
    };
    Ok(HostState {
        network: NetworkCapability::new(manifest.capabilities.network),
        storage,
    })
}

fn invoke(engine: &Engine, module: &Module, host_state: HostState) -> Result<(), String> {
    let mut linker = Linker::new(engine);
    add_host_functions(&mut linker)?;
    let mut store = Store::new(engine, host_state);
    let instance = linker
        .instantiate(&mut store, module)
        .map_err(runtime_error)?;
    if let Some(initialize) = instance.get_func(&mut store, "_initialize") {
        initialize
            .call(&mut store, &[], &mut [])
            .map_err(runtime_error)?;
    }
    if let Some(handle) = instance.get_func(&mut store, "handle_request") {
        handle
            .call(&mut store, &[], &mut [])
            .map_err(runtime_error)?;
    }
    Ok(())
}

fn runtime_error(error: wasmtime::Error) -> String {
    format!("{error:?}")
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
                eprint!("{}", String::from_utf8_lossy(&output));
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
            network: NetworkCapability::new(network),
            storage,
        }
    }

    fn manifest() -> Manifest {
        Manifest {
            name: "hello".into(),
            version: "0.1.0".into(),
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
        let storage = super::host_state(&project, Some(&state), &manifest())
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
        let storage = super::host_state(&project, None, &manifest())
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
