#!/usr/bin/env node
import { createInterface } from "node:readline";

const BASE = "http://127.0.0.1:18765";
const SPLAYER = "http://127.0.0.1:25884";

const tools = [
  {
    name: "zradio_status",
    description: "Get ZRadio playback status.",
    inputSchema: { type: "object", properties: {} },
  },
  {
    name: "zradio_play",
    description: "Resume or start playback in ZRadio.",
    inputSchema: { type: "object", properties: {} },
  },
  {
    name: "zradio_pause",
    description: "Pause ZRadio.",
    inputSchema: { type: "object", properties: {} },
  },
  {
    name: "zradio_next",
    description: "Skip to next track.",
    inputSchema: { type: "object", properties: {} },
  },
  {
    name: "zradio_prev",
    description: "Go to previous track.",
    inputSchema: { type: "object", properties: {} },
  },
  {
    name: "zradio_import_url",
    description: "Import a YouTube or Bilibili URL as local audio into ~/音乐 via ZRadio.",
    inputSchema: {
      type: "object",
      properties: { url: { type: "string" } },
      required: ["url"],
    },
  },
  {
    name: "zradio_search_splayer",
    description: "Search songs through the running SPlayer local API (Netease/QQ).",
    inputSchema: {
      type: "object",
      properties: {
        query: { type: "string" },
        limit: { type: "number" },
      },
      required: ["query"],
    },
  },
  {
    name: "zradio_download_splayer",
    description: "Search SPlayer and ask ZRadio to download the first playable match into ~/音乐.",
    inputSchema: {
      type: "object",
      properties: { query: { type: "string" } },
      required: ["query"],
    },
  },
];

async function post(path, body = {}) {
  const res = await fetch(BASE + path, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  return await res.text();
}

async function splayerSearch(query, limit = 8) {
  const q = encodeURIComponent(query);
  const net = await fetch(
    `${SPLAYER}/api/netease/cloudsearch?keywords=${q}&limit=${limit}&type=1`,
  ).then((r) => r.json()).catch(() => ({}));
  const songs = net?.result?.songs ?? [];
  return songs.slice(0, limit).map((s) => ({
    source: "netease",
    id: String(s.id),
    title: s.name,
    artist: (s.ar || s.artists || []).map((a) => a.name).join(" / "),
    album: (s.al || s.album || {}).name || "",
  }));
}

async function callTool(name, args = {}) {
  switch (name) {
    case "zradio_status":
      return post("/status");
    case "zradio_play":
      return post("/play");
    case "zradio_pause":
      return post("/pause");
    case "zradio_next":
      return post("/next");
    case "zradio_prev":
      return post("/prev");
    case "zradio_import_url":
      return post("/import", { url: args.url });
    case "zradio_search_splayer": {
      const hits = await splayerSearch(args.query, args.limit ?? 8);
      return JSON.stringify({ hits }, null, 2);
    }
    case "zradio_download_splayer":
      return post("/search", { query: args.query });
    default:
      throw new Error(`unknown tool ${name}`);
  }
}

function reply(id, result) {
  process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id, result }) + "\n");
}

function fail(id, message) {
  process.stdout.write(
    JSON.stringify({ jsonrpc: "2.0", id, error: { code: -32000, message } }) + "\n",
  );
}

const rl = createInterface({ input: process.stdin });
rl.on("line", async (line) => {
  if (!line.trim()) return;
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return;
  }
  const { id, method, params } = msg;
  try {
    if (method === "initialize") {
      reply(id, {
        protocolVersion: "2024-11-05",
        capabilities: { tools: {} },
        serverInfo: { name: "zradio-mcp-server", version: "0.1.0" },
      });
      return;
    }
    if (method === "notifications/initialized") return;
    if (method === "tools/list") {
      reply(id, { tools });
      return;
    }
    if (method === "tools/call") {
      const text = await callTool(params.name, params.arguments || {});
      reply(id, { content: [{ type: "text", text }] });
      return;
    }
    if (id !== undefined) reply(id, {});
  } catch (err) {
    fail(id, String(err.message || err));
  }
});
