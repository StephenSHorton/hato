// Hato GUI frontend — talks to the Rust backend over the global Tauri bridge
// (`window.__TAURI__`, enabled by `withGlobalTauri` in tauri.conf.json). No
// bundler, no npm: just the core `invoke` + `event` APIs.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

// ── Helpers ───────────────────────────────────────────────
function humanBytes(n) {
  if (!Number.isFinite(n)) return "—";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v >= 100 || i === 0 ? Math.round(v) : v.toFixed(1)} ${units[i]}`;
}

function addrSummary(relays, ips) {
  const parts = [`${relays} relay${relays === 1 ? "" : "s"}`, `${ips} direct`];
  let line = `Ticket offers ${parts.join(", ")}.`;
  if (ips === 0) line += " No direct address — this transfer routes through the relay.";
  return line;
}

function show(el, on = true) {
  el.classList.toggle("hidden", !on);
}

// ── Tabs ──────────────────────────────────────────────────
document.querySelectorAll(".tab").forEach((tab) => {
  tab.addEventListener("click", () => {
    const name = tab.dataset.tab;
    document.querySelectorAll(".tab").forEach((t) => t.classList.toggle("is-active", t === tab));
    document.querySelectorAll(".panel").forEach((p) => {
      p.classList.toggle("is-active", p.dataset.panel === name);
    });
  });
});

// ── SEND ──────────────────────────────────────────────────
const dropzone = $("dropzone");
const sendPreparing = $("send-preparing");
const sendPreparingText = $("send-preparing-text");
const sendError = $("send-error");
const sendResult = $("send-result");

async function startSend(path) {
  if (!path) return;
  show(sendError, false);
  show(sendResult, false);
  sendPreparingText.textContent = `Preparing ${path.split(/[\\/]/).pop()}…`;
  show(sendPreparing, true);

  try {
    const relay = $("relay").checked;
    const res = await invoke("start_send", { path, relay });
    $("send-name").textContent = res.name;
    $("ticket").value = res.ticket;
    $("send-addr").textContent = addrSummary(res.relays, res.ips);
    show(sendResult, true);
  } catch (err) {
    sendError.textContent = String(err);
    show(sendError, true);
  } finally {
    show(sendPreparing, false);
  }
}

$("pick-file").addEventListener("click", async () => {
  const path = await invoke("pick_file");
  if (path) startSend(path);
});

$("copy-ticket").addEventListener("click", async () => {
  const text = $("ticket").value;
  const btn = $("copy-ticket");
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    // Fallback for environments without the async clipboard API.
    const ta = $("ticket");
    ta.focus();
    ta.select();
    document.execCommand("copy");
  }
  const prev = btn.textContent;
  btn.textContent = "Copied ✓";
  setTimeout(() => (btn.textContent = prev), 1400);
});

$("stop-send").addEventListener("click", async () => {
  try {
    await invoke("stop_send");
  } catch (err) {
    sendError.textContent = String(err);
    show(sendError, true);
  }
  show(sendResult, false);
});

// Drag-drop is delivered by Tauri (HTML5 drop is disabled when dragDropEnabled),
// and — unlike a browser File — gives us real absolute paths.
listen("tauri://drag-enter", () => dropzone.classList.add("is-drag"));
listen("tauri://drag-leave", () => dropzone.classList.remove("is-drag"));
listen("tauri://drag-drop", (e) => {
  dropzone.classList.remove("is-drag");
  const paths = e.payload && e.payload.paths;
  if (paths && paths.length) startSend(paths[0]);
});

// ── RECEIVE ───────────────────────────────────────────────
const recvError = $("recv-error");
const recvProgress = $("recv-progress");
const recvDone = $("recv-done");
const recvBar = $("recv-bar");
const recvPct = $("recv-pct");
const recvBytes = $("recv-bytes");
const recvStatus = $("recv-status");
const dest = $("dest");

let destPath = null;

async function loadDefaultDir() {
  const dir = await invoke("default_download_dir");
  if (dir) {
    destPath = dir;
    dest.value = dir;
  }
}
loadDefaultDir();

$("pick-folder").addEventListener("click", async () => {
  const dir = await invoke("pick_folder");
  if (dir) {
    destPath = dir;
    dest.value = dir;
  }
});

function setProgress(done, total) {
  const pct = total > 0 ? Math.min(100, (done / total) * 100) : 0;
  recvBar.style.width = `${pct}%`;
  recvPct.textContent = `${Math.round(pct)}%`;
  recvBytes.textContent = `${humanBytes(done)} / ${humanBytes(total)}`;
  recvStatus.textContent = total > 0 && done >= total ? "Finishing…" : "Downloading…";
}

listen("receive-progress", (e) => {
  const { done, total } = e.payload;
  setProgress(done, total);
});

$("receive").addEventListener("click", async () => {
  const ticket = $("ticket-in").value.trim();
  if (!ticket) {
    recvError.textContent = "Paste a ticket first.";
    show(recvError, true);
    return;
  }
  show(recvError, false);
  show(recvDone, false);
  show(recvProgress, true);
  recvStatus.textContent = "Connecting…";
  recvPct.textContent = "0%";
  recvBar.style.width = "0%";
  recvBytes.textContent = "";

  const btn = $("receive");
  btn.disabled = true;
  try {
    const res = await invoke("start_receive", { ticket, dir: destPath });
    setProgress(res.total_bytes, res.total_bytes);
    const resumed =
      res.already_had > 0 ? ` (resumed — ${humanBytes(res.already_had)} already had)` : "";
    recvDone.textContent = `✅ Received ${humanBytes(res.total_bytes)} into ${res.out_dir}${resumed}`;
    show(recvDone, true);
    show(recvProgress, false);
  } catch (err) {
    recvError.textContent = String(err);
    show(recvError, true);
    show(recvProgress, false);
  } finally {
    btn.disabled = false;
  }
});
