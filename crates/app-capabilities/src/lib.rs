use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

pub trait Capability {
    fn capability(&self) -> &'static str;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityError {
    CapabilityDenied {
        capability: &'static str,
        operation: &'static str,
    },
    InvalidPath(String),
    Host(String),
}

impl fmt::Display for CapabilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CapabilityError::CapabilityDenied {
                capability,
                operation,
            } => write!(
                f,
                "CapabilityDenied {{ capability: \"{capability}\", operation: \"{operation}\" }}"
            ),
            CapabilityError::InvalidPath(message) | CapabilityError::Host(message) => {
                f.write_str(message)
            }
        }
    }
}

impl From<CapabilityError> for String {
    fn from(error: CapabilityError) -> Self {
        error.to_string()
    }
}

#[derive(Clone)]
pub struct NetworkCapability {
    allowed: bool,
}

impl NetworkCapability {
    pub fn new(allowed: bool) -> Self {
        Self { allowed }
    }

    pub fn connect(&self) -> Result<(), CapabilityError> {
        if self.allowed {
            Ok(())
        } else {
            Err(CapabilityError::CapabilityDenied {
                capability: "network",
                operation: "connect",
            })
        }
    }
}

impl Capability for NetworkCapability {
    fn capability(&self) -> &'static str {
        "network"
    }
}

#[derive(Clone)]
pub struct SecretCapability {
    declared: BTreeSet<String>,
    values: BTreeMap<String, String>,
}

impl SecretCapability {
    pub fn new(
        required: &[String],
        values: BTreeMap<String, String>,
    ) -> Result<Self, CapabilityError> {
        let declared = required.iter().cloned().collect::<BTreeSet<_>>();
        for name in values.keys() {
            if !declared.contains(name) {
                return Err(CapabilityError::CapabilityDenied {
                    capability: "secret",
                    operation: "read",
                });
            }
        }
        for name in &declared {
            if !values.contains_key(name) {
                return Err(CapabilityError::Host(format!(
                    "required secret '{name}' was not provided"
                )));
            }
        }
        Ok(Self { declared, values })
    }

    pub fn get(&self, name: &str) -> Result<&str, CapabilityError> {
        if !self.declared.contains(name) {
            return Err(CapabilityError::CapabilityDenied {
                capability: "secret",
                operation: "read",
            });
        }
        self.values.get(name).map(String::as_str).ok_or_else(|| {
            CapabilityError::Host(format!("required secret '{name}' was not provided"))
        })
    }
}

impl Capability for SecretCapability {
    fn capability(&self) -> &'static str {
        "secret"
    }
}

pub trait StateProvider: Send + Sync {
    fn read(&self, path: &str) -> Result<Vec<u8>, String>;
    fn write(&self, path: &str, value: &[u8]) -> Result<(), String>;
    fn delete(&self, path: &str) -> Result<(), String>;
    fn list(&self, path: &str) -> Result<Vec<String>, String>;
}

#[derive(Clone)]
pub struct LocalDirectoryStateProvider {
    root: PathBuf,
}

impl LocalDirectoryStateProvider {
    pub fn new(root: impl AsRef<Path>) -> Result<Self, String> {
        fs::create_dir_all(root.as_ref()).map_err(|e| e.to_string())?;
        Ok(Self {
            root: root.as_ref().canonicalize().map_err(|e| e.to_string())?,
        })
    }
    fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        Ok(resolve_relative_path(path, &self.root)?)
    }
}

impl StateProvider for LocalDirectoryStateProvider {
    fn read(&self, path: &str) -> Result<Vec<u8>, String> {
        fs::read(self.resolve(path)?).map_err(|e| e.to_string())
    }

    fn write(&self, path: &str, value: &[u8]) -> Result<(), String> {
        let path = self.resolve(path)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::write(path, value).map_err(|e| e.to_string())
    }
    fn delete(&self, path: &str) -> Result<(), String> {
        fs::remove_file(self.resolve(path)?).map_err(|e| e.to_string())
    }
    fn list(&self, path: &str) -> Result<Vec<String>, String> {
        fs::read_dir(self.resolve(path)?)
            .map_err(|e| e.to_string())?
            .map(|e| {
                e.map(|e| e.file_name().to_string_lossy().into_owned())
                    .map_err(|e| e.to_string())
            })
            .collect()
    }
}

#[derive(Clone)]
pub struct FeltDBStateProvider {
    store: PathBuf,
    namespace: String,
}

impl FeltDBStateProvider {
    pub fn new(store: impl AsRef<Path>, application_id: impl Into<String>) -> Result<Self, String> {
        fs::create_dir_all(store.as_ref()).map_err(|e| e.to_string())?;
        let namespace = application_id.into();
        if namespace.is_empty() {
            return Err("FeltDB namespace requires an Application ID".into());
        }
        Ok(Self {
            store: store.as_ref().to_path_buf(),
            namespace,
        })
    }

