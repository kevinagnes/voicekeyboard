const invoke = window.__TAURI__.core.invoke;
const listen = window.__TAURI__.event.listen;

const $ = (id) => document.getElementById(id);

const MIC_LABELS = {
  authorized: "granted",
  denied: "denied — open System Settings → Privacy & Security → Microphone and enable VoiceKeyboard.",
  notDetermined: "not requested",
};

async function refreshPermissions() {
  try {
    const p = await invoke("get_permissions");
    const list = $("permissions");
    list.innerHTML = "";

    const micOk = p.micPermission === "authorized";
    const accOk = p.accessibilityTrusted;

    const items = [
      {
        label: "Microphone",
        ok: micOk,
        missing: MIC_LABELS[p.micPermission] || MIC_LABELS.notDetermined,
      },
      {
        label: "Accessibility (auto-paste)",
        ok: accOk,
        missing: "Not granted — open System Settings → Privacy & Security → Accessibility and enable VoiceKeyboard.",
      },
    ];
    for (const it of items) {
      const li = document.createElement("li");
      li.innerHTML = `${it.label}: `;
      const span = document.createElement("span");
      span.className = it.ok ? "ok" : "missing";
      span.textContent = it.ok ? "granted" : it.missing;
      li.appendChild(span);
      list.appendChild(li);
    }

    const micBtn = $("micAction");
    if (!micOk && p.micPermission === "denied") {
      micBtn.textContent = "Open System Settings…";
      micBtn.disabled = false;
    } else if (!micOk && p.micPermission === "notDetermined") {
      micBtn.textContent = "Request Microphone…";
      micBtn.disabled = false;
    } else {
      micBtn.textContent = "Request Microphone…";
      micBtn.disabled = true;
    }

    const accBtn = $("accAction");
    if (accOk) {
      accBtn.disabled = true;
      accBtn.textContent = "Request Accessibility…";
    } else {
      accBtn.disabled = false;
      accBtn.textContent = "Request Accessibility…";
    }

    $("bundleNote").hidden = true;
    if (!p.runningFromBundle && p.micPermission !== "authorized") {
      $("bundleNote").hidden = false;
      $("bundleNote").textContent =
        "Note: run the installed VoiceKeyboard.app, not the raw binary — macOS attributes the microphone prompt to the launching terminal otherwise.";
    }

    const status = $("status");
    if (micOk && accOk) {
      status.className = "status ok";
      status.textContent = "All permissions granted — continuing…";
    } else {
      status.className = "status";
      status.textContent = "Permissions missing — VoiceKeyboard is paused until granted.";
    }
  } catch (e) {
    $("status").textContent = "Error reading permissions";
  }
}

async function init() {
  $("quit").addEventListener("click", () => invoke("quit_app"));

  $("micAction").addEventListener("click", async () => {
    const btn = $("micAction");
    btn.disabled = true;
    const p = await invoke("get_permissions");
    try {
      if (p.micPermission === "denied") {
        await invoke("open_mic_settings");
      } else {
        await invoke("request_microphone");
      }
    } catch (e) {
      $("status").textContent = `Microphone request failed: ${e}`;
    }
    setTimeout(() => btn.disabled = false, 1500);
    refreshPermissions();
  });

  $("accAction").addEventListener("click", async () => {
    const btn = $("accAction");
    btn.disabled = true;
    try {
      await invoke("request_accessibility");
    } catch (e) {
      $("status").textContent = `Accessibility request failed: ${e}`;
    }
    setTimeout(() => btn.disabled = false, 1500);
    refreshPermissions();
  });

  listen("permissions-granted", () => {
    refreshPermissions();
  });

  setInterval(refreshPermissions, 1000);
  await refreshPermissions();
}

init().catch((e) => {
  $("status").textContent = `Failed to initialize: ${e}`;
});