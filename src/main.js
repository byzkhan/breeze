import { Terminal } from "@xterm/xterm";
import { WebglAddon } from "@xterm/addon-webgl";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const term = new Terminal({
  fontFamily: "'JetBrains Mono', monospace",
  fontSize: 14,
  cursorBlink: true,
  cursorStyle: "bar",
  theme: {
    background: "#0D1117",
    foreground: "#E6EDF3",
    cursor: "#58A6FF",
    selectionBackground: "#264F78",
    black: "#484F58",
    red: "#FF7B72",
    green: "#3FB950",
    yellow: "#D29922",
    blue: "#58A6FF",
    magenta: "#BC8CFF",
    cyan: "#39D2C0",
    white: "#B1BAC4",
    brightBlack: "#6E7681",
    brightRed: "#FFA198",
    brightGreen: "#56D364",
    brightYellow: "#E3B341",
    brightBlue: "#79C0FF",
    brightMagenta: "#D2A8FF",
    brightCyan: "#56D4DD",
    brightWhite: "#F0F6FC",
  },
});

const fitAddon = new FitAddon();
term.loadAddon(fitAddon);

const container = document.getElementById("terminal-container");
term.open(container);

try {
  const webglAddon = new WebglAddon();
  term.loadAddon(webglAddon);
} catch (e) {
  console.warn("WebGL addon failed to load, falling back to canvas:", e);
}

fitAddon.fit();

async function startShell() {
  await invoke("spawn_shell", { rows: term.rows, cols: term.cols });

  listen("pty-data", (event) => {
    term.write(event.payload);
  });

  listen("pty-exit", () => {
    term.write("\r\n[Process exited]\r\n");
  });

  term.onData((data) => {
    invoke("write_pty", { data });
  });

  term.onResize(({ rows, cols }) => {
    invoke("resize_pty", { rows, cols });
  });
}

startShell();

window.addEventListener("resize", () => {
  fitAddon.fit();
});

const ro = new ResizeObserver(() => {
  fitAddon.fit();
});
ro.observe(container);
