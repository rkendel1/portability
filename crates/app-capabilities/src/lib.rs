use std::fs;
use std::path::{Path, PathBuf};

pub trait StateProvider {
    fn read(&self, path: &str) -> Result<Vec<u8>, String>;
    fn write(&self, path: &str, value: &[u8]) -> Result<(), String>;
    fn delete(&self, path: &str) -> Result<(), String>;
    fn list(&self, path: &str) -> Result<Vec<String>, String>;
}

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
        let relative = Path::new(path.trim_start_matches('/'));
        if relative.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        }) {
            return Err("CapabilityDenied: filesystem path escapes declared storage".into());
        }
        Ok(self.root.join(relative))
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

pub fn require_network(allowed: bool, operation: &str) -> Result<(), String> {
    if allowed {
        Ok(())
    } else {
        Err(format!(
            "CapabilityDenied:\n  operation: {operation}\n  capability: network\n  declared: false"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn state_cannot_escape_its_root() {
        let state = LocalDirectoryStateProvider::new(tempfile::tempdir().unwrap().path()).unwrap();
        assert!(
            state
                .write("../outside", b"x")
                .unwrap_err()
                .contains("CapabilityDenied")
        );
    }
}
