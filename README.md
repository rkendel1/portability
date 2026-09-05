# app — daemonless WASM application runtime

`app` compiles a declared application to WASM and runs it directly. There is
no daemon, Dockerfile, OCI image, registry, or orchestration layer.

```sh
cargo run -p app -- init hello
cd hello
cargo run -p app -- build
cargo run -p app -- run
```

`app.toml` declares the application, build, runtime, HTTP endpoint, capabilities,
and state mount. `app build` creates `target/app.wasm` and
`target/app.manifest.json`; `app inspect` displays the immutable artifact hash and
the enforced contract.

The v0.1 guest ABI is intentionally small: a guest may export `handle_request`.
The runtime owns the socket and invokes the guest for every request. State is
provided through an explicit local-directory capability; paths cannot escape its
declared root. Network access defaults to denied and is checked by the capability
boundary.

`examples/hello` builds through Rust, and `examples/hello-go` builds through Go.
Both emit the same `target/app.wasm` and `target/app.manifest.json` artifact
shape and run through the same `app run` runtime.

## Non-goals

v0.1 does not replace Docker for arbitrary Linux applications and does not support
Dockerfiles, OCI, native binaries, daemons, registries, orchestration, Kubernetes,
multi-node scheduling, production networking, multi-tenant isolation, secrets
management, or distributed state.