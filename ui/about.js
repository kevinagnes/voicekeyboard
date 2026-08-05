const invoke = window.__TAURI__.core.invoke;

async function loadVersion() {
  try {
    const u = await invoke("get_update_status");
    document.getElementById("aboutVersion").textContent = u.currentVersion || "—";
  } catch {
    document.getElementById("aboutVersion").textContent = "—";
  }
}

loadVersion();
