const invoke = window.__TAURI__.core.invoke;
const listen = window.__TAURI__.event.listen;

const label = document.getElementById("label");
const dot = document.getElementById("dot");
const canvas = document.getElementById("wave");
const ctx = canvas.getContext("2d");

const BARS = 34;
const PAD = 6;
let W = 150;
let H = 34;

let lastW = 0;
let lastH = 0;
let lastDpr = 0;

function resize() {
  const cw = canvas.clientWidth || W;
  const ch = canvas.clientHeight || H;
  const dpr = window.devicePixelRatio || 1;
  if (cw === lastW && ch === lastH && dpr === lastDpr) {
    return;
  }
  W = cw;
  H = ch;
  lastW = cw;
  lastH = ch;
  lastDpr = dpr;
  canvas.width = Math.round(cw * dpr);
  canvas.height = Math.round(ch * dpr);
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
}
resize();

function roundRectPath(x, y, w, h, r) {
  const rr = Math.min(r, w / 2, h / 2);
  ctx.beginPath();
  ctx.moveTo(x + rr, y);
  ctx.arcTo(x + w, y, x + w, y + h, rr);
  ctx.arcTo(x + w, y + h, x, y + h, rr);
  ctx.arcTo(x, y + h, x, y, rr);
  ctx.arcTo(x, y, x + w, y, rr);
  ctx.closePath();
}

const history = new Array(BARS).fill(2);
let lastSample = 0;
let rollingMax = 0;
let active = "recording";
let wavePhase = 0;

function setState(kind, text) {
  active = kind;
  if (kind === "transcribing") {
    dot.style.display = "none";
    label.style.display = "none";
    wavePhase = 0;
    return;
  }
  dot.style.display = "";
  label.style.display = "";
  label.textContent = text;
  dot.style.background = kind === "recording" ? "#ff3b30" : "#34c759";
  dot.style.animation = kind === "recording" ? "pulse 0.9s ease-in-out infinite" : "none";
}

async function sampleLevel() {
  if (active !== "recording") {
    return;
  }
  try {
    const level = await invoke("get_audio_level");
    if (typeof level === "number") {
      lastSample = level;
    }
  } catch {
    lastSample = 0;
  }
}

function draw() {
  resize();
  ctx.clearRect(0, 0, W, H);

  if (active === "transcribing") {
    wavePhase += 0.09;
    const barW = (W - PAD * 2) / BARS;
    for (let i = 0; i < BARS; i++) {
      const h = 6 + 11 * (0.5 + 0.5 * Math.sin(wavePhase + i * 0.55));
      const x = PAD + i * barW;
      ctx.fillStyle = "rgba(245, 166, 35, 0.75)";
      roundRectPath(x + 0.5, H - h - 2, barW - 1.5, h, 2);
      ctx.fill();
    }
    requestAnimationFrame(draw);
    return;
  }

  if (active === "recording") {
    rollingMax = Math.max(lastSample, rollingMax * 0.92);
    const gain = Math.min(2.5, 520 / Math.max(60, rollingMax));
    const boosted = Math.min(1000, lastSample * gain);
    const barW = (W - PAD * 2) / BARS;
    const maxH = H - 4;
    const decay = 0.75;
    for (let i = 0; i < BARS; i++) {
      const target =
        (boosted / 1000) * maxH * (0.35 + 0.65 * Math.abs(Math.sin(i * 0.9)));
      history[i] += (Math.max(target, 2) - history[i]) * decay;
      const h = Math.min(maxH, history[i]);
      const x = PAD + i * barW;
      const y = H - (H - h) / 2 - h;
      const alpha = 0.45 + 0.55 * (h / maxH);
      ctx.fillStyle = `rgba(${70 + h * 3}, ${140 + h * 2}, 255, ${alpha.toFixed(2)})`;
      roundRectPath(x + 0.5, y, barW - 1.5, h, 2);
      ctx.fill();
    }
  }
  requestAnimationFrame(draw);
}

function safeListen(name, handler) {
  Promise.resolve(listen(name, handler)).catch(() => {});
}

safeListen("recording-started", () => {
  lastSample = 0;
  rollingMax = 0;
  setState("recording", "Recording");
});
safeListen("transcribing", () => setState("transcribing", ""));
safeListen("transcript", () => {
  active = "done";
  dot.style.display = "none";
  label.style.display = "none";
});
safeListen("discarded", (e) => {
  const why = (e.payload && String(e.payload)) || "";
  setState("done", why ? `Discarded: ${why.slice(0, 26)}` : "Discarded");
});
safeListen("secure-skipped", () => setState("done", "Skipped (password field)"));
safeListen("app-error", () => setState("done", "Error — see Settings"));

setInterval(sampleLevel, 50);
requestAnimationFrame(draw);
