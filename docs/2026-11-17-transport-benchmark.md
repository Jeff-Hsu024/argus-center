# Argus Transport Baseline — Chisel 1.12.0 vs Rathole 0.5.0

Date: **2026-11-17 (Asia/Taipei)**. Status: **measured locally; no Oracle migration**.

## Scope and method

This is an initial **same-host loopback** comparison of transport subprocesses, separate from Argus Center/Cloudflare overhead. Both candidates forward a single 127.0.0.1 HTTP origin through one reverse TCP service and both secure the transport:

- Chisel **1.12.0**: SSH-over-HTTP, pinned server fingerprint, scoped reverse authentication.
- Rathole **0.5.0**: Noise NK authenticated encryption, separate service token.
- Host: Windows WSL2 Ubuntu, kernel `6.18.58.1-microsoft-standard-WSL2`, Linux x86_64.
- Source: `bench/compare.sh`, Node v22.22.1 synthetic 64 KiB response body.
- 3 independent rounds per connection mode, each **6000 HTTP requests at concurrency 16** per engine, 65,536 bytes/response; 12 measured batches / **72,000 responses**, all reported zero failures.
- Connection modes: (a) **cold TCP**: HTTP `Connection: close` / no keepalive; (b) **persistent**: HTTP keepalive enabled.
- Server/client idle RSS and sampled peak RSS collected via `ps -o rss`; CPU seconds from `/proc/<pid>/stat`; throughput & latency (p50/p95/p99) from Node monotonic timer. Per-engine process sampling is coarse and peak is not a guaranteed maximum.
- No performance claims about actual cross-region cloud links, Cloudflare, Oracle, availability, watchdog, SSH or Orca pairing are made by this test.

### Results (mean of 3 independent rounds)

| Test | Engine | Idle server/client RSS MiB | Peak server/client RSS MiB | Req/s | MiB/s | p50 ms | p95 ms | p99 ms |
| --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: |
| **Cold TCP** | Chisel | 17.00 / 15.03 | 20.88 / 20.73 | **6393.13** | **399.57** | 2.32 | **4.23** | **6.74** |
| Cold TCP | Rathole Noise | **13.49 / 14.01** | 24.19 / 22.43 | 3617.10 | 226.07 | **0.55** | 43.99 | 48.00 |
| **Persistent TCP** | Chisel | 17.07 / 13.11 | 19.84 / 18.83 | **8041.93** | **502.62** | **1.79** | **3.27** | **5.55** |
| Persistent TCP | Rathole Noise | **12.23 / 12.31** | **14.50 / 14.26** | 355.30 | 22.21 | 44.25 | 48.45 | 49.06 |

### Diagnostics and interpretation

1. **Rathole had lower idle RSS in both test modes** (combined idle RSS ~24.54 MiB versus ~30.18 MiB for Chisel under keepalive), but **higher peak sampled RSS under cold TCP** (Rathole ~46.62 MiB combined versus Chisel ~41.61 MiB). The lower-memory claim must be conditioned on workload, not assumed universally.
2. **Rathole 0.5.0 had a persistent ~44–49 ms HTTP keepalive delay** in this Linux loopback fixture. A separate 400-request diagnostic using Rathole **unencrypted TCP** reproduced ~44 ms p50 with keepalive, so this delay is *not shown to be caused by Noise encryption*. Delayed ACK/buffering is a hypothesis only; this test did **not** collect TCP packet traces to prove the kernel mechanism.
3. Chisel performed better under this particular HTTP-request stress fixture, especially persistent connections. A realistic Orca WebSocket tunnel is **not equivalent** to repeatedly requesting 64 KiB HTTP bodies; do not generalize request throughput as application performance.
4. Both engines were tested without Argus HTTP reverse router. Rathole's upstream WebSocket client constructs a `ws://<host:port>` URL without a configurable path (`src/transport/websocket.rs`, `WebsocketTransport::connect`, source inspected 2026-11-17). A single-URL Argus deployment using Rathole would require routing the WebSocket **root** to one Rathole server or a patched adapter to select non-root paths; no Rathole-over-Cloudflare production claim is made here.
5. **Decision for current deployment: retain Chisel Core**. Rathole remains a candidate for future tuning, but current data do not justify changing the NB or Oracle running transport. Oracle was not modified.

