use app_spec::AppSpec;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs;
use std::path::Path;

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub appport: Option<AppPortMapping>,
    pub runtime: String,
    pub artifact: Artifact,
    pub http: Option<HttpCapability>,
    pub capabilities: ManifestCapabilities,
    pub storage: Option<Storage>,
    #[serde(default, skip_serializing_if = "Secrets::is_empty")]
    pub secrets: Secrets,
    #[serde(default, skip_serializing_if = "Config::is_empty")]
    pub config: Config,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<Network>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Resources>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AppPortMapping {
    pub application_id: String,
    pub operation: AppPortOperation,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AppPortOperation {
    pub name: String,
    pub version: u64,
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

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Secrets {
    #[serde(default)]
    pub required: Vec<String>,
}

impl Secrets {
    pub fn is_empty(&self) -> bool {
        self.required.is_empty()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub allowed: Vec<String>,
}

impl Config {
    pub fn is_empty(&self) -> bool {
        self.allowed.is_empty()
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Network {
    #[serde(default)]
    pub outbound: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Resources {
    pub memory_mb: u64,
    pub timeout_ms: u64,
    #[serde(default = "default_max_concurrent_requests")]
    pub max_concurrent_requests: u32,
}

impl Manifest {
    pub fn from_spec(spec: &AppSpec, bytes: &[u8]) -> Self {
        let sha256 = hex(Sha256::digest(bytes).as_slice());
        Self {
            name: spec.name.clone(),
            version: spec.version.clone(),
            appport: None,
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
            secrets: Secrets {
                required: spec.secrets.required.clone(),
            },
            config: Config {
                allowed: spec.config.allowed.clone(),
            },
            network: spec.network.as_ref().map(|network| Network {
                outbound: network.outbound.clone(),
            }),
            resources: spec.resources.as_ref().map(|resources| Resources {
                memory_mb: resources.memory_mb,
                timeout_ms: resources.timeout_ms,
                max_concurrent_requests: resources.max_concurrent_requests,
            }),
        }
    }

    pub fn from_appport_operation(
        appport: &Value,
        operation_name: &str,
        operation_version: u64,
        wasm: &[u8],
    ) -> Result<Self, String> {
        let protocol = appport
            .get("protocol")
            .and_then(Value::as_str)
            .ok_or("AppPort manifest requires protocol")?;
        if !protocol.eq_ignore_ascii_case("appport/1") {
            return Err(format!("unsupported AppPort protocol '{protocol}'"));
        }
        let application = appport
            .get("application")
            .and_then(Value::as_object)
            .ok_or("AppPort manifest requires application")?;
        let appport_application_id = string_field(application, "id", "AppPort application.id")?;
        let capability = find_appport_capability(appport, operation_name, operation_version)?;
        let authorization = capability
            .get("authorization")
            .and_then(Value::as_array)
            .ok_or("AppPort capability requires authorization")?;
        let adapter = appboundry_adapter(appport)?;
        let filesystem = declares_appport_authorization(authorization, "filesystem")
            || declares_appport_authorization(authorization, "storage");
        let network = declares_appport_authorization(authorization, "network");
        let storage = if filesystem {
            let storage = adapter.get("storage").and_then(Value::as_object).ok_or(
                "AppPort filesystem/storage capability requires attributes.appboundry.storage",
            )?;
            Some(Storage {
                mount: string_field(storage, "mount", "attributes.appboundry.storage.mount")?,
                path: storage
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or(".app/data")
                    .to_string(),
            })
        } else {
            None
        };
        let resources = if declares_appport_authorization(authorization, "resources") {
            let resources = adapter
                .get("resources")
                .and_then(Value::as_object)
                .ok_or("AppPort resources capability requires attributes.appboundry.resources")?;
            Some(Resources {
                memory_mb: u64_field(
                    resources,
                    "memory_mb",
                    "attributes.appboundry.resources.memory_mb",
                )
                .or_else(|_| {
                    u64_field(
                        resources,
                        "memoryMb",
                        "attributes.appboundry.resources.memoryMb",
                    )
                })?,
                timeout_ms: u64_field(
                    resources,
                    "timeout_ms",
                    "attributes.appboundry.resources.timeout_ms",
                )
                .or_else(|_| {
                    u64_field(
                        resources,
                        "timeoutMs",
                        "attributes.appboundry.resources.timeoutMs",
                    )
                })?,
                max_concurrent_requests: resources
                    .get("max_concurrent_requests")
                    .or_else(|| resources.get("maxConcurrentRequests"))
                    .and_then(Value::as_u64)
                    .unwrap_or(1) as u32,
            })
        } else {
            None
        };
        Ok(Self {
            name: string_field(application, "name", "AppPort application.name")?,
            version: string_field(application, "version", "AppPort application.version")?,
            appport: Some(AppPortMapping {
                application_id: appport_application_id,
                operation: AppPortOperation {
                    name: operation_name.to_string(),
                    version: operation_version,
                },
            }),
            runtime: adapter
                .get("runtime")
                .and_then(Value::as_str)
                .unwrap_or("wasm")
                .to_string(),
            artifact: Artifact {
                file: appport_artifact_file(appport)?,
                sha256: sha256(wasm),
                size: wasm.len() as u64,
            },
            http: adapter
                .get("http")
                .and_then(Value::as_object)
                .and_then(|http| http.get("listen"))
                .and_then(Value::as_u64)
                .map(|listen| HttpCapability {
                    listen: listen as u16,
                }),
            capabilities: ManifestCapabilities {
                network,
                filesystem,
            },
            storage,
            secrets: if declares_appport_authorization(authorization, "secrets") {
                Secrets {
                    required: string_array(adapter, "secrets", "required")?,
                }
            } else {
                Secrets::default()
            },
            config: if declares_appport_authorization(authorization, "config") {
                Config {
                    allowed: string_array(adapter, "config", "allowed")?,
                }
            } else {
                Config::default()
            },
            network: network.then(|| Network {
                outbound: string_array(adapter, "network", "outbound").unwrap_or_default(),
            }),
            resources,
        })
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

pub fn appport_artifact_file(appport: &Value) -> Result<String, String> {
    Ok(appboundry_adapter(appport)?
        .get("artifact")
        .and_then(|artifact| artifact.get("file"))
        .and_then(Value::as_str)
        .unwrap_or("app.wasm")
        .to_string())
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
        #[serde(skip_serializing_if = "Option::is_none")]
        appport: Option<CanonicalAppPort<'a>>,
        runtime: &'a str,
        artifact: CanonicalArtifact<'a>,
        http: Option<CanonicalHttp>,
        capabilities: CanonicalCapabilities,
        storage: Option<CanonicalStorage<'a>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        secrets: Option<CanonicalSecrets<'a>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        config: Option<CanonicalConfig<'a>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        network: Option<CanonicalNetwork<'a>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        resources: Option<CanonicalResources>,
    }

    #[derive(Serialize)]
    struct CanonicalArtifact<'a> {
        file: &'a str,
        sha256: &'a str,
        size: u64,
    }

    #[derive(Serialize)]
    struct CanonicalAppPort<'a> {
        application_id: &'a str,
        operation: CanonicalAppPortOperation<'a>,
    }

    #[derive(Serialize)]
    struct CanonicalAppPortOperation<'a> {
        name: &'a str,
        version: u64,
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

    #[derive(Serialize)]
    struct CanonicalSecrets<'a> {
        required: &'a [String],
    }

    #[derive(Serialize)]
    struct CanonicalConfig<'a> {
        allowed: &'a [String],
    }

    #[derive(Serialize)]
    struct CanonicalNetwork<'a> {
        outbound: &'a [String],
    }

    #[derive(Serialize)]
    struct CanonicalResources {
        memory_mb: u64,
        timeout_ms: u64,
        max_concurrent_requests: u32,
    }

    let canonical = CanonicalManifest {
        name: &manifest.name,
        version: &manifest.version,
        appport: manifest.appport.as_ref().map(|appport| CanonicalAppPort {
            application_id: &appport.application_id,
            operation: CanonicalAppPortOperation {
                name: &appport.operation.name,
                version: appport.operation.version,
            },
        }),
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
        secrets: (!manifest.secrets.required.is_empty()).then_some(CanonicalSecrets {
            required: &manifest.secrets.required,
        }),
        config: (!manifest.config.allowed.is_empty()).then_some(CanonicalConfig {
            allowed: &manifest.config.allowed,
        }),
        network: manifest.network.as_ref().map(|network| CanonicalNetwork {
            outbound: &network.outbound,
        }),
        resources: manifest
            .resources
            .as_ref()
            .map(|resources| CanonicalResources {
                memory_mb: resources.memory_mb,
                timeout_ms: resources.timeout_ms,
                max_concurrent_requests: resources.max_concurrent_requests,
            }),
    };

    serde_json::to_vec(&canonical).map_err(|e| e.to_string())
}

fn find_appport_capability<'a>(
    appport: &'a Value,
    operation_name: &str,
    operation_version: u64,
) -> Result<&'a Value, String> {
    appport
        .get("capabilities")
        .and_then(Value::as_array)
        .ok_or("AppPort manifest requires capabilities")?
        .iter()
        .find(|capability| {
            capability.get("name").and_then(Value::as_str) == Some(operation_name)
                && capability
                    .get("versions")
                    .and_then(Value::as_array)
                    .is_some_and(|versions| {
                        versions
                            .iter()
                            .any(|version| version.as_u64() == Some(operation_version))
                    })
        })
        .ok_or_else(|| {
            format!("AppPort operation {operation_name}@{operation_version} is not declared")
        })
}

