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
        let manifest = fs::read_to_string(project.join("target/app.manifest.json")).unwrap();
        assert!(manifest.contains(r#""file": "app.wasm""#), "{manifest}");
        let built_inspect = inspect(&project);
        assert!(
            built_inspect.contains("  Integrity: verified"),
            "{built_inspect}"
        );
        let built_id = application_id(&built_inspect);
        app(&project).arg("build").assert_success();
        let rebuilt_inspect = inspect(&project);
        assert_eq!(application_id(&rebuilt_inspect), built_id);

        let artifact = temp_project(&format!("{example}-artifact"));
        fs::copy(project.join("target/app.wasm"), artifact.join("app.wasm")).unwrap();
        fs::copy(
            project.join("target/app.manifest.json"),
            artifact.join("app.manifest.json"),
        )
        .unwrap();
        let relocated_inspect =
            inspect_manifest(&std::env::temp_dir(), &artifact.join("app.manifest.json"));
        assert!(
            relocated_inspect.contains("  Integrity: verified"),
            "{relocated_inspect}"
        );
        assert_eq!(application_id(&relocated_inspect), built_id);

        let state_a = temp_project(&format!("{example}-state-a"));
        let state_b = temp_project(&format!("{example}-state-b"));
        let feltdb_state_a = temp_project(&format!("{example}-feltdb-state-a"));
        let feltdb_state_b = temp_project(&format!("{example}-feltdb-state-b"));
        let default_home = temp_project(&format!("{example}-default-home"));
        let default_state_root = default_feltdb_state(&default_home, built_id);

        let mut portable = run_app_manifest_with_state(
            &std::env::temp_dir(),
            &artifact.join("app.manifest.json"),
            &state_a,
        );
        wait_for_listen(port, &mut portable);
        wait_for_http(port);
        stop_app(port, portable);
        assert_eq!(fs::read_to_string(state_a.join("counter")).unwrap(), "1");
        assert!(!artifact.join(".app/data/counter").exists());

        let relocated_inspect_again =
            inspect_manifest(&std::env::temp_dir(), &artifact.join("app.manifest.json"));
        assert_eq!(application_id(&relocated_inspect_again), built_id);
        let mut portable = run_app_manifest_with_state(
            &std::env::temp_dir(),
            &artifact.join("app.manifest.json"),
            &state_b,
        );
        wait_for_listen(port, &mut portable);
        wait_for_http(port);
        stop_app(port, portable);
        assert_eq!(fs::read_to_string(state_b.join("counter")).unwrap(), "1");
        assert_eq!(fs::read_to_string(state_a.join("counter")).unwrap(), "1");
        assert!(!artifact.join(".app/data/counter").exists());

        let mut feltdb = run_app_manifest_with_state_provider(
            &std::env::temp_dir(),
            &artifact.join("app.manifest.json"),
            &feltdb_state_a,
            "feltdb",
        );
        wait_for_listen(port, &mut feltdb);
        wait_for_http(port);
        stop_app(port, feltdb);
        assert_eq!(felt_state(&feltdb_state_a, built_id, "counter"), "MQ==");
        assert!(!artifact.join(".app/data/counter").exists());

        let relocated_inspect_after_feltdb =
            inspect_manifest(&std::env::temp_dir(), &artifact.join("app.manifest.json"));
        assert_eq!(application_id(&relocated_inspect_after_feltdb), built_id);
        let mut feltdb = run_app_manifest_with_state_provider(
            &std::env::temp_dir(),
            &artifact.join("app.manifest.json"),
            &feltdb_state_a,
            "feltdb",
        );
        wait_for_listen(port, &mut feltdb);
        wait_for_http(port);
        stop_app(port, feltdb);
        assert_eq!(felt_state(&feltdb_state_a, built_id, "counter"), "MQ==");
        assert!(!artifact.join(".app/data/counter").exists());

        let mut isolated_feltdb = run_app_manifest_with_state_provider(
            &std::env::temp_dir(),
            &artifact.join("app.manifest.json"),
            &feltdb_state_b,
            "feltdb",
        );
        wait_for_listen(port, &mut isolated_feltdb);
        wait_for_http(port);
        stop_app(port, isolated_feltdb);
        assert_eq!(felt_state(&feltdb_state_b, built_id, "counter"), "MQ==");
        assert_eq!(felt_state(&feltdb_state_a, built_id, "counter"), "MQ==");
        assert!(!artifact.join(".app/data/counter").exists());

        let mut first = run_app_manifest_with_home(
            &std::env::temp_dir(),
            &artifact.join("app.manifest.json"),
            &default_home,
        );
        wait_for_listen(port, &mut first);
        wait_for_http(port);
        stop_app(port, first);
        assert_eq!(felt_state(&default_state_root, built_id, "counter"), "MQ==");
        assert!(!artifact.join(".app/data/counter").exists());

        let mut second = run_app_manifest_with_home(
            &std::env::temp_dir(),
            &artifact.join("app.manifest.json"),
            &default_home,
        );
        wait_for_listen(port, &mut second);
        wait_for_http(port);
        stop_app(port, second);
        assert_eq!(felt_state(&default_state_root, built_id, "counter"), "MQ==");
        assert!(!artifact.join(".app/data/counter").exists());

        let lifecycle_home = temp_project(&format!("{example}-lifecycle-home"));
        let lifecycle_state = default_feltdb_state(&lifecycle_home, built_id);
        let started = start_app_manifest_with_home(
            &std::env::temp_dir(),
            &artifact.join("app.manifest.json"),
            &lifecycle_home,
        );
        assert!(started.contains("Started hello"), "{started}");
        assert!(started.contains(built_id), "{started}");
        wait_for_http(port);

        let moved_artifact = temp_project(&format!("{example}-moved-artifact"));
        fs::copy(artifact.join("app.wasm"), moved_artifact.join("app.wasm")).unwrap();
        fs::copy(
            artifact.join("app.manifest.json"),
            moved_artifact.join("app.manifest.json"),
        )
        .unwrap();
        let moved_inspect = inspect_manifest(
            &std::env::temp_dir(),
            &moved_artifact.join("app.manifest.json"),
        );
        assert_eq!(application_id(&moved_inspect), built_id);

        let status = status_manifest_with_home(
            &std::env::temp_dir(),
            &moved_artifact.join("app.manifest.json"),
            &lifecycle_home,
        );
        assert!(status.contains("Status:       running"), "{status}");
        assert!(
            status.contains(&format!("Endpoint:     http://127.0.0.1:{port}")),
            "{status}"
        );
        assert!(status.contains("State:        FeltDB"), "{status}");
        assert!(status.contains("Artifact:     verified"), "{status}");
        wait_for_http(port);
        assert_eq!(felt_state(&lifecycle_state, built_id, "counter"), "MQ==");

        let duplicate = start_app_manifest_with_home_failure(
            &std::env::temp_dir(),
            &moved_artifact.join("app.manifest.json"),
            &lifecycle_home,
        );
        assert!(duplicate.contains("already running"), "{duplicate}");

        let stopped = stop_manifest_with_home(
            &std::env::temp_dir(),
            &moved_artifact.join("app.manifest.json"),
            &lifecycle_home,
        );
        assert!(stopped.contains("Status:       stopped"), "{stopped}");
        wait_for_port_release(port);

        let restarted = start_app_manifest_with_home(
            &std::env::temp_dir(),
            &moved_artifact.join("app.manifest.json"),
            &lifecycle_home,
        );
        assert!(restarted.contains(built_id), "{restarted}");
        wait_for_http(port);
        let logs = logs_manifest_with_home(
            &std::env::temp_dir(),
            &moved_artifact.join("app.manifest.json"),
            &lifecycle_home,
        );
        assert!(
            logs.contains(&format!("{example} listening on http://127.0.0.1:{port}")),
            "{logs}"
        );
        stop_manifest_with_home(
            &std::env::temp_dir(),
            &moved_artifact.join("app.manifest.json"),
            &lifecycle_home,
        );
        wait_for_port_release(port);
        assert_eq!(felt_state(&lifecycle_state, built_id, "counter"), "MQ==");

        let lifecycle_fs_state = temp_project(&format!("{example}-lifecycle-fs-state"));
        let lifecycle_fs_home = temp_project(&format!("{example}-lifecycle-fs-home"));
        let fs_started = start_app_manifest_with_state_provider_and_home(
            &std::env::temp_dir(),
            &moved_artifact.join("app.manifest.json"),
            &lifecycle_fs_state,
            &lifecycle_fs_home,
            "filesystem",
        );
        assert!(fs_started.contains("Started hello"), "{fs_started}");
        wait_for_http(port);
        stop_manifest_with_home(
            &std::env::temp_dir(),
            &moved_artifact.join("app.manifest.json"),
            &lifecycle_fs_home,
        );
        wait_for_port_release(port);
        assert_eq!(
            fs::read_to_string(lifecycle_fs_state.join("counter")).unwrap(),
            "1"
        );

        let tampered = temp_project(&format!("{example}-tampered-artifact"));
        fs::copy(
            moved_artifact.join("app.manifest.json"),
            tampered.join("app.manifest.json"),
        )
        .unwrap();
        fs::write(tampered.join("app.wasm"), b"tampered").unwrap();
        let tampered_start =
            start_app_manifest_failure(&std::env::temp_dir(), &tampered.join("app.manifest.json"));
        assert!(
            tampered_start.contains("artifact hash mismatch"),
            "{tampered_start}"
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

#[test]
fn rust_secret_is_injected_only_at_runtime() {
    if !toolchain_available("rust") {
        return;
    }
    let project = temp_project("rust-runtime-secret");
    let port = free_port();
    write_secret_project(&project, port);
    app(&project).arg("build").assert_success();
    let inspect_output = inspect(&project);
    let built_id = application_id(&inspect_output).to_string();
    let manifest = fs::read_to_string(project.join("target/app.manifest.json")).unwrap();
    let secret_value = "runtime-only-secret";

    assert!(manifest.contains("OPENAI_API_KEY"), "{manifest}");
    assert!(!manifest.contains(secret_value), "{manifest}");
    assert!(
        !fs::read(project.join("target/app.wasm"))
            .unwrap()
            .windows(secret_value.len())
            .any(|window| window == secret_value.as_bytes())
    );
    assert!(!built_id.contains(secret_value), "{built_id}");

    let missing_flag = failed_output(app(&project).arg("run").env("OPENAI_API_KEY", secret_value));
    assert!(
        missing_flag.contains("required secret 'OPENAI_API_KEY' was not provided"),
        "{missing_flag}"
    );

    let missing_env = failed_output(
        app(&project)
            .arg("run")
            .arg("--secret")
            .arg("OPENAI_API_KEY")
            .env_remove("OPENAI_API_KEY"),
    );
    assert!(
        missing_env.contains("secret OPENAI_API_KEY requested but environment variable is not set"),
        "{missing_env}"
    );

    let lifecycle_home = temp_project("rust-secret-lifecycle-home");
    let started = successful_output(
        app(&project)
            .arg("start")
            .arg("--secret")
            .arg("OPENAI_API_KEY")
            .env("OPENAI_API_KEY", secret_value)
            .env("HOME", &lifecycle_home),
    );
    assert!(started.contains(&built_id), "{started}");
    wait_for_http(port);

    let record = fs::read_to_string(runtime_record_path(&lifecycle_home, &built_id)).unwrap();
    assert!(!record.contains(secret_value), "{record}");

    let status = status_manifest_with_home(
        &project,
        &project.join("target/app.manifest.json"),
        &lifecycle_home,
    );
    assert!(status.contains("Status:       running"), "{status}");
    assert!(!status.contains(secret_value), "{status}");

    let stopped = stop_manifest_with_home(
        &project,
        &project.join("target/app.manifest.json"),
        &lifecycle_home,
    );
    assert!(stopped.contains("Status:       stopped"), "{stopped}");
    wait_for_port_release(port);
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

fn write_secret_project(project: &Path, port: u16) {
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(
        project.join("app.toml"),
        format!(
            r#"name = "secret-app"
version = "0.1.0"
[build]
source = "src"
entry = "src/main.rs"
[runtime]
kind = "wasm"
[http]
listen = {port}
[capabilities]
network = false
filesystem = false
[secrets]
required = ["OPENAI_API_KEY"]
"#
        ),
    )
    .unwrap();
    fs::write(
        project.join("Cargo.toml"),
        r#"[package]
name = "secret-app"
version = "0.1.0"
edition = "2024"
autobins = false

[lib]
path = "src/main.rs"
crate-type = ["cdylib"]
"#,
    )
    .unwrap();
    fs::write(
        project.join("src/main.rs"),
        r#"#[link(wasm_import_module = "app_capabilities")]
unsafe extern "C" {
    fn get_secret(name_ptr: *const u8, name_len: usize, value_ptr: *mut u8, value_len: usize) -> usize;
}

#[unsafe(no_mangle)]
pub extern "C" fn handle_request() {
    const NAME: &[u8] = b"OPENAI_API_KEY";
    let mut value = [0_u8; 128];
    unsafe {
        get_secret(NAME.as_ptr(), NAME.len(), value.as_mut_ptr(), value.len());
    }
}
"#,
    )
    .unwrap();
}

fn app(project: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_app"));
    command.current_dir(project);
    command
}

fn inspect(project: &Path) -> String {
    successful_output(app(project).arg("inspect"))
}

fn inspect_manifest(cwd: &Path, manifest: &Path) -> String {
    successful_output(app(cwd).arg("inspect").arg(manifest))
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

fn application_id(inspect_output: &str) -> &str {
    inspect_output
        .lines()
        .skip_while(|line| *line != "Application ID:")
        .nth(1)
        .expect("inspect output should include Application ID")
        .trim()
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

fn run_app_manifest_with_home(cwd: &Path, manifest: &Path, home: &Path) -> Child {
    app(cwd)
        .arg("run")
        .arg(manifest)
        .env("HOME", home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn default_feltdb_state(home: &Path, application_id: &str) -> std::path::PathBuf {
    let mut path = home.join(".appboundry").join("state");
    for segment in application_id.split(':') {
        if !segment.is_empty() {
            path.push(segment);
        }
    }
    path
}

fn runtime_record_path(home: &Path, application_id: &str) -> std::path::PathBuf {
    let mut path = home.join(".appboundry").join("runtime");
    for segment in application_id.split(':') {
        if !segment.is_empty() {
            path.push(segment);
        }
    }
    path.join("runtime.json")
}

fn run_app_manifest_with_state(cwd: &Path, manifest: &Path, state: &Path) -> Child {
    run_app_manifest_with_state_provider(cwd, manifest, state, "filesystem")
}

fn run_app_manifest_with_state_provider(
    cwd: &Path,
    manifest: &Path,
    state: &Path,
    provider: &str,
) -> Child {
    app(cwd)
        .arg("run")
        .arg(manifest)
        .arg("--state")
        .arg(state)
        .arg("--state-provider")
        .arg(provider)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn start_app_manifest_with_home(cwd: &Path, manifest: &Path, home: &Path) -> String {
    successful_output(app(cwd).arg("start").arg(manifest).env("HOME", home))
}

fn start_app_manifest_with_home_failure(cwd: &Path, manifest: &Path, home: &Path) -> String {
    failed_output(app(cwd).arg("start").arg(manifest).env("HOME", home))
}

fn start_app_manifest_with_state_provider_and_home(
    cwd: &Path,
    manifest: &Path,
    state: &Path,
    home: &Path,
    provider: &str,
) -> String {
    successful_output(
        app(cwd)
            .arg("start")
            .arg(manifest)
            .arg("--state")
            .arg(state)
            .arg("--state-provider")
            .arg(provider)
            .env("HOME", home),
    )
}

fn start_app_manifest_failure(cwd: &Path, manifest: &Path) -> String {
    failed_output(app(cwd).arg("start").arg(manifest))
}

fn status_manifest_with_home(cwd: &Path, manifest: &Path, home: &Path) -> String {
    successful_output(app(cwd).arg("status").arg(manifest).env("HOME", home))
}

fn stop_manifest_with_home(cwd: &Path, manifest: &Path, home: &Path) -> String {
    successful_output(app(cwd).arg("stop").arg(manifest).env("HOME", home))
}

fn logs_manifest_with_home(cwd: &Path, manifest: &Path, home: &Path) -> String {
    successful_output(app(cwd).arg("logs").arg(manifest).env("HOME", home))
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

fn failed_output(command: &mut Command) -> String {
    let output = command.output().unwrap();
    assert!(
        !output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn stop_app(port: u16, mut child: Child) {
    child.kill().ok();
    child.wait().ok();
    wait_for_port_release(port);
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
        Ok(Ok(line)) => panic!(
            "unexpected app run output before listening: {line}\nstderr:\n{}",
            child_stderr(child)
        ),
        Ok(Err(error)) => panic!("failed to read app run output: {error}"),
        Err(_) => panic!("timed out waiting for app run to listen on {port}"),
    }
}

fn child_stderr(child: &mut Child) -> String {
    if child.try_wait().ok().flatten().is_none() {
        return "<still running>".into();
    }
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        pipe.read_to_string(&mut stderr).ok();
    }
    stderr
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