### Reproduction

Requirements: Node.js (test fixture only), Chisel 1.12, Rathole 0.5, OpenSSL, curl, GNU/Linux `/proc`. **No Python dependency**. No Argus package build is required to compare transport subprocesses.

```sh
cd argus-center
CHISEL_BINARY=/path/to/chisel \
RATHOLE_BINARY=/path/to/rathole \
BENCH_REQUESTS=6000 BENCH_CONCURRENCY=16 \
BENCH_KEEPALIVE=false \
BENCH_OUTPUT=bench/results/custom-cold.jsonl \
bash bench/compare.sh

# persistent HTTP
CHISEL_BINARY=/path/to/chisel \
RATHOLE_BINARY=/path/to/rathole \
BENCH_REQUESTS=6000 BENCH_CONCURRENCY=16 \
BENCH_KEEPALIVE=true \
BENCH_OUTPUT=bench/results/custom-keepalive.jsonl \
bash bench/compare.sh
```

Raw datasets: `bench/results/round-{1,2,3}.jsonl`, `bench/results/keepalive-{1,2,3}.jsonl`. Scripts generate ephemeral random credentials and delete them on completion; the committed JSONL results contain no tokens or keys.

## Separate multi-node acceptance evidence (not this benchmark)

- NB Windows Argus Center 0.2.0 at `https://argus-nb.hsuzero.com` and a dedicated test Cloudflare Tunnel; no Oracle changes. PC→NB `/healthz` HTTP 200 was confirmed.
- Muse briefly returned `remote_runtime_unavailable` and NB listener health `false`, then its Orca runtime reconnected with a new runtime ID. Argus Client **0.2.1** was installed there as a boot-enabled **systemd system service** because `systemctl --user` returned `Failed to connect to bus: No medium found`. Fresh verification: `systemctl is-enabled`=`enabled`, `is-active`=`active`; forced **SIGKILL** of only `argus-client-muse.service` yielded `NRestarts=1`, a new `ExecMainPID`, and PC→NB→Muse HTTP 200 plus WSS upgrade 101 after recovery. This proves service restart/reconnect, **not** a full VM reboot.
- Codex VM had successfully cloned/installed Argus Client in the earlier session, but 2026-11-17 current repeat curl through its configured egress proxy returned **CONNECT 403 for `argus-nb.hsuzero.com` and `queqiao-nb.hsuzero.com`** while `oracle-relay.hsuzero.com` answered HTTP 404. The denial occurs at the Codex outbound CONNECT proxy before reaching NB. This is an **observed egress restriction**, not a Chisel/Rathole failure and not resolved by changing engines. No alternate route was used to evade that restriction.
- Native Orca authenticated RPC over the new Argus route remains **NOT VERIFIED**. WebSocket HTTP 101 only establishes a transport upgrade. An on-VM smoke fixture (`argus-client/tests/orca-auth-route.sh`) generated and consumed an ephemeral pairing offer without disclosing it outside the VM, but its authenticated `orca status` returned `remote_runtime_unavailable` **for both the Argus hostname and the pre-existing Muse hostname**. This failure is not evidence that Argus alone is broken; the self-pairing test needs a separately established baseline before a route-specific conclusion.
- Muse systemd **boot-enabled install** and **automatic process restart** are verified; an actual **VM reboot** and provider recreation are **NOT VERIFIED**. A deleted cloud VM needs provider bootstrap.

## Next acceptance gates

- Run a fresh native Orca authenticated status/RPC test from an independent paired host specifically through the Argus path; same-host self-pairing failed on *both* the old Muse endpoint and Argus, so that comparison was inconclusive. Do not circulate actual pairing secrets through tool output.
- Obtain Codex egress permission for the new Argus hostname through its supported network-policy configuration; verify HTTP 200 before attempting enrollment. Do not reroute via an already-allowed domain merely to evade proxy 403.
- For Rathole, measure latency across real VM paths and investigate the ~44 ms delay with packet tracing, socket options, or a newer audited implementation before selecting a core.
- Extend the benchmark for persistent WebSockets, multiple VM/services, reconnect time, and idle CPU/heartbeat bytes. Keep engine selection independent of the application-level Pair/Service CLI.
