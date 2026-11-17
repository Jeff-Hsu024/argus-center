# Argus Center (Rust)

A lightweight, independent reverse-relay **center**. One HTTP/WSS ingress handles any number of registered machines and application services. The optional reverse-transport engine is Chisel 1.12 in the first POC, launched as a managed subprocess. No Python runtime or per-VM Cloudflare route is required.

## Architecture

```
Cloudflare Tunnel (one-time hostname -> http://127.0.0.1:18999)
  -> Argus Center router
     /_wire/{pair} -> Chisel reverse ingress on 127.0.0.1:18998
     /s/{pair}/{service}/{path} -> per-pair loopback reverse listener
     /api/enroll/{one_time_token} -> enrollment, single use
  -> Chisel reverse -> Argus Client -> VM-local service
```

Native TCP/SSH services are never exposed as an unprotected HTTP path. They require an authenticated viewer tunnel (future adapter); initial release only handles HTTP/WebSocket service routing. Orca's own E2EE pairing is separate from Argus producer enrollment. **Do not treat /healthz or WebSocket 101 as authenticated Orca acceptance.**

## Quick start

```sh
cargo build --release
argus-center --state ./state init --public-url https://argus.example.com --listen 127.0.0.1:18999 --chisel-port 18998 --chisel /path/to/chisel
argus-center --state ./state serve
# Separate terminal after center ready:
argus-center --state ./state pair-create --name example-client --service orca:6768
argus-center --state ./state pair-create --name muse --service orca:6768
argus-center --state ./state status
```

`pair-create` prints a single-use 15-minute enrollment command. **It does not store the enrollment token plaintext.** Argus Client uses it to persist connection credentials locally; never commit state directories. 
"service-add" updates the authfile and pair revision; existing clients poll /api/sync and restart their own reverse transport.

## v0.2: Health and watchdog

Use the **same public hostname** for all HTTP/WS services, with paths `/s/{pair}/{service}`. A new service does not require a Cloudflare DNS/hostname change. Monitor loopback reverse listeners with `argus-center --state ./state status`; health snapshots refresh every 30 seconds.

Opt in a service watchdog by providing an audited Bash restore script (maximum 16 KiB):

```sh
argus-center --state ./state watchdog-set --pair muse --service orca --script ./safe-restore-orca.sh --cooldown-secs 300
argus-center --state ./state service-add --pair muse --service queqiao:12997
argus-center --state ./state install-service
```

Watchdog script contents and SHA-256 are delivered on initial enrollment and subsequent sync. The client verifies the checksum, stores an executable private script, and restarts **only its own configured service** after two failed TCP checks with cooldown. The center does not directly control a remote VM shell; explicitly registered scripts are the intended restore interface.

Integration test (`node` only for a disposable mock HTTP service, **not** needed to run Argus):

```sh
cargo build --release
(cd ../argus-client && cargo build --release)
ARGUS_CHISEL=/path/to/chisel bash ./tests/e2e.sh
```

### First measured transport baseline (2026-11-17)

The [reproducible loopback Chisel vs Rathole report](docs/2026-11-17-transport-benchmark.md) includes three 6,000-request rounds for both cold and persistent TCP, with 72,000 HTTP responses and no observed errors. In this controlled fixture, Rathole Noise NK had a smaller idle RSS but a persistent ~44 ms HTTP keepalive delay; Chisel 1.12.0 outperformed Rathole 0.5.0 for the measured request workload. **No engine migration** is justified on this evidence. All results and source test scripts are public under `bench/`.

### Engine roadmap

`TransportAdapter` is the next abstraction: keep the application-level pair/service registry stable while benchmarking Chisel and Rathole under identical workloads. Rathole is **not** yet implemented; do not interpret HTTP health checks as a Rathole comparison.


## Security and caveats

- The control-plane endpoints use HTTPS when crossing the internet, single-use hashed enrollment tokens, scoped producer credentials, and 127.0.0.1 reverse listeners.
- Transport and Orca use separate credentials. The Chisel server SSH key is pinned on enrollment; client refuses a missing fingerprint.
- Existing Oracle/Cloudflare infrastructure is not touched by this package.
- Once exposed, a service route is reachable to anyone with the URL **unless the upstream application implements authentication**. Only register app-authenticated HTTP/WS services during initial testing.
- No SSH-over-URL claims; native TCP requires an explicit local TCP viewer feature, intentionally not advertised as complete.
- No silent VM-replacement guarantees; ephemeral cloud VMs need their provider's startup/bootstrap invocation.
- Current implementation is a POC, not a production security audit.

## CI
Pushes build Rust on Windows and Linux; build artifacts are available from GitHub Actions.

## Operator-approved join (no token copy between agents)

For agent-managed VMs where passing an enrollment secret between remote
agents is inappropriate, the existing pair may instead accept a
client-initiated, operator-approved join. This does not change the
Chisel or Orca authentication scheme.

On the remote VM:

    argus-client --state-dir /private/argus-state join --center https://argus.example.com --pair muse

This creates a private 256-bit random nonce on the Client and submits only
its SHA-256 commitment. The Client displays ARGUS_JOIN_PENDING with a public
64-character proof and waits up to 15 minutes. On the Center, confirm the
proof exactly matches the one seen in the intended VM's trusted terminal:

    argus-center --state /private/center-state join-list --pair muse
    argus-center --state /private/center-state join-approve --pair muse --proof <64-hex-commitment>

Approval is single-use. The nonce remains in Client process memory and is
submitted only over HTTPS directly to the designated Center. Without
approval, claim returns HTTP 202; unknown nonces return HTTP 401.
Pending requests are bounded per pair.
