use serde::Deserialize;
use std::fs;
use std::path::Path;

#[derive(Debug, Deserialize, PartialEq)]
pub struct AppSpec {
    pub name: String,
    pub version: String,
    pub build: Build,
    pub runtime: Runtime,
    #[serde(default)]
    pub http: Option<Http>,
    #[serde(default)]
    pub capabilities: Capabilities,
    #[serde(default)]
    pub storage: Option<Storage>,
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct Build {
    pub source: String,
    pub entry: String,
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct Runtime {
    pub kind: String,
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct Http {
    pub listen: u16,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
pub struct Capabilities {
    #[serde(default)]
    pub network: bool,
    #[serde(default)]
    pub filesystem: bool,
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct Storage {
    pub path: String,
    pub mount: String,
}

impl AppSpec {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text =
            fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let spec: Self = toml::from_str(&text).map_err(|e| format!("invalid app.toml: {e}"))?;
        spec.validate()?;
        Ok(spec)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.name.is_empty()
            || !self
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err("application name must contain only letters, digits, '-' or '_'".into());
        }
        if self.runtime.kind != "wasm" {
            return Err("runtime.kind must be 'wasm'".into());
        }
        if self.capabilities.filesystem && self.storage.is_none() {
            return Err("filesystem capability requires a [storage] declaration".into());
        }
        if !self.capabilities.filesystem && self.storage.is_some() {
            return Err("storage declaration requires filesystem capability".into());
        }
        if let Some(storage) = &self.storage {
            if !storage.mount.starts_with('/') {
                return Err("storage.mount must be an absolute virtual path".into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_filesystem_without_storage() {
        let spec: AppSpec = toml::from_str(
            r#"
name = "hello"
version = "0.1.0"
[build]
source = "src"
entry = "src/main.rs"
[runtime]
kind = "wasm"
[capabilities]
filesystem = true
"#,
        )
        .unwrap();
        assert_eq!(
            spec.validate().unwrap_err(),
            "filesystem capability requires a [storage] declaration"
        );
    }

    #[test]
    fn rejects_storage_without_filesystem() {
        let spec: AppSpec = toml::from_str(
            r#"
name = "hello"
version = "0.1.0"
[build]
source = "src"
entry = "src/main.rs"
[runtime]
kind = "wasm"
[capabilities]
filesystem = false
[storage]
path = ".app/data"
mount = "/data"
"#,
        )
        .unwrap();
        assert_eq!(
            spec.validate().unwrap_err(),
            "storage declaration requires filesystem capability"
        );
    }
}
