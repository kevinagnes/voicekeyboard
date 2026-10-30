const invoke = window.__TAURI__.core.invoke;
const listen = window.__TAURI__.event.listen;

const dot = document.getElementById("dot");
const phaseEl = document.getElementById("phase");
const transcriptEl = document.getElementById("transcript");
const replyEl = document.getElementById("reply");
const errorEl = document.getElementById("error");
const speakingEl = document.getElementById("speaking");
const levelFill = document.getElementById("levelfill");
const cancelBtn = document.getElementById("cancelBtn");
const sourcesEl = document.getElementById("sources");
const historyEl = document.getElementById("history");
const sessionTitleEl = document.getElementById("sessionTitle");
const newSessionBtn = document.getElementById("newSessionBtn");
const historyToggle = document.getElementById("historyToggle");
const shotChip = document.getElementById("shotChip");
const planEl = document.getElementById("plan");
const toolEl = document.getElementById("tool");
const artifactsEl = document.getElementById("artifacts");
const permissionEl = document.getElementById("permission");
const permissionTitleEl = document.getElementById("permissionTitle");
const permissionDetailEl = document.getElementById("permissionDetail");
const permissionButtonsEl = document.getElementById("permissionButtons");

let sessionTurns = [];
let historyExpanded = false;

const PHASE_DOT = {
  listening: "#ff3b30",
  transcribing: "#ffa02e",
  thinking: "#b392f0",
  tool: "#f5a623",
  speaking: "#34c759",
  done: "#34c759",
  cancelled: "#8e8e93",
  cancelling: "#8e8e93",
  error: "#ff3b30",
};

const PHASE_LABEL = {
  listening: "Listening…",
  transcribing: "Transcribing…",
  thinking: "Thinking",
  tool: "Working",
  speaking: "Speaking",
  done: "Done",
  cancelled: "Cancelled",
  cancelling: "Cancelling…",
  error: "Error",
};

const CANCEL_PHASES = new Set([
  "listening",
  "transcribing",
  "thinking",
  "tool",
  "speaking",
]);

function setPhase(phase) {
  dot.style.background = PHASE_DOT[phase] || "#ff3b30";
  const label = PHASE_LABEL[phase] || phase;
  if (phase === "thinking") {
    phaseEl.innerHTML = `<span class="thinking-dots">${label}</span>`;
  } else {
    phaseEl.textContent = label;
  }
  speakingEl.style.display = phase === "speaking" ? "block" : "none";
  cancelBtn.style.display = CANCEL_PHASES.has(phase) ? "block" : "none";
  newSessionBtn.style.display = phase === "listening" ? "block" : "none";
  if (phase === "listening") {
    levelFill.parentElement.style.display = "";
  } else {
    levelFill.parentElement.style.display = "none";
    levelFill.style.width = "0%";
  }
}

function setText(el, value) {
  el.textContent = value;
  el.classList.toggle("placeholder", !value);
}

function setSources(list) {
  sourcesEl.innerHTML = "";
  if (!list || !list.length) {
    sourcesEl.style.display = "none";
    return;
  }
  const chips = list
    .slice(0, 5)
    .map((t) => `<span class="source">${escapeHtml(t)}</span>`)
    .join("");
  sourcesEl.innerHTML = `Sources: ${chips}`;
  sourcesEl.style.display = "block";
}

function setPlan(items) {
  if (!items || !items.length) {
    planEl.style.display = "none";
    planEl.innerHTML = "";
    return;
  }
  const current =
    items.find((i) => i.status === "in_progress") ||
    items.find((i) => i.status === "pending") ||
    items[items.length - 1];
  const done = items.filter((i) => i.status === "completed").length;
  planEl.innerHTML = `<b>Plan ${done}/${items.length}</b> · ${escapeHtml(
    current.title
  )}${current.status === "blocked" ? " (blocked)" : ""}`;
  planEl.style.display = "block";
}

function setTool(tools) {
  if (!tools || !tools.length) {
    toolEl.style.display = "none";
    toolEl.innerHTML = "";
    return;
  }
  const last = tools[tools.length - 1];
  toolEl.innerHTML = `<b>${escapeHtml(last)}</b> · ${tools.length} tool${
    tools.length === 1 ? "" : "s"
  } this step`;
  toolEl.style.display = "block";
}

