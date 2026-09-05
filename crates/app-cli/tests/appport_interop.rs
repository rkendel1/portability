use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

#[test]
fn sdk_appport_application_invokes_wasm_and_persists_state() {
    if !rust_wasm_target_available() {
        return;
    }
    let project = copy_example("hello");
    rewrite_listen(&project, free_port());
    app(&project).arg("build").assert_success();

    let target = project.join("target");
    let appport_manifest = target.join("appport.manifest.json");
    write_appport_manifest_with_sdk(&appport_manifest, None);
    let request = target.join("appport.request.json");
    fs::write(
        &request,
        r#"{
  "protocol": "appport/1",
  "type": "request",
  "requestId": "request-1",
  "capability": { "name": "hello.request", "version": 1 },
  "input": {}
}
"#,
    )
    .unwrap();

    let state = temp_project("appport-state");
    let response = successful_output(
        app(&project)
            .arg("appport-invoke")
            .arg(&appport_manifest)
            .arg(&request)
            .arg("--state")
            .arg(&state)
            .arg("--state-provider")
            .arg("filesystem"),
    );

    assert!(response.contains(r#""type":"response""#), "{response}");
    assert!(
        response.contains(r#""requestId":"request-1""#),
        "{response}"
    );
    assert!(response.contains(r#""ok":true"#), "{response}");
    assert_eq!(fs::read_to_string(state.join("counter")).unwrap(), "1");
    assert!(!target.join(".app/data/counter").exists());
}

#[test]
fn sdk_appport_application_uses_appboundry_lifecycle_and_keeps_state_across_restart() {
    if !rust_wasm_target_available() {
        return;
    }
    let project = copy_example("hello");
    let port = free_port();
    rewrite_listen(&project, port);
    app(&project).arg("build").assert_success();

    let target = project.join("target");
    let appport_manifest = target.join("appport.manifest.json");
    write_appport_manifest_with_sdk(&appport_manifest, Some(port));
    let home = temp_project("appport-lifecycle-home");

    let started = appport_start_with_home(&project, &appport_manifest, &home);
    assert!(started.contains("Started hello"), "{started}");
    assert!(
        started.contains("AppPort Application ID: com.example.hello"),
        "{started}"
    );
    let application_id = appboundry_application_id(&started);
    wait_for_http(port);
    let state = default_feltdb_state(&home, application_id);
    assert_eq!(felt_state(&state, application_id, "counter"), "MQ==");

    let status = appport_status_with_home(&project, &appport_manifest, &home);
    assert!(
        status.contains("AppPort Application ID: com.example.hello"),
        "{status}"
    );
    assert!(
        status.contains(&format!("AppBoundry Application ID: {application_id}")),
        "{status}"
    );
    assert!(status.contains("Execution: running"), "{status}");
    assert!(status.contains("Artifact: verified"), "{status}");
    assert!(status.contains("State Provider: feltdb"), "{status}");
    assert!(
        status.contains(&format!("State Scope: {application_id}")),
        "{status}"
    );
    assert!(
        status.contains(&format!("Endpoint: http://127.0.0.1:{port}")),
        "{status}"
    );

    let stopped = appport_stop_with_home(&project, &appport_manifest, &home);
    assert!(stopped.contains("Execution: stopped"), "{stopped}");
    wait_for_port_release(port);

    let restarted = appport_start_with_home(&project, &appport_manifest, &home);
    assert!(
        restarted.contains(&format!("AppBoundry Application ID: {application_id}")),
        "{restarted}"
    );
    wait_for_http(port);
    assert_eq!(felt_state(&state, application_id, "counter"), "MQ==");

    let logs = fs::read_to_string(runtime_log_path(&home, application_id)).unwrap();
    assert!(
        logs.contains(&format!("execution-unit {application_id}#1 create")),
        "{logs}"
    );
    assert!(
        logs.contains(&format!("execution-unit {application_id}#2 create")),
        "{logs}"
    );

    let stopped = appport_stop_with_home(&project, &appport_manifest, &home);
    assert!(stopped.contains("Execution: stopped"), "{stopped}");
    wait_for_port_release(port);
}

fn write_appport_manifest_with_sdk(path: &Path, listen: Option<u16>) {
    let output = Command::new("node")
        .current_dir(repository_root())
        .arg("--input-type=module")
        .arg("--eval")
        .arg(
            r#"
import fs from "node:fs";
import { createApplication, defineCapability, s } from "@appport/sdk";

const handleRequest = defineCapability({
  name: "hello.request",
  version: 1,
  input: s.object({}),
  output: s.object({}),
  authorization: ["filesystem", "storage", "resources"],
  handler() {
    return {};
  }
});
const application = createApplication({
  application: {
    id: "com.example.hello",
    name: "hello",
    version: "0.1.0"
  },
  capabilities: [handleRequest],
  transports: [{ kind: "appboundry-wasm" }],
  builtins: false
});
const manifest = application.manifest();
manifest.attributes = {
  appboundry: {
    runtime: "wasm",
    operation: { name: "hello.request", version: 1 },
    artifact: { file: "app.wasm" },
    storage: { mount: "/data", path: ".app/data" },
    resources: {
      memory_mb: 256,
      timeout_ms: 30000,
      max_concurrent_requests: 1
    }
  }
};
if (process.env.APPBOUNDRY_LISTEN) {
  manifest.attributes.appboundry.http = { listen: Number(process.env.APPBOUNDRY_LISTEN) };
}
fs.writeFileSync(process.env.APPPORT_MANIFEST, `${JSON.stringify(manifest, null, 2)}\n`);
"#,
        )
        .env("APPPORT_MANIFEST", path)
        .env(
            "APPBOUNDRY_LISTEN",
            listen.map(|port| port.to_string()).unwrap_or_default(),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn rust_wasm_target_available() -> bool {
    Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .is_some_and(|targets| {
            targets
                .lines()
                .any(|target| target == "wasm32-unknown-unknown")
        })
}

fn app(project: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_app"));
    command.current_dir(project);
    command
}

fn successful_output(command: &mut Command) -> String {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn appport_start_with_home(cwd: &Path, manifest: &Path, home: &Path) -> String {
    successful_output(
        app(cwd)
            .arg("appport-start")
            .arg(manifest)
            .env("HOME", home),
    )
}

fn appport_status_with_home(cwd: &Path, manifest: &Path, home: &Path) -> String {
    successful_output(
        app(cwd)
            .arg("appport-status")
            .arg(manifest)
            .env("HOME", home),
    )
}

fn appport_stop_with_home(cwd: &Path, manifest: &Path, home: &Path) -> String {
    successful_output(app(cwd).arg("appport-stop").arg(manifest).env("HOME", home))
}

fn appboundry_application_id(output: &str) -> &str {
    output
        .lines()
        .find_map(|line| line.strip_prefix("AppBoundry Application ID: "))
        .unwrap()
}

trait AssertCommand {
    fn assert_success(&mut self);
}

impl AssertCommand for Command {
    fn assert_success(&mut self) {
        let output = self.output().unwrap();
        assert!(
            output.status.success(),
            "stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn copy_example(name: &str) -> PathBuf {
    let source = repository_root().join("examples").join(name);
    let project = temp_project(name);
    copy_dir(&source, &project);
    project
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn temp_project(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("app-{name}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&path).unwrap();
    path
}

fn copy_dir(source: &Path, target: &Path) {
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let ty = entry.file_type().unwrap();
        let target_path = target.join(entry.file_name());
        if ty.is_dir() {
            fs::create_dir_all(&target_path).unwrap();
            copy_dir(&entry.path(), &target_path);
        } else {
            fs::copy(entry.path(), target_path).unwrap();
        }
    }
}

fn rewrite_listen(project: &Path, port: u16) {
    let app_toml = project.join("app.toml");
    let text = fs::read_to_string(&app_toml).unwrap();
    let rewritten = text
        .lines()
        .map(|line| {
            if line.starts_with("listen = ") {
                format!("listen = {port}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(app_toml, format!("{rewritten}\n")).unwrap();
}

fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn default_feltdb_state(home: &Path, application_id: &str) -> PathBuf {
    let mut path = home.join(".appboundry").join("state");
    for segment in application_id.split(':') {
        if !segment.is_empty() {
            path.push(segment);
        }
    }
    path
}

fn runtime_log_path(home: &Path, application_id: &str) -> PathBuf {
    let mut path = home.join(".appboundry").join("runtime");
    for segment in application_id.split(':') {
        if !segment.is_empty() {
            path.push(segment);
        }
    }
    path.join("runtime.log")
}

fn felt_state(state: &Path, application_id: &str, key: &str) -> String {
    let bridge =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../app-capabilities/feltdb-state-provider.mjs");
    let output = Command::new("node")
        .arg(bridge)
        .arg("read")
        .arg(state)
        .arg(application_id)
        .arg(key)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn wait_for_http(port: u16) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
            write!(
                stream,
                "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).ok();
            if response.contains("Hello from WASM") {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for http://127.0.0.1:{port}");
}

fn wait_for_port_release(port: u16) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for port {port} to be released");
}
