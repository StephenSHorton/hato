// Hato GUI frontend — window.__TAURI__ (withGlobalTauri). No bundler.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

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
  if (!el) return;
  el.classList.toggle("hidden", !on);
}

function setNotice(el, text, kind) {
  if (!el) return;
  el.textContent = text;
  el.classList.remove("notice-error", "notice-ok");
  if (kind === "error") el.classList.add("notice-error");
  if (kind === "ok") el.classList.add("notice-ok");
  show(el, !!text);
}

// ── Tabs ──────────────────────────────────────────────────
document.querySelectorAll(".tab").forEach((tab) => {
  tab.addEventListener("click", () => {
    const name = tab.dataset.tab;
    document.querySelectorAll(".tab").forEach((t) => t.classList.toggle("is-active", t === tab));
    document.querySelectorAll(".panel").forEach((p) => {
      p.classList.toggle("is-active", p.dataset.panel === name);
    });
    if (name === "contacts") refreshContacts();
    if (name === "settings") loadSettings();
    if (name === "send") fillContactSelect();
  });
});

// ── SEND ──────────────────────────────────────────────────
let sendMode = "ticket"; // "ticket" | "contact"
const dropzone = $("dropzone");
const sendPreparing = $("send-preparing");
const sendPreparingText = $("send-preparing-text");
const sendError = $("send-error");
const sendResult = $("send-result");
const sendToStatus = $("send-to-status");

document.querySelectorAll("[data-send-mode]").forEach((btn) => {
  btn.addEventListener("click", () => {
    sendMode = btn.dataset.sendMode;
    document.querySelectorAll("[data-send-mode]").forEach((b) => {
      b.classList.toggle("is-active", b === btn);
    });
    show($("send-contact-row"), sendMode === "contact");
    show(sendResult, false);
    show(sendToStatus, false);
    if (sendMode === "contact") fillContactSelect();
  });
});

async function fillContactSelect() {
  const sel = $("send-contact");
  const prev = sel.value;
  try {
    const list = await invoke("list_contacts");
    sel.innerHTML = '<option value="">Select a contact…</option>';
    for (const c of list) {
      const opt = document.createElement("option");
      opt.value = c.id;
      opt.textContent = `${c.name} (${c.id})`;
      sel.appendChild(opt);
    }
    if (prev) sel.value = prev;
  } catch (err) {
    console.error(err);
  }
}

async function startSend(path) {
  if (!path) return;
  show(sendError, false);
  show(sendResult, false);
  show(sendToStatus, false);
  const base = path.split(/[\\/]/).pop();
  const relay = $("relay").checked;

  if (sendMode === "contact") {
    const contact = $("send-contact").value;
    if (!contact) {
      setNotice(sendError, "Pick a contact first (or pair one under Contacts).", "error");
      return;
    }
    sendPreparingText.textContent = `Offering ${base} to contact…`;
    show(sendPreparing, true);
    try {
      await invoke("send_to_contact", { path, contact, relay });
    } catch (err) {
      setNotice(sendError, String(err), "error");
    } finally {
      show(sendPreparing, false);
    }
    return;
  }

  sendPreparingText.textContent = `Preparing ${base}…`;
  show(sendPreparing, true);
  try {
    const res = await invoke("start_send", { path, relay });
    $("send-name").textContent = res.name;
    $("ticket").value = res.ticket;
    $("send-addr").textContent = addrSummary(res.relays, res.ips);
    show(sendResult, true);
  } catch (err) {
    setNotice(sendError, String(err), "error");
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
    setNotice(sendError, String(err), "error");
  }
  show(sendResult, false);
});

listen("tauri://drag-enter", () => dropzone.classList.add("is-drag"));
listen("tauri://drag-leave", () => dropzone.classList.remove("is-drag"));
listen("tauri://drag-drop", (e) => {
  dropzone.classList.remove("is-drag");
  const paths = e.payload && e.payload.paths;
  if (paths && paths.length) startSend(paths[0]);
});

