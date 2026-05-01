# Internal caching proxy for devcontainers

**Status:** proposal · not yet implemented
**Author:** working notes between Jim and Copilot · 2026-04
**Replaces:** the user-managed Squid-in-Docker proxy currently used for
HTTP_PROXY/HTTPS_PROXY in dev containers.

## Goals

1. Remove the external Squid dependency from the dev container workflow.
2. Cache HTTPS traffic for the package managers we actually use (pip,
   npm, cargo, GitHub release tarballs) so cold-start `up` is fast and
   works offline once warmed.
3. Run automatically as part of devcontainers-cli / wiki3-app — no
   user-managed config files, no Docker Desktop side-cars.
4. Auto-inject `HTTP_PROXY` / `HTTPS_PROXY` into every container we
   spawn, "Codespaces-style".

## Non-goals

- Reverse-proxy / ingress functionality. Pingora and similar tools are
  built for that. We are a forward proxy.
- General-purpose MITM debugging. We are not building mitmproxy.
- Caching arbitrary HTTPS. We cache the protocols and hosts where it
  actually pays off.

## Design overview

```
┌──────────────────────────────────────────────────────────────┐
│  devcontainer-core (shared crate)                            │
│  └── crates/devcontainer-proxy/                              │
│      ├── server.rs   TcpListener on 192.168.64.1:<port>      │
│      ├── connect.rs  CONNECT / cleartext-HTTP forwarder      │
│      ├── ca.rs       persistent root CA (per-machine)        │
│      ├── leaf.rs     on-demand SNI leaf cert mint (LRU)      │
│      ├── mitm.rs     TLS-terminate, hand to handler          │
│      ├── handlers/                                           │
│      │   ├── pypi.rs     mirror logic for pypi.org           │
│      │   ├── github.rs   release-tarball cache               │
│      │   ├── npm.rs      registry.npmjs.org                  │
│      │   └── passthrough.rs   no-cache copy_bidirectional    │
│      ├── cache.rs    content-addressable disk cache          │
│      └── policy.rs   bypass / no-cache rules                 │
└──────────────────────────────────────────────────────────────┘
        │                                    │
        ▼                                    ▼
   devcontainers-cli                    wiki3-app
   (binds proxy on launch)              (talks to existing proxy)
```

Both apps embed the same crate. First-app-up wins the bind; the second
detects the listener and just consumes it. Cache directory is
`~/Library/Caches/io.devcontainers/proxy/` — shared between apps.

## Crate pick

Not pingora. Pingora's API is shaped for reverse-proxy load-balancers
(`upstream_peer`, peer pools, health checks) and forward MITM is
fighting its grain. We have two viable picks:

