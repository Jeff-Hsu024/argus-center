const http = require("node:http");
const port = Number(process.env.ORIGIN_PORT || 27882);
const payload = Buffer.alloc(Number(process.env.PAYLOAD_BYTES || 65536), 0x61);
http.createServer((req, res) => {
  res.setHeader("content-type", "application/octet-stream");
  res.setHeader("content-length", String(payload.length));
  res.end(payload);
}).listen(port, "127.0.0.1", () => console.log("BENCH_ORIGIN_READY=" + port));