# perry-bridge

A minimal slice of the dev container CLI that compiles to a single native
binary via [Perry](https://github.com/perry-dev/perry) so it can be embedded in
non-Node hosts — specifically the [wiki3-app](https://github.com/wiki3-ai/wiki3-app)
Tauri 2 application — without bundling a Node runtime.

## Scope

This sub-project covers exactly one capability:

> Load a `devcontainer.json` from disk, parse it (JSONC), apply the
> "old property" upgrade, and run **pre-container** variable substitution.

It deliberately does **not** cover anything that requires Docker, an OCI
registry, a PTY, tarball streaming, or a remote shell. Concretely, it excludes:

- `devcontainer up`
- `devcontainer build`
- `devcontainer exec`
- Feature fetching / Feature install order
- Image metadata merge against a running container or pulled image
- Docker Compose resolution

Those continue to be implemented in the existing Node CLI and are out of scope
here. See `../example-usage/` for the Node-based examples that exercise them.

## Layout

```
perry-bridge/
├── README.md                  ← this file
├── PERRY_VERSION              ← pinned Perry CLI version
├── PROTOCOL.md                ← TS ↔ Rust host-call protocol
├── TAURI_INTEGRATION.md       ← how to embed in a Tauri 2 app
├── api-audit.md               ← Node / npm dependency audit (step 1)
├── tsconfig.perry.json        ← Perry-targeted TS config
├── src/
│   ├── entry.ts               ← Perry entry point
│   ├── host.ts                ← FileHost implementation + host-call client
│   ├── re-exports.ts          ← imports of the existing slice modules
│   └── uri.ts                 ← minimal file-URI helper (used iff vscode-uri
│                                fails the Perry spike)
├── spike/                     ← capability spike for Perry stdlib coverage
│   └── spike.ts
└── rust/
    └── devcontainer-config/   ← Rust crate (lib + thin CLI)
        ├── Cargo.toml
        ├── build.rs
        ├── bin/               ← Perry-compiled binary lands here per triple
        └── src/
            ├── lib.rs
            └── bin/cli.rs
```

## Building

The Perry build is **independent** of the existing Node build. Running
`npm run compile`, `npm run package`, `npm test`, etc. is unaffected by
anything in this directory.

```sh
# 1. Compile the TS slice to a native binary with Perry.
./scripts/build-perry.sh

# 2. Build the Rust crate (looks up the binary produced in step 1).
cd perry-bridge/rust/devcontainer-config && cargo build --release

# 3. Run the Cargo example end-to-end.
cd ../../../example-usage/parse-config-rust && cargo run
```

If the Perry binary is not present at build time, the Rust crate still
compiles; it will return a clear `BinaryNotFound` error at runtime so the
Tauri host can surface a useful message during development.

## Why Perry?

We need a single artifact that runs on macOS, Windows, and Linux without a
JavaScript runtime, while reusing the JSONC / variable-substitution / "old
property" logic that already exists in this repository. Perry compiles a
constrained subset of Node-flavoured TypeScript to a per-triple native binary,
which is exactly that shape. The TS we feed it is a deliberately narrow slice
(see `api-audit.md`); anything Perry doesn't cover gets shimmed via the
`PROTOCOL.md` host-call channel implemented by the Rust crate.

## Status

Scaffolding only. The Perry build itself must be run on a machine that has the
pinned Perry CLI installed (see `PERRY_VERSION`). The Rust crate, the example,
and the host-call protocol are fully present and self-tested where they don't
require a running Perry binary.
