import { Terminal } from "@xterm/xterm";
import { WebglAddon } from "@xterm/addon-webgl";
import { FitAddon } from "@xterm/addon-fit";
import { UnicodeGraphemesAddon } from "@xterm/addon-unicode-graphemes";
import "@xterm/xterm/css/xterm.css";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const { getCurrentWindow } = window.__TAURI__.window;

// ── Terminal theme (shared across all tabs) ──
const TERM_THEME = {
  background: "#000000",
  foreground: "#CCCCCC",
  cursor: "#7AA2F7",
  selectionBackground: "#264F78",
  black: "#3B3B4F",
  red: "#F87171",
  green: "#4ADE80",
  yellow: "#FACC15",
  blue: "#7AA2F7",
  magenta: "#C084FC",
  cyan: "#22D3EE",
  white: "#D4D4D8",
  brightBlack: "#71717A",
  brightRed: "#FCA5A5",
  brightGreen: "#86EFAC",
  brightYellow: "#FDE68A",
  brightBlue: "#93C5FD",
  brightMagenta: "#D8B4FE",
  brightCyan: "#67E8F9",
  brightWhite: "#FAFAFA",
};

// ── Tab state ──
const tabs = new Map(); // tabId → { term, fitAddon, container, xtermEl, editorEl, tabEl, atPrompt, historyIndex, editorDraft }
let activeTabId = null;
let tabCounter = 0;

// ── RAF write batching ──
const pendingWrites = new Map();  // tabId → string[]
let rafScheduled = false;

// ── Flow control ──
const HIGH_WATER = 500 * 1024;  // 500 KB — pause reading
const LOW_WATER = 100 * 1024;   // 100 KB — resume reading
const pendingBytes = new Map();  // tabId → number
const pausedTabs = new Set();

