use app_capabilities::LocalDirectoryStateProvider;
use app_manifest::{Manifest, sha256};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use wasmtime::{Engine, Instance, Module, Store};

pub fn run(project: &Path) -> Result<(), String> {
    let target = project.join("target");
    let manifest = Manifest::load(&target.join("app.manifest.json"))?;
    let wasm = fs::read(target.join(&manifest.artifact.path)).map_err(|e| e.to_string())?;
    if sha256(&wasm) != manifest.artifact.sha256 {
        return Err("artifact hash does not match manifest".into());
    }
    if let Some(state) = &manifest.state {
        LocalDirectoryStateProvider::new(project.join(&state.path))?;
    }
    let engine = Engine::default();
    let module = Module::new(&engine, wasm).map_err(|e| e.to_string())?;
    let http = manifest
        .capabilities
        .http
        .ok_or("no HTTP endpoint declared")?;
    let listener = TcpListener::bind(("127.0.0.1", http.listen)).map_err(|e| e.to_string())?;
    println!(
        "{} listening on http://127.0.0.1:{}",
        manifest.name, http.listen
    );
    for stream in listener.incoming() {
        let mut stream = stream.map_err(|e| e.to_string())?;
        let mut request = [0; 1024];
        stream.read(&mut request).map_err(|e| e.to_string())?;
        invoke(&engine, &module)?;
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 15\r\nConnection: close\r\n\r\nHello from WASM").map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn invoke(engine: &Engine, module: &Module) -> Result<(), String> {
    let mut store = Store::new(engine, ());
    let instance = Instance::new(&mut store, module, &[]).map_err(|e| e.to_string())?;
    if let Some(handle) = instance.get_func(&mut store, "handle_request") {
        handle
            .call(&mut store, &[], &mut [])
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}
