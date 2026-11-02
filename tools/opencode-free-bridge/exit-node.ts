// CONNECT-only exit that adds this host's public IP to the bridge exit pool.
// Bind to a tailnet address; the target allowlist keeps it from being an open proxy.
// Usage: bun exit-node.ts --listen 100.x.y.z:18890

import { connect, createServer } from "node:net";

const args = process.argv.slice(2);
const listen = args[args.indexOf("--listen") + 1] ?? "127.0.0.1:18890";
const [host, portText] = listen.split(":");
const ALLOWED = new Set(["opencode.ai:443", "api.ipify.org:443"]);

const server = createServer((client) => {
  client.once("data", (head) => {
    const line = head.toString("latin1").split("\r\n", 1)[0];
    const match = line.match(/^CONNECT ([^\s]+) HTTP\/1\.[01]$/);
    if (!match || !ALLOWED.has(match[1])) {
      client.end("HTTP/1.1 403 Forbidden\r\n\r\n");
      return;
    }
    const [targetHost, targetPort] = match[1].split(":");
    const upstream = connect(Number(targetPort), targetHost, () => {
      client.write("HTTP/1.1 200 Connection Established\r\n\r\n");
      client.pipe(upstream);
      upstream.pipe(client);
    });
    upstream.on("error", () => client.destroy());
    client.on("error", () => upstream.destroy());
  });
});

server.listen(Number(portText), host, () => {
  console.log(`opencode exit node listening on ${host}:${portText}`);
});