- **[`hudsucker`](https://crates.io/crates/hudsucker)** — purpose-built
  for forward HTTP/S MITM with a CA. Small maintainer, limited release
  cadence, but the API is exactly what we need.
- **Hand-rolled** on `tokio` + `rustls` + `hyper`. ~500 LoC including
  cache and CA. More code to own but no external API churn risk.

**Recommendation:** start hand-rolled. The MITM forward-proxy loop is
small and well-understood, and we want full control over the cache
hooks anyway.

## Phased rollout

The work splits cleanly into independent phases. Each phase is
shippable on its own.

### Phase 1 — replace Squid (no caching, no MITM)

Just a CONNECT-only forward proxy bound on `192.168.64.1:<port>`. Each
CONNECT becomes `tokio::io::copy_bidirectional` to the upstream host.
Plain HTTP requests are forwarded with hyper.

What this gets us:
- Squid container goes away.
- `HTTP_PROXY`/`HTTPS_PROXY` auto-injected into every container we
  spawn (already half-implemented: `merge_proxy_build_args` runs at
  `up_inner`).
- Same caching behavior as today (= none — Squid wasn't caching
  HTTPS anyway).

Estimated work: ~1 day.

**Deliverables**
- `crates/devcontainer-proxy/` with `server`, `connect`, `policy`.
- Tauri startup hook that binds the listener and exposes `Proxy::url()`.
- Lifecycle integration: when no host `HTTP_PROXY` is set and the
  internal proxy is running, inject *its* URL into the container env.

### Phase 2 — per-protocol caching reverse-mirrors (no MITM yet)

Skip CA-MITM entirely for the highest-value protocols:

| Protocol | Mechanism | What containers see |
| --- | --- | --- |
| pip | local PEP-503 simple-index mirror | `PIP_INDEX_URL=http://192.168.64.1:<port>/pypi/simple/` |
| npm | local registry mirror | `npm_config_registry=http://192.168.64.1:<port>/npm/` |
| GitHub release tarballs | URL-canonicalising cache (strips signed-S3 query params from cache key) | unchanged URLs; we publish a self-signed cert under a domain we ship in the container's trust store |

This avoids the "MITM every TLS connection" problem. We only intercept
the URLs we know how to cache, by being a *named* HTTPS endpoint
(`http://192.168.64.1:<port>/...`) rather than a generic proxy.

The catch: `git clone https://github.com/...` and `curl` of release
tarballs need TLS, which means we *do* need a CA — but only one that
signs `192.168.64.1` / our chosen mirror hostname. That's a much
smaller blast radius than MITM-everything.

**Cache layout**
- Content-addressed by SHA-256 of canonicalised URL + Vary headers.
- LRU eviction when cache dir > configured cap (default 20 GB).
- Negative-cache for 404s with short TTL (avoids hammering upstream
  when a wheel is genuinely missing).
- Redirect-following on the proxy side: a request for
  `github.com/.../releases/download/...` follows to the signed S3 URL,
  but the cache key is the canonical github.com URL. Solves the
  GitHub-403-cache-poisoning problem we hit with Squid.

Estimated work: ~2 days.

### Phase 3 — generic MITM caching

If Phase 2 isn't enough (e.g. crates.io, RubyGems, conda), add a
hudsucker-style or hand-rolled MITM path.

This requires:
1. Persistent root CA in app data dir (generated once, reused).
2. CA installation into containers at `up` time:
   - Detect distro from `/etc/os-release`.
   - Debian/Ubuntu: copy to `/usr/local/share/ca-certificates/` →
     `update-ca-certificates`.
   - RHEL/Fedora: copy to `/etc/pki/ca-trust/source/anchors/` →
     `update-ca-trust`.
   - Alpine: copy to `/usr/local/share/ca-certificates/` →
     `update-ca-certificates`.
   - Distroless / scratch: skip + emit warning; container falls back
     to direct-passthrough for those domains.
3. Per-SNI leaf certs minted on demand, cached in memory (LRU, 1k
   entries).
4. Same cache layout as Phase 2; new handler for "just cache it" with
   no protocol-specific knowledge.

Failure mode: if CA install fails we downgrade gracefully — the proxy
becomes a transparent CONNECT forwarder for that container.

Estimated work: ~3 days.

### Phase 4 — coordination and lifecycle

- Move the proxy to a separate launchd service
  (`io.devcontainers.proxy.plist`) so a single instance serves all
  apps.
- Both apps probe for the existing service on startup; install +
  bootstrap if missing.
- Cache dir stays shared.
- Stats / debug UI in devcontainers-cli (hit rate, current cache
  size, recent requests).

Estimated work: ~1 day.

## Bypass / no-cache policy

Never proxied:
- `localhost`, `127.0.0.1`, `::1`, `192.168.64.0/24` — direct.
- The runtime's own image-registry pulls — those go through the
  Apple Container CLI, not via our proxy.
- Anything resolving to RFC1918 / link-local / loopback.

Never cached:
- Non-GET methods.
- Responses with `Cache-Control: no-store`, `private`, or
  `Authorization`/`Cookie` request headers (unless the handler
  explicitly opts in — pip's mirror does, since indices are public).
- Responses > 2 GB (configurable).

## CA strategy — answer to "can we lean on the host's CA?"

Short answer: not really, and here's why.

Containers have their own `/etc/ssl/certs` trust store, distinct from
the macOS Keychain. To present a cert for `pypi.org` that the
container will trust during MITM, the cert has to be signed by a CA
the container trusts — and no public CA will sign `pypi.org` for us.

Three options to dodge that:

1. **Don't MITM** (Phase 2 strategy). Be a *named* mirror endpoint
   that containers reach by URL, not a transparent proxy. We still
   need a CA but only for *our* hostname, and we install that one cert
   into the container.
2. **Generate a per-machine CA** (Phase 3 strategy). Install it into
   each container at `up` time. Standard practice; well-understood;
   blast-radius is "this one machine's containers".
3. **Use mkcert's CA** if the user has it. mkcert generates a local
   CA already trusted by the macOS Keychain. We could write our leaf
   certs against it. Doesn't help inside containers (Keychain ≠
   container trust store) but does help with host-side tooling that
   talks to the proxy. Niche.

Recommendation: **(1)** for Phase 2 and **(2)** for Phase 3.

## Open questions

- **Proxy port:** static (e.g. 31280) or ephemeral with discovery?
  Static is simpler; ephemeral avoids conflicts. Lean static, document
  as configurable.
- **TLS for the listener itself:** containers expect plain HTTP for
  their forward proxy (`http://192.168.64.1:port`). MITM termination
  happens on the *upstream* side, not the listener. Listener stays
  cleartext.
- **What do we expose to the user?** A status pill in the dashboard
  (cache size, hit rate)? A button to flush cache? A list of recently
  cached URLs? Nice-to-have, not blocking.
- **How does the user opt out for a specific workspace?** Probably a
  `customizations.devcontainersCli.proxy: false` flag in
  devcontainer.json. Container would then get *no* `HTTP_PROXY` env
  and go direct.

## Out of scope (for now)

- Authenticated upstreams (private registries with creds). We'd
  passthrough those without caching.
- Per-workspace cache isolation. Single shared cache.
- Cross-machine cache sharing. The README mentions IPFS as a possible
  future direction; not pursuing that here.

## Concrete next step

Start Phase 1: ~1 day of work, removes the Squid dependency, lets us
exercise the auto-injection plumbing end-to-end before we get into
the CA / MITM weeds.
