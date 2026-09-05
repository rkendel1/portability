use app_spec::AppSpec;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs;
use std::path::Path;

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub runtime: String,
    pub artifact: Artifact,
    pub http: Option<HttpCapability>,
    pub capabilities: ManifestCapabilities,
    pub storage: Option<Storage>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Artifact {
    pub file: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicationId(String);

#[derive(Debug, Serialize, Deserialize)]
pub struct ManifestCapabilities {
    pub network: bool,
    pub filesystem: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HttpCapability {
    pub listen: u16,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Storage {
    pub mount: String,
    #[serde(default = "default_storage_path")]
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
                file: "app.wasm".into(),
                sha256,
                size: bytes.len() as u64,
            },
            http: spec
                .http
                .as_ref()
                .map(|h| HttpCapability { listen: h.listen }),
            capabilities: ManifestCapabilities {
                network: spec.capabilities.network,
                filesystem: spec.capabilities.filesystem,
            },
            storage: spec.storage.as_ref().map(|s| Storage {
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

    pub fn application_id(&self, wasm: &[u8]) -> Result<ApplicationId, String> {
        ApplicationId::from_manifest_and_wasm(self, wasm)
    }
}

impl ApplicationId {
    pub fn from_manifest_and_wasm(manifest: &Manifest, wasm: &[u8]) -> Result<Self, String> {
        let mut identity_bytes = canonical_manifest(manifest)?;
        identity_bytes.extend_from_slice(wasm);
        Ok(Self(format!("sha256:{}", sha256(&identity_bytes))))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ApplicationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

pub fn sha256(bytes: &[u8]) -> String {
    hex(Sha256::digest(bytes).as_slice())
}

pub fn canonical_manifest(manifest: &Manifest) -> Result<Vec<u8>, String> {
    #[derive(Serialize)]
    struct CanonicalManifest<'a> {
        name: &'a str,
        version: &'a str,
        runtime: &'a str,
        artifact: CanonicalArtifact<'a>,
        http: Option<CanonicalHttp>,
        capabilities: CanonicalCapabilities,
        storage: Option<CanonicalStorage<'a>>,
    }

    #[derive(Serialize)]
    struct CanonicalArtifact<'a> {
        file: &'a str,
        sha256: &'a str,
        size: u64,
    }

    #[derive(Serialize)]
    struct CanonicalHttp {
        listen: u16,
    }

    #[derive(Serialize)]
    struct CanonicalCapabilities {
        network: bool,
        filesystem: bool,
    }

    #[derive(Serialize)]
    struct CanonicalStorage<'a> {
        mount: &'a str,
        path: &'a str,
    }

    let canonical = CanonicalManifest {
        name: &manifest.name,
        version: &manifest.version,
        runtime: &manifest.runtime,
        artifact: CanonicalArtifact {
            file: &manifest.artifact.file,
            sha256: &manifest.artifact.sha256,
            size: manifest.artifact.size,
        },
        http: manifest.http.as_ref().map(|http| CanonicalHttp {
            listen: http.listen,
        }),
        capabilities: CanonicalCapabilities {
            network: manifest.capabilities.network,
            filesystem: manifest.capabilities.filesystem,
        },
        storage: manifest.storage.as_ref().map(|storage| CanonicalStorage {
            mount: &storage.mount,
            path: &storage.path,
        }),
    };

    serde_json::to_vec(&canonical).map_err(|e| e.to_string())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn default_storage_path() -> String {
    ".app/data".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        Manifest {
            name: "hello".into(),
            version: "0.1.0".into(),
            runtime: "wasm".into(),
            artifact: Artifact {
                file: "app.wasm".into(),
                sha256: sha256(b"wasm"),
                size: 4,
            },
            http: Some(HttpCapability { listen: 8080 }),
            capabilities: ManifestCapabilities {
                network: false,
                filesystem: true,
            },
            storage: Some(Storage {
                mount: "/data".into(),
                path: ".app/data".into(),
            }),
        }
    }

    #[test]
    fn artifact_hash_is_deterministic() {
        assert_eq!(sha256(b"same"), sha256(b"same"));
    }

    #[test]
    fn canonical_manifest_has_stable_field_order_and_no_whitespace() {
        let canonical = String::from_utf8(canonical_manifest(&manifest()).unwrap()).unwrap();

        assert_eq!(
            canonical,
            r#"{"name":"hello","version":"0.1.0","runtime":"wasm","artifact":{"file":"app.wasm","sha256":"336154bf67f765f8f75d16a0accee61b5ee5f6a75b2a2905703df913bd550f3e","size":4},"http":{"listen":8080},"capabilities":{"network":false,"filesystem":true},"storage":{"mount":"/data","path":".app/data"}}"#
        );
    }

    #[test]
    fn application_id_is_deterministic() {
        let manifest = manifest();

        assert_eq!(
            manifest.application_id(b"wasm").unwrap(),
            manifest.application_id(b"wasm").unwrap()
        );
    }

    #[test]
    fn application_id_changes_when_contract_changes() {
        let original = manifest();
        let mut changed = manifest();
        changed.capabilities.network = true;

        assert_ne!(
            original.application_id(b"wasm").unwrap(),
            changed.application_id(b"wasm").unwrap()
        );
    }

    #[test]
    fn application_id_changes_when_wasm_changes() {
        let manifest = manifest();

        assert_ne!(
            manifest.application_id(b"wasm").unwrap(),
            manifest.application_id(b"changed wasm").unwrap()
        );
    }

    #[test]
    fn storage_path_defaults_for_minimal_artifact_manifest() {
        let manifest: Manifest = serde_json::from_str(
            r#"
{
  "name": "hello",
  "version": "0.1.0",
  "runtime": "wasm",
  "artifact": {
    "file": "app.wasm",
    "sha256": "abc",
    "size": 1
  },
  "capabilities": {
    "network": false,
    "filesystem": true
  },
  "storage": {
    "mount": "/data"
  }
}
"#,
        )
        .unwrap();

        assert_eq!(manifest.storage.unwrap().path, ".app/data");
    }
}
