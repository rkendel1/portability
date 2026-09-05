use serde::Deserialize;
use std::collections::BTreeSet;
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
    #[serde(default)]
    pub secrets: Secrets,
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct Build {
    pub source: String,
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default)]
    pub toolchain: Option<String>,
    pub entry: String,
    #[serde(default = "default_build_target")]
    pub target: String,
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

#[derive(Debug, Default, Deserialize, PartialEq)]
pub struct Secrets {
    #[serde(default)]
    pub required: Vec<String>,
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
        if self.build.target != "wasm" {
            return Err("build.target must be 'wasm'".into());
        }
        if !matches!(self.build.language.as_str(), "rust" | "go") {
            return Err("build.language must be 'rust' or 'go'".into());
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
        let mut secret_names = BTreeSet::new();
        for secret in &self.secrets.required {
            if !valid_secret_name(secret) {
                return Err(
                    "secret names must contain only uppercase letters, digits or '_' and start with a letter or '_'"
                        .into(),
                );
            }
            if !secret_names.insert(secret) {
                return Err(format!("duplicate required secret '{secret}'"));
            }
        }

        Ok(())
    }
}

fn default_language() -> String {
    "rust".into()
}

fn default_build_target() -> String {
    "wasm".into()
}

fn valid_secret_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_uppercase() || c == '_')
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
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

    #[test]
    fn defaults_build_language_to_rust_wasm() {
        let spec: AppSpec = toml::from_str(
            r#"
name = "hello"
version = "0.1.0"
[build]
source = "src"
entry = "src/main.rs"
[runtime]
kind = "wasm"
"#,
        )
        .unwrap();

        assert_eq!(spec.build.language, "rust");
        assert_eq!(spec.build.target, "wasm");
    }

    #[test]
    fn rejects_unknown_build_language() {
        let spec: AppSpec = toml::from_str(
            r#"
name = "hello"
version = "0.1.0"
[build]
source = "src"
language = "python"
entry = "src/main.py"
target = "wasm"
[runtime]
kind = "wasm"
"#,
        )
        .unwrap();
        assert_eq!(
            spec.validate().unwrap_err(),
            "build.language must be 'rust' or 'go'"
        );
    }

    #[test]
    fn parses_required_secret_names() {
        let spec: AppSpec = toml::from_str(
            r#"
name = "hello"
version = "0.1.0"
[build]
source = "src"
entry = "src/main.rs"
[runtime]
kind = "wasm"
[secrets]
required = ["OPENAI_API_KEY"]
"#,
        )
        .unwrap();

        assert_eq!(spec.secrets.required, ["OPENAI_API_KEY"]);
        spec.validate().unwrap();
    }

    #[test]
    fn rejects_invalid_secret_names() {
        let spec: AppSpec = toml::from_str(
            r#"
name = "hello"
version = "0.1.0"
[build]
source = "src"
entry = "src/main.rs"
[runtime]
kind = "wasm"
[secrets]
required = ["openai_api_key"]
"#,
        )
        .unwrap();

        assert_eq!(
            spec.validate().unwrap_err(),
            "secret names must contain only uppercase letters, digits or '_' and start with a letter or '_'"
        );
    }
}