    fn key(path: &str) -> Result<&str, String> {
        reject_escaping_path(path)?;
        Ok(path)
    }

    fn run(&self, operation: &str, key: &str, value: Option<&[u8]>) -> Result<Vec<u8>, String> {
        let bridge = Path::new(env!("CARGO_MANIFEST_DIR")).join("feltdb-state-provider.mjs");
        let mut command = Command::new("node");
        command
            .arg(bridge)
            .arg(operation)
            .arg(&self.store)
            .arg(&self.namespace)
            .arg(key);
        if let Some(value) = value {
            command.arg(base64_encode(value));
        }
        let output = command.output().map_err(|e| {
            format!("failed to run FeltDB state provider; is Node.js available? {e}")
        })?;
        if output.status.success() {
            Ok(output.stdout)
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            Err(if stderr.is_empty() {
                format!("FeltDB state provider failed with {}", output.status)
            } else {
                stderr
            })
        }
    }
}

impl StateProvider for FeltDBStateProvider {
    fn read(&self, path: &str) -> Result<Vec<u8>, String> {
        let encoded = self.run("read", Self::key(path)?, None)?;
        base64_decode(std::str::from_utf8(&encoded).map_err(|e| e.to_string())?)
    }

    fn write(&self, path: &str, value: &[u8]) -> Result<(), String> {
        self.run("write", Self::key(path)?, Some(value))?;
        Ok(())
    }

    fn delete(&self, path: &str) -> Result<(), String> {
        self.run("delete", Self::key(path)?, None)?;
        Ok(())
    }

    fn list(&self, path: &str) -> Result<Vec<String>, String> {
        let names = self.run("list", Self::key(path)?, None)?;
        let text = String::from_utf8(names).map_err(|e| e.to_string())?;
        parse_json_string_array(&text)
    }
}

#[derive(Clone)]
pub struct StorageCapability {
    mount: String,
    provider: Arc<dyn StateProvider>,
}

impl StorageCapability {
    pub fn new(mount: impl Into<String>, host_root: impl AsRef<Path>) -> Result<Self, String> {
        Self::with_provider(mount, LocalDirectoryStateProvider::new(host_root)?)
    }

    pub fn with_provider(
        mount: impl Into<String>,
        provider: impl StateProvider + 'static,
    ) -> Result<Self, String> {
        Ok(Self {
            mount: mount.into(),
            provider: Arc::new(provider),
        })
    }

    pub fn read(&self, path: &str) -> Result<Vec<u8>, CapabilityError> {
        self.provider
            .read(&self.resolve(path, "read")?)
            .map_err(CapabilityError::Host)
    }

    pub fn write(&self, path: &str, value: &[u8]) -> Result<(), CapabilityError> {
        self.provider
            .write(&self.resolve(path, "write")?, value)
            .map_err(CapabilityError::Host)
    }

    fn resolve(&self, requested: &str, operation: &'static str) -> Result<String, CapabilityError> {
        storage_relative_path(&self.mount, requested).map_err(|_| {
            CapabilityError::CapabilityDenied {
                capability: "filesystem",
                operation,
            }
        })
    }
}

impl Capability for StorageCapability {
    fn capability(&self) -> &'static str {
        "filesystem"
    }
}

pub fn resolve_storage_path(
    mount: &str,
    requested: &str,
    host_root: &Path,
) -> Result<PathBuf, String> {
    let relative = storage_relative_path(mount, requested)?;
    resolve_relative_path(&relative, host_root)
}

fn storage_relative_path(mount: &str, requested: &str) -> Result<String, String> {
    let relative = requested
        .strip_prefix(mount)
        .ok_or_else(|| "requested path is outside declared storage mount".to_string())?;
    if !relative.is_empty() && !relative.starts_with('/') {
        return Err("requested path is outside declared storage mount".into());
    }
    let relative = relative.trim_start_matches('/');
    for component in Path::new(relative).components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err("requested path is outside declared storage mount".into());
        }
    }
    Ok(relative.into())
}

fn resolve_relative_path(path: &str, host_root: &Path) -> Result<PathBuf, String> {
    reject_escaping_path(path)?;
    let root = host_root.canonicalize().map_err(|e| e.to_string())?;
    let mut resolved = root.clone();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(segment) => resolved.push(segment),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => unreachable!(),
        }
    }
    if resolved.starts_with(&root) {
        Ok(resolved)
    } else {
        Err("CapabilityDenied: filesystem path escapes declared storage".into())
    }
}

