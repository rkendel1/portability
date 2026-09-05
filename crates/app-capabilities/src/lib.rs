use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

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

pub trait StateProvider {
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
pub struct StorageCapability {
    mount: String,
    provider: LocalDirectoryStateProvider,
}

impl StorageCapability {
    pub fn new(mount: impl Into<String>, host_root: impl AsRef<Path>) -> Result<Self, String> {
        Ok(Self {
            mount: mount.into(),
            provider: LocalDirectoryStateProvider::new(host_root)?,
        })
    }

    pub fn read(&self, path: &str) -> Result<Vec<u8>, CapabilityError> {
        fs::read(self.resolve(path, "read")?).map_err(|e| CapabilityError::Host(e.to_string()))
    }

    pub fn write(&self, path: &str, value: &[u8]) -> Result<(), CapabilityError> {
        let path = self.resolve(path, "write")?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| CapabilityError::Host(e.to_string()))?;
        }
        fs::write(path, value).map_err(|e| CapabilityError::Host(e.to_string()))
    }

    fn resolve(
        &self,
        requested: &str,
        operation: &'static str,
    ) -> Result<PathBuf, CapabilityError> {
        resolve_storage_path(&self.mount, requested, &self.provider.root).map_err(|_| {
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
    let relative = requested
        .strip_prefix(mount)
        .ok_or_else(|| "requested path is outside declared storage mount".to_string())?;
    if !relative.is_empty() && !relative.starts_with('/') {
        return Err("requested path is outside declared storage mount".into());
    }
    resolve_relative_path(relative.trim_start_matches('/'), host_root)
}

fn resolve_relative_path(path: &str, host_root: &Path) -> Result<PathBuf, String> {
    let root = host_root.canonicalize().map_err(|e| e.to_string())?;
    let mut resolved = root.clone();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(segment) => resolved.push(segment),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err("CapabilityDenied: filesystem path escapes declared storage".into());
            }
        }
    }
    if resolved.starts_with(&root) {
        Ok(resolved)
    } else {
        Err("CapabilityDenied: filesystem path escapes declared storage".into())
    }
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
}
