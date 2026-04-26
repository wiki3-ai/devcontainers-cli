# Host-call protocol (TS ↔ Rust)

The Perry-built binary and the Rust crate communicate over the binary's
**stdin** and **stdout** using newline-delimited JSON (NDJSON). One JSON
object per line, no embedded newlines inside objects.

`stderr` is reserved for human-readable diagnostics; the Rust side forwards it
to its own logging.

## Lifetime

Each invocation processes a single command. The Rust crate spawns the binary,
writes one `request` line, services zero or more `host` calls, reads exactly
one `result` or `error` line, then waits for the process to exit.

```
Rust → child : {"kind":"request", ...}            ⏎
child → Rust : {"kind":"host","id":1, ...}        ⏎    (zero or more)
Rust → child : {"kind":"host-reply","id":1, ...}  ⏎
…
child → Rust : {"kind":"result","value":...}      ⏎    (exactly one)
[child exits with status 0]
```

On a fatal TS-side error the child instead emits

```
{"kind":"error","message":"...","stack":"..."}
```

and exits non-zero.

## Request envelope (Rust → child, exactly once)

```json
{
  "kind": "request",
  "command": "loadConfig",
  "workspaceFolder": "/abs/path/to/workspace",
  "configFile": "/abs/path/.devcontainer/devcontainer.json",
  "platform": "linux",
  "env": { "HOME": "/home/user", "...": "..." }
}
```

- `command` — currently only `"loadConfig"`. Future commands (e.g.
  `"validateConfig"`) follow the same envelope.
- `workspaceFolder` — absolute path. Used for `${localWorkspaceFolder}` and
  related substitutions.
- `configFile` — optional. If omitted, the binary searches the well-known
  paths under `workspaceFolder`.
- `platform` — `"linux" | "darwin" | "win32"`. Drives `path.posix` vs
  `path.win32` selection inside the slice and Windows-specific env
  case-insensitivity.
- `env` — full environment as a flat string→string map. The Rust side decides
  what to forward; the binary never touches the OS environment directly.

## Host calls (child → Rust, zero or more)

Only four ops are defined. All paths are absolute and already resolved on the
Rust side's filesystem; no path translation happens inside the binary.

### `fs.readFile`
```json
{"kind":"host","id":1,"op":"fs.readFile","args":{"path":"/abs/x"}}
```
Reply on success:
```json
{"kind":"host-reply","id":1,"ok":true,"value":{"bytesBase64":"..."}}
```
Reply on error:
```json
{"kind":"host-reply","id":1,"ok":false,"error":{"code":"ENOENT","message":"..."}}
```
`code` follows Node-style error codes (`ENOENT`, `EACCES`, `EISDIR`, …) so the
TS slice's existing error handling (`err.code === 'ENOENT'`) keeps working.

### `fs.writeFile`
```json
{"kind":"host","id":2,"op":"fs.writeFile","args":{"path":"/abs/x","bytesBase64":"..."}}
```
Reply: `{"kind":"host-reply","id":2,"ok":true,"value":{}}`.

### `fs.stat`
```json
{"kind":"host","id":3,"op":"fs.stat","args":{"path":"/abs/x"}}
```
Reply on success:
```json
{"kind":"host-reply","id":3,"ok":true,"value":{"kind":"file","size":1234}}
```
`kind` is one of `"file" | "dir" | "other" | "missing"`. `"missing"` is **not**
an error — the slice asks `isFile` style questions and treats missing as
`false`.

### `fs.readDir`
```json
{"kind":"host","id":4,"op":"fs.readDir","args":{"path":"/abs/x"}}
```
Reply:
```json
{"kind":"host-reply","id":4,"ok":true,"value":{"entries":[{"name":"a","kind":"file"},{"name":"b","kind":"dir"}]}}
```

## Result envelope (child → Rust, exactly once)

```json
{
  "kind": "result",
  "value": {
    "config": { /* substituted DevContainerConfig */ },
    "raw":    { /* same shape, before substitution */ },
    "configFilePath": "/abs/path/devcontainer.json"
  }
}
```

The `value` shape is intentionally the JSON projection of the slice's
`SubstitutedConfig<DevContainerConfig>` (minus the `substitute` function,
which is not serialisable). The Rust crate exposes this verbatim as
`serde_json::Value`; consumers may then deserialise into typed structs at
their own pace.

## Error envelope

```json
{"kind":"error","message":"Dev container config not found","stack":"…"}
```

The Rust crate maps `error` to `Err(BridgeError::Tool { message, stack })` and
host-reply `ok:false` to `Err(BridgeError::Host { code, message })`.

## Versioning

The first line emitted by the binary on startup, *before* the request is read,
is:

```json
{"kind":"hello","protocol":1,"slice":"devcontainers-cli@0.86.0"}
```

The Rust crate aborts if `protocol` is not in its supported range. Bumping the
protocol number is a breaking change.
