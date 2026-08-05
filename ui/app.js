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
    $("version").textContent = u.current_version || "";
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
  state.settings.autoUpdate = $("autoUpdate").checked;

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
  $("autoUpdate").checked = state.settings.autoUpdate !== false;

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
  await refreshUpdateStatus();
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
