#!/usr/bin/env bash
# Argus v0.2 end-to-end check, isolated to loopback (no production endpoints).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
C="$ROOT/target/release/argus-center"
A="$ROOT/../argus-client/target/release/argus-client"
CH="$(printenv ARGUS_CHISEL || echo chisel)"
D="$(mktemp -d /tmp/argus-v02.XXXXXX)"
P=;S=;N=
cleanup(){ for p in "$P" "$S" "$N"; do if [ -n "$p" ];then kill "$p" 2>/dev/null||true;fi;done;rm -rf "$D"; }
trap cleanup EXIT
command -v node >/dev/null || { echo "Node.js needed for test-only mock origin"; exit 2; }
node -e 'require("http").createServer((q,s)=>s.end("ARGUS_V02_OK")).listen(28996,"127.0.0.1")' >/dev/null 2>&1 & N=$!
"$C" --state "$D/center" init --public-url http://127.0.0.1:28999 --listen 127.0.0.1:28999 --chisel-port 28998 --chisel "$CH" >/dev/null
"$C" --state "$D/center" pair-create --name testvm --service demo:28996 > "$D/invite"
"$C" --state "$D/center" service-add --pair testvm --service probe:28997 >/dev/null
printf '#!/bin/sh\necho restored > %s/marker\nexit 0\n' "$D" > "$D/safe.sh"
"$C" --state "$D/center" watchdog-set --pair testvm --service probe --script "$D/safe.sh" --cooldown-secs 60 >/dev/null
"$C" --state "$D/center" serve >"$D/center.log" 2>&1 & S=$!
for i in $(seq 1 30);do test -s "$D/center/fingerprint.txt" && break;sleep .2;done
token="$(sed -n 's/^ENROLL_TOKEN=//p' "$D/invite")"
"$A" --state-dir "$D/client" enroll --center http://127.0.0.1:28999 --token "$token" >/dev/null
test -s "$D/client/watchdogs/testvm-probe.sh"
"$A" --state-dir "$D/client" watchdog --pair testvm --check-only 2>&1 | grep -q CONFIGURED=true
test ! -e "$D/marker"
"$A" --state-dir "$D/client" watchdog --pair testvm 2>&1 | grep -q SCRIPT_FINISHED_BUT_UNHEALTHY
test -f "$D/marker"
"$A" --state-dir "$D/client" watchdog --pair testvm 2>&1 | grep -q COOLDOWN
echo ARGUS_WATCHDOG_INJECTION_AND_COOLDOWN=PASS
"$A" --state-dir "$D/client" run --pair testvm --chisel "$CH" >"$D/client.log" 2>&1 & P=$!
passed=false
for i in $(seq 1 25);do
  body="$(curl --noproxy '*' -sS --max-time 1 http://127.0.0.1:28999/s/testvm/demo 2>/dev/null||true)"
  if [ "$body" = ARGUS_V02_OK ];then passed=true;break;fi
  sleep .25
done
test "$passed" = true || { echo ARGUS_V02_HTTP_FAILED; tail -n 8 "$D/client.log"; exit 11; }
echo ARGUS_V02_FULL_REVERSE_AND_HTTP=PASS
