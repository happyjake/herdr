#!/usr/bin/env node

const ACTIONS = new Set(["click", "scroll_up", "scroll_down"]);

function usage() {
  return `usage: scripts/probe_send_mouse.mjs --url WS_URL --pane PANE_ID \\
       --action click|scroll_up|scroll_down --row ROW --col COL [--lines N] [--token TOKEN]

Send one pane.send_mouse request over Herdr's WebSocket JSON API.
ROW and COL are zero-based terminal cells. Pass the pairing URL printed by
\`herdr pair\`, or pass its base WS URL plus --token. --lines applies only to
scroll actions and defaults server-side to one.

examples:
  scripts/probe_send_mouse.mjs --url 'ws://host:8787/?token=PAIR_TOKEN' \\
    --pane pane_1 --action click --row 12 --col 8
  scripts/probe_send_mouse.mjs --url 'ws://host:8787/' --token PAIR_TOKEN \\
    --pane pane_1 --action scroll_down --row 23 --col 40 --lines 3`;
}

function parseOptions(argv) {
  const options = {};
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (argument === "--help" || argument === "-h") {
      options.help = true;
      continue;
    }
    if (!argument.startsWith("--")) {
      throw new Error(`unexpected argument: ${argument}`);
    }
    const value = argv[index + 1];
    if (value === undefined || value.startsWith("--")) {
      throw new Error(`${argument} requires a value`);
    }
    options[argument.slice(2)] = value;
    index += 1;
  }
  return options;
}

function parseInteger(name, value, maximum) {
  if (!/^\d+$/.test(value ?? "")) {
    throw new Error(`--${name} must be a non-negative integer`);
  }
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed) || parsed > maximum) {
    throw new Error(`--${name} must be at most ${maximum}`);
  }
  return parsed;
}

function requestFrom(options) {
  for (const name of ["url", "pane", "action", "row", "col"]) {
    if (!options[name]) {
      throw new Error(`--${name} is required`);
    }
  }
  if (!ACTIONS.has(options.action)) {
    throw new Error(`unsupported --action: ${options.action}`);
  }
  if (options.action === "click" && options.lines !== undefined) {
    throw new Error("--lines applies only to scroll actions");
  }

  const url = new URL(options.url);
  if (url.protocol !== "ws:" && url.protocol !== "wss:") {
    throw new Error("--url must use ws:// or wss://");
  }
  if (options.token) {
    url.searchParams.set("token", options.token);
  }
  if (!url.searchParams.get("token")) {
    throw new Error("the pairing token must be present in --url or --token");
  }

  const params = {
    pane_id: options.pane,
    action: options.action,
    row: parseInteger("row", options.row, 0xffffffff),
    col: parseInteger("col", options.col, 0xffffffff),
  };
  if (options.lines !== undefined) {
    const lines = parseInteger("lines", options.lines, 0xffff);
    if (lines === 0) {
      throw new Error("--lines must be at least 1");
    }
    params.lines = lines;
  }

  return {
    url,
    request: {
      id: `mouse-probe-${process.pid}-${Date.now()}`,
      method: "pane.send_mouse",
      params,
    },
  };
}

async function sendRequest(url, request) {
  await new Promise((resolve, reject) => {
    const socket = new WebSocket(url);
    let settled = false;
    const timeout = setTimeout(() => {
      socket.close();
      finish(new Error("timed out waiting for pane.send_mouse response"));
    }, 10_000);

    function finish(error) {
      if (settled) return;
      settled = true;
      clearTimeout(timeout);
      if (error) reject(error);
      else resolve();
    }

    socket.addEventListener("open", () => {
      socket.send(JSON.stringify(request));
    });
    socket.addEventListener("message", (event) => {
      let response;
      try {
        response = JSON.parse(event.data);
      } catch {
        finish(new Error("server returned a non-JSON WebSocket message"));
        return;
      }
      if (response.id !== request.id) return;
      console.log(JSON.stringify(response, null, 2));
      if (response.error) process.exitCode = 1;
      socket.close();
      finish();
    });
    socket.addEventListener("error", () => {
      finish(new Error("WebSocket connection failed"));
    });
    socket.addEventListener("close", () => {
      if (!settled) finish(new Error("WebSocket closed before the response arrived"));
    });
  });
}

async function main() {
  const options = parseOptions(process.argv.slice(2));
  if (options.help) {
    console.log(usage());
    return;
  }
  const { url, request } = requestFrom(options);
  await sendRequest(url, request);
}

main().catch((error) => {
  console.error(`mouse probe: ${error.message}`);
  console.error("run with --help for usage");
  process.exitCode = 1;
});
