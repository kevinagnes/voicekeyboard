const invoke = window.__TAURI__.core.invoke;

async function loadVersion() {
  try {
    const u = await invoke("get_update_status");
    document.getElementById("aboutVersion").textContent = u.current_version || "—";
  } catch {
    document.getElementById("aboutVersion").textContent = "—";
  }
}

loadVersion();