listen("send-to-status", (e) => {
  const p = e.payload || {};
  if (p.phase === "preparing") {
    sendPreparingText.textContent = `Preparing for ${p.contact}…`;
    show(sendPreparing, true);
  } else if (p.phase === "offering") {
    sendPreparingText.textContent = `Waiting for ${p.contact} to accept…`;
    show(sendPreparing, true);
  } else if (p.phase === "done") {
    show(sendPreparing, false);
    setNotice(sendToStatus, `✅ ${p.contact} finished downloading ${p.label || ""}.`, "ok");
  }
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
let listenDirPath = null;

async function loadDefaultDir() {
  const dir = await invoke("default_download_dir");
  if (dir) {
    destPath = dir;
    dest.value = dir;
    listenDirPath = dir;
    $("listen-dir").value = dir;
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
    setNotice(recvError, "Paste a ticket first.", "error");
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
    setNotice(
      recvDone,
      `✅ Received ${humanBytes(res.total_bytes)} into ${res.out_dir}${resumed}`,
      "ok"
    );
    show(recvProgress, false);
  } catch (err) {
    setNotice(recvError, String(err), "error");
    show(recvProgress, false);
  } finally {
    btn.disabled = false;
  }
});

// ── CONTACTS ──────────────────────────────────────────────
async function loadMe() {
  try {
    const me = await invoke("get_me");
    $("me-name").textContent = me.displayName;
    $("me-id").textContent = `${me.endpointShort}  ·  ${me.endpointId}`;
    if ($("display-name")) $("display-name").value = me.displayName;
    if ($("config-dir")) $("config-dir").textContent = me.configDir || "—";
  } catch (err) {
    $("me-name").textContent = "Error";
    $("me-id").textContent = String(err);
  }
}

async function refreshContacts() {
  await loadMe();
  const list = $("contacts-list");
  list.innerHTML = "";
  try {
    const contacts = await invoke("list_contacts");
    show($("contacts-empty"), contacts.length === 0);
    for (const c of contacts) {
      const li = document.createElement("li");
      li.className = "contact-item";
      li.innerHTML = `
        <div class="contact-main">
          <strong class="contact-name">${escapeHtml(c.name)}</strong>
          <span class="mono dim">${escapeHtml(c.id)} · ${escapeHtml(c.endpointShort)}</span>
        </div>
        <div class="contact-actions">
          <button class="btn btn-ghost btn-sm" data-rename="${escapeAttr(c.id)}">Rename</button>
          <button class="btn btn-ghost btn-sm danger" data-remove="${escapeAttr(c.id)}">Remove</button>
        </div>`;
      list.appendChild(li);
    }
    list.querySelectorAll("[data-rename]").forEach((btn) => {
      btn.addEventListener("click", async () => {
        const id = btn.dataset.rename;
        const next = prompt("New display name:");
        if (!next) return;
        try {
          await invoke("rename_contact", { contact: id, newName: next });
          refreshContacts();
          fillContactSelect();
        } catch (err) {
          alert(String(err));
        }
      });
    });
    list.querySelectorAll("[data-remove]").forEach((btn) => {
      btn.addEventListener("click", async () => {
        const id = btn.dataset.remove;
        if (!confirm(`Remove contact ${id}?`)) return;
        try {
          await invoke("remove_contact", { contact: id });
          refreshContacts();
          fillContactSelect();
        } catch (err) {
          alert(String(err));
        }
      });
    });
  } catch (err) {
    setNotice($("contacts-empty"), String(err), "error");
    show($("contacts-empty"), true);
  }
}

function escapeHtml(s) {
  return String(s)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}
function escapeAttr(s) {
  return escapeHtml(s).replace(/'/g, "&#39;");
}

$("refresh-contacts").addEventListener("click", refreshContacts);

// Pair host
$("pair-host").addEventListener("click", async () => {
  const status = $("pair-host-status");
  const codeBox = $("pair-code-box");
  show(codeBox, false);
  show($("pair-sas"), false);
  setNotice(status, "Waiting for friend to join… (mailbox must be up)", "ok");
  $("pair-host").disabled = true;
  try {
    const mailbox = $("mailbox")?.value || null;
    const res = await invoke("pair_host", {
      name: null,
      mailbox: mailbox || null,
      insecure: false,
    });
    setNotice(status, `✅ Paired with ${res.contactName} (${res.contactId})`, "ok");
    show(codeBox, false);
    refreshContacts();
    fillContactSelect();
  } catch (err) {
    setNotice(status, String(err), "error");
  } finally {
    $("pair-host").disabled = false;
  }
});

listen("pair-code", (e) => {
  $("pair-code").textContent = e.payload;
  show($("pair-code-box"), true);
  setNotice($("pair-host-status"), "Code ready — share it with your friend.", "ok");
});

listen("pair-sas", (e) => {
  const el = $("pair-sas");
  el.textContent = `Verify aloud (optional): ${e.payload}`;
  show(el, true);
});

$("copy-pair-code").addEventListener("click", async () => {
  try {
    await navigator.clipboard.writeText($("pair-code").textContent);
  } catch {
    /* ignore */
  }
});

$("pair-join").addEventListener("click", async () => {
  const code = $("pair-join-code").value.trim();
  if (!code) {
    setNotice($("pair-join-status"), "Enter a pair code.", "error");
    return;
  }
  setNotice($("pair-join-status"), "Joining…", "ok");
  $("pair-join").disabled = true;
  try {
    const mailbox = $("mailbox")?.value || null;
    const res = await invoke("pair_join", {
      code,
      name: null,
      mailbox: mailbox || null,
      insecure: false,
    });
    setNotice($("pair-join-status"), `✅ Paired with ${res.contactName} (${res.contactId})`, "ok");
    refreshContacts();
    fillContactSelect();
  } catch (err) {
    setNotice($("pair-join-status"), String(err), "error");
  } finally {
    $("pair-join").disabled = false;
  }
});

// Listen
function setListenUi(running) {
  $("listen-start").disabled = running;
  $("listen-stop").disabled = !running;
  $("listen-state-label").textContent = running ? "On" : "Off";
  show($("listen-pill"), running);
}

$("listen-pick-folder").addEventListener("click", async () => {
  const dir = await invoke("pick_folder");
  if (dir) {
    listenDirPath = dir;
    $("listen-dir").value = dir;
  }
});

$("listen-start").addEventListener("click", async () => {
  try {
    await invoke("start_listen", { dir: listenDirPath });
    setListenUi(true);
    appendListenLog("Listening for contact offers…");
  } catch (err) {
    appendListenLog(String(err), true);
  }
});

$("listen-stop").addEventListener("click", async () => {
  try {
    await invoke("stop_listen");
  } catch (err) {
    appendListenLog(String(err), true);
  }
  setListenUi(false);
});

function appendListenLog(text, isErr) {
  const log = $("listen-log");
  show(log, true);
  const line = document.createElement("div");
  line.className = isErr ? "log-err" : "log-line";
  line.textContent = text;
  log.appendChild(line);
  log.scrollTop = log.scrollHeight;
}

listen("listen-status", (e) => {
  const p = e.payload || {};
  setListenUi(!!p.running);
  if (p.running) {
    appendListenLog(`Online as ${p.endpointShort || "…"} → ${p.dir || ""}`);
  } else {
    appendListenLog("Stopped listening.");
  }
});

listen("listen-offer", (e) => {
  const p = e.payload || {};
  appendListenLog(`Offer from ${p.from}: ${p.label}`);
});

listen("listen-progress", (e) => {
  const { done, total } = e.payload || {};
  if (total) {
    const pct = Math.round((done / total) * 100);
    // Keep last progress line light — overwrite via data attr not needed
    appendListenLog(`Downloading… ${pct}%`);
  }
});

listen("listen-done", (e) => {
  const p = e.payload || {};
  appendListenLog(`✅ Received ${p.label} (${humanBytes(p.totalBytes)}) → ${p.outDir}`);
  refreshContacts();
});

listen("listen-error", (e) => {
  appendListenLog(String(e.payload), true);
});

// ── SETTINGS + UPDATES ────────────────────────────────────
async function loadSettings() {
  await loadMe();
  try {
    $("mailbox").value = await invoke("get_mailbox");
  } catch {
    $("mailbox").value = "";
  }
  try {
    $("app-version").textContent = "v" + (await invoke("get_version"));
  } catch {
    $("app-version").textContent = "unknown";
  }
}

$("save-name").addEventListener("click", async () => {
  const name = $("display-name").value.trim();
  try {
    await invoke("set_display_name", { name });
    setNotice($("name-status"), "Saved.", "ok");
    loadMe();
  } catch (err) {
    setNotice($("name-status"), String(err), "error");
  }
});

async function checkUpdate(manual) {
  const status = $("update-status");
  if (manual) setNotice(status, "Checking…", "ok");
  try {
    const info = await invoke("check_for_update");
    if (!info) {
      if (manual) setNotice(status, "You're up to date.", "ok");
      return;
    }
    setNotice(status, `Updating to v${info.version}…`, "ok");
    show($("update-overlay"), true);
    $("update-overlay-text").textContent = `Updating to v${info.version}… Hato will restart.`;
    await invoke("download_and_install", { info });
  } catch (err) {
    show($("update-overlay"), false);
    if (manual) setNotice(status, String(err), "error");
  }
}

$("check-update").addEventListener("click", () => checkUpdate(true));

listen("update:installing", (e) => {
  show($("update-overlay"), true);
  $("update-overlay-text").textContent = `Updating to v${e.payload}… Hato will restart.`;
});

// ── Boot ──────────────────────────────────────────────────
(async () => {
  await loadDefaultDir();
  await loadMe();
  await fillContactSelect();
  try {
    const listening = await invoke("is_listening");
    setListenUi(!!listening);
  } catch {
    /* ignore */
  }
  try {
    $("app-version").textContent = "v" + (await invoke("get_version"));
  } catch {
    /* ignore */
  }
  // Backend also auto-updates on startup; frontend is for UI / manual check.
})();
