const invoke = window.__TAURI__.core.invoke;
const listen = window.__TAURI__.event.listen;

const $ = (id) => document.getElementById(id);
const state = { settings: null, capturing: false, sessionSelected: null };

function switchTab(name) {
  document.querySelectorAll(".tab").forEach((b) => {
    b.classList.toggle("active", b.dataset.tab === name);
  });
  document.querySelectorAll(".tab-panel").forEach((p) => {
    p.hidden = p.id !== `tab-${name}`;
  });
}

async function refreshStatus() {
  try {
    const status = await invoke("get_status");
    const el = $("status");
    if (status.recording) {
      el.className = "status recording";
      el.textContent = "● Recording… release to transcribe";
    } else if (status.busy) {
      el.className = "status busy";
      el.textContent = "Transcribing…";
    } else {
      el.className = "status ok";
      const llm = status.llmHotkey
        ? ` · ${status.llmHotkey} to ask Gemini`
        : "";
      el.textContent = `Ready — hold ${status.hotkey} to record${llm}`;
    }
    $("hotkeyHint").textContent = status.hotkey;
  } catch (e) {
    $("status").textContent = "Error reading status";
  }
}

async function refreshDevices() {
  try {
    const devices = await invoke("get_input_devices");
    const sel = $("inputDevice");
    const current = state.settings.inputDevice || "";
    sel.innerHTML = '<option value="">System default</option>';
    for (const d of devices) {
      const opt = document.createElement("option");
      opt.value = d;
      opt.textContent = d;
      opt.selected = d === current;
      sel.appendChild(opt);
    }
  } catch (e) {
    // devices not available
  }
}

async function refreshPermissions() {
  const p = await invoke("get_permissions");
  const list = $("permissions");
  list.innerHTML = "";
  const micStatus = p.micPermission;
  const micLabels = {
    authorized: "granted",
    denied: "denied — open System Settings → Privacy & Security → Microphone and enable VoiceKeyboard.",
    notDetermined: "not requested — click “Request Microphone…”",
  };
  let micText = micLabels[micStatus] || micLabels.notDetermined;
  if (!p.runningFromBundle && micStatus !== "authorized") {
    micText += " Note: run the installed VoiceKeyboard.app, not the raw binary.";
  }
  const items = [
    {
      label: "Microphone",
      ok: micStatus === "authorized",
      missing: micText,
    },
    {
      label: "Accessibility (auto-paste)",
      ok: p.accessibilityTrusted,
      missing: "Not granted — open System Settings → Privacy & Security → Accessibility and enable VoiceKeyboard.",
    },
  ];
  for (const it of items) {
    const li = document.createElement("li");
    li.innerHTML = `${it.label}: `;
    const span = document.createElement("span");
    if (it.ok) {
      span.className = "ok";
      span.textContent = "granted";
    } else {
      span.className = "missing";
      span.textContent = it.missing;
    }
    li.appendChild(span);
    list.appendChild(li);
  }
  const micBtn = $("requestMicrophone");
  const openBtn = $("openMicSettings");
  const resetBtn = $("resetMicrophone");
  if (micStatus === "denied") {
    micBtn.hidden = true;
    openBtn.hidden = false;
    resetBtn.hidden = false;
  } else if (micStatus === "notDetermined") {
    micBtn.hidden = false;
    micBtn.disabled = false;
    micBtn.textContent = "Request Microphone…";
    openBtn.hidden = true;
    resetBtn.hidden = true;
  } else {
    micBtn.hidden = true;
    openBtn.hidden = true;
    resetBtn.hidden = true;
  }
}

async function refreshStats() {
  try {
    const s = await invoke("get_inference_stats");
    $("statRuns").textContent = s.total_runs || "—";
    $("statAvgMs").textContent = s.avg_ms ? `${s.avg_ms} ms` : "—";
    $("statTotalMs").textContent = s.total_ms ? `${(s.total_ms / 1000).toFixed(1)} s` : "—";
    $("statAvgSegs").textContent = s.avg_segments || "—";
  } catch (e) {
    // stats not available yet
  }
}

async function refreshRecent() {
  try {
    const items = await invoke("get_recent_transcriptions");
    const container = $("recentList");
    container.innerHTML = "";
    if (!items || items.length === 0) {
      container.innerHTML = '<div class="hint">No transcriptions yet.</div>';
      return;
    }
    for (let i = 0; i < items.length; i++) {
      const row = document.createElement("div");
      row.className = "recent-row";
      const text = document.createElement("span");
      text.className = "recent-text";
      text.textContent = items[i];
      const btn = document.createElement("button");
      btn.className = "secondary recent-copy";
      btn.textContent = "Copy";
      btn.addEventListener("click", async () => {
        try {
          await invoke("copy_transcription", { index: i });
          btn.textContent = "Copied!";
          setTimeout(() => { btn.textContent = "Copy"; }, 1500);
        } catch (e) {
          btn.textContent = "Failed";
          setTimeout(() => { btn.textContent = "Copy"; }, 1500);
        }
      });
      row.appendChild(text);
      row.appendChild(btn);
      container.appendChild(row);
    }
  } catch (e) {
    // recent not available yet
  }
}

function fmtWhen(ms) {
  if (!ms) return "";
  const d = new Date(ms);
  const today = new Date();
  const sameDay = d.toDateString() === today.toDateString();
  const time = d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  if (sameDay) return time;
  const yesterday = new Date(today);
  yesterday.setDate(today.getDate() - 1);
  if (d.toDateString() === yesterday.toDateString()) return `yesterday ${time}`;
  return `${d.toLocaleDateString()} ${time}`;
}

function messageCount(n) {
  return n === 1 ? "1 message" : `${n} messages`;
}

async function refreshSessions() {
  let sessions;
  try {
    sessions = await invoke("list_sessions");
  } catch (e) {
    return;
  }
  const active = sessions.find((s) => s.active);
  $("sessionsHint").textContent = active
    ? `Active: “${active.title}”. New questions continue this session — use “New session” for a clean context.`
    : "Active: new session — the next question starts a clean context.";

  const list = $("sessionList");
  list.innerHTML = "";
  if (!sessions.length) {
    list.innerHTML = '<div class="hint">No sessions yet. Hold the ask-Gemini hotkey and speak a question.</div>';
  }
  for (const s of sessions) {
    const item = document.createElement("button");
    item.type = "button";
    item.className = "session-item" + (s.active ? " active" : "");
    const title = document.createElement("span");
    title.className = "session-item-title";
    title.textContent = s.title;
    if (s.active) {
      const badge = document.createElement("span");
      badge.className = "badge active";
      badge.textContent = "active";
      title.appendChild(badge);
    }
    const meta = document.createElement("span");
    meta.className = "session-item-meta";
    meta.textContent = `${messageCount(s.turns)} · ${fmtWhen(s.updatedAt)}`;
    item.appendChild(title);
    item.appendChild(meta);
    item.addEventListener("click", () => selectSession(s.id));
    list.appendChild(item);
  }

  if (state.sessionSelected) {
    if (sessions.some((s) => s.id === state.sessionSelected)) {
      await loadSessionDetail(state.sessionSelected);
    } else {
      state.sessionSelected = null;
      $("sessionDetailCard").hidden = true;
    }
  }
}

async function selectSession(id) {
  state.sessionSelected = id;
  await loadSessionDetail(id);
}

async function loadSessionDetail(id) {
  let session;
  try {
    session = await invoke("get_session", { id });
  } catch (e) {
    session = null;
  }
  if (!session) {
    state.sessionSelected = null;
    $("sessionDetailCard").hidden = true;
    return;
  }
  $("sessionDetailCard").hidden = false;
  $("sessionDetailTitle").textContent = session.title;
  $("sessionActiveBadge").hidden = !session.active;
  $("sessionDetailMeta").textContent =
    `Created ${fmtWhen(session.createdAt)} · ${messageCount(session.turns.length)} · last activity ${fmtWhen(session.updatedAt)}`;
  $("sessionUse").hidden = session.active;
  $("sessionRenameRow").hidden = true;

  const turns = $("sessionTurns");
  turns.innerHTML = "";
  for (const t of session.turns) {
    const row = document.createElement("div");
    row.className = `session-turn ${t.role === "user" ? "user" : "gemini"}`;
    const who = document.createElement("div");
    who.className = "session-turn-who";
    who.textContent = `${t.role === "user" ? "You" : "Gemini"} · ${fmtWhen(t.at)}`;
    const text = document.createElement("div");
    text.className = "session-turn-text";
    text.textContent = t.text;
    row.appendChild(who);
    row.appendChild(text);
    turns.appendChild(row);
  }
  renderPlan($("sessionPlan"), session.plan);
  renderArtifacts($("sessionArtifacts"), session.artifacts);
  renderTools($("sessionTools"), session.toolRuns);
}

async function newSession() {
  try {
    await invoke("new_session");
  } catch (e) {
    return;
  }
  state.sessionSelected = null;
  $("sessionDetailCard").hidden = true;
  await refreshSessions();
}

async function renameSelectedSession() {
  if (!state.sessionSelected) return;
  const title = $("sessionRenameInput").value.trim();
  try {
    await invoke("rename_session", { id: state.sessionSelected, title });
  } catch (e) {
    return;
  }
  $("sessionRenameRow").hidden = true;
  await refreshSessions();
}

let deleteArmed = false;
function deleteSelectedSession() {
  if (!state.sessionSelected) return;
  const btn = $("sessionDelete");
  if (!deleteArmed) {
    deleteArmed = true;
    btn.textContent = "Confirm delete";
    setTimeout(() => {
      deleteArmed = false;
      btn.textContent = "Delete";
    }, 2500);
    return;
  }
  deleteArmed = false;
  btn.textContent = "Delete";
  invoke("delete_session", { id: state.sessionSelected })
    .then(() => {
      state.sessionSelected = null;
      $("sessionDetailCard").hidden = true;
      return refreshSessions();
    })
    .catch(() => {});
}

function fmtBytes(n) {
  const units = ["B", "KB", "MB", "GB"];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i += 1;
  }
  return `${v.toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}

async function refreshUpdateStatus() {
  try {
    const u = await invoke("get_update_status");
    $("version").textContent = u.currentVersion || "";
    const av = $("aboutVersionSettings");
    if (av) av.textContent = u.currentVersion || "";
    $("checkUpdates").disabled = u.checking || u.installing;
    $("installUpdate").disabled = u.checking || u.installing;
    if (u.installing) {
      $("updateStatus").textContent = u.total > 0
        ? `Downloading v${u.available ? u.available.latestVersion : "…"}… ${fmtBytes(u.downloaded)} / ${fmtBytes(u.total)}`
        : "Preparing update…";
      $("updateProgress").hidden = false;
      $("updateBar").value = u.total > 0 ? u.downloaded / u.total : 0;
      $("updateText").textContent = u.total > 0 ? `${fmtBytes(u.downloaded)} / ${fmtBytes(u.total)}` : "Downloading…";
      $("installUpdate").hidden = true;
      return;
    }
    if (u.checking) {
      $("updateStatus").textContent = "Checking for updates…";
      $("installUpdate").hidden = true;
      return;
    }
    $("updateProgress").hidden = true;
    if (u.available) {
      $("updateStatus").textContent = `Update available: v${u.available.latestVersion}`;
      $("installUpdate").hidden = false;
      $("updateNotes").hidden = false;
      $("updateNotes").textContent = (u.available.notes || "").slice(0, 400) || "See the release page for details.";
    } else {
      $("updateStatus").textContent = "You’re up to date.";
      $("installUpdate").hidden = true;
      $("updateNotes").hidden = true;
    }
  } catch (e) {
    $("updateStatus").textContent = "Update status unavailable";
  }
}

const LANGUAGE_LABELS = {
  auto: "Auto-detect",
  en: "English",
  pt: "Português (Brasil)",
  es: "Español",
  fr: "Français",
  de: "Deutsch",
  it: "Italiano",
  nl: "Nederlands",
  pl: "Polski",
  tr: "Türkçe",
  ru: "Русский",
  uk: "Українська",
  ja: "日本語",
  ko: "한국어",
  zh: "中文",
  hi: "हिन्दी",
  ar: "العربية",
  he: "עברית",
  sv: "Svenska",
  da: "Dansk",
  fi: "Suomi",
  no: "Norsk",
  cs: "Čeština",
  hu: "Magyar",
  ro: "Română",
  el: "Ελληνικά",
  th: "ไทย",
  vi: "Tiếng Việt",
  id: "Bahasa Indonesia",
  ms: "Bahasa Melayu",
};

function fillLanguageSelect(languages) {
  const sel = $("language");
  sel.innerHTML = "";
  for (const code of languages || ["auto", "en", "pt"]) {
    const opt = document.createElement("option");
    opt.value = code;
    opt.textContent = LANGUAGE_LABELS[code] || code;
    sel.appendChild(opt);
  }
  sel.value = state.settings.language && languages.includes(state.settings.language)
    ? state.settings.language
    : "auto";
}

async function refreshModels() {
  const models = await invoke("get_models");
  const container = $("models");
  container.innerHTML = "";
  for (const m of models) {
    const row = document.createElement("div");
    row.className = "model";

    const radio = document.createElement("input");
    radio.type = "radio";
    radio.name = "model";
    radio.value = m.id;
    radio.checked = m.active;
    radio.addEventListener("change", () => {
      state.settings.modelId = m.id;
      fillLanguageSelect(m.languages);
      markDirty();
    });

    const body = document.createElement("div");
    body.className = "model-body";
    const name = document.createElement("div");
    name.className = "model-name";
    name.textContent = m.name;
    if (m.cloud) {
      const badge = document.createElement("span");
      badge.className = "badge active";
      badge.textContent = "cloud · Gemini";
      name.appendChild(badge);
    } else if (m.downloaded) {
      const badge = document.createElement("span");
      badge.className = "badge downloaded";
      badge.textContent = "downloaded";
      name.appendChild(badge);
    }
    if (m.active) {
      const badge = document.createElement("span");
      badge.className = "badge active";
      badge.textContent = "active";
      name.appendChild(badge);
    }
    const desc = document.createElement("div");
    desc.className = "model-desc";
    desc.textContent = m.description;

    body.appendChild(name);
    body.appendChild(desc);

    row.appendChild(radio);
    row.appendChild(body);

    if (!m.downloaded && !m.cloud) {
      const btn = document.createElement("button");
      btn.className = "secondary";
      btn.textContent = "Download";
      btn.addEventListener("click", async () => {
        btn.disabled = true;
        $("downloadProgress").hidden = false;
        $("downloadBar").value = 0;
        $("downloadText").textContent = "Starting download…";
        try {
          await invoke("download_model", { modelId: m.id });
        } catch (e) {
          $("downloadText").textContent = `Failed: ${e}`;
          btn.disabled = false;
        }
      });
      row.appendChild(btn);
    }

    container.appendChild(row);
  }
  return models;
}

const MODIFIER_CODES = new Set([
  "ShiftLeft", "ShiftRight",
  "ControlLeft", "ControlRight",
  "AltLeft", "AltRight",
  "MetaLeft", "MetaRight",
]);

function startHotkeyCapture(inputId, buttonId, settingsKey) {
  state.capturing = true;
  const btn = $(buttonId);
  btn.textContent = "Press keys… (Esc to cancel)";

  const held = { ctrl: false, alt: false, cmd: false, shift: false };

  function buildSpec(code) {
    const mods = [];
    if (held.ctrl) mods.push("ctrl");
    if (held.alt) mods.push("alt");
    if (held.cmd) mods.push("cmd");
    if (held.shift) mods.push("shift");
    mods.push(code);
    return mods.join("+");
  }

  const onKeyDown = (event) => {
    event.preventDefault();
    event.stopPropagation();
    const code = event.code;
    if (code === "Escape") {
      finish();
      return;
    }
    if (MODIFIER_CODES.has(code)) {
      if (code.startsWith("Control")) held.ctrl = true;
      else if (code.startsWith("Alt")) held.alt = true;
      else if (code.startsWith("Meta")) held.cmd = true;
      else if (code.startsWith("Shift")) held.shift = true;
      return;
    }
    const spec = buildSpec(code);
    $(inputId).value = spec;
    state.settings[settingsKey] = spec;
    markDirty();
    finish();
  };

  const onKeyUp = (event) => {
    const code = event.code;
    if (code.startsWith("Control")) held.ctrl = false;
    else if (code.startsWith("Alt")) held.alt = false;
    else if (code.startsWith("Meta")) held.cmd = false;
    else if (code.startsWith("Shift")) held.shift = false;
  };

  function finish() {
    state.capturing = false;
    btn.textContent = "Capture keys…";
    document.removeEventListener("keydown", onKeyDown, true);
    document.removeEventListener("keyup", onKeyUp, true);
    window.removeEventListener("blur", finish);
  }

  document.addEventListener("keydown", onKeyDown, true);
  document.addEventListener("keyup", onKeyUp, true);
  window.addEventListener("blur", finish);
}

function markDirty() {
  $("saved").hidden = true;
}

function fillSingleOption(sel, value) {
  sel.innerHTML = "";
  if (!value) return;
  const opt = document.createElement("option");
  opt.value = value;
  opt.textContent = value;
  opt.selected = true;
  sel.appendChild(opt);
}

function populateSelect(sel, items, current) {
  sel.innerHTML = "";
  const values = items.includes(current) ? items : [current, ...items].filter(Boolean);
  for (const v of values) {
    const opt = document.createElement("option");
    opt.value = v;
    opt.textContent = v;
    opt.selected = v === current;
    sel.appendChild(opt);
  }
}

function populateVoiceSelect(voices, current) {
  const sel = $("ttsVoice");
  sel.innerHTML = "";
  const groups = new Map();
  for (const v of voices) {
    const lang = v.languages && v.languages.length ? v.languages[0] : "Other";
    if (!groups.has(lang)) groups.set(lang, []);
    const detail = [v.persona, v.gender].filter(Boolean).join(", ");
    groups.get(lang).push({ id: v.id, label: detail ? `${v.id} — ${detail}` : v.id });
  }
  const langPref = (state.settings.language || "en").split("-")[0];
  const sortedLangs = [...groups.keys()].sort((a, b) => {
    const pa = a.toLowerCase().startsWith(langPref) ? 0 : 1;
    const pb = b.toLowerCase().startsWith(langPref) ? 0 : 1;
    return pa - pb || a.localeCompare(b);
  });
  let selected = false;
  for (const lang of sortedLangs) {
    const group = document.createElement("optgroup");
    group.label = lang;
    for (const v of groups.get(lang)) {
      const opt = document.createElement("option");
      opt.value = v.id;
      opt.textContent = v.label;
      if (!selected && v.id === current) {
        opt.selected = true;
        selected = true;
      }
      group.appendChild(opt);
    }
    sel.appendChild(group);
  }
  if (!selected && sel.options.length) sel.options[0].selected = true;
}

async function loadGeminiCatalog() {
  const btn = $("loadCatalog");
  const status = $("catalogStatus");
  btn.disabled = true;
  status.textContent = "Loading…";
  try {
    const catalog = await invoke("get_gemini_catalog", {
      apiKey: $("geminiKey").value || null,
      language: $("language").value || null,
    });
    populateSelect($("llmModel"), catalog.chatModels, state.settings.llmModel);
    populateSelect($("ttsModel"), catalog.ttsModels, state.settings.ttsModel);
    populateVoiceSelect(catalog.voices, state.settings.ttsVoice);
    state.settings.llmModel = $("llmModel").value;
    state.settings.ttsModel = $("ttsModel").value;
    state.settings.ttsVoice = $("ttsVoice").value;
    const lang = $("language").value;
    const langLabel = LANGUAGE_LABELS[lang] || lang || "all languages";
    status.textContent = `Loaded ${catalog.chatModels.length} LLM models, ${catalog.ttsModels.length} TTS models, ${catalog.voices.length} voices (${langLabel})`;
    markDirty();
  } catch (e) {
    status.textContent = `Failed: ${e}`;
  } finally {
    btn.disabled = false;
  }
}

async function save() {
  const el = $("hotkey");
  state.settings.hotkey = el.value.trim();
  state.settings.language = $("language").value;
  state.settings.sounds = $("sounds").checked;
  state.settings.launchAtLogin = $("launchAtLogin").checked;
  state.settings.initialPrompt = $("initialPrompt").value;
  state.settings.minRecordingMs = parseInt($("minMs").value, 10) || 300;
  state.settings.maxRecordingSecs = parseInt($("maxSecs").value, 10) || 120;
  state.settings.inputDevice = $("inputDevice").value;
  state.settings.autoUpdate = $("autoUpdate").checked;
  state.settings.llmHotkey = $("llmHotkey").value.trim();
  state.settings.screenshotHotkey = $("screenshotHotkey").value.trim();
  state.settings.geminiApiKey = $("geminiKey").value.trim();
  state.settings.llmModel = $("llmModel").value;
  state.settings.ttsModel = $("ttsModel").value;
  state.settings.ttsVoice = $("ttsVoice").value;
  state.settings.llmSystemPrompt = $("llmPrompt").value;
  state.settings.sessionLimit = parseInt($("sessionLimit").value, 10) || 30;
  state.settings.agentDeepWork = $("agentDeepWork").checked;
  state.settings.agentMaxSteps = parseInt($("agentMaxSteps").value, 10) || 24;
  state.settings.agentContextBudget = parseInt($("agentContextBudget").value, 10) || 120000;
  state.settings.workspaceRoot = $("workspaceRoot").value.trim();
  state.settings.agentLogEverything = $("agentLogEverything").checked;

  try {
    await invoke("update_settings", { settings: state.settings });
    $("saved").hidden = false;
    $("savedAgent").hidden = false;
    await refreshStatus();
    await refreshAgentTab();
  } catch (e) {
    alert(`Failed to save settings: ${e}`);
  }
}

function renderPlan(el, plan) {
  if (!el) return;
  el.innerHTML = "";
  if (!plan || !plan.length) return;
  const title = document.createElement("div");
  title.className = "agent-subhead";
  title.textContent = "Plan";
  el.appendChild(title);
  for (const item of plan) {
    const row = document.createElement("div");
    row.className = `plan-item ${item.status}`;
    const mark = document.createElement("span");
    mark.className = "plan-mark";
    mark.textContent =
      item.status === "completed"
        ? "✓"
        : item.status === "in_progress"
          ? "▶"
          : item.status === "blocked"
            ? "!"
            : "○";
    const text = document.createElement("span");
    text.textContent = item.title + (item.note ? ` — ${item.note}` : "");
    row.appendChild(mark);
    row.appendChild(text);
    el.appendChild(row);
  }
}

function renderArtifacts(el, artifacts) {
  if (!el) return;
  el.innerHTML = "";
  if (!artifacts || !artifacts.length) return;
  const title = document.createElement("div");
  title.className = "agent-subhead";
  title.textContent = "Artifacts";
  el.appendChild(title);
  for (const artifact of artifacts) {
    const row = document.createElement("div");
    row.className = "agent-artifact";
    row.innerHTML = `<span class="artifact-path">${escapeHtml(artifact.path)}</span><span class="artifact-meta">${fmtBytes(artifact.bytes)} · ${escapeHtml(artifact.sha256.slice(0, 10))}</span>`;
    el.appendChild(row);
  }
}

function renderTools(el, tools) {
  if (!el) return;
  el.innerHTML = "";
  if (!tools || !tools.length) return;
  const title = document.createElement("div");
  title.className = "agent-subhead";
  title.textContent = "Tool timeline";
  el.appendChild(title);
  for (const tool of tools) {
    const row = document.createElement("div");
    row.className = `tool-row ${tool.status}`;
    row.innerHTML = `<span class="tool-name">${escapeHtml(tool.name)}</span><span class="tool-status">${escapeHtml(tool.status)}</span>`;
    el.appendChild(row);
  }
}

async function refreshAgentPlan() {
  let session = null;
  try {
    session = await invoke("get_active_session");
  } catch {
    session = null;
  }
  $("agentSessionLabel").textContent = session
    ? `· ${session.title}`
    : "· no active session";
  renderPlan($("agentPlan"), session ? session.plan : []);
  renderArtifacts($("agentArtifacts"), session ? session.artifacts : []);
}

async function refreshRuns() {
  let runs = [];
  try {
    runs = await invoke("list_runs", { limit: 50 });
  } catch {
    runs = [];
  }
  const list = $("runList");
  list.innerHTML = "";
  if (!runs.length) {
    list.innerHTML = '<div class="hint">No runs yet. Ask the assistant something.</div>';
    $("runDetail").hidden = true;
    return;
  }
  for (const run of runs) {
    const row = document.createElement("div");
    row.className = `run-row ${run.status}`;
    const when = fmtWhen(run.startedAt);
    row.innerHTML = `<span class="run-status">${escapeHtml(run.status)}</span><span class="run-when">${when}</span><span class="run-meta">${run.stepCount} steps · ${run.tokensIn + run.tokensOut} tok</span>`;
    row.addEventListener("click", () => showRun(run.id));
    list.appendChild(row);
  }
}

async function showRun(id) {
  let detail = null;
  try {
    detail = await invoke("get_run", { id });
  } catch {
    detail = null;
  }
  const el = $("runDetail");
  if (!detail || !detail.run) {
    el.hidden = true;
    return;
  }
  const run = detail.run;
  const steps = detail.steps || [];
  const tools = detail.tools || [];
  const events = detail.events || [];
  el.hidden = false;
  el.innerHTML = "";
  const head = document.createElement("div");
  head.className = "run-detail-head";
  head.innerHTML = `<b>${escapeHtml(run.status)}</b> · ${escapeHtml(run.model || "")} · ${run.stepCount} steps · ${run.tokensIn} in / ${run.tokensOut} out · ${fmtWhen(run.startedAt)}`;
  const undo = document.createElement("button");
  undo.className = "secondary";
  undo.textContent = "Undo file changes";
  undo.addEventListener("click", async () => {
    if (!window.confirm("Restore the files this run changed?")) return;
    try {
      const count = await invoke("undo_run", { runId: run.id });
      addActivity(`Undo restored ${count} file(s)`, "ok");
      await refreshRuns();
    } catch (e) {
      addActivity(`Undo failed: ${e}`, "err");
    }
  });
  head.appendChild(undo);
  el.appendChild(head);

  const addSection = (title) => {
    const sub = document.createElement("div");
    sub.className = "agent-subhead";
    sub.textContent = title;
    el.appendChild(sub);
  };
  if (steps.length) {
    addSection("Steps");
    for (const step of steps) {
      const row = document.createElement("div");
      row.className = "run-step";
      row.textContent = `#${step.step}.${step.attempt} ${step.finishReason || "—"} · ${step.inputTokens}+${step.outputTokens} tok · ${step.durationMs} ms${step.error ? ` · ${step.error}` : ""}`;
      el.appendChild(row);
    }
  }
  if (tools.length) {
    addSection("Tools");
    for (const tool of tools) {
      const row = document.createElement("div");
      row.className = `run-tool ${tool.status}`;
      row.textContent = `${tool.name} · ${tool.status}${tool.outputPath ? " · spilled" : ""}`;
      el.appendChild(row);
    }
  }
  if (events.length) {
    addSection("Events");
    for (const event of events.slice(-40)) {
      const row = document.createElement("div");
      row.className = "run-event";
      row.textContent = `${event.type} · ${JSON.stringify(event).slice(0, 220)}`;
      el.appendChild(row);
    }
  }
}

async function refreshAgentPermissions() {
  let items = [];
  try {
    items = await invoke("list_permissions");
  } catch {
    items = [];
  }
  const list = $("permissionList");
  list.innerHTML = "";
  if (!items.length) {
    list.innerHTML = '<div class="hint">No saved grants. The assistant asks each time a write or side effect needs approval.</div>';
    return;
  }
  for (const item of items) {
    const row = document.createElement("div");
    row.className = "permission-row";
    row.innerHTML = `<span class="permission-action">${escapeHtml(item.action)}</span><span class="permission-resource">${escapeHtml(item.resource)}</span><span class="permission-scope">${item.scope === "global" ? "global" : "session"}</span>`;
    const revoke = document.createElement("button");
    revoke.className = "secondary";
    revoke.textContent = "Revoke";
    revoke.addEventListener("click", async () => {
      await invoke("revoke_permission", { id: item.id });
      await refreshAgentPermissions();
    });
    row.appendChild(revoke);
    list.appendChild(row);
  }
}

async function refreshJobs() {
  let jobs = [];
  try {
    jobs = await invoke("list_jobs", { limit: 50 });
  } catch {
    jobs = [];
  }
  const list = $("jobList");
  list.innerHTML = "";
  if (!jobs.length) {
    list.innerHTML = '<div class="hint">No background jobs. Start one above; it survives an app restart.</div>';
    return;
  }
  for (const job of jobs) {
    const row = document.createElement("div");
    row.className = `job-row ${job.status}`;
    row.innerHTML = `<span class="job-status">${escapeHtml(job.status)}</span><span class="job-kind">${escapeHtml(job.kind)}</span><span class="job-meta">${Math.round((job.progress || 0) * 100)}% · ${fmtWhen(job.createdAt)}</span>`;
    if (job.status === "queued" || job.status === "waiting" || job.status === "running") {
      const cancel = document.createElement("button");
      cancel.className = "secondary";
      cancel.textContent = "Cancel";
      cancel.addEventListener("click", async () => {
        await invoke("cancel_job", { id: job.id });
        await refreshJobs();
      });
      row.appendChild(cancel);
    }
    list.appendChild(row);
  }
}

async function refreshAgentStats() {
  let m = null;
  try {
    m = await invoke("agent_metrics");
  } catch {
    m = null;
  }
  const el = $("agentStats");
  el.innerHTML = "";
  if (!m) return;
  const rows = [
    ["Runs", m.runs],
    ["Steps", m.steps],
    ["Compactions", m.compactions],
    ["Retries", m.retries],
    ["Tool calls", m.tools],
    ["Tool failures", m.toolFailures],
    ["Input tokens", m.inputTokens],
    ["Output tokens", m.outputTokens],
    ["Avg first text", `${Math.round(m.avgFirstTextMs || 0)} ms`],
  ];
  for (const [label, value] of rows) {
    const stat = document.createElement("div");
    stat.className = "stat";
    stat.innerHTML = `<span class="stat-label">${label}:</span> <span>${value ?? "—"}</span>`;
    el.appendChild(stat);
  }
}

async function refreshAgentTab() {
  try {
    const path = await invoke("workspace_path");
    $("workspacePath").textContent = path;
  } catch {
    $("workspacePath").textContent = "";
  }
  await refreshAgentPlan();
  await refreshRuns();
  await refreshAgentPermissions();
  await refreshJobs();
  await refreshAgentStats();
}


async function init() {
  state.settings = await invoke("get_settings");
  $("hotkey").value = state.settings.hotkey;
  $("sounds").checked = state.settings.sounds;
  $("launchAtLogin").checked = state.settings.launchAtLogin;
  $("initialPrompt").value = state.settings.initialPrompt;
  $("minMs").value = state.settings.minRecordingMs;
  $("maxSecs").value = state.settings.maxRecordingSecs;
  $("inputDevice").value = state.settings.inputDevice || "";
  $("autoUpdate").checked = state.settings.autoUpdate !== false;
  $("llmHotkey").value = state.settings.llmHotkey || "";
  $("screenshotHotkey").value = state.settings.screenshotHotkey || "";
  $("geminiKey").value = state.settings.geminiApiKey || "";
  $("llmPrompt").value = state.settings.llmSystemPrompt || "";
  $("sessionLimit").value = state.settings.sessionLimit || 30;
  $("agentDeepWork").checked = !!state.settings.agentDeepWork;
  $("agentMaxSteps").value = state.settings.agentMaxSteps || 24;
  $("agentContextBudget").value = state.settings.agentContextBudget || 120000;
  $("workspaceRoot").value = state.settings.workspaceRoot || "";
  $("agentLogEverything").checked = !!state.settings.agentLogEverything;
  fillSingleOption($("llmModel"), state.settings.llmModel);
  fillSingleOption($("ttsModel"), state.settings.ttsModel);
  fillSingleOption($("ttsVoice"), state.settings.ttsVoice);

  $("loadCatalog").addEventListener("click", loadGeminiCatalog);

  $("captureHotkey").addEventListener("click", () =>
    startHotkeyCapture("hotkey", "captureHotkey", "hotkey"),
  );
  $("captureLlmHotkey").addEventListener("click", () =>
    startHotkeyCapture("llmHotkey", "captureLlmHotkey", "llmHotkey"),
  );
  $("captureShotHotkey").addEventListener("click", () =>
    startHotkeyCapture("screenshotHotkey", "captureShotHotkey", "screenshotHotkey"),
  );
  $("hotkey").addEventListener("change", (e) => {
    state.settings.hotkey = e.target.value.trim();
    markDirty();
  });
  $("llmHotkey").addEventListener("change", (e) => {
    state.settings.llmHotkey = e.target.value.trim();
    markDirty();
  });
  $("inputDevice").addEventListener("change", (e) => {
    state.settings.inputDevice = e.target.value;
    markDirty();
  });
  $("language").addEventListener("change", () => {
    markDirty();
    // Voice lists are pre-filtered by the selected language.
    if ($("geminiKey").value || state.settings.geminiApiKey) {
      loadGeminiCatalog();
    }
  });
  ["llmModel", "ttsModel", "ttsVoice"].forEach((id) => {
    $(id).addEventListener("change", markDirty);
  });
  $("sessionLimit").addEventListener("change", markDirty);
  $("save").addEventListener("click", save);
  $("saveAgent").addEventListener("click", save);
  $("refreshRuns").addEventListener("click", refreshRuns);
  $("refreshJobs").addEventListener("click", refreshJobs);
  $("enqueueJob").addEventListener("click", async () => {
    const prompt = $("jobPrompt").value.trim();
    if (!prompt) return;
    try {
      await invoke("enqueue_agent_job", { prompt, deep: true });
      $("jobPrompt").value = "";
      await refreshJobs();
    } catch (e) {
      addActivity(`Could not start job: ${e}`, "err");
    }
  });
  ["agentDeepWork", "agentMaxSteps", "agentContextBudget", "workspaceRoot", "agentLogEverything"].forEach(
    (id) => $(id).addEventListener("change", markDirty),
  );

  $("newSession").addEventListener("click", newSession);
  $("sessionUse").addEventListener("click", async () => {
    if (!state.sessionSelected) return;
    try {
      await invoke("set_active_session", { id: state.sessionSelected });
    } catch (e) {
      return;
    }
    await refreshSessions();
  });
  $("sessionRename").addEventListener("click", () => {
    $("sessionRenameInput").value = $("sessionDetailTitle").textContent;
    $("sessionRenameRow").hidden = false;
    $("sessionRenameInput").focus();
    $("sessionRenameInput").select();
  });
  $("sessionRenameSave").addEventListener("click", renameSelectedSession);
  $("sessionRenameCancel").addEventListener("click", () => {
    $("sessionRenameRow").hidden = true;
  });
  $("sessionRenameInput").addEventListener("keydown", (e) => {
    if (e.key === "Enter") renameSelectedSession();
    if (e.key === "Escape") $("sessionRenameRow").hidden = true;
  });
  $("sessionDelete").addEventListener("click", deleteSelectedSession);

  document.querySelectorAll(".tab").forEach((b) => {
    b.addEventListener("click", () => {
      switchTab(b.dataset.tab);
      if (b.dataset.tab === "agent") refreshAgentTab();
    });
  });

  const micButton = $("requestMicrophone");
  micButton.addEventListener("click", async () => {
    micButton.disabled = true;
    micButton.textContent = "Waiting for permission…";
    try {
      const result = await invoke("request_microphone");
      if (result === "authorized") {
        addActivity("Microphone access granted", "ok");
      } else if (result === "denied") {
        addActivity("Microphone access is denied — open System Settings to enable it", "warn");
        micButton.disabled = false;
        micButton.textContent = "Request Microphone…";
      } else {
        addActivity("Microphone permission prompt shown — check for the system dialog", "info");
      }
    } catch (e) {
      addActivity(`Microphone request failed: ${e}`, "err");
    }
    await refreshPermissions();
  });

  const openMicButton = $("openMicSettings");
  openMicButton.addEventListener("click", async () => {
    try {
      await invoke("open_mic_settings");
    } catch (e) {
      addActivity(`Failed to open System Settings: ${e}`, "err");
    }
  });

  const resetMicButton = $("resetMicrophone");
  resetMicButton.addEventListener("click", async () => {
    resetMicButton.disabled = true;
    resetMicButton.textContent = "Resetting…";
    try {
      const result = await invoke("reset_microphone");
      if (result === "requested") {
        addActivity("Mic permission reset — check for the system prompt now", "info");
      } else if (result === "authorized") {
        addActivity("Microphone access granted", "ok");
      } else {
        addActivity("Mic still denied after reset — enable it in System Settings", "warn");
      }
    } catch (e) {
      addActivity(`Reset failed: ${e}`, "err");
    }
    resetMicButton.disabled = false;
    resetMicButton.textContent = "Reset & ask again…";
    await refreshPermissions();
  });

  const accButton = $("requestAccessibility");
  accButton.addEventListener("click", async () => {
    try {
      await invoke("request_accessibility");
      await refreshPermissions();
      const guide = $("accGuide");
      guide.hidden = false;
      guide.textContent =
        "Grant it in System Settings → Privacy & Security → Accessibility, then relaunch VoiceKeyboard.";
      setTimeout(() => refreshPermissions(), 4000);
    } catch (e) {
      guide.hidden = false;
      guide.textContent = `Failed to request: ${e}`;
    }
  });
  setInterval(refreshPermissions, 3000);

  $("checkUpdates").addEventListener("click", async () => {
    try {
      await invoke("check_for_updates");
    } catch (e) {
      $("updateStatus").textContent = `Failed: ${e}`;
    }
  });
  $("installUpdate").addEventListener("click", async () => {
    try {
      await invoke("install_update");
    } catch (e) {
      $("updateStatus").textContent = `Failed: ${e}`;
    }
  });
  listen("update-status", () => {
    refreshUpdateStatus();
    refreshStatus();
  });
  listen("update-download-progress", () => {
    refreshUpdateStatus();
  });

  listen("model-download-progress", (e) => {
    const p = e.payload;
    $("downloadProgress").hidden = false;
    if (p.done) {
      $("downloadText").textContent = p.error ? `Download failed: ${p.error}` : "Done.";
      if (!p.error) refreshModels();
      addActivity(p.error ? `Model download failed: ${p.error}` : "Model downloaded", "info");
      return;
    }
    $("downloadBar").value = p.total > 0 ? p.downloaded / p.total : 0;
    $("downloadText").textContent = p.total > 0
      ? `${fmtBytes(p.downloaded)} / ${fmtBytes(p.total)}`
      : `${fmtBytes(p.downloaded)}`;
  });

  listen("model-loaded", () => {
    refreshModels();
    addActivity("Model loaded", "ok");
  });
  listen("model-missing", () => {
    refreshModels();
    addActivity("Model not downloaded yet", "warn");
  });
  listen("recording-started", () => {
    refreshStatus();
    addActivity("Recording… (hold the hotkey)", "rec");
  });
  listen("recording-stopped", refreshStatus);
  listen("transcribing", () => {
    refreshStatus();
    addActivity("Running speech-to-text…", "busy");
  });
  listen("transcript", (e) => {
    refreshStatus();
    const text = (e.payload && e.payload.text) || "";
    const ms = (e.payload && e.payload.inference_ms) || 0;
    const segs = (e.payload && e.payload.n_segments) || 0;
    const suffix = ms > 0 ? ` (${ms}ms, ${segs} segs)` : "";
    addActivity(`Copied: ${text}${suffix}`, "ok");
    refreshStats();
    refreshRecent();
  });
  listen("secure-skipped", () => {
    refreshStatus();
    addActivity("Skipped — password field detected", "warn");
  });
  listen("llm-status", (e) => {
    const s = (e && e.payload) || {};
    if (s.phase === "done") {
      addActivity(`Gemini: ${s.reply ? s.reply.slice(0, 120) : "…"}`, "ok");
    } else if (s.phase === "error") {
      addActivity(`Gemini error: ${s.error || "unknown"}`, "err");
    } else if (s.phase === "cancelled") {
      addActivity("Ask Gemini cancelled", "warn");
    } else if (s.phase === "listening") {
      addActivity("Ask Gemini — speak your question", "rec");
    }
  });
  listen("llm-session", () => {
    refreshSessions();
    refreshAgentPlan();
  });
  listen("agent-runs", () => {
    if (!$("tab-agent").hidden) refreshRuns();
  });
  listen("open-sessions", () => switchTab("sessions"));
  listen("paste-failed", (e) => {
    refreshStatus();
    addActivity(e.payload || "Failed to paste — text copied to clipboard", "err");
  });
  listen("app-error", (e) => {
    const text = `VoiceKeyboard: ${e.payload}`;
    const div = document.createElement("div");
    div.textContent = text;
    div.className = "status";
    document.body.prepend(div);
    addActivity(text, "err");
  });

  const models = await refreshModels();
  const activeModel = models.find((m) => m.active);
  fillLanguageSelect(activeModel ? activeModel.languages : null);
  await refreshDevices();
  await refreshPermissions();
  await refreshUpdateStatus();
  await refreshStats();
  await refreshRecent();
  await refreshSessions();
  await refreshStatus();
}

function addActivity(text, kind) {
  const list = $("activity");
  const empty = list.querySelector(".activity-empty");
  if (empty) empty.remove();
  const li = document.createElement("li");
  const time = new Date().toLocaleTimeString([], { hour12: false });
  li.innerHTML = `<span class="act-time">${time}</span><span class="act-${kind}">${escapeHtml(text)}</span>`;
  list.prepend(li);
  while (list.children.length > 30) list.removeChild(list.lastChild);
}

function escapeHtml(s) {
  return String(s)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

init().catch((e) => {
  $("status").textContent = `Failed to initialise: ${e}`;
});
