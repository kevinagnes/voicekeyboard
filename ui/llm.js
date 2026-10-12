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

const HISTORY_TURNS = 8;

const PHASE_DOT = {
  listening: "#ff3b30",
  transcribing: "#ffa02e",
  thinking: "#b392f0",
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

function escapeHtml(s) {
  return String(s)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
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
  const turns = (session && session.turns) || [];
  sessionTitleEl.textContent = session
    ? `· ${session.title}`
    : "· new session";
  newSessionBtn.disabled = !session;

  historyEl.innerHTML = "";
  if (!turns.length) {
    historyEl.style.display = "none";
    return;
  }
  const shown = turns.slice(-HISTORY_TURNS);
  const hidden = turns.length - shown.length;
  if (hidden > 0) {
    const more = document.createElement("div");
    more.id = "historyMore";
    more.textContent = `${hidden} earlier ${hidden === 1 ? "message" : "messages"} — open Sessions in Settings to read them`;
    historyEl.appendChild(more);
  }
  for (const t of shown) {
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
    cancel();
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
  }
  if (s.transcript) setText(transcriptEl, s.transcript);
  if (s.reply) setText(replyEl, s.reply);
  if (s.sources) setSources(s.sources);
  if (s.error) showError(s.error);
  if (phase === "listening") refreshSession();
});

// Keep the recalled history in sync when sessions change from the menu or
// Settings — but ignore mid-flight changes: the live turn must not appear
// twice (once as history, once as the current question/reply).
safeListen("llm-session", () => {
  if (currentPhase === "listening") refreshSession();
});

refreshSession();
setPhase("listening");
setInterval(sampleLevel, 80);