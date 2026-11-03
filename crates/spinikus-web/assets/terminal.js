// The terminal page: draws a terminal with the vendored xterm.js in #terminal and connects it
// to the WebSocket its data-socket names. Binary messages are the terminal's bytes both ways;
// text messages are JSON: {"type":"resize","rows","columns"} from here, and
// {"type":"exit","code"} or {"type":"error","message"} from the server before it closes.
//
// Spinikus's own code, not vendored. The page loads in full (not boosted), so this runs once
// per terminal, and leaving the page closes the socket.
(() => {
  "use strict";

  const element = document.getElementById("terminal");
  if (!element || typeof Terminal === "undefined") return;
  const status = document.getElementById("terminal-status");
  const reconnect = document.getElementById("terminal-reconnect");

  const dark = window.matchMedia("(prefers-color-scheme: dark)").matches;
  const terminal = new Terminal({
    cursorBlink: true,
    fontFamily: 'ui-monospace, SFMono-Regular, Menlo, Consolas, "Liberation Mono", monospace',
    fontSize: 13,
    scrollback: 5000,
    theme: { background: "#09090b", foreground: dark ? "#e4e4e7" : "#f4f4f5" },
  });
  const fit = new FitAddon.FitAddon();
  terminal.loadAddon(fit);
  terminal.open(element);
  fit.fit();

  const url = new URL(element.dataset.socket, window.location.href);
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  url.searchParams.set("rows", terminal.rows);
  url.searchParams.set("columns", terminal.cols);
  const socket = new WebSocket(url);
  socket.binaryType = "arraybuffer";
  const encoder = new TextEncoder();
  let ending = null;

  function say(text) {
    status.textContent = text;
  }

  function ended(text) {
    say(text);
    reconnect.hidden = false;
    terminal.options.cursorBlink = false;
  }

  function send(data) {
    if (socket.readyState === WebSocket.OPEN) socket.send(data);
  }

  socket.addEventListener("open", () => {
    say("Connected");
    terminal.focus();
  });
  socket.addEventListener("message", (event) => {
    if (typeof event.data !== "string") {
      terminal.write(new Uint8Array(event.data));
      return;
    }
    let message;
    try {
      message = JSON.parse(event.data);
    } catch {
      return;
    }
    if (message.type === "exit") ending = `Exited with code ${message.code}`;
    if (message.type === "error") ending = message.message;
  });
  socket.addEventListener("close", () => ended(ending || "Disconnected"));

  terminal.onData((data) => send(encoder.encode(data)));
  // Some keys (and mouse reports) come as bytes in a string, one character per byte.
  terminal.onBinary((data) => send(Uint8Array.from(data, (character) => character.charCodeAt(0))));
  terminal.onResize(({ rows, cols }) => send(JSON.stringify({ type: "resize", rows, columns: cols })));
  new ResizeObserver(() => fit.fit()).observe(element);

  reconnect.addEventListener("click", () => window.location.reload());
  window.addEventListener("pagehide", () => socket.close());
})();
