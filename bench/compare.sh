#!/usr/bin/env bash
# Reproducible same-host loopback transport test. No Python and no Cloudflare.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
CHISEL="$(printenv CHISEL_BINARY || echo chisel)"
RATHOLE="$(printenv RATHOLE_BINARY || echo rathole)"
REQUESTS="$(printenv BENCH_REQUESTS || echo 600)"
CONCURRENCY="$(printenv BENCH_CONCURRENCY || echo 12)"
BYTES="$(printenv BENCH_PAYLOAD || echo 65536)"
KEEPALIVE="$(printenv BENCH_KEEPALIVE || echo false)"
OUT="$(printenv BENCH_OUTPUT || echo "$HERE/results/latest.jsonl")"
D="$(mktemp -d /tmp/argus-bench.XXXXXX)"
srv=;cli=;origin=;sample=
cleanup() {
  for pid in "$sample" "$cli" "$srv" "$origin"; do
    if [ -n "$pid" ]; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
  done
  rm -rf "$D"
}
trap cleanup EXIT
mkdir -p "$(dirname "$OUT")"
: > "$OUT"
command -v node >/dev/null
test -x "$CHISEL"
test -x "$RATHOLE"
PAYLOAD_BYTES="$BYTES" ORIGIN_PORT=27882 node "$HERE/http-origin.cjs" >"$D/origin.log" 2>&1 & origin=$!
for i in $(seq 1 20); do
  code=$(curl --noproxy '*' -sS --max-time 1 -o /dev/null -w '%{http_code}' http://127.0.0.1:27882/ 2>/dev/null || true)
  if [ "$code" = 200 ]; then break; fi
  sleep .1
done
test "$code" = 200 || { echo "origin not ready"; exit 5; }
keypair="$("$RATHOLE" --genkey)"
priv="$(printf '%s\n' "$keypair" | grep -A1 'Private Key:' | tail -n1)"
pub="$(printf '%s\n' "$keypair" | grep -A1 'Public Key:' | tail -n1)"
token="$(openssl rand -hex 24)"
"$CHISEL" server --keygen "$D/chisel.key" >/dev/null 2>&1
cat > "$D/rathole-server.toml" <<EOF
[server]
bind_addr = "127.0.0.1:27880"
[server.transport]
type = "noise"
[server.transport.noise]
local_private_key = "$priv"
[server.services.web]
token = "$token"
bind_addr = "127.0.0.1:27881"
EOF
cat > "$D/rathole-client.toml" <<EOF
[client]
remote_addr = "127.0.0.1:27880"
[client.transport]
type = "noise"
[client.transport.noise]
remote_public_key = "$pub"
[client.services.web]
token = "$token"
local_addr = "127.0.0.1:27882"
EOF
printf '{"u:%s":["^R:127\\\\.0\\\\.0\\\\.1:27881$"]}\n' "$token" > "$D/chisel-users.json"
rss() { ps -o rss= -p "$1" | tr -d ' '; }
ticks() { awk '{print $14+$15}' "/proc/$1/stat"; }
wait_route() {
  local body=
  for i in $(seq 1 40); do
    body="$(curl --noproxy '*' -fsS --max-time 1 http://127.0.0.1:27881/ 2>/dev/null | wc -c || true)"
    if [ "$body" = "$BYTES" ]; then return 0; fi
    sleep .15
  done
  echo "ENGINE_ROUTE_FAILED:$(tail -n 6 "$D/server.log"; tail -n 6 "$D/client.log")" >&2
  return 1
}
for engine in chisel rathole; do
  if [ "$engine" = chisel ]; then
    "$CHISEL" server --host 127.0.0.1 --port 27880 --reverse --keyfile "$D/chisel.key" --authfile "$D/chisel-users.json" >"$D/server.log" 2>&1 & srv=$!
    fingerprint=
    for i in $(seq 1 30); do fingerprint="$(grep -oE 'Fingerprint [A-Za-z0-9+/]{43}=' "$D/server.log" | head -n1 | cut -d' ' -f2 || true)";test -n "$fingerprint" && break;sleep .1;done
    test -n "$fingerprint"
    AUTH="u:$token" "$CHISEL" client --fingerprint "$fingerprint" --keepalive 25s --max-retry-interval 1s http://127.0.0.1:27880 R:127.0.0.1:27881:127.0.0.1:27882 >"$D/client.log" 2>&1 & cli=$!
  else
    "$RATHOLE" "$D/rathole-server.toml" >"$D/server.log" 2>&1 & srv=$!
    sleep .2
    "$RATHOLE" "$D/rathole-client.toml" >"$D/client.log" 2>&1 & cli=$!
  fi
  wait_route
  node "$HERE/http-load.cjs" http://127.0.0.1:27881/ 30 5 "$BYTES" "$KEEPALIVE" >"$D/warmup"
  idle_server_rss=$(rss "$srv")
  idle_client_rss=$(rss "$cli")
  before_srv="$(ticks "$srv")"
  before_cli="$(ticks "$cli")"
  (while kill -0 "$srv" "$cli" 2>/dev/null; do
    printf '%s %s\n' "$(rss "$srv")" "$(rss "$cli")"
    sleep .1
  done) >"$D/rss-$engine" 2>/dev/null & sample=$!
  node "$HERE/http-load.cjs" http://127.0.0.1:27881/ "$REQUESTS" "$CONCURRENCY" "$BYTES" "$KEEPALIVE" >"$D/load-$engine.json"
  kill "$sample" 2>/dev/null || true
  wait "$sample" 2>/dev/null || true
  sample=
  after_srv="$(ticks "$srv")"
  after_cli="$(ticks "$cli")"
  peak_server_rss=$(awk '{if($1>m)m=$1}END{print m+0}' "$D/rss-$engine")
  peak_client_rss=$(awk '{if($2>m)m=$2}END{print m+0}' "$D/rss-$engine")
  server_cpu_ticks=$((after_srv-before_srv))
  client_cpu_ticks=$((after_cli-before_cli))
  BENCH_ENGINE="$engine" BENCH_IDLE_SRV="$idle_server_rss" BENCH_IDLE_CLI="$idle_client_rss" BENCH_PEAK_SRV="$peak_server_rss" BENCH_PEAK_CLI="$peak_client_rss" BENCH_CPU_SRV="$server_cpu_ticks" BENCH_CPU_CLI="$client_cpu_ticks" BENCH_CPU_HZ="$(getconf CLK_TCK)" BENCH_RUN_FILE="$D/load-$engine.json" node -e '
const fs=require("fs");
const b=JSON.parse(fs.readFileSync(process.env.BENCH_RUN_FILE,"utf8"));
const n=k=>Number(process.env[k]);
console.log(JSON.stringify({
  engine:process.env.BENCH_ENGINE,
  transport:process.env.BENCH_ENGINE==="rathole"?"Noise NK (encrypted)":"Chisel SSH over HTTP (encrypted)",
  topology:"same-host loopback, without Cloudflare/Argus router",
  idleServerRSSMiB:Number((n("BENCH_IDLE_SRV")/1024).toFixed(2)),
  idleClientRSSMiB:Number((n("BENCH_IDLE_CLI")/1024).toFixed(2)),
  peakServerRSSMiB:Number((n("BENCH_PEAK_SRV")/1024).toFixed(2)),
  peakClientRSSMiB:Number((n("BENCH_PEAK_CLI")/1024).toFixed(2)),
  serverCpuSeconds:Number((n("BENCH_CPU_SRV")/n("BENCH_CPU_HZ")).toFixed(3)),
  clientCpuSeconds:Number((n("BENCH_CPU_CLI")/n("BENCH_CPU_HZ")).toFixed(3)),
  ...b
}));' | tee -a "$OUT"
  kill "$cli" "$srv" 2>/dev/null || true
  wait "$cli" "$srv" 2>/dev/null || true
  cli=;srv=
  sleep .35
done
echo "BENCH_RESULTS=$OUT"