fn appboundry_adapter(appport: &Value) -> Result<&serde_json::Map<String, Value>, String> {
    appport
        .get("attributes")
        .and_then(|attributes| attributes.get("appboundry"))
        .and_then(Value::as_object)
        .ok_or_else(|| "AppPort manifest requires attributes.appboundry adapter metadata".into())
}

fn declares_appport_authorization(authorization: &[Value], capability: &str) -> bool {
    authorization.iter().filter_map(Value::as_str).any(|token| {
        token == capability
            || token.ends_with(&format!(".{capability}"))
            || token.ends_with(&format!(":{capability}"))
            || token.ends_with(&format!("/{capability}"))
    })
}

fn string_field(
    object: &serde_json::Map<String, Value>,
    field: &str,
    label: &str,
) -> Result<String, String> {
    object
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("{label} must be a string"))
}

fn u64_field(
    object: &serde_json::Map<String, Value>,
    field: &str,
    label: &str,
) -> Result<u64, String> {
    object
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{label} must be an integer"))
}

fn string_array(
    adapter: &serde_json::Map<String, Value>,
    section: &str,
    field: &str,
) -> Result<Vec<String>, String> {
    let Some(section_value) = adapter.get(section) else {
        return Ok(Vec::new());
    };
    let Some(values) = section_value.get(field).and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    values
        .iter()
        .map(|value| {
            value.as_str().map(str::to_string).ok_or_else(|| {
                format!("attributes.appboundry.{section}.{field} must contain only strings")
            })
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn default_storage_path() -> String {
    ".app/data".into()
}

fn default_max_concurrent_requests() -> u32 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        Manifest {
            name: "hello".into(),
            version: "0.1.0".into(),
            appport: None,
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
            secrets: Secrets::default(),
            config: Config::default(),
            network: None,
            resources: None,
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

    #[test]
    fn manifest_includes_required_secret_names_but_not_values() {
        let spec = AppSpec {
            name: "hello".into(),
            version: "0.1.0".into(),
            build: app_spec::Build {
                source: "src".into(),
                language: "rust".into(),
                toolchain: None,
                entry: "src/main.rs".into(),
                target: "wasm".into(),
            },
            runtime: app_spec::Runtime {
                kind: "wasm".into(),
            },
            http: None,
            capabilities: app_spec::Capabilities::default(),
            storage: None,
            secrets: app_spec::Secrets {
                required: vec!["OPENAI_API_KEY".into()],
            },
            config: app_spec::Config::default(),
            network: None,
            resources: None,
        };
        let manifest = Manifest::from_spec(&spec, b"wasm");
        let json = serde_json::to_string(&manifest).unwrap();

        assert!(json.contains("OPENAI_API_KEY"), "{json}");
        assert!(!json.contains("test-secret-value"), "{json}");
    }

    #[test]
    fn application_id_changes_when_secret_contract_changes() {
        let original = manifest();
        let mut changed = manifest();
        changed.secrets.required.push("OPENAI_API_KEY".into());

        assert_ne!(
            original.application_id(b"wasm").unwrap(),
            changed.application_id(b"wasm").unwrap()
        );
    }

    #[test]
    fn manifest_includes_config_names_network_policy_and_resource_limits() {
        let spec = AppSpec {
            name: "hello".into(),
            version: "0.1.0".into(),
            build: app_spec::Build {
                source: "src".into(),
                language: "rust".into(),
                toolchain: None,
                entry: "src/main.rs".into(),
                target: "wasm".into(),
            },
            runtime: app_spec::Runtime {
                kind: "wasm".into(),
            },
            http: None,
            capabilities: app_spec::Capabilities {
                network: true,
                filesystem: false,
            },
            storage: None,
            secrets: app_spec::Secrets::default(),
            config: app_spec::Config {
                allowed: vec!["LOG_LEVEL".into()],
            },
            network: Some(app_spec::Network {
                outbound: vec!["api.example.com".into()],
            }),
            resources: Some(app_spec::Resources {
                memory_mb: 256,
                timeout_ms: 30000,
                max_concurrent_requests: 1,
            }),
        };

        let manifest = Manifest::from_spec(&spec, b"wasm");
        let json = serde_json::to_string(&manifest).unwrap();

        assert!(json.contains("LOG_LEVEL"), "{json}");
        assert!(json.contains("api.example.com"), "{json}");
        assert!(json.contains("memory_mb"), "{json}");
    }

    #[test]
    fn appport_operation_maps_identity_and_capabilities_to_manifest() {
        let appport: Value = serde_json::from_str(
            r#"
{
  "protocol": "appport/1",
  "application": {
    "id": "com.example.hello",
    "name": "hello",
    "version": "0.1.0"
  },
  "capabilities": [
    {
      "name": "hello.request",
      "versions": [1],
      "latestVersion": 1,
      "kind": "request",
      "authorization": ["network", "filesystem", "storage", "config", "secrets", "resources"]
    }
  ],
  "events": [],
  "transports": [],
  "attributes": {
    "appboundry": {
      "runtime": "wasm",
      "artifact": { "file": "app.wasm" },
      "storage": { "mount": "/data", "path": ".app/data" },
      "config": { "allowed": ["LOG_LEVEL"] },
      "secrets": { "required": ["OPENAI_API_KEY"] },
      "network": { "outbound": ["api.example.com"] },
      "resources": {
        "memory_mb": 256,
        "timeout_ms": 30000,
        "max_concurrent_requests": 1
      }
    }
  }
}
"#,
        )
        .unwrap();

        let manifest =
            Manifest::from_appport_operation(&appport, "hello.request", 1, b"wasm").unwrap();

        assert_eq!(manifest.name, "hello");
        assert_eq!(
            manifest.appport.unwrap().application_id,
            "com.example.hello"
        );
        assert!(manifest.capabilities.network);
        assert!(manifest.capabilities.filesystem);
        assert_eq!(manifest.storage.unwrap().mount, "/data");
        assert_eq!(manifest.config.allowed, ["LOG_LEVEL"]);
        assert_eq!(manifest.secrets.required, ["OPENAI_API_KEY"]);
        assert_eq!(manifest.network.unwrap().outbound, ["api.example.com"]);
        assert_eq!(manifest.resources.unwrap().memory_mb, 256);
    }
}
