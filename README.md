# app — daemonless WASM application runtime

`app` compiles a declared application to WASM and runs it directly. There is
no daemon, Dockerfile, OCI image, registry, or orchestration layer.

```sh
cargo run -p app -- init hello
cd hello
cargo run -p app -- build
cargo run -p app -- run
cargo run -p app -- run target/app.manifest.json --state /var/lib/app/hello
```

`app.toml` declares the application, build, runtime, HTTP endpoint, capabilities,
and state mount. `app build` creates `target/app.wasm` and
`target/app.manifest.json`; `app run target/app.manifest.json` runs the compiled
artifact without the source tree as long as the manifest can find `app.wasm`
beside it. Persistent state is runtime configuration: pass `--state DIR` to bind
the declared state capability to a local directory. For local development,
`app run` still defaults to `.app/data/` outside the deployable artifact.
`app inspect` displays the immutable artifact hash and the enforced contract.

The Application ID is `sha256:<hash>` over the deployable application: the
canonical manifest immediately followed by the WASM bytes. The canonical
manifest is compact JSON with fields emitted in this order: `name`, `version`,
`runtime`, `artifact`, `http`, `capabilities`, `storage`; nested objects also use
the field order shown by `target/app.manifest.json`. The manifest's own
`artifact.sha256` still identifies only the WASM bytes for integrity checks.

The v0.1 guest ABI is intentionally small: a guest may export `handle_request`.
The runtime owns the socket and invokes the guest for every request. State is
provided through an explicit local-directory capability; paths cannot escape its
declared root. The same `app.wasm` and `app.manifest.json` can be relocated and
run against different `--state` directories without changing the Application ID.
Network access defaults to denied and is checked by the capability boundary.

`examples/hello` builds through Rust, and `examples/hello-go` builds through Go.
Both emit the same `target/app.wasm` and `target/app.manifest.json` artifact
shape and run through the same `app run` runtime.

## Non-goals

v0.1 does not replace Docker for arbitrary Linux applications and does not support
Dockerfiles, OCI, native binaries, daemons, registries, orchestration, Kubernetes,
multi-node scheduling, production networking, multi-tenant isolation, secrets
management, or distributed state.