function setArtifacts(items) {
  if (!items || !items.length) {
    artifactsEl.style.display = "none";
    artifactsEl.innerHTML = "";
    return;
  }
  const names = items.map((a) => a.title || a.path).slice(-3);
  artifactsEl.innerHTML = `<b>Files</b> · ${names.map(escapeHtml).join(" · ")}`;
  artifactsEl.style.display = "block";
}

let currentPermission = null;

function closePermission() {
  currentPermission = null;
  permissionEl.style.display = "none";
  permissionButtonsEl.innerHTML = "";
}

function respond(decision) {
  if (!currentPermission) return;
  const id = currentPermission.id;
  closePermission();
  invoke("respond_permission", { id, decision }).catch(() => {});
}

function denyWithReason() {
  const reason = window.prompt("Tell the assistant why (it will see this):", "");
  if (reason === null) return;
  respond({ denyWithReason: reason });
}

function showPermission(request) {
  currentPermission = request;
  permissionTitleEl.textContent = request.title || "Permission";
  permissionDetailEl.textContent = request.detail || "";
  permissionButtonsEl.innerHTML = "";
  const allow = document.createElement("button");
  allow.className = "primary";
  allow.textContent = "Allow once";
  allow.addEventListener("click", () => respond("allowOnce"));
  permissionButtonsEl.appendChild(allow);
  if (request.allowAlways) {
    const always = document.createElement("button");
    always.textContent = "Always allow for this session";
    always.addEventListener("click", () => respond("alwaysAllow"));
    permissionButtonsEl.appendChild(always);
  }
  const deny = document.createElement("button");
  deny.className = "danger";
  deny.textContent = "Deny";
  deny.addEventListener("click", () => respond("deny"));
  permissionButtonsEl.appendChild(deny);
  const reason = document.createElement("button");
  reason.textContent = "Deny with reason…";
  reason.addEventListener("click", denyWithReason);
  permissionButtonsEl.appendChild(reason);
  permissionEl.style.display = "flex";
  permissionEl.style.flexDirection = "column";
}


function showError(message) {
  errorEl.textContent = message;
  errorEl.style.display = "block";
}

function clearError() {
  errorEl.style.display = "none";
  errorEl.textContent = "";
}

async function sampleLevel() {
  if (!listening) return;
  try {
    const level = await invoke("get_audio_level");
    if (typeof level === "number") {
      levelFill.style.width = `${Math.min(100, level / 10)}%`;
    }
  } catch {
    levelFill.style.width = "0%";
  }
}

let listening = true;
let currentPhase = "listening";

async function refreshSession() {
  let session = null;
  try {
    session = await invoke("get_active_session");
  } catch {
    session = null;
  }
  sessionTurns = (session && session.turns) || [];
  sessionTitleEl.textContent = session
    ? `· ${session.title}`
    : "· new session";
  newSessionBtn.disabled = !session;
  historyToggle.style.display = sessionTurns.length ? "block" : "none";
  historyToggle.textContent = historyExpanded ? "Hide" : "History";
  historyToggle.classList.toggle("on", historyExpanded);
  renderHistory();
}

function renderHistory() {
  historyEl.innerHTML = "";
  if (!historyExpanded || !sessionTurns.length) {
    historyEl.style.display = "none";
    return;
  }
  for (const t of sessionTurns) {
    const row = document.createElement("div");
    row.className = `hist-row ${t.role === "user" ? "you" : "gemini"}`;
    const who = document.createElement("div");
    who.className = "hist-who";
    who.textContent = t.role === "user" ? "You" : "Gemini";
    const text = document.createElement("div");
    text.className = "hist-text";
    text.textContent = t.text;
    row.appendChild(who);
    row.appendChild(text);
    historyEl.appendChild(row);
  }
  historyEl.style.display = "flex";
  historyEl.scrollTop = historyEl.scrollHeight;
}

