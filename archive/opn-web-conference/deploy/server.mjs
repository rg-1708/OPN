// Production server — one Node process, no nginx. It does what Vite's dev server
// does in development, so the browser stays same-origin and the app code is
// unchanged between dev and prod:
//   GET  /*      → the built SPA (app/dist), with SPA fallback to index.html
//   POST /join   → mint a Core session (holds the tenant key; see dev-auth/join.mjs)
//   WS   /ws     → reverse-proxied to Core's gateway (Origin preserved, Host rewritten)
//
// Coolify's Traefik is the sole ingress: it terminates TLS and routes the web
// domain to this process. This process is NOT a second ingress — just the app
// server that has to exist inside the container to serve files and forward /ws.
//
// Stdlib only: node:http(s) + node:fs. No dependencies.

import { createServer } from "node:http";
import http from "node:http";
import https from "node:https";
import { readFile } from "node:fs/promises";
import { extname, join, normalize, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { assertEnv, CORE_URL, handleJoin, sendJson } from "../dev-auth/join.mjs";
import { routeRooms } from "../dev-auth/rooms.mjs";

assertEnv();
const PORT = Number(process.env.PORT) || 8080;
const WEB_ROOT = process.env.WEB_ROOT
  ? normalize(process.env.WEB_ROOT)
  : fileURLToPath(new URL("../app/dist", import.meta.url));

const core = new URL(CORE_URL);
const coreLib = core.protocol === "https:" ? https : http;
const corePort = core.port || (core.protocol === "https:" ? 443 : 80);

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".jpg": "image/jpeg",
  ".ico": "image/x-icon",
  ".woff2": "font/woff2",
  ".map": "application/json; charset=utf-8",
};

async function serveStatic(req, res) {
  const url = new URL(req.url, "http://localhost");
  let pathname = decodeURIComponent(url.pathname);
  if (pathname.endsWith("/")) pathname += "index.html";
  // Resolve inside WEB_ROOT and reject any path that escapes it (traversal).
  const filePath = normalize(join(WEB_ROOT, pathname));
  if (filePath !== WEB_ROOT && !filePath.startsWith(WEB_ROOT + sep)) {
    return sendJson(res, 403, { code: "forbidden", msg: "path escapes web root" });
  }
  try {
    const buf = await readFile(filePath);
    res.writeHead(200, { "Content-Type": MIME[extname(filePath)] ?? "application/octet-stream" });
    return res.end(buf);
  } catch {
    // SPA fallback — unknown paths render index.html (client-side routing).
    try {
      const buf = await readFile(join(WEB_ROOT, "index.html"));
      res.writeHead(200, { "Content-Type": "text/html; charset=utf-8" });
      return res.end(buf);
    } catch {
      return sendJson(res, 404, { code: "not_found", msg: "not found" });
    }
  }
}

// Reverse-proxy a same-origin HTTP request to Core, forwarding the browser's
// Authorization (JWT). Used for /v1 reads (channel history) so the browser never
// needs Core's URL. Streams the body through untouched.
function proxyToCore(req, res) {
  const proxyReq = coreLib.request(
    {
      hostname: core.hostname,
      port: corePort,
      path: req.url,
      method: req.method,
      headers: { ...req.headers, host: core.host },
    },
    (coreRes) => {
      res.writeHead(coreRes.statusCode ?? 502, coreRes.headers);
      coreRes.pipe(res);
    },
  );
  proxyReq.on("error", (e) => {
    console.error(`v1 proxy: cannot reach Core (${e.message})`);
    sendJson(res, 502, { code: "internal", msg: "core unreachable" });
  });
  req.pipe(proxyReq);
}

const server = createServer((req, res) => {
  const { pathname } = new URL(req.url, "http://localhost");
  if (req.method === "GET" && pathname === "/healthz") {
    return sendJson(res, 200, { ok: true });
  }
  if (pathname === "/join") {
    if (req.method !== "POST") {
      return sendJson(res, 405, { code: "invalid", msg: "use POST /join" });
    }
    return handleJoin(req, res);
  }
  // Lobby roster (W1) — same handlers as the dev sidecar.
  if (routeRooms(req, res, pathname)) return;
  // Core REST reads (channel history): forward to Core with the browser's JWT.
  if (pathname.startsWith("/v1/")) return proxyToCore(req, res);
  return serveStatic(req, res);
});

// WebSocket reverse proxy for /ws → Core. Host is rewritten to Core's (so Core's
// Traefik routes it); Origin is preserved (so Core's per-tenant allowed_origins
// check sees the real browser origin — that origin must be allowlisted on Core).
server.on("upgrade", (req, clientSocket, head) => {
  if (!req.url.startsWith("/ws")) {
    clientSocket.destroy();
    return;
  }
  const proxyReq = coreLib.request({
    hostname: core.hostname,
    port: corePort,
    path: req.url,
    method: "GET",
    headers: { ...req.headers, host: core.host },
  });
  proxyReq.on("upgrade", (proxyRes, proxySocket) => {
    const headerLines = Object.entries(proxyRes.headers)
      .map(([k, v]) => `${k}: ${v}`)
      .join("\r\n");
    clientSocket.write(`HTTP/1.1 101 Switching Protocols\r\n${headerLines}\r\n\r\n`);
    if (head && head.length) proxySocket.write(head);
    proxySocket.pipe(clientSocket);
    clientSocket.pipe(proxySocket);
    const bail = () => {
      proxySocket.destroy();
      clientSocket.destroy();
    };
    proxySocket.on("error", bail);
    clientSocket.on("error", bail);
  });
  // Core answered WITHOUT upgrading (e.g. 403 origin not allowed). Forward the
  // status so the browser fails fast + logs it, instead of hanging on no 101.
  proxyReq.on("response", (proxyRes) => {
    console.error(`ws proxy: Core replied ${proxyRes.statusCode} (not 101) for ${req.url} — origin allowlisted on Core?`);
    const headerLines = Object.entries(proxyRes.headers)
      .map(([k, v]) => `${k}: ${v}`)
      .join("\r\n");
    clientSocket.write(
      `HTTP/1.1 ${proxyRes.statusCode} ${proxyRes.statusMessage ?? ""}\r\n${headerLines}\r\n\r\n`,
    );
    clientSocket.end();
  });
  proxyReq.on("error", (e) => {
    console.error(`ws proxy: cannot reach Core (${e.message})`);
    clientSocket.destroy();
  });
  proxyReq.end();
});

server.listen(PORT, () => {
  console.log(`opn-web serving :${PORT} — root=${WEB_ROOT} core=${CORE_URL}`);
});