function flushWrites() {
  rafScheduled = false;
  for (const [tabId, chunks] of pendingWrites) {
    if (chunks.length === 0) continue;
    const tab = tabs.get(tabId);
    if (!tab) { pendingWrites.delete(tabId); continue; }

    const batch = chunks.join("");
    pendingWrites.set(tabId, []);

    // Track bytes for flow control
    const byteLen = batch.length;  // approximate (JS string length ≈ bytes for ASCII/VT)
    pendingBytes.set(tabId, (pendingBytes.get(tabId) || 0) + byteLen);

    tab.term.write(batch, () => {
      // Callback fires after xterm.js processes the write
      const remaining = (pendingBytes.get(tabId) || 0) - byteLen;
      pendingBytes.set(tabId, Math.max(0, remaining));

      // Resume reading if we dropped below low water
      if (pausedTabs.has(tabId) && remaining < LOW_WATER) {
        pausedTabs.delete(tabId);
        invoke("resume_pty", { tabId });
      }
    });

    // Pause reading if we're above high water
    if (!pausedTabs.has(tabId) && (pendingBytes.get(tabId) || 0) > HIGH_WATER) {
      pausedTabs.add(tabId);
      invoke("pause_pty", { tabId });
    }

    // Prompt detection on the tail of this batch
    // Strip ANSI escape sequences (CSI, OSC, charset) before testing,
    // because zsh emits codes like \x1b[?2004h after the prompt character.
    const stripped = batch.replace(/\x1b\[[0-9;?]*[A-Za-z]|\x1b\][^\x07]*\x07|\x1b\(B/g, "");
    if (/[%$#>❯]\s*$/.test(stripped)) {
      tab.atPrompt = true;
      showEditor(tabId);
    }
  }
}

function scheduleFlush() {
  if (!rafScheduled) {
    rafScheduled = true;
    requestAnimationFrame(flushWrites);
  }
}

let inputBuffer = "";
let aiMode = false;
let pendingCommand = null;
let aiLineCount = 0; // how many terminal lines the AI indicator occupies

const recentCommands = [];
const MAX_HISTORY = 10;

// ── Known commands for English detection ──
const KNOWN_COMMANDS = new Set([
  "ls","cd","pwd","echo","cat","cp","mv","rm","mkdir","rmdir","touch","chmod",
  "chown","chgrp","ln","find","grep","awk","sed","sort","uniq","wc","head",
  "tail","less","more","man","which","whereis","whoami","who","w","id","su",
  "sudo","passwd","useradd","userdel","usermod","groupadd","groupdel","groups",
  "ps","top","htop","kill","killall","pkill","bg","fg","jobs","nohup","nice",
  "renice","df","du","mount","umount","lsblk","fdisk","free","uname","hostname",
  "uptime","date","cal","history","alias","unalias","export","env","set","unset",
  "source","exec","exit","logout","clear","reset","screen","tmux","ssh","scp",
  "rsync","curl","wget","ping","traceroute","netstat","ss","ifconfig","ip",
  "dig","nslookup","host","iptables","tar","gzip","gunzip","zip","unzip",
  "bzip2","xz","file","stat","diff","patch","tee","xargs","watch","crontab",
  "at","make","gcc","g++","python","python3","pip","pip3","node","npm","npx",
  "yarn","pnpm","bun","deno","ruby","gem","perl","java","javac","go","cargo",
  "rustc","docker","docker-compose","podman","kubectl","helm","terraform",
  "ansible","vagrant","git","gh","svn","hg","vim","vi","nvim","nano","emacs",
  "code","subl","open","pbcopy","pbpaste","say","brew","apt","apt-get","yum",
  "dnf","pacman","snap","flatpak","systemctl","service","journalctl","dmesg",
  "lsof","strace","ltrace","gdb","valgrind","nc","nmap","openssl","base64",
  "md5","sha256sum","jq","yq","awk","column","cut","tr","rev","seq","yes",
  "true","false","test","[","[[","printf","read","while","for","if","case",
  "do","done","then","else","fi","elif","esac","in","select","until","break",
  "continue","return","shift","trap","wait","sleep","time","type","command",
]);

const ENGLISH_INDICATORS = new Set([
  "a","an","the","this","that","these","those",
  "my","your","our","their","its","me","it","them",
  "some","all","every","each","any","new","old",
  "in","into","from","to","for","with","on","about","inside","between","onto",
  "folder","directory","file","files","called","named","please","everything",
  "nothing","something","here","there","using",
]);

function looksLikeEnglish(input) {
  const trimmed = input.trim();
  if (!trimmed) return false;

  const words = trimmed.split(/\s+/);
  if (words.length < 2) return false;

  const first = words[0].toLowerCase();

  if (/^[.~\/]/.test(trimmed)) return false;
  if (/^[A-Za-z_]\w*=/.test(trimmed)) return false;
  if (/[|><;&$`\\]/.test(first)) return false;

  if (KNOWN_COMMANDS.has(first)) {
    const rest = words.slice(1).map(w => w.toLowerCase());
    if (rest.some(w => ENGLISH_INDICATORS.has(w))) return true;
    if (words.length >= 5 && !trimmed.includes("-")) return true;
    return false;
  }

  return true;
}

// Erase any previously written AI lines from the terminal
function clearAiLines(tabId) {
  if (aiLineCount <= 0) return;
  const tab = tabs.get(tabId);
  if (!tab) return;
  // Move up and clear each line we wrote
  for (let i = 0; i < aiLineCount; i++) {
    tab.term.write("\x1b[2K\x1b[A");
  }
  // Move cursor to start of current line
  tab.term.write("\r");
  aiLineCount = 0;
}

// Write AI status/suggestion inline in the terminal (like Claude Code)
function writeAiLines(tabId, lines) {
  const tab = tabs.get(tabId);
  if (!tab) return;
  clearAiLines(tabId);
  tab.term.write("\r\n" + lines.join("\r\n"));
  aiLineCount = lines.length;
}

function showSuggestion(tabId, command, explanation, dangerous) {
  pendingCommand = command;
  const icon = dangerous ? "\x1b[31m\u26A0\x1b[0m" : "\x1b[32m\u2713\x1b[0m";
  const cmdColor = dangerous ? "\x1b[31m" : "\x1b[36m";
  const lines = [
    `  ${icon} ${cmdColor}${command}\x1b[0m`,
    `  \x1b[90m${explanation}\x1b[0m`,
    `  \x1b[90mEnter to run \u00B7 Escape to cancel\x1b[0m`,
  ];
  writeAiLines(tabId, lines);
  aiMode = true;
}

function showLoading(tabId) {
  writeAiLines(tabId, ["  \x1b[33m\u2731\x1b[0m \x1b[38;5;209mThinking\u2026\x1b[0m"]);
  aiMode = true;
}

function hideSuggestion(tabId) {
  clearAiLines(tabId);
  aiMode = false;
  pendingCommand = null;
}

async function handleEnglishInput(tabId, text) {
  // Verify tab exists before starting
  if (!tabs.has(tabId)) return;

  showLoading(tabId);

  try {
    let cwd = "~";
    try {
      cwd = await invoke("get_shell_cwd", { tabId });
    } catch (_) {}

    // Check if tab still exists and is still active after await
    if (!tabs.has(tabId) || activeTabId !== tabId) {
      hideSuggestion(tabId);
      return;
    }

    const history = recentCommands.slice(-5);
    const raw = await invoke("translate_command", { prompt: text, cwd, history });

    // Check if tab still exists and is still active after await
    if (!tabs.has(tabId) || activeTabId !== tabId) {
      hideSuggestion(tabId);
      return;
    }

    const cleaned = raw.replace(/^```(?:json)?\n?/, "").replace(/\n?```$/, "");
    const result = JSON.parse(cleaned);
    showSuggestion(tabId, result.command, result.explanation, result.dangerous);
  } catch (err) {
    // Check if tab still exists before showing error
    if (!tabs.has(tabId) || activeTabId !== tabId) {
      hideSuggestion(tabId);
      return;
    }
    showSuggestion(tabId, "Error: " + err, "Failed to translate", false);
  }
}

// ── Agent mode ──

// Simple markdown → HTML renderer for agent responses
function renderMarkdown(text) {
  const esc = (s) => s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  const lines = text.split("\n");
  let html = "";
  let inCodeBlock = false;
  let codeLang = "";
  let codeLines = [];
  let inList = false;
  let listType = "";

  function closeList() {
    if (inList) {
      html += listType === "ol" ? "</ol>" : "</ul>";
      inList = false;
    }
  }

  function inlineFormat(line) {
    // Inline code
    line = line.replace(/`([^`]+)`/g, '<code>$1</code>');
    // Bold
    line = line.replace(/\*\*(.+?)\*\*/g, '<strong>$1</strong>');
    // Italic
    line = line.replace(/(?<!\*)\*([^*]+)\*(?!\*)/g, '<em>$1</em>');
    return line;
  }

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];

    // Code block toggle
    if (line.startsWith("```")) {
      if (!inCodeBlock) {
        closeList();
        inCodeBlock = true;
        codeLang = line.slice(3).trim();
        codeLines = [];
      } else {
        const langLabel = codeLang || "code";
        const codeContent = esc(codeLines.join("\n"));
        html += `<pre><div class="agent-code-header"><span class="agent-code-lang">${esc(langLabel)}</span><button class="agent-code-copy" onclick="navigator.clipboard.writeText(this.closest('pre').querySelector('code').textContent).then(()=>{this.textContent='Copied!';setTimeout(()=>this.textContent='Copy',1500)})">Copy</button></div><code>${codeContent}</code></pre>`;
        inCodeBlock = false;
        codeLang = "";
      }
      continue;
    }

    if (inCodeBlock) {
      codeLines.push(line);
      continue;
    }

    // Headings
    const headingMatch = line.match(/^(#{1,3})\s+(.+)/);
    if (headingMatch) {
      closeList();
      const level = headingMatch[1].length;
      html += `<h${level}>${inlineFormat(esc(headingMatch[2]))}</h${level}>`;
      continue;
    }

    // Unordered list
    const ulMatch = line.match(/^(\s*)[*-]\s+(.+)/);
    if (ulMatch) {
      if (!inList || listType !== "ul") {
        closeList();
        html += "<ul>";
        inList = true;
        listType = "ul";
      }
      html += `<li>${inlineFormat(esc(ulMatch[2]))}</li>`;
      continue;
    }

    // Ordered list
    const olMatch = line.match(/^\s*\d+\.\s+(.+)/);
    if (olMatch) {
      if (!inList || listType !== "ol") {
        closeList();
        html += "<ol>";
        inList = true;
        listType = "ol";
      }
      html += `<li>${inlineFormat(esc(olMatch[1]))}</li>`;
      continue;
    }

    closeList();

    // Empty line
    if (!line.trim()) {
      continue;
    }

    // Paragraph
    html += `<p>${inlineFormat(esc(line))}</p>`;
  }

  // Close unclosed code block (streaming may end mid-block)
  if (inCodeBlock) {
    const langLabel = codeLang || "code";
    const codeContent = esc(codeLines.join("\n"));
    html += `<pre><div class="agent-code-header"><span class="agent-code-lang">${esc(langLabel)}</span></div><code>${codeContent}</code></pre>`;
  }
  closeList();

  return html;
}

function createAgentConversation(tabId) {
  const el = document.createElement("div");
  el.className = "agent-conversation";

  // Banner at top
  const banner = document.createElement("div");
  banner.className = "agent-banner";
  banner.innerHTML = `<span class="agent-banner-icon">✦</span><span class="agent-banner-text">Agent Mode — ask anything, or type <strong>/exit</strong> to return to terminal</span><button class="agent-banner-exit">Exit</button>`;
  banner.querySelector(".agent-banner-exit").addEventListener("click", () => {
    exitAgentMode(tabId);
  });
  el.appendChild(banner);

  // Spacer pushes content to bottom
  const spacer = document.createElement("div");
  spacer.className = "agent-conversation-spacer";
  el.appendChild(spacer);

  return el;
}

function addAgentMessage(tabId, role, content) {
  const tab = tabs.get(tabId);
  if (!tab || !tab.agentConversationEl) return null;

  const msg = document.createElement("div");
  msg.className = `agent-msg agent-msg-${role}`;

  const label = document.createElement("div");
  label.className = "agent-msg-label";
  label.textContent = role === "user" ? "You" : "Agent";

  const body = document.createElement("div");
  body.className = "agent-msg-content";

  if (role === "user") {
    body.textContent = content;
  } else {
    body.innerHTML = renderMarkdown(content);
  }

  msg.appendChild(label);
  msg.appendChild(body);
  tab.agentConversationEl.appendChild(msg);

  // Scroll to bottom
  tab.agentConversationEl.scrollTop = tab.agentConversationEl.scrollHeight;

  return body;
}

function addThinkingIndicator(tabId) {
  const tab = tabs.get(tabId);
  if (!tab || !tab.agentConversationEl) return null;

  const el = document.createElement("div");
  el.className = "agent-thinking";
  el.innerHTML = `<div class="agent-thinking-dot"><span></span><span></span><span></span></div>Thinking…`;
  tab.agentConversationEl.appendChild(el);
  tab.agentConversationEl.scrollTop = tab.agentConversationEl.scrollHeight;

  return el;
}

function showAgentView(tabId) {
  const tab = tabs.get(tabId);
  if (!tab) return;

  // Create conversation element if it doesn't exist
  if (!tab.agentConversationEl) {
    tab.agentConversationEl = createAgentConversation(tabId);
    // Insert before the editor
    tab.container.insertBefore(tab.agentConversationEl, tab.editorEl);
  }

  tab.xtermEl.style.display = "none";
  tab.agentConversationEl.classList.add("visible");
}

function hideAgentView(tabId) {
  const tab = tabs.get(tabId);
  if (!tab) return;

  tab.xtermEl.style.display = "";
  if (tab.agentConversationEl) {
    tab.agentConversationEl.classList.remove("visible");
  }

  // Re-fit xterm
  requestAnimationFrame(() => {
    if (!tabs.has(tabId)) return;
    tab.fitAddon.fit();
  });
}

function exitAgentMode(tabId) {
  const tab = tabs.get(tabId);
  if (!tab) return;

  tab.agentHistory = [];
  tab.agentStreaming = false;

  // Remove the conversation element entirely so next /agent starts fresh
  if (tab.agentConversationEl) {
    tab.agentConversationEl.remove();
    tab.agentConversationEl = null;
  }

  hideAgentView(tabId);
  tab.atPrompt = true;
  showEditor(tabId);
}

async function handleAgentInput(tabId, text) {
  const tab = tabs.get(tabId);
  if (!tab) return;

  if (tab.agentStreaming) return; // Prevent concurrent agent calls

  // Show the conversation view
  showAgentView(tabId);

  // Add user message
  addAgentMessage(tabId, "user", text);

  // Show thinking indicator
  const thinkingEl = addThinkingIndicator(tabId);

  hideEditor(tabId);
  tab.agentStreaming = true;
  tab._agentStreamBuffer = "";
  tab._agentStreamEl = null;

  try {
    const result = await invoke("agent_chat", {
      tabId,
      message: text,
      history: tab.agentHistory,
    });

    // Remove thinking indicator if still present
    if (thinkingEl && thinkingEl.parentNode) {
      thinkingEl.remove();
    }

    // If no streaming el was created (e.g. empty response), add the final message
    if (!tab._agentStreamEl) {
      addAgentMessage(tabId, "assistant", result);
    } else {
      // Do final render with complete text
      tab._agentStreamEl.innerHTML = renderMarkdown(result);
      tab.agentConversationEl.scrollTop = tab.agentConversationEl.scrollHeight;
    }

    // Push to conversation history (cap at 20 messages)
    tab.agentHistory.push({ role: "user", content: text });
    tab.agentHistory.push({ role: "assistant", content: result });
    while (tab.agentHistory.length > 20) {
      tab.agentHistory.shift();
    }
  } catch (err) {
    if (thinkingEl && thinkingEl.parentNode) {
      thinkingEl.remove();
    }
    addAgentMessage(tabId, "assistant", "Error: " + String(err));
  } finally {
    tab.agentStreaming = false;
    tab._agentStreamBuffer = "";
    tab._agentStreamEl = null;
    tab.atPrompt = true;
    showEditor(tabId);
  }
}

// ── Input Editor helpers ──

function escapeHtml(str) {
  return str.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

function highlightLine(line) {
  if (!line) return "";
  // Comment
  if (/^\s*#/.test(line)) return `<span class="sh-comment">${escapeHtml(line)}</span>`;

  let result = "";
  let i = 0;
  let isFirstWord = true;

  while (i < line.length) {
    // Whitespace
    if (/\s/.test(line[i])) {
      const start = i;
      while (i < line.length && /\s/.test(line[i])) i++;
      result += escapeHtml(line.slice(start, i));
      continue;
    }

    // Inline comment
    if (line[i] === "#" && i > 0 && /\s/.test(line[i - 1])) {
      result += `<span class="sh-comment">${escapeHtml(line.slice(i))}</span>`;
      break;
    }

    // Strings (single/double quoted)
    if (line[i] === '"' || line[i] === "'") {
      const q = line[i];
      const start = i;
      i++;
      while (i < line.length && line[i] !== q) {
        if (line[i] === "\\" && q === '"') i++;
        i++;
      }
      if (i < line.length) i++; // closing quote
      result += `<span class="sh-string">${escapeHtml(line.slice(start, i))}</span>`;
      isFirstWord = false;
      continue;
    }

    // Variable $FOO or ${FOO}
    if (line[i] === "$") {
      const start = i;
      i++;
      if (i < line.length && line[i] === "{") {
        const end = line.indexOf("}", i);
        i = end >= 0 ? end + 1 : line.length;
      } else {
        while (i < line.length && /[A-Za-z0-9_]/.test(line[i])) i++;
      }
      result += `<span class="sh-var">${escapeHtml(line.slice(start, i))}</span>`;
      isFirstWord = false;
      continue;
    }

    // Operators: | & ; > < ||  &&
    if (/[|&;<>]/.test(line[i])) {
      const start = i;
      // Consume double operators
      if (i + 1 < line.length && (line.slice(i, i + 2) === "||" || line.slice(i, i + 2) === "&&" || line.slice(i, i + 2) === ">>" || line.slice(i, i + 2) === "<<")) {
        i += 2;
      } else {
        i++;
      }
      result += `<span class="sh-op">${escapeHtml(line.slice(start, i))}</span>`;
      isFirstWord = true; // next word after pipe/operator is a command
      continue;
    }

    // Word token
    const start = i;
    while (i < line.length && !/[\s|&;<>"'$#]/.test(line[i])) i++;
    const word = line.slice(start, i);

    if (isFirstWord) {
      result += `<span class="sh-cmd">${escapeHtml(word)}</span>`;
      isFirstWord = false;
    } else if (word.startsWith("-")) {
      result += `<span class="sh-flag">${escapeHtml(word)}</span>`;
    } else {
      result += escapeHtml(word);
    }
  }
  return result;
}

function highlightShellSyntax(text) {
  return text.split("\n").map(highlightLine).join("\n");
}

function updateHighlight(textarea, codeEl) {
  codeEl.innerHTML = highlightShellSyntax(textarea.value) + "\n"; // trailing newline prevents collapse
}

function autoResizeEditor(textarea) {
  textarea.style.height = "auto";
  const lineHeight = 18;
  const maxHeight = lineHeight * 10;
  textarea.style.height = Math.min(textarea.scrollHeight, maxHeight) + "px";
}

function createInputEditor(tabId) {
  const editor = document.createElement("div");
  editor.className = "input-editor";

  const prompt = document.createElement("span");
  prompt.className = "input-editor-prompt";
  prompt.textContent = "\u276F";

  const wrap = document.createElement("div");
  wrap.className = "input-editor-wrap";

  const textarea = document.createElement("textarea");
  textarea.className = "input-editor-textarea";
  textarea.rows = 1;
  textarea.spellcheck = false;
  textarea.autocomplete = "off";
  textarea.autocapitalize = "off";

  const codeEl = document.createElement("div");
  codeEl.className = "input-editor-highlight";

  wrap.appendChild(codeEl);
  wrap.appendChild(textarea);
  editor.appendChild(prompt);
  editor.appendChild(wrap);

  textarea.addEventListener("input", () => {
    updateHighlight(textarea, codeEl);
    autoResizeEditor(textarea);
    // Save draft
    const tab = tabs.get(tabId);
    if (tab) tab.editorDraft = textarea.value;
  });

  textarea.addEventListener("keydown", (e) => {
    handleEditorKeydown(e, tabId, textarea, codeEl);
  });

  return { editor, textarea, codeEl };
}

function handleEditorKeydown(e, tabId, textarea, codeEl) {
  const tab = tabs.get(tabId);
  if (!tab) return;

  // AI mode intercept
  if (aiMode) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      if (pendingCommand && !pendingCommand.startsWith("Error:")) {
        const cmd = pendingCommand;
        hideSuggestion(activeTabId);
        recentCommands.push(cmd);
        if (recentCommands.length > MAX_HISTORY) recentCommands.shift();
        invoke("write_pty", { tabId: activeTabId, data: "\x15" + cmd + "\r" });
        tab.atPrompt = false;
        hideEditor(tabId);
      } else {
        hideSuggestion(activeTabId);
        invoke("write_pty", { tabId: activeTabId, data: "\x03" });
      }
      textarea.value = "";
      updateHighlight(textarea, codeEl);
      autoResizeEditor(textarea);
      return;
    }
    if (e.key === "Escape" || (e.key === "n" && !e.metaKey && !e.ctrlKey)) {
      e.preventDefault();
      hideSuggestion(activeTabId);
      invoke("write_pty", { tabId: activeTabId, data: "\x03" });
      textarea.value = "";
      updateHighlight(textarea, codeEl);
      autoResizeEditor(textarea);
      return;
    }
    e.preventDefault();
    return;
  }

  // Enter — submit command
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    const text = textarea.value;
    const trimmed = text.trim();

    textarea.value = "";
    updateHighlight(textarea, codeEl);
    autoResizeEditor(textarea);

    // 1. /agent <text> → start agent conversation
    if (trimmed.startsWith("/agent ")) {
      const agentText = trimmed.slice(7).trim();
      if (agentText) handleAgentInput(tabId, agentText);
      return;
    }

    // 2. /exit or /clear → reset agent conversation and return to terminal
    if (trimmed === "/exit" || trimmed === "/clear") {
      exitAgentMode(tabId);
      return;
    }

    // 3. Active agent conversation (follow-up without /agent prefix)
    if (tab.agentHistory.length > 0 && !trimmed.startsWith("/")) {
      if (trimmed) handleAgentInput(tabId, trimmed);
      return;
    }

    // 4. English input → single-command translate
    if (looksLikeEnglish(text)) {
      handleEnglishInput(tabId, trimmed);
      return;
    }

    // 5. Regular shell command — also reset agent history
    tab.agentHistory = [];
    submitCommand(tabId, text);
    return;
  }

  // Shift+Enter — newline
  if (e.key === "Enter" && e.shiftKey) {
    // Default textarea behavior inserts newline — let it happen
    return;
  }

  // Escape — clear editor
  if (e.key === "Escape") {
    e.preventDefault();
    textarea.value = "";
    updateHighlight(textarea, codeEl);
    autoResizeEditor(textarea);
    tab.historyIndex = -1;
    tab.editorDraft = "";
    return;
  }

  // Ctrl+C — interrupt when empty
  if (e.key === "c" && e.ctrlKey && !e.metaKey) {
    if (textarea.value === "") {
      e.preventDefault();
      invoke("write_pty", { tabId, data: "\x03" });
      return;
    }
    // Non-empty: let native copy happen
    return;
  }

  // Ctrl+U — clear line
  if (e.key === "u" && e.ctrlKey && !e.metaKey) {
    e.preventDefault();
    textarea.value = "";
    updateHighlight(textarea, codeEl);
    autoResizeEditor(textarea);
    tab.editorDraft = "";
    return;
  }

  // Tab — insert 2 spaces
  if (e.key === "Tab") {
    e.preventDefault();
    const start = textarea.selectionStart;
    const end = textarea.selectionEnd;
    textarea.value = textarea.value.substring(0, start) + "  " + textarea.value.substring(end);
    textarea.selectionStart = textarea.selectionEnd = start + 2;
    updateHighlight(textarea, codeEl);
    autoResizeEditor(textarea);
    return;
  }

  // Up/Down — history navigation (only when cursor is on first/last line)
  if (e.key === "ArrowUp" || e.key === "ArrowDown") {
    const val = textarea.value;
    const cursorPos = textarea.selectionStart;
    const beforeCursor = val.substring(0, cursorPos);
    const afterCursor = val.substring(cursorPos);
    const isFirstLine = !beforeCursor.includes("\n");
    const isLastLine = !afterCursor.includes("\n");

    if (e.key === "ArrowUp" && isFirstLine && recentCommands.length > 0) {
      e.preventDefault();
      if (tab.historyIndex === -1) {
        tab.editorDraft = val;
        tab.historyIndex = recentCommands.length - 1;
      } else if (tab.historyIndex > 0) {
        tab.historyIndex--;
      }
      textarea.value = recentCommands[tab.historyIndex] || "";
      textarea.selectionStart = textarea.selectionEnd = textarea.value.length;
      updateHighlight(textarea, codeEl);
      autoResizeEditor(textarea);
      return;
    }

    if (e.key === "ArrowDown" && isLastLine) {
      e.preventDefault();
      if (tab.historyIndex >= 0) {
        tab.historyIndex++;
        if (tab.historyIndex >= recentCommands.length) {
          tab.historyIndex = -1;
          textarea.value = tab.editorDraft || "";
        } else {
          textarea.value = recentCommands[tab.historyIndex] || "";
        }
        textarea.selectionStart = textarea.selectionEnd = textarea.value.length;
        updateHighlight(textarea, codeEl);
        autoResizeEditor(textarea);
      }
      return;
    }
  }
}

function submitCommand(tabId, text) {
  const tab = tabs.get(tabId);
  if (!tab) return;

  const trimmed = text.trim();
  if (trimmed) {
    recentCommands.push(trimmed);
    if (recentCommands.length > MAX_HISTORY) recentCommands.shift();
  }

  tab.atPrompt = false;
  tab.historyIndex = -1;
  tab.editorDraft = "";
  hideEditor(tabId);

  // \x15 clears the line in case shell already has partial input, then send command
  invoke("write_pty", { tabId, data: "\x15" + text + "\r" });
}

function showEditor(tabId) {
  const tab = tabs.get(tabId);
  if (!tab || !tab.editorEl) return;

  tab.editorEl.classList.add("visible");
  tab.historyIndex = -1;

  // Update prompt indicator based on agent mode
  const promptEl = tab.editorEl.querySelector(".input-editor-prompt");
  if (promptEl) {
    if (tab.agentHistory.length > 0) {
      promptEl.textContent = "✦";
      promptEl.style.color = "#FACC15"; // yellow
    } else {
      promptEl.textContent = "❯";
      promptEl.style.color = ""; // reset to default (blue via CSS)
    }
  }

  // Restore draft if any
  const textarea = tab.editorEl.querySelector(".input-editor-textarea");
  if (textarea && tab.editorDraft) {
    textarea.value = tab.editorDraft;
    const codeEl = tab.editorEl.querySelector(".input-editor-highlight");
    if (codeEl) updateHighlight(textarea, codeEl);
    autoResizeEditor(textarea);
  }

  // Re-fit xterm since available height changed
  requestAnimationFrame(() => {
    tab.fitAddon.fit();
    if (textarea && tabId === activeTabId) textarea.focus();
  });
}

function hideEditor(tabId) {
  const tab = tabs.get(tabId);
  if (!tab || !tab.editorEl) return;

  tab.editorEl.classList.remove("visible");

  // Re-fit xterm since available height changed
  requestAnimationFrame(() => {
    tab.fitAddon.fit();
  });
}

// ── Tab management ──
const tabsContainer = document.getElementById("tabs-container");
const terminalContainer = document.getElementById("terminal-container");

function generateTabId() {
  return "tab-" + (++tabCounter) + "-" + Math.random().toString(36).slice(2, 8);
}

function createTab() {
  const tabId = generateTabId();

  // Create terminal instance
  const term = new Terminal({
    fontFamily: "Menlo, 'SF Mono', 'JetBrains Mono', monospace",
    fontSize: 12,
    lineHeight: 1.0,
    cursorBlink: false,
    cursorStyle: "bar",
    cursorInactiveStyle: "none",
    scrollback: 5000,
    allowProposedApi: true,
    theme: TERM_THEME,
  });

  const fitAddon = new FitAddon();
  term.loadAddon(fitAddon);

  // Create pane container inside #terminal-container
  const pane = document.createElement("div");
  pane.className = "terminal-pane";
  pane.dataset.tabId = tabId;
  terminalContainer.appendChild(pane);

  // xterm mounts inside a flex child, not the pane directly
  const xtermEl = document.createElement("div");
  xtermEl.className = "terminal-xterm";
  pane.appendChild(xtermEl);

  term.open(xtermEl);

  const unicodeAddon = new UnicodeGraphemesAddon();
  term.loadAddon(unicodeAddon);
  term.unicode.activeVersion = "15";

  try {
    const webglAddon = new WebglAddon();
    webglAddon.onContextLoss(() => {
      webglAddon.dispose();
    });
    term.loadAddon(webglAddon);
  } catch (e) {
    console.warn("WebGL addon failed to load, falling back to canvas:", e);
  }

  // Create input editor and append to pane
  const { editor: editorEl, textarea: editorTextarea, codeEl: editorCodeEl } = createInputEditor(tabId);
  pane.appendChild(editorEl);

  // Click on xterm surface while at prompt → redirect focus to editor
  xtermEl.addEventListener("mouseup", () => {
    const tabInfo = tabs.get(tabId);
    if (tabInfo && tabInfo.atPrompt && tabId === activeTabId) {
      // Small delay to allow text selection in xterm to complete
      setTimeout(() => {
        const sel = window.getSelection();
        if (!sel || sel.isCollapsed) {
          editorTextarea.focus();
        }
      }, 50);
    }
  });

  // Create tab element in tab bar
  const tabEl = document.createElement("div");
  tabEl.className = "tab";
  tabEl.dataset.tabId = tabId;
  tabEl.innerHTML = `<span class="tab-label">Shell ${tabCounter}</span><button class="tab-close">\u00D7</button>`;

  tabEl.addEventListener("click", (e) => {
    if (e.target.classList.contains("tab-close")) {
      closeTab(tabId);
    } else {
      switchTab(tabId);
    }
  });

  tabsContainer.appendChild(tabEl);

  // Store in map
  tabs.set(tabId, { term, fitAddon, container: pane, xtermEl, editorEl, tabEl, atPrompt: true, historyIndex: -1, editorDraft: "", agentHistory: [], agentStreaming: false, agentConversationEl: null, lastCwd: "" });

  // Switch to the new tab, then fit + push cursor to bottom + spawn shell
  switchTab(tabId);

  requestAnimationFrame(() => {
    fitAddon.fit();
    // Bottom-up flow: push cursor to last row so first prompt appears at bottom
    term.write("\r\n".repeat(Math.max(0, term.rows - 1)));
    invoke("spawn_shell", { tabId, rows: term.rows, cols: term.cols });
  });

  // Wire terminal input
  term.onData((data) => {
    if (tabId !== activeTabId) return;

    // Program running (not at shell prompt) — pass everything through
    const tabInfo = tabs.get(tabId);
    if (tabInfo && !tabInfo.atPrompt) {
      invoke("write_pty", { tabId, data });
      return;
    }

    // At prompt — redirect to editor
    if (tabInfo && tabInfo.atPrompt) {
      // Redirect printable characters to the editor textarea
      const textarea = tabInfo.editorEl?.querySelector(".input-editor-textarea");
      if (textarea) {
        textarea.focus();
        // Insert printable chars into textarea
        if (data.length === 1 && data.charCodeAt(0) >= 32) {
          const start = textarea.selectionStart;
          const end = textarea.selectionEnd;
          textarea.value = textarea.value.substring(0, start) + data + textarea.value.substring(end);
          textarea.selectionStart = textarea.selectionEnd = start + 1;
          textarea.dispatchEvent(new Event("input"));
        }
      }
      return;
    }

    invoke("write_pty", { tabId, data });
  });

  term.onResize(({ rows, cols }) => {
    invoke("resize_pty", { tabId, rows, cols });
  });

  return tabId;
}

function switchTab(tabId) {
  if (!tabs.has(tabId)) return;

  activeTabId = tabId;
  inputBuffer = "";

  // Update pane visibility
  for (const [id, tab] of tabs) {
    if (id === tabId) {
      tab.container.classList.add("active");
      tab.tabEl.classList.add("active");
    } else {
      tab.container.classList.remove("active");
      tab.tabEl.classList.remove("active");
    }
  }

  // Show/hide agent view and editor based on state
  const tab = tabs.get(tabId);

  // Toggle agent conversation view visibility
  if (tab.agentHistory.length > 0 && tab.agentConversationEl) {
    showAgentView(tabId);
  } else {
    hideAgentView(tabId);
  }

  if (tab.atPrompt) {
    showEditor(tabId);
  } else {
    hideEditor(tabId);
  }

  requestAnimationFrame(() => {
    tab.fitAddon.fit();
    if (tab.atPrompt && tab.editorEl) {
      const textarea = tab.editorEl.querySelector(".input-editor-textarea");
      if (textarea) textarea.focus();
    } else {
      tab.term.focus();
    }
  });

  updateContextBar();
}

function closeTab(tabId) {
  const tab = tabs.get(tabId);
  if (!tab) return;

  // Tell backend to drop the session
  invoke("close_tab", { tabId });

  // Clean up DOM, terminal, and tracking maps
  tab.term.dispose();
  tab.container.remove();
  tab.tabEl.remove();
  tabs.delete(tabId);
  pendingWrites.delete(tabId);
  pendingBytes.delete(tabId);
  pausedTabs.delete(tabId);

  // If we closed the active tab, switch to another
  if (activeTabId === tabId) {
    if (tabs.size > 0) {
      const nextId = tabs.keys().next().value;
      switchTab(nextId);
    } else {
      // Last tab closed — create a fresh one
      createTab();
    }
  }
}

// ── Event listeners (routed by tab_id) ──
listen("pty-data", (event) => {
  const { tab_id, data } = event.payload;
  if (!tabs.has(tab_id)) return;
  if (!pendingWrites.has(tab_id)) pendingWrites.set(tab_id, []);
  pendingWrites.get(tab_id).push(data);
  scheduleFlush();
});

listen("pty-exit", (event) => {
  const { tab_id } = event.payload;
  const tab = tabs.get(tab_id);
  if (tab) {
    tab.term.write("\r\n[Process exited]\r\n");
  }
});

listen("agent-chunk", (event) => {
  const { tab_id, text } = event.payload;
  const tab = tabs.get(tab_id);
  if (!tab || !tab.agentStreaming) return;

  // Remove thinking indicator on first chunk
  if (!tab._agentStreamEl) {
    const thinking = tab.agentConversationEl?.querySelector(".agent-thinking");
    if (thinking) thinking.remove();

    // Create assistant message block for streaming
    const contentEl = addAgentMessage(tab_id, "assistant", "");
    tab._agentStreamEl = contentEl;
  }

  // Accumulate text and re-render markdown
  tab._agentStreamBuffer += text;
  if (tab._agentStreamEl) {
    tab._agentStreamEl.innerHTML = renderMarkdown(tab._agentStreamBuffer);
    tab.agentConversationEl.scrollTop = tab.agentConversationEl.scrollHeight;
  }
});

listen("agent-tool-call", (event) => {
  const { tab_id, command } = event.payload;
  const tab = tabs.get(tab_id);
  if (!tab || !tab.agentConversationEl) return;

  // Finalize current streaming text block (if any) so tool block appears after it
  if (tab._agentStreamEl) {
    tab._agentStreamEl.innerHTML = renderMarkdown(tab._agentStreamBuffer);
    tab._agentStreamEl = null;
    tab._agentStreamBuffer = "";
  }

  // Remove thinking indicator
  const thinking = tab.agentConversationEl.querySelector(".agent-thinking");
  if (thinking) thinking.remove();

  // Create tool execution block
  const block = document.createElement("div");
  block.className = "agent-tool-block";
  block.dataset.command = command;

  const header = document.createElement("div");
  header.className = "agent-tool-header";
  header.innerHTML = `<span class="agent-tool-icon running">⟳</span><span class="agent-tool-label">Running command</span>`;

  const cmdEl = document.createElement("div");
  cmdEl.className = "agent-tool-command";
  cmdEl.textContent = command;

  block.appendChild(header);
  block.appendChild(cmdEl);
  tab.agentConversationEl.appendChild(block);
  tab.agentConversationEl.scrollTop = tab.agentConversationEl.scrollHeight;
});

listen("agent-tool-result", (event) => {
  const { tab_id, command, output } = event.payload;
  const tab = tabs.get(tab_id);
  if (!tab || !tab.agentConversationEl) return;

  // Find the matching tool block (last one with this command)
  const blocks = tab.agentConversationEl.querySelectorAll(".agent-tool-block");
  let targetBlock = null;
  for (let i = blocks.length - 1; i >= 0; i--) {
    if (blocks[i].dataset.command === command) {
      targetBlock = blocks[i];
      break;
    }
  }

  if (!targetBlock) return;

  // Update header — stop spinner
  const icon = targetBlock.querySelector(".agent-tool-icon");
  if (icon) {
    icon.textContent = "✓";
    icon.classList.remove("running");
    icon.style.color = "#4ADE80";
  }
  const label = targetBlock.querySelector(".agent-tool-label");
  if (label) label.textContent = "Completed";

  // Add output (collapsed if long)
  const lines = output.split("\n");
  const isLong = lines.length > 8;

  const outputEl = document.createElement("div");
  outputEl.className = "agent-tool-output";
  outputEl.textContent = output;
  if (isLong) {
    outputEl.style.maxHeight = "0";
    outputEl.style.padding = "0 12px";
    outputEl.style.borderTop = "none";
  }
  targetBlock.appendChild(outputEl);

  if (isLong) {
    const toggle = document.createElement("div");
    toggle.className = "agent-tool-toggle";
    toggle.innerHTML = `▶ Show output (${lines.length} lines)`;
    let expanded = false;
    toggle.addEventListener("click", () => {
      expanded = !expanded;
      if (expanded) {
        outputEl.style.maxHeight = "200px";
        outputEl.style.padding = "8px 12px";
        outputEl.style.borderTop = "1px solid rgba(255, 255, 255, 0.06)";
        toggle.innerHTML = `▼ Hide output`;
      } else {
        outputEl.style.maxHeight = "0";
        outputEl.style.padding = "0 12px";
        outputEl.style.borderTop = "none";
        toggle.innerHTML = `▶ Show output (${lines.length} lines)`;
      }
    });
    targetBlock.appendChild(toggle);
  }

  tab.agentConversationEl.scrollTop = tab.agentConversationEl.scrollHeight;
});

// ── Context bar ──
const ctxCwdText = document.getElementById("ctx-cwd-text");
const ctxGit = document.getElementById("ctx-git");
const ctxGitText = document.getElementById("ctx-git-text");
const ctxNode = document.getElementById("ctx-node");
const ctxNodeText = document.getElementById("ctx-node-text");

function friendlyCwd(path) {
  const home = "/Users/" + path.split("/")[2];
  let friendly = path;
  if (path.startsWith(home)) {
    friendly = "~" + path.slice(home.length);
  }
  const parts = friendly.split("/").filter(Boolean);
  if (parts.length <= 2) return parts.join(" \u2192 ");
  return parts.slice(-2).join(" \u2192 ");
}

async function updateContextBar() {
  if (!activeTabId) return;
  const tab = tabs.get(activeTabId);
  if (!tab) return;
  try {
    const cwd = await invoke("get_shell_cwd", { tabId: activeTabId });
    if (cwd === tab.lastCwd) return;
    tab.lastCwd = cwd;

    ctxCwdText.textContent = friendlyCwd(cwd);

    try {
      const branch = await invoke("get_git_branch", { cwd });
      ctxGitText.textContent = branch;
      ctxGit.style.display = "inline-flex";
    } catch {
      ctxGit.style.display = "none";
    }

    try {
      const version = await invoke("get_node_version");
      ctxNodeText.textContent = version;
      ctxNode.style.display = "inline-flex";
    } catch {
      ctxNode.style.display = "none";
    }
  } catch {
    // Tab or shell not ready yet
  }
}

// ── Toast ──
let toastTimer = null;
const toastEl = document.getElementById("toast");

function showToast(msg, duration = 3000) {
  toastEl.textContent = msg;
  toastEl.style.display = "block";
  if (toastTimer) clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    toastEl.style.display = "none";
    toastTimer = null;
  }, duration);
}

// ── Launcher bar ──
document.querySelectorAll(".launcher-btn").forEach((btn) => {
  btn.addEventListener("click", async () => {
    if (aiMode) hideSuggestion(activeTabId);
    if (!activeTabId) return;

    const cmd = btn.getAttribute("data-cmd");
    if (!cmd) return;
    const firstWord = cmd.split(" ")[0];

    const exists = await invoke("check_command_exists", { command: firstWord });
    if (!exists) {
      showToast(`${firstWord} is not installed`);
      return;
    }

    const tab = tabs.get(activeTabId);
    if (tab) {
      // Clear editor and hide it before sending command
      const textarea = tab.editorEl?.querySelector(".input-editor-textarea");
      if (textarea) {
        textarea.value = "";
        const codeEl = tab.editorEl?.querySelector(".input-editor-highlight");
        if (codeEl) updateHighlight(textarea, codeEl);
        autoResizeEditor(textarea);
      }
      tab.atPrompt = false;
      tab.editorDraft = "";
      hideEditor(activeTabId);
    }

    invoke("write_pty", { tabId: activeTabId, data: "\x15" + cmd + "\r" });
    recentCommands.push(cmd);
    if (recentCommands.length > MAX_HISTORY) recentCommands.shift();
  });
});

// ── Fullscreen toggle ──
const appWindow = getCurrentWindow();

document.getElementById("drag-handle").addEventListener("dblclick", async () => {
  const isMax = await appWindow.isMaximized();
  if (isMax) {
    await appWindow.unmaximize();
  } else {
    await appWindow.maximize();
  }
});

// ── Keyboard shortcuts ──
document.addEventListener("keydown", async (e) => {
  // Escape — cancel agent streaming
  if (e.key === "Escape" && activeTabId) {
    const tab = tabs.get(activeTabId);
    if (tab && tab.agentStreaming) {
      e.preventDefault();
      tab.agentStreaming = false;

      // Remove thinking indicator
      const thinking = tab.agentConversationEl?.querySelector(".agent-thinking");
      if (thinking) thinking.remove();

      // If streaming was in progress, finalize the partial message
      if (tab._agentStreamEl && tab._agentStreamBuffer) {
        tab._agentStreamEl.innerHTML = renderMarkdown(tab._agentStreamBuffer + "\n\n*[Cancelled]*");
      }

      tab._agentStreamBuffer = "";
      tab._agentStreamEl = null;
      tab.atPrompt = true;
      showEditor(activeTabId);
      return;
    }
  }

  // Cmd+Ctrl+F — native fullscreen
  if (e.metaKey && e.ctrlKey && e.key === "f") {
    e.preventDefault();
    const isFull = await appWindow.isFullscreen();
    await appWindow.setFullscreen(!isFull);
  }

  // Cmd+T — new tab
  if (e.metaKey && !e.ctrlKey && !e.shiftKey && e.key === "t") {
    e.preventDefault();
    createTab();
  }

  // Cmd+W — close tab
  if (e.metaKey && !e.ctrlKey && !e.shiftKey && e.key === "w") {
    e.preventDefault();
    if (activeTabId) closeTab(activeTabId);
  }
});

// ── New tab button ──
document.getElementById("new-tab-btn").addEventListener("click", () => {
  createTab();
});

// ── Startup ──
createTab();
setTimeout(updateContextBar, 500);
setInterval(updateContextBar, 2000);

// Debounced resize — shared by window resize and ResizeObserver
let resizeTimer = null;
function debouncedFit() {
  if (resizeTimer) clearTimeout(resizeTimer);
  resizeTimer = setTimeout(() => {
    resizeTimer = null;
    if (activeTabId) {
      const tab = tabs.get(activeTabId);
      if (tab) tab.fitAddon.fit();
    }
  }, 100);
}

window.addEventListener("resize", debouncedFit);
const ro = new ResizeObserver(debouncedFit);
ro.observe(terminalContainer);

// ── Focus management ──
window.addEventListener("focus", () => {
  if (!activeTabId) return;
  const tab = tabs.get(activeTabId);
  if (!tab) return;
  if (tab.atPrompt && tab.editorEl) {
    const textarea = tab.editorEl.querySelector(".input-editor-textarea");
    if (textarea) textarea.focus();
  } else {
    tab.term.focus();
  }
});
