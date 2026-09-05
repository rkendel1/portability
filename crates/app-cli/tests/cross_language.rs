use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn rust_and_go_examples_build_run_and_persist_state() {
    let examples = [
        ("hello", toolchain_available("rust")),
        ("hello-go", toolchain_available("go")),
    ];
    let mut exercised = 0;

    for (example, available) in examples {
        if !available {
            continue;
        }
        exercised += 1;
        let project = copy_example(example);
        let port = free_port();
        rewrite_listen(&project, port);

        app(&project).arg("build").assert_success();
        assert!(project.join("target/app.wasm").is_file());
        assert!(project.join("target/app.manifest.json").is_file());

        let mut first = run_app(&project);
        wait_for_listen(port, &mut first);
        assert!(http_get(port, "/").contains("Hello from WASM"));
        first.kill().ok();
        first.wait().ok();
        assert_eq!(
            fs::read_to_string(project.join(".app/data/counter")).unwrap(),
            "1"
        );

        let mut second = run_app(&project);
        wait_for_listen(port, &mut second);
        assert!(http_get(port, "/").contains("Hello from WASM"));
        second.kill().ok();
        second.wait().ok();
        assert_eq!(
            fs::read_to_string(project.join(".app/data/counter")).unwrap(),
            "1"
        );
    }

    assert!(
        exercised > 0,
        "no WASM application toolchains were available"
    );
}

#[test]
fn go_network_denial_matches_runtime_capability_error() {
    if !toolchain_available("go") {
        return;
    }
    let project = temp_project("go-network-denied");
    fs::write(
        project.join("app.toml"),
        format!(
            r#"name = "go-network-denied"
version = "0.1.0"
[build]
source = "."
language = "go"
toolchain = "go"
entry = "main.go"
target = "wasm"
[runtime]
kind = "wasm"
[http]
listen = {}
[capabilities]
network = false
filesystem = false
"#,
            free_port()
        ),
    )
    .unwrap();
    fs::write(
        project.join("go.mod"),
        "module go-network-denied\n\ngo 1.24\n",
    )
    .unwrap();
    fs::write(
        project.join("main.go"),
        r#"package main

//go:wasmimport app_capabilities network_connect
func networkConnect()

//go:wasmexport handle_request
func handleRequest() {
	networkConnect()
}

func main() {}
"#,
    )
    .unwrap();

    app(&project).arg("build").assert_success();
    let port = listen_port(&project);
    let mut child = run_app(&project);
    wait_for_listen(port, &mut child);
    let _ = http_get(port, "/");
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stderr.contains("CapabilityDenied { capability: \"network\", operation: \"connect\" }"),
        "{stderr}"
    );
}

fn toolchain_available(language: &str) -> bool {
    match language {
        "rust" => Command::new("rustup")
            .args(["target", "list", "--installed"])
            .output()
            .ok()
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .is_some_and(|targets| {
                targets
                    .lines()
                    .any(|target| target == "wasm32-unknown-unknown")
            }),
        "go" => Command::new("go")
            .arg("version")
            .output()
            .is_ok_and(|output| output.status.success()),
        _ => false,
    }
}

fn app(project: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_app"));
    command.current_dir(project);
    command
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

fn copy_example(name: &str) -> std::path::PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(name);
    let project = temp_project(name);
    copy_dir(&source, &project);
    project
}

fn temp_project(name: &str) -> std::path::PathBuf {
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

fn listen_port(project: &Path) -> u16 {
    fs::read_to_string(project.join("app.toml"))
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("listen = "))
        .unwrap()
        .parse()
        .unwrap()
}

fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn run_app(project: &Path) -> Child {
    app(project)
        .arg("run")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn wait_for_listen(port: u16, child: &mut Child) {
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
        tx.send(result).ok();
    });

    match rx.recv_timeout(Duration::from_secs(60)) {
        Ok(Ok(line)) if line.contains(&format!("listening on http://127.0.0.1:{port}")) => {}
        Ok(Ok(line)) => panic!("unexpected app run output before listening: {line}"),
        Ok(Err(error)) => panic!("failed to read app run output: {error}"),
        Err(_) => panic!("timed out waiting for app run to listen on {port}"),
    }
}

fn http_get(port: u16, path: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).ok();
    response
}
