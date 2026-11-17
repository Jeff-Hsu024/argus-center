const http = require("node:http");
const { performance } = require("node:perf_hooks");
const [target, requests = "500", concurrency = "12", expectedBytes = "65536", reuseArg = "false"] = process.argv.slice(2);
if (!target) throw new Error("usage: node http-load.js URL REQUESTS CONCURRENCY EXPECTED_BYTES");
const count = Number(requests), parallel = Number(concurrency), expected = Number(expectedBytes);
const url = new URL(target);
let started = 0, completed = 0, failed = 0, received = 0;
const samples = [];
const keepAlive = reuseArg === "true";
const agent = new http.Agent({ keepAlive, maxSockets: parallel });
const start = performance.now();
async function once() {
  const t = performance.now();
  return new Promise(resolve => {
    const req = http.get(url, { agent, timeout: 4000 }, res => {
      let len = 0;
      res.on("data", b => { len += b.length; });
      res.on("end", () => {
        received += len;
        if (res.statusCode !== 200 || len !== expected) failed++;
        samples.push(performance.now() - t);
        completed++;
        resolve();
      });
      res.on("error", () => { failed++; completed++; resolve(); });
    });
    req.on("timeout", () => req.destroy(new Error("timeout")));
    req.on("error", () => { failed++; completed++; resolve(); });
  });
}
async function worker() {
  for (;;) {
    if (started++ >= count) return;
    await once();
  }
}
(async () => {
  await Promise.all(Array.from({ length: parallel }, worker));
  agent.destroy();
  samples.sort((a,b) => a-b);
  const percentile = n => samples.length ? Number(samples[Math.min(samples.length-1, Math.floor((samples.length-1)*n))].toFixed(3)) : null;
  const seconds = (performance.now()-start) / 1000;
  console.log(JSON.stringify({
    requested: count, completed, failed, concurrency: parallel, expectedBytes: expected, keepAlive,
    receivedBytes: received, elapsedSeconds: Number(seconds.toFixed(4)),
    requestsPerSecond: Number((completed/seconds).toFixed(1)),
    throughputMiBps: Number((received/1048576/seconds).toFixed(2)),
    p50ms: percentile(.5), p95ms: percentile(.95), p99ms: percentile(.99)
  }));
  if (failed) process.exitCode = 2;
})().catch(e => { console.error(e); process.exitCode = 3; });