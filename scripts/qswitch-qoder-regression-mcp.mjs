#!/usr/bin/env node
// Deliberately tiny stdio MCP fixture for Qoder's end-to-end regression.
// It exposes one deterministic, side-effect-free tool and never reads files,
// runs commands, uses the network, or inspects environment variables.
import readline from "node:readline";

const tool = {
  name: "qswitch_regression_echo",
  description: "Returns the supplied fixed regression marker unchanged.",
  inputSchema: {
    type: "object",
    properties: {
      marker: { type: "string", maxLength: 120 },
    },
    required: ["marker"],
    additionalProperties: false,
  },
};

const reply = (id, result) => {
  process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", id, result })}\n`);
};

readline.createInterface({ input: process.stdin }).on("line", (line) => {
  let request;
  try {
    request = JSON.parse(line);
  } catch {
    return;
  }
  if (request.id === undefined) return;

  if (request.method === "initialize") {
    reply(request.id, {
      protocolVersion: "2024-11-05",
      capabilities: { tools: {} },
      serverInfo: { name: "qswitch-qoder-regression", version: "1.0.0" },
    });
    return;
  }
  if (request.method === "tools/list") {
    reply(request.id, { tools: [tool] });
    return;
  }
  if (request.method === "tools/call" && request.params?.name === tool.name) {
    const marker = String(request.params.arguments?.marker ?? "").slice(0, 120);
    reply(request.id, { content: [{ type: "text", text: marker }] });
    return;
  }
  reply(request.id, { content: [{ type: "text", text: "unsupported" }], isError: true });
});