function toggleHistory() {
  if (!sessionTurns.length) return;
  historyExpanded = !historyExpanded;
  invoke("set_llm_overlay_expanded", { expanded: historyExpanded }).catch(() => {});
  historyToggle.textContent = historyExpanded ? "Hide" : "History";
  historyToggle.classList.toggle("on", historyExpanded);
  renderHistory();
}

historyToggle.addEventListener("click", (e) => {
  e.stopPropagation();
  toggleHistory();
});
transcriptEl.addEventListener("click", toggleHistory);
replyEl.addEventListener("click", toggleHistory);

function safeListen(name, handler) {
  Promise.resolve(listen(name, handler)).catch(() => {});
}

function cancel() {
  invoke("cancel_recording").catch(() => {});
}

cancelBtn.addEventListener("click", (e) => {
  e.stopPropagation();
  cancel();
});

newSessionBtn.addEventListener("click", (e) => {
  e.stopPropagation();
  invoke("new_session")
    .then(() => refreshSession())
    .catch(() => {});
});

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    e.preventDefault();
    if (currentPermission) {
      respond("deny");
    } else {
      cancel();
    }
  }
});

safeListen("llm-status", (e) => {
  const s = (e && e.payload) || {};
  const phase = s.phase || "done";
  currentPhase = phase;
  setPhase(phase);
  listening = phase === "listening";
  if (phase === "listening" || phase === "cancelled" || phase === "cancelling") {
    clearError();
    setText(transcriptEl, "");
    setText(replyEl, "");
    setSources(null);
    setPlan(null);
    setTool(null);
    setArtifacts(null);
    shotChip.style.display = "none";
    if (phase === "listening" && historyExpanded) {
      historyExpanded = false;
      invoke("set_llm_overlay_expanded", { expanded: false }).catch(() => {});
      historyToggle.textContent = "History";
      historyToggle.classList.remove("on");
      renderHistory();
    }
  }
  if (s.transcript) setText(transcriptEl, s.transcript);
  if (s.reply) setText(replyEl, s.reply);
  if (s.sources) setSources(s.sources);
  if (s.plan) setPlan(s.plan);
  if (s.tools) setTool(s.tools);
  if (s.artifacts) setArtifacts(s.artifacts);
  if (s.detail && (phase === "tool" || phase === "thinking")) {
    phaseEl.textContent = s.detail;
  }
  if (s.error) showError(s.error);
  if (phase === "listening") refreshSession();
});

safeListen("agent-permission", (e) => {
  const request = (e && e.payload) || {};
  if (request.id) showPermission(request);
});

safeListen("agent-permission-closed", () => {
  closePermission();
});

safeListen("stt-partial", (e) => {
  const text = e && e.payload && e.payload.text;
  if (text && (currentPhase === "listening" || currentPhase === "transcribing")) {
    setText(transcriptEl, text);
  }
});

safeListen("screenshot-attached", () => {
  shotChip.style.display = "block";
});

// Remember where the user drags the panel. Coordinates are physical pixels so
// the position survives monitor scaling and restarts.
try {
  const windowApi = window.__TAURI__ && window.__TAURI__.window;
  const current = windowApi && windowApi.getCurrentWindow ? windowApi.getCurrentWindow() : null;
  if (current && current.onMoved) {
    let moveTimer = null;
    current.onMoved(({ payload }) => {
      if (!payload) return;
      clearTimeout(moveTimer);
      const x = Math.round(payload.x);
      const y = Math.round(payload.y);
      moveTimer = setTimeout(() => {
        invoke("save_overlay_position", { panel: "llm", x, y }).catch(() => {});
      }, 300);
    });
  }
} catch {
  // older webview / API unavailable: the panel still works, just not persisted
}

// Keep the recalled history in sync when sessions change from the menu or
// Settings — but ignore mid-flight changes: the live turn must not appear
// twice (once as history, once as the current question/reply).
safeListen("llm-session", () => {
  if (currentPhase === "listening") refreshSession();
});

refreshSession();
setPhase("listening");
setInterval(sampleLevel, 80);

invoke("pending_permission")
  .then((request) => {
    if (request && request.id) showPermission(request);
  })
  .catch(() => {});