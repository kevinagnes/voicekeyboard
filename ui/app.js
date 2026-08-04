const invoke = window.__TAURI__.core.invoke;
const listen = window.__TAURI__.event.listen;

const $ = (id) => document.getElementById(id);
const state = { settings: null, capturing: false };

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
    } else if (!status.model_loaded) {
      el.className = "status";
      el.textContent = "Model not downloaded";
    } else {
      el.className = "status ok";
      el.textContent = `Ready — hold ${status.hotkey} to record`;
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
  const items = [
    {
      label: "Microphone",
      ok: p.hasInputDevice,
      missing: "No input device found. Connect a microphone.",
    },
    {
      label: "Accessibility (paste + password detection)",
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
      markDirty();
    });

    const body = document.createElement("div");
    body.className = "model-body";
    const name = document.createElement("div");
    name.className = "model-name";
    name.textContent = m.name;
    if (m.downloaded) {
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

    if (!m.downloaded) {
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
}

const MODIFIER_CODES = new Set([
  "ShiftLeft", "ShiftRight",
  "ControlLeft", "ControlRight",
  "AltLeft", "AltRight",
  "MetaLeft", "MetaRight",
]);

function startHotkeyCapture() {
  state.capturing = true;
  const btn = $("captureHotkey");
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
    $("hotkey").value = spec;
    state.settings.hotkey = spec;
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

  try {
    await invoke("update_settings", { settings: state.settings });
    $("saved").hidden = false;
    await refreshStatus();
  } catch (e) {
    alert(`Failed to save settings: ${e}`);
  }
}

async function init() {
  state.settings = await invoke("get_settings");
  $("hotkey").value = state.settings.hotkey;
  $("language").value = state.settings.language;
  $("sounds").checked = state.settings.sounds;
  $("launchAtLogin").checked = state.settings.launchAtLogin;
  $("initialPrompt").value = state.settings.initialPrompt;
  $("minMs").value = state.settings.minRecordingMs;
  $("maxSecs").value = state.settings.maxRecordingSecs;
  $("inputDevice").value = state.settings.inputDevice || "";

  $("captureHotkey").addEventListener("click", startHotkeyCapture);
  $("hotkey").addEventListener("change", (e) => {
    state.settings.hotkey = e.target.value.trim();
    markDirty();
  });
  $("inputDevice").addEventListener("change", (e) => {
    state.settings.inputDevice = e.target.value;
    markDirty();
  });
  $("save").addEventListener("click", save);

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
  listen("app-error", (e) => {
    const text = `VoiceKeyboard: ${e.payload}`;
    const div = document.createElement("div");
    div.textContent = text;
    div.className = "status";
    document.body.prepend(div);
    addActivity(text, "err");
  });

  await refreshModels();
  await refreshDevices();
  await refreshPermissions();
  await refreshStats();
  await refreshRecent();
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