fn reject_escaping_path(path: &str) -> Result<(), String> {
    for component in Path::new(path).components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err("CapabilityDenied: filesystem path escapes declared storage".into());
        }
    }
    Ok(())
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        output.push(TABLE[(b0 >> 2) as usize] as char);
        output.push(TABLE[(((b0 & 0b0000_0011) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            output.push(TABLE[(((b1 & 0b0000_1111) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(TABLE[(b2 & 0b0011_1111) as usize] as char);
        } else {
            output.push('=');
        }
    }
    output
}

fn base64_decode(text: &str) -> Result<Vec<u8>, String> {
    let bytes = text.trim().as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err("invalid base64 length from FeltDB state provider".into());
    }
    let mut output = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let a = base64_value(chunk[0])?;
        let b = base64_value(chunk[1])?;
        let c = if chunk[2] == b'=' {
            0
        } else {
            base64_value(chunk[2])?
        };
        let d = if chunk[3] == b'=' {
            0
        } else {
            base64_value(chunk[3])?
        };
        output.push((a << 2) | (b >> 4));
        if chunk[2] != b'=' {
            output.push(((b & 0b0000_1111) << 4) | (c >> 2));
        }
        if chunk[3] != b'=' {
            output.push(((c & 0b0000_0011) << 6) | d);
        }
    }
    Ok(output)
}

fn base64_value(byte: u8) -> Result<u8, String> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        b'=' => Err("unexpected base64 padding".into()),
        _ => Err("invalid base64 character from FeltDB state provider".into()),
    }
}

fn parse_json_string_array(text: &str) -> Result<Vec<String>, String> {
    let text = text.trim();
    if text == "[]" {
        return Ok(Vec::new());
    }
    let inner = text
        .strip_prefix('[')
        .and_then(|text| text.strip_suffix(']'))
        .ok_or_else(|| "invalid FeltDB list response".to_string())?;
    inner
        .split(',')
        .map(|item| {
            item.trim()
                .strip_prefix('"')
                .and_then(|item| item.strip_suffix('"'))
                .map(|item| item.replace("\\\"", "\"").replace("\\\\", "\\"))
                .ok_or_else(|| "invalid FeltDB list entry".to_string())
        })
        .collect()
}

pub fn require_network(allowed: bool, _operation: &str) -> Result<(), String> {
    NetworkCapability::new(allowed)
        .connect()
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_cannot_escape_its_root() {
        let tempdir = tempfile::tempdir().unwrap();
        let state = LocalDirectoryStateProvider::new(tempdir.path()).unwrap();
        assert!(
            state
                .write("../outside", b"x")
                .unwrap_err()
                .contains("CapabilityDenied")
        );
    }

    #[test]
    fn storage_resolution_accepts_paths_inside_mount() {
        let tempdir = tempfile::tempdir().unwrap();
        let root = LocalDirectoryStateProvider::new(tempdir.path())
            .unwrap()
            .root;
        assert_eq!(
            resolve_storage_path("/data", "/data/foo/bar.txt", &root).unwrap(),
            root.join("foo/bar.txt")
        );
    }

    #[test]
    fn storage_resolution_rejects_traversal() {
        let tempdir = tempfile::tempdir().unwrap();
        let root = LocalDirectoryStateProvider::new(tempdir.path())
            .unwrap()
            .root;
        for path in [
            "/data/../secret",
            "/data/../../secret",
            "/data/a/../../secret",
        ] {
            assert!(resolve_storage_path("/data", path, &root).is_err());
        }
    }

    #[test]
    fn network_denial_is_deterministic() {
        assert_eq!(
            require_network(false, "connect").unwrap_err(),
            "CapabilityDenied { capability: \"network\", operation: \"connect\" }"
        );
    }

    #[test]
    fn feltdb_state_persists_across_provider_instances() {
        let tempdir = tempfile::tempdir().unwrap();
        FeltDBStateProvider::new(tempdir.path(), "app-a")
            .unwrap()
            .write("counter", b"persisted")
            .unwrap();

        let restored = FeltDBStateProvider::new(tempdir.path(), "app-a").unwrap();

        assert_eq!(restored.read("counter").unwrap(), b"persisted");
    }

    #[test]
    fn feltdb_state_is_namespaced_by_application_id() {
        let tempdir = tempfile::tempdir().unwrap();
        FeltDBStateProvider::new(tempdir.path(), "app-a")
            .unwrap()
            .write("counter", b"a")
            .unwrap();
        FeltDBStateProvider::new(tempdir.path(), "app-b")
            .unwrap()
            .write("counter", b"b")
            .unwrap();

        assert_eq!(
            FeltDBStateProvider::new(tempdir.path(), "app-a")
                .unwrap()
                .read("counter")
                .unwrap(),
            b"a"
        );
        assert_eq!(
            FeltDBStateProvider::new(tempdir.path(), "app-b")
                .unwrap()
                .read("counter")
                .unwrap(),
            b"b"
        );
    }

    #[test]
    fn feltdb_state_rejects_traversal() {
        let tempdir = tempfile::tempdir().unwrap();
        let state = FeltDBStateProvider::new(tempdir.path(), "app-a").unwrap();

        assert!(
            state
                .write("../outside", b"x")
                .unwrap_err()
                .contains("CapabilityDenied")
        );
    }
}
