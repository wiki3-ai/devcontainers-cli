# Host network compatibility plan

Goal: make Apple Container–based dev containers work on corporate Macs that ship
with network filters / ZTNA agents (FortiClient, Zscaler, Cisco AnyConnect /
Secure Client, Netskope, Crowdstrike Falcon) and on machines with leftover
container runtimes (Docker Desktop `vmnetd`, vpnkit, lima, podman-machine,
qemu). On the affected M2 MacBook Air the vmnet bridge itself is healthy
(`192.168.64.1` pings, container has a valid IPv4 lease) but Apple Container's
host-side `127.0.0.1` publish-proxy is being killed by the filter, while the
direct vmnet path to the container still works. That observation drives the
strategy below.

## Strategy

Prefer the direct container IP when the publish-proxy is unreachable, detect
known offenders early, and stop forcing host DNS into the container.

### 1. Drop `--dns` flags on Apple Container build/run

- Today we pass `--dns <host-resolver-entry>` per resolver. On Tahoe these
  sometimes include IPv6 link-local / ULA addresses that fail Apple
  Container's `configureDns` step (observed crash:
  `failed to bootstrap container ... cause: configureDns`).
- Apple Container ships its own resolvers on `127.0.0.1:1053` (localhost) and
  `127.0.0.1:2053` (container hostnames). Letting it own DNS removes the
  failure mode and avoids leaking corporate DNS (often poisoned by Forti /
  Zscaler) into the container.
- Action: stop emitting `--dns` flags from the Apple Container backend. Keep
  `--dns-search` only if explicitly configured by the user.

### 2. Health-probe after `container start`; pick a working URL

After publishing ports, race two probes in parallel for each published TCP
port:

- `127.0.0.1:<hostPort>` (Apple Container publish-proxy)
- `<container_ipv4>:<containerPort>` (direct via vmnet bridge)

Whichever responds first becomes the canonical "service URL" recorded for the
container. If only the direct IP responds, log a single actionable line:

> Host loopback proxy unreachable (likely a network filter / ZTNA agent).
> Using direct container address `http://192.168.64.4:8000/` instead.

If neither responds within the timeout, surface the existing failure plus the
preflight findings (#3).

### 3. Preflight detector

In `cli_helpers::ensure_service_running` (and on container up), scan for known
offenders and emit a structured warning the UI can render. No mutations — only
detection.

Detection sources:

- File presence under `/Library/LaunchDaemons`,
  `/Library/PrivilegedHelperTools`, `/Library/SystemExtensions`.
- `systemextensionsctl list` (network filter extensions).
- Process scan for `com.docker.vmnetd`, `vpnkit`, `lima`, `podman-machine`,
  `qemu`.

Known offenders to flag:

| Vendor / agent | Symptom |
| --- | --- |
| Docker Desktop `com.docker.vmnetd` | vmnet enters `--variant reserved`; bridge starves |
| FortiClient (`com.fortinet.forticlient.*`) | RST on `127.0.0.1` publish-proxy; bridge stalls if ZTNA enforces |
| Zscaler (`com.zscaler.*`) | Same class of RST / hijack |
| Cisco AnyConnect / Secure Client (`com.cisco.anyconnect.*`) | DNS hijack + loopback filter |
| Netskope (`com.netskope.*`) | TLS interception + filter |
| Crowdstrike Falcon (`com.crowdstrike.falcon.*`) | Connection blocking under EDR rules |
| vpnkit / lima / podman-machine / qemu helpers | vmnet contention |

Output: a single structured `PreflightFinding { severity, vendor, message,
remediation_url }` per hit, surfaced to the Logs panel and exposed to
`wiki3-app`.

### 4. Surface the working URL to `wiki3-app`

The wiki card / "Open" button uses the URL chosen by #2 instead of a hardcoded
`http://127.0.0.1:<port>`. JupyterLite's service worker is fine registering
against `192.168.64.4` as long as we pin to a single host per container —
which is exactly what #2 records.

### 5. Manual override ("corp-laptop mode")

Settings toggle in `wiki3-app`: **Container address** = `Auto` (default) /
`Loopback` / `Direct`. Users on locked-down corporate machines can force
`Direct` and skip the probe entirely.

## Sequencing

1. **#1 (drop `--dns`)** — small, fixes the IPv6 `configureDns` crash, no
   downside.
2. **#2 + #4 (probe + use working URL)** — the actual compatibility win;
   makes FortiClient / Zscaler users productive without disabling anything.
3. **#3 (preflight warnings)** — diagnostics layer.
4. **#5 (manual override)** — only if the auto-probe proves flaky in the
   field.

## Out of scope

- Mutating system state (unloading kexts, killing daemons). The CLI must
  never touch IT-managed agents.
- Re-implementing publish-proxy. We rely on Apple Container's; we just route
  around it when the host blocks loopback.
