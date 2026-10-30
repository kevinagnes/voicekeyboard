const invoke = window.__TAURI__.core.invoke;

const shade = document.getElementById("shade");
const selection = document.getElementById("selection");
const hint = document.getElementById("hint");

let start = null;
let current = null;
let done = false;

function rect(a, b) {
  return {
    x: Math.min(a.x, b.x),
    y: Math.min(a.y, b.y),
    width: Math.abs(a.x - b.x),
    height: Math.abs(a.y - b.y),
  };
}

function draw(box) {
  selection.style.display = "block";
  selection.style.left = `${box.x}px`;
  selection.style.top = `${box.y}px`;
  selection.style.width = `${box.width}px`;
  selection.style.height = `${box.height}px`;
}

window.addEventListener("mousedown", (e) => {
  if (e.button !== 0 || done) return;
  start = { x: e.clientX, y: e.clientY };
  current = start;
  shade.style.display = "none";
  draw(rect(start, current));
});

window.addEventListener("mousemove", (e) => {
  if (!start || done) return;
  current = { x: e.clientX, y: e.clientY };
  draw(rect(start, current));
});

window.addEventListener("mouseup", (e) => {
  if (!start || done || e.button !== 0) return;
  const box = rect(start, { x: e.clientX, y: e.clientY });
  start = null;
  if (box.width < 8 || box.height < 8) {
    selection.style.display = "none";
    shade.style.display = "block";
    return;
  }
  done = true;
  selection.style.display = "none";
  hint.textContent = "Capturing…";
  invoke("screenshot_region_selected", {
    x: box.x,
    y: box.y,
    width: box.width,
    height: box.height,
  }).catch(() => {
    hint.textContent = "Could not capture the screenshot";
    setTimeout(() => invoke("cancel_screenshot").catch(() => {}), 900);
  });
});

window.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    e.preventDefault();
    invoke("cancel_screenshot").catch(() => {});
  }
});

window.addEventListener("contextmenu", (e) => e.preventDefault());
