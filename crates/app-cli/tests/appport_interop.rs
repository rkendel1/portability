use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;

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
    write_appport_manifest_with_sdk(&appport_manifest);
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

fn write_appport_manifest_with_sdk(path: &Path) {
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
    artifact: { file: "app.wasm" },
    storage: { mount: "/data", path: ".app/data" },
    resources: {
      memory_mb: 256,
      timeout_ms: 30000,
      max_concurrent_requests: 1
    }
  }
};
fs.writeFileSync(process.env.APPPORT_MANIFEST, `${JSON.stringify(manifest, null, 2)}\n`);
"#,
        )
        .env("APPPORT_MANIFEST", path)
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
