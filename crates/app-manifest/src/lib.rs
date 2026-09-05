use app_spec::AppSpec;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub runtime: String,
    pub artifact: Artifact,
    pub capabilities: ManifestCapabilities,
    pub state: Option<State>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Artifact {
    pub path: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ManifestCapabilities {
    pub http: Option<HttpCapability>,
    pub network: bool,
    pub filesystem: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HttpCapability {
    pub listen: u16,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct State {
    pub provider: String,
    pub mount: String,
    pub path: String,
}

impl Manifest {
    pub fn from_spec(spec: &AppSpec, bytes: &[u8]) -> Self {
        let sha256 = hex(Sha256::digest(bytes).as_slice());
        Self {
            name: spec.name.clone(),
            version: spec.version.clone(),
            runtime: spec.runtime.kind.clone(),
            artifact: Artifact {
                path: "app.wasm".into(),
                sha256,
                size: bytes.len() as u64,
            },
            capabilities: ManifestCapabilities {
                http: spec
                    .http
                    .as_ref()
                    .map(|h| HttpCapability { listen: h.listen }),
                network: spec.capabilities.network,
                filesystem: if spec.capabilities.filesystem {
                    spec.storage
                        .as_ref()
                        .map(|s| vec![s.mount.clone()])
                        .unwrap_or_default()
                } else {
                    Vec::new()
                },
            },
            state: spec.storage.as_ref().map(|s| State {
                provider: "local".into(),
                mount: s.mount.clone(),
                path: s.path.clone(),
            }),
        }
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        serde_json::from_reader(fs::File::open(path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())
    }

    pub fn write(&self, path: &Path) -> Result<(), String> {
        fs::write(
            path,
            format!(
                "{}\n",
                serde_json::to_string_pretty(self).map_err(|e| e.to_string())?
            ),
        )
        .map_err(|e| e.to_string())
    }
}

pub fn sha256(bytes: &[u8]) -> String {
    hex(Sha256::digest(bytes).as_slice())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn artifact_hash_is_deterministic() {
        assert_eq!(sha256(b"same"), sha256(b"same"));
    }
}
