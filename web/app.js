// suzerain web ui — vanilla SPA (hash routing, fetch polling)
const $ = (sel, el = document) => el.querySelector(sel);
const main = $("#main");

async function api(path, opts = {}) {
  const r = await fetch(path, opts.method === "POST" ? {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(opts.body ?? {}),
  } : undefined);
  if (!r.ok) {
    let msg = `${r.status}`;
    try { msg = (await r.json()).error || msg; } catch {}
    throw new Error(msg);
  }
  return r.json();
}
const post = (path, body) => api(path, { method: "POST", body });

async function runAction(name, action, confirmText, force) {
  if (confirmText && !confirmAction(confirmText)) return;
  try {
    await post(`/api/v1/agents/${name}/${action}`, force ? { force: true } : {});
    toast(`${action} ${name}${force ? " (forced)" : ""}`, "ok");
    route();
  } catch (e) {
    // Daemon offline: offer to force the registry state anyway (the VM may
    // keep running orphaned; the force is audit-logged server-side).
    if (!force && (action === "stop" || action === "destroy") && /unreachable/.test(e.message) &&
        confirmAction(`Daemon unreachable. Force ${action} '${name}' anyway? The VM may keep running on that host.`)) {
      return runAction(name, action, null, true);
    }
    // Wedged agent: the daemon believes it's running but it isn't
    // processing — offer a force-restart (tears down the stale entry).
    if (!force && action === "start" && /already running/.test(e.message) &&
        confirmAction(`The daemon says '${name}' is already running. If it's wedged (not processing messages), force-restart will tear down the stale entry and start fresh. Force start?`)) {
      return runAction(name, action, null, true);
    }
    toast(`${action} failed: ${e.message}`, "err");
  }
}

function confirmAction(text) { return window.confirm(text); }

function actionButtons(a) {
  const btns = [];
  if (a.state === "suspended" || a.state === "failed") btns.push(`<button onclick="runAction('${a.name}','start')">Start</button>`);
  // Stop is always available: agents stuck in provisioning/restoring (or
  // whose daemon lost the agent) can still be stopped; unreachable daemons
  // trigger a force-stop prompt.
  btns.push(`<button onclick="runAction('${a.name}','stop')">Stop</button>`);
  if (a.state === "active") {
    btns.push(`<button onclick="runAction('${a.name}','suspend')">Suspend</button>`);
  }
  btns.push(`<button class="danger" onclick="destroyAgent('${a.name}')">Destroy</button>`);
  return `<div class="btn-row">${btns.join("")}</div>`;
}

window.runAction = runAction;
window.destroyAgent = (name) => {
  const typed = window.prompt(`Type the agent name to confirm destroy: ${name}`);
  if (typed === name) runAction(name, "destroy");
};

function toast(text, kind = "") {
  const t = document.createElement("div");
  t.className = `toast ${kind}`;
  t.textContent = text;
  $("#toasts").appendChild(t);
  setTimeout(() => t.remove(), 5000);
}

// LLM provider/model catalog (pi's supported providers), served by the
// control plane from web/providers.json (regenerate: node tools/gen-providers.mjs).
let catalogPromise = null;
function loadCatalog() {
  catalogPromise ??= fetch("/providers.json").then((r) => (r.ok ? r.json() : null)).catch(() => null);
  return catalogPromise;
}

// Harnesses suzerain can provision, with the versions the web UI offers.
// Served by the control plane from web/harnesses.json (single source of
// truth shared with the MCP server); falls back to a static default.
let harnessesPromise = null;
function loadHarnesses() {
  harnessesPromise ??= fetch("/harnesses.json").then((r) => (r.ok ? r.json() : null)).catch(() => null);
  return harnessesPromise;
}
const HARNESSES_FALLBACK = { pi: { label: "pi", versions: ["0.84.1"] } };

const esc = (s) => String(s ?? "").replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));

// Markdown rendering for chat messages: marked (GFM, single newlines → <br>)
// + DOMPurify for XSS safety. Links open in a new tab.
marked.setOptions({ gfm: true, breaks: true });
DOMPurify.addHook("afterSanitizeAttributes", (node) => {
  if (node.tagName === "A") { node.setAttribute("target", "_blank"); node.setAttribute("rel", "noopener"); }
});
const md = (text) => DOMPurify.sanitize(marked.parse(String(text ?? "")));
const shortId = (id) => (id || "").slice(0, 8);
const mib = (n) => (n == null ? "—" : n >= 1024 ? (n / 1024).toFixed(1) + " GiB" : n + " MiB");
const ago = (iso) => {
  if (!iso) return "—";
  const s = Math.max(0, (Date.now() - new Date(iso).getTime()) / 1000);
  if (s < 60) return `${Math.floor(s)}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
};
const stateBadge = (st) => `<span class="state ${esc(st)}">${esc(st)}</span>`;
const meter = (used, total) => {
  const pct = total > 0 ? Math.min(100, Math.round((100 * used) / total)) : 0;
  return `<div class="meter"><div style="width:${pct}%"></div></div>`;
};
const chips = (obj, cls = "") =>
  Object.entries(obj || {}).map(([k, v]) => `<span class="chip ${cls}">${esc(k)}=${esc(v)}</span>`).join("");

// ── router ────────────────────────────────────────────────────────────────
const routes = {
  fleet: viewFleet,
  // Plural keys take an optional id param: #/agents → list,
  // #/agents/<name> → details (links throughout use the plural form).
  castellans: (p) => (p ? viewCastellan(p) : viewCastellans()),
  "castellan-add": viewCastellanAdd,
  agents: (p) => (p ? viewAgent(p) : viewAgents()),
  create: viewCreate,
  session: viewSession,
  secrets: viewSecrets,
  activity: viewActivity,
};

// Form-heavy routes are excluded from auto-polling: re-rendering would wipe
// in-progress input (the secrets-add bug).
const NO_POLL = new Set(["secrets", "create", "castellan-add", "session"]);

let pollTimer = null;
function route() {
  clearTimeout(pollTimer);
  const hash = location.hash.slice(2) || "fleet";
  const [name, ...rest] = hash.split("/");
  const param = decodeURIComponent(rest.join("/"));
  document.querySelectorAll("#nav a").forEach((a) =>
    a.classList.toggle("active", a.dataset.route === name || (name === "fleet" && a.dataset.route === "fleet"))
  );
  const view = routes[name] || routes.fleet;
  const render = () => view(param).catch((e) => {
    main.innerHTML = `<div class="empty">error: ${esc(e.message)}</div>`;
  });
  render();
  if (!NO_POLL.has(name)) {
    pollTimer = setTimeout(function loop() {
      render();
      pollTimer = setTimeout(loop, 5000);
    }, 5000);
  }
}
window.addEventListener("hashchange", route);

// ── top bar ───────────────────────────────────────────────────────────────
async function topbar() {
  try {
    const [ep, ov] = await Promise.all([api("/api/v1/endpoint"), api("/api/v1/overview")]);
    $("#eid").textContent = shortId(ep.endpoint_id);
    $("#counts").textContent = `${ov.daemons_online}/${ov.daemons_total} daemons · ${ov.agents_total} agents`;
  } catch {}
}

// ── views ─────────────────────────────────────────────────────────────────
async function viewFleet() {
  const [ov, ds, act] = await Promise.all([
    api("/api/v1/overview"), api("/api/v1/daemons"), api("/api/v1/audit?tail=10"),
  ]);
  const stateRows = Object.entries(ov.agents_by_state).map(([s, n]) =>
    `<div class="card"><div class="num">${n}</div><div class="lbl">${stateBadge(s)}</div></div>`).join("");
  main.innerHTML = `
    <h1>Fleet</h1>
    <div class="cards">
      <div class="card"><div class="num">${ov.daemons_online}<span class="muted">/${ov.daemons_total}</span></div><div class="lbl">daemons online</div></div>
      <div class="card"><div class="num">${ov.agents_total}</div><div class="lbl">agents</div></div>
      ${stateRows}
    </div>
    ${ov.daemons_total === 0 ? `<div class="panel empty">No castellans yet — <a href="#/castellans">add one</a>.</div>` : ""}
    <h2>Capacity</h2>
    <div class="panel"><table>
      <tr><th>daemon</th><th>vcpu free</th><th>memory free</th><th>disk free</th></tr>
      ${ds.daemons.map((d) => {
        const c = d.capacity, u = d.usage;
        return `<tr class="clickable" onclick="location.hash='#/castellans/${d.endpoint_id}'">
          <td><span class="dot ${d.online ? "online" : "offline"}"></span>${esc(d.hostname)} <span class="muted mono">${shortId(d.endpoint_id)}</span></td>
          <td>${c.vcpu_total ?? "—"}</td><td>${mib(u.memory_mib_free)}</td><td>${mib(u.disk_mib_free)}</td></tr>`;
      }).join("")}
    </table></div>
    <h2>Recent activity</h2>
    <div class="panel">${act.entries.map((e) =>
      `<div class="logline"><span class="muted">${ago(e.at)}</span> <span class="kind">${esc(e.action)}</span> ${esc(JSON.stringify(e.detail)).slice(0, 140)}</div>`).join("") || '<div class="empty">quiet</div>'}
    </div>`;
}

async function viewCastellans() {
  const ds = await api("/api/v1/daemons");
  main.innerHTML = `
    <h1>Castellans <button class="primary" style="float:right" onclick="location.hash='#/castellan-add'">Add castellan</button></h1>
    <div id="pending"></div>
    <div class="panel"><table>
      <tr><th></th><th>hostname</th><th>endpoint</th><th>labels</th><th>capacity</th><th>free mem</th><th>last seen</th></tr>
      ${ds.daemons.map((d) => `<tr class="clickable" onclick="location.hash='#/castellans/${d.endpoint_id}'">
        <td><span class="dot ${d.online ? "online" : "offline"}"></span></td>
        <td>${esc(d.hostname)}</td><td class="mono muted">${shortId(d.endpoint_id)}</td>
        <td>${chips(d.labels)}</td>
        <td>${d.capacity.vcpu_total ?? "?"} vcpu · ${mib(d.capacity.memory_mib_total)} · ${(d.capacity.gpus || []).length} gpu</td>
        <td>${mib(d.usage.memory_mib_free)}</td>
        <td class="muted">${ago(d.last_seen)}</td></tr>`).join("")}
    </table></div>
    ${ds.daemons.length === 0 ? '<div class="panel empty">No castellans enrolled.</div>' : ""}`;
  renderPending();
}

async function viewCastellan(id) {
  const d = await api(`/api/v1/daemons/${id}`);
  const c = d.capacity, u = d.usage;
  const gpuRows = (c.gpus || []).map((g, i) => {
    const gu = (u.gpus || []).find((x) => x.index === g.index) || g;
    return `<tr><td>${g.index}</td><td>${esc(g.kind)}</td><td>${esc(g.name)}</td><td>${mib(g.vram_total_mib)}</td><td>${mib(gu.vram_free_mib)}</td></tr>`;
  }).join("");
  main.innerHTML = `
    <h1><span class="dot ${d.online ? "online" : "offline"}"></span>${esc(d.hostname)} <span class="muted mono">${shortId(d.endpoint_id)}</span></h1>
    <div class="cards">
      <div class="card"><div class="num">${c.vcpu_total ?? "—"}</div><div class="lbl">vcpu total</div></div>
      <div class="card"><div class="num">${mib(c.memory_mib_total)}</div><div class="lbl">memory total</div></div>
      <div class="card"><div class="num">${mib(u.memory_mib_free)}</div><div class="lbl">memory free</div></div>
      <div class="card"><div class="num">${mib(u.disk_mib_free)}</div><div class="lbl">disk free</div></div>
      <div class="card"><div class="num">${(c.gpus || []).length}</div><div class="lbl">gpus</div></div>
    </div>
    <div class="grid2">
      <div>
        <h2>Labels <button style="float:right" onclick="editLabels('${d.endpoint_id}')">edit</button></h2>
        <div class="panel">
          ${chips(d.labels)}
          ${Object.keys(d.label_overrides || {}).length ? `<div class="muted" style="margin-top:6px">overrides: ${chips(d.label_overrides, "override")}</div>` : ""}
          ${Object.keys(d.labels || {}).length === 0 ? '<span class="muted">none</span>' : ""}
        </div>
        <h2>Details</h2>
        <div class="panel"><dl class="kv">
          <dt>endpoint</dt><dd class="mono">${esc(d.endpoint_id)}</dd>
          <dt>os/arch</dt><dd>${esc(d.os)}/${esc(d.arch)}</dd>
          <dt>approved</dt><dd>${d.approved ? "yes" : "no"}</dd>
          <dt>load (1m)</dt><dd>${(u.cpu_load1 ?? 0).toFixed(2)}</dd>
          <dt>last seen</dt><dd>${ago(d.last_seen)}</dd>
        </dl></div>
        ${gpuRows ? `<h2>GPUs</h2><div class="panel"><table><tr><th>#</th><th>kind</th><th>name</th><th>vram total</th><th>vram free</th></tr>${gpuRows}</table></div>` : ""}
      </div>
      <div>
        <h2>Agents on this daemon (${(d.agents || []).length})</h2>
        <div class="panel"><table>
          <tr><th>name</th><th>state</th><th>resources</th></tr>
          ${(d.agents || []).map((a) => `<tr class="clickable" onclick="location.hash='#/agents/${a.name}'">
            <td>${esc(a.name)}</td><td>${stateBadge(a.state)}</td>
            <td class="muted">${a.manifest.resources.vcpu}vcpu ${mib(a.manifest.resources.memory_mib)}${a.manifest.resources.gpu ? " gpu:" + a.manifest.resources.gpu.count : ""}</td></tr>`).join("") || '<tr><td class="muted">none</td></tr>'}
        </table></div>
        <h2>Activity</h2>
        <div class="panel">${(d.audit || []).slice(-15).map((e) =>
          `<div class="logline"><span class="muted">${ago(e.at)}</span> <span class="kind">${esc(e.action)}</span> ${esc(JSON.stringify(e.detail)).slice(0, 120)}</div>`).join("") || '<div class="empty">quiet</div>'}
        </div>
      </div>
    </div>`;
}

async function viewAgents() {
  const data = await api("/api/v1/agents");
  main.innerHTML = `
    <h1>Agents <button class="primary" style="float:right" onclick="location.hash='#/create'">Create agent</button></h1>
    <div class="panel"><table>
      <tr><th>name</th><th>state</th><th>daemon</th><th>model</th><th>resources</th><th>created</th><th></th></tr>
      ${data.agents.map((a) => `<tr class="clickable" onclick="location.hash='#/agents/${a.name}'">
        <td>${esc(a.name)}</td><td>${stateBadge(a.state)}</td>
        <td class="muted">${esc(a.daemon_hostname || shortId(a.daemon_endpoint_id))}</td>
        <td class="muted">${esc(a.manifest.model.provider)}/${esc(a.manifest.model.id)}</td>
        <td class="muted">${a.manifest.resources.vcpu}vcpu ${mib(a.manifest.resources.memory_mib)}${a.manifest.resources.gpu ? " gpu:" + a.manifest.resources.gpu.count : ""}</td>
        <td class="muted">${ago(a.created_at)}</td>
        <td onclick="event.stopPropagation()"><div class="btn-row">${
          (a.state === "active"
            ? `<button class="primary" onclick="location.hash='#/session/${a.name}'">Chat</button>`
            : "") +
          (a.state === "suspended" || a.state === "failed"
            ? `<button onclick="runAction('${a.name}','start')">Start</button>`
            : "") +
          `<button onclick="runAction('${a.name}','stop')">Stop</button>` +
          `<button class="danger" onclick="destroyAgent('${a.name}')">Delete</button>`
        }</div></td></tr>`).join("")}
    </table></div>
    ${data.agents.length === 0 ? '<div class="panel empty">No agents yet — create one above.</div>' : ""}`;
}

async function viewAgent(name) {
  const a = await api(`/api/v1/agents/${name}`);
  const logs = await api(`/api/v1/agents/${name}/logs?tail=60`);
  const d = a.daemon || {};
  main.innerHTML = `
    <h1>${esc(a.name)} ${stateBadge(a.state)}
    ${a.state === "active"
      ? `<button class="primary chat-cta" onclick="location.hash='#/session/${a.name}'">Join chat</button>`
      : `<button class="chat-cta" disabled title="chat is available while the agent is active">Join chat</button>`}</h1>
    <div class="panel">${actionButtons(a)}</div>
    <div class="panel"><dl class="kv">
      <dt>id</dt><dd class="mono">${esc(a.id)}</dd>
      <dt>daemon</dt><dd><a href="#/castellans/${d.endpoint_id || ""}">${esc(d.hostname || "")}</a> <span class="muted mono">${shortId(a.daemon_endpoint_id)}</span></dd>
      <dt>model</dt><dd>${esc(a.manifest.model.provider)}/${esc(a.manifest.model.id)}${a.manifest.model.thinking ? " (" + esc(a.manifest.model.thinking) + ")" : ""}</dd>
      <dt>resources</dt><dd>${a.manifest.resources.vcpu} vcpu · ${mib(a.manifest.resources.memory_mib)} · ${mib(a.manifest.resources.disk_mib)}${a.manifest.resources.gpu ? ` · gpu ${a.manifest.resources.gpu.count}${a.manifest.resources.gpu.vram_mib ? " (" + mib(a.manifest.resources.gpu.vram_mib) + " vram)" : ""}` : ""}</dd>
      <dt>created</dt><dd>${ago(a.created_at)}</dd>
      <dt>events</dt><dd>${a.event_count} (last ${ago(a.last_event_at)})</dd>
      <dt>session file</dt><dd class="mono muted">${esc(a.session_file || "—")}</dd>
    </dl></div>
    <div class="grid2">
      <div>
        <h2>Manifest</h2>
        <div class="panel"><pre>${esc(a.manifest_toml)}</pre></div>
      </div>
      <div>
        <h2>Logs (latest ${logs.events.length})</h2>
        <div class="panel">${logs.events.map((e) =>
          `<div class="logline"><span class="seq muted">#${e.seq}</span><span class="kind">${esc(e.kind)}</span> <span class="muted">${ago(e.at)}</span></div>`).join("") || '<div class="empty">no events</div>'}
        </div>
      </div>
    </div>`;
}

async function viewSecrets() {
  const [s, catalog] = await Promise.all([api("/api/v1/secrets"), loadCatalog()]);
  const providers = s.entries.filter((e) => e.kind === "provider");
  const git = s.entries.filter((e) => e.kind === "git");
  const extras = s.entries.filter((e) => e.kind === "extra");
  const configured = new Set(providers.map((e) => e.name));
  const catalogIds = catalog ? Object.keys(catalog.providers) : [];
  // Provider picker: all pi-supported providers (incl. kimi-coding). Falls
  // back to a free-form input if the catalog can't be loaded.
  const providerPicker = catalogIds.length
    ? `<select id="new-provider" style="width:240px">${catalogIds.map((p) =>
        `<option value="${esc(p)}"${configured.has(p) ? ' data-has-key' : ""}>${esc(p)}${configured.has(p) ? " (key configured)" : ""}</option>`).join("")}</select>`
    : `<input id="new-provider" placeholder="provider id (e.g. openai)" style="width:220px">`;
  const row = (e, kind, name) => `<tr>
    <td class="muted">${esc(kind)}</td><td>${esc(name)}</td><td class="muted">${e.used_by || "—"}</td>
    <td><button onclick="revealSecret('${kind}','${name}')">reveal</button>
        <button onclick="editSecret('${kind}','${name}')">replace</button>
        <button class="danger" onclick="deleteSecret('${kind}','${name}')">delete</button></td></tr>`;
  main.innerHTML = `
    <h1>Secrets</h1>
    ${!s.store_present ? '<div class="panel empty">No secrets store configured — create secrets.sops.yaml via sops (see README).</div>' : ""}
    <h2>Providers</h2>
    <div class="panel"><table>
      <tr><th>kind</th><th>provider</th><th>used by</th><th></th></tr>
      ${providers.map((e) => row(e, "provider", e.name)).join("") || '<tr><td class="muted" colspan="4">none</td></tr>'}
    </table>
    <div style="margin-top:10px" class="btn-row">
      ${providerPicker}
      <input id="new-provider-value" placeholder="api key (write-only)" style="width:320px" type="password">
      <button onclick="addProvider()">Add provider</button>
    </div></div>
    <h2>Git deploy key</h2>
    <div class="panel">
      ${git.length ? `<p>deploy key configured (masked) <button onclick="revealSecret('git','deploy_key')">reveal</button> <button class="danger" onclick="deleteSecret('git','deploy_key')">delete</button></p>` : '<p class="muted">not configured</p>'}
      <textarea id="new-deploy-key" rows="4" placeholder="-----BEGIN OPENSSH PRIVATE KEY----- (write-only)"></textarea>
      <div class="btn-row" style="margin-top:8px"><button onclick="addDeployKey()">${git.length ? "Replace" : "Add"} deploy key</button></div>
    </div>
    <h2>Extra secrets</h2>
    <div class="panel"><table>
      <tr><th>kind</th><th>name</th><th></th><th></th></tr>
      ${extras.map((e) => row(e, "extra", e.name)).join("") || '<tr><td class="muted" colspan="4">none</td></tr>'}
    </table>
    <div style="margin-top:10px" class="btn-row">
      <input id="new-extra" placeholder="name" style="width:200px">
      <input id="new-extra-value" placeholder="value (write-only)" style="width:320px" type="password">
      <button onclick="addExtra()">Add</button>
    </div></div>
    <p class="muted">Values are masked and write-only. Reveal returns a value once and is audit-logged.</p>`;
}

window.revealSecret = async (kind, name) => {
  try {
    const r = await post("/api/v1/secrets/reveal", { kind, name });
    const dlg = document.createElement("div");
    dlg.className = "toast";
    dlg.style.borderLeftColor = "var(--warn)";
    dlg.innerHTML = `<b>${esc(name)}</b> (shown once, audit-logged):<br><code>${esc(r.value)}</code>`;
    $("#toasts").appendChild(dlg);
    setTimeout(() => dlg.remove(), 12000);
  } catch (e) { toast(`reveal failed: ${e.message}`, "err"); }
};

window.editSecret = async (kind, name) => {
  const value = window.prompt(`New value for ${name} (write-only):`);
  if (!value) return;
  await setSecret(kind, name, value);
};

window.addProvider = async () => {
  const id = $("#new-provider").value.trim(), v = $("#new-provider-value").value.trim();
  if (!id || !v) return toast("provider id and value required", "err");
  await setSecret("provider", id, v, true);
};

window.addDeployKey = async () => {
  const v = $("#new-deploy-key").value.trim();
  if (!v) return toast("paste the key", "err");
  await setSecret("git", "deploy_key", v, true);
};

window.addExtra = async () => {
  const name = $("#new-extra").value.trim(), v = $("#new-extra-value").value.trim();
  if (!name || !v) return toast("name and value required", "err");
  await setSecret("extra", name, v, true);
};

window.deleteSecret = async (kind, name) => {
  if (!confirmAction(`Delete ${kind} '${name}'? Agents using it will fail to spawn.`)) return;
  try {
    const path = kind === "provider" ? `/api/v1/secrets/providers/${name}`
      : kind === "git" ? "/api/v1/secrets/git-deploy-key"
      : `/api/v1/secrets/extra/${name}`;
    await fetch(path, { method: "DELETE" });
    toast(`deleted ${name}`, "ok");
    route();
  } catch (e) { toast(`delete failed: ${e.message}`, "err"); }
};

async function setSecret(kind, name, value, isNew) {
  try {
    const path = kind === "provider" ? `/api/v1/secrets/providers/${name}`
      : kind === "git" ? "/api/v1/secrets/git-deploy-key"
      : `/api/v1/secrets/extra/${name}`;
    const r = await fetch(path, { method: "PUT", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ value }) });
    if (!r.ok) throw new Error((await r.json()).error || `${r.status}`);
    toast(`${isNew ? "added" : "replaced"} ${name}`, "ok");
    route();
  } catch (e) { toast(`save failed: ${e.message}`, "err"); }
}

// ── M4: add castellan ──────────────────────────────────────────────────────
async function renderPending() {
  const el = $("#pending");
  if (!el) return;
  try {
    const p = await api("/api/v1/daemons/pending");
    if (p.pending.length === 0) { el.innerHTML = ""; return; }
    el.innerHTML = `
      <h2>Pending enrollments (${p.pending.length})</h2>
      <div class="panel" style="border-color:var(--warn)"><table>
        <tr><th>hostname</th><th>endpoint</th><th>os/arch</th><th>capacity</th><th>seen</th><th></th></tr>
        ${p.pending.map((d) => `<tr>
          <td>${esc(d.hostname)}</td><td class="mono muted">${shortId(d.endpoint_id)}</td>
          <td class="muted">${esc(d.os)}/${esc(d.arch)}</td>
          <td class="muted">${d.capacity.vcpu_total ?? "?"} vcpu · ${mib(d.capacity.memory_mib_total)}</td>
          <td class="muted">${ago(d.last_seen)}</td>
          <td><button class="primary" onclick="approvePending('${d.endpoint_id}')">Approve</button>
              <button class="danger" onclick="dismissPending('${d.endpoint_id}')">Dismiss</button></td></tr>`).join("")}
      </table></div>`;
  } catch {}
}

window.approvePending = async (id) => {
  try {
    await post(`/api/v1/daemons/pending/${id}/approve`);
    toast("daemon approved", "ok");
    route();
  } catch (e) { toast(`approve failed: ${e.message}`, "err"); }
};

window.dismissPending = async (id) => {
  try {
    await post(`/api/v1/daemons/pending/${id}/dismiss`);
    toast("dismissed", "ok");
    renderPending();
  } catch (e) { toast(`dismiss failed: ${e.message}`, "err"); }
};

async function viewCastellanAdd() {
  const ep = await api("/api/v1/endpoint");
  main.innerHTML = `
    <h1>Add castellan</h1>
    <div class="panel">
      <h2>1 · On the new machine</h2>
      <p>Install prerequisites (qemu + mise), install the binaries, then:</p>
      <pre>castellan init --suzerain ${esc(ep.endpoint_id)}
castellan run</pre>
      <p class="muted">On the same LAN, mDNS discovery finds this control plane automatically; off-LAN it uses the public iroh relays.</p>
      <h2>2 · Approve it here</h2>
      <p>The daemon appears under <a href="#/castellans">Castellans → pending enrollments</a>, or paste its EndpointId:</p>
      <div class="btn-row">
        <input id="manual-eid" placeholder="daemon EndpointId" class="mono" style="flex:1">
        <button class="primary" onclick="manualApprove()">Approve</button>
      </div>
    </div>
    <div id="pending"></div>`;
  renderPending();
  const t = setInterval(() => {
    if (!location.hash.startsWith("#/castellan-add")) clearInterval(t);
    else renderPending();
  }, 5000);
}

window.manualApprove = async () => {
  const id = $("#manual-eid").value.trim();
  if (!id) return;
  try {
    await post(`/api/v1/daemons/pending/${id}/approve`);
    toast("approved", "ok");
    location.hash = "#/castellans";
  } catch (e) {
    // Fall back to the direct approve path for daemons that never registered.
    try {
      await post("/api/v1/daemons/approve", { endpoint_id: id });
      toast("approved", "ok");
      location.hash = "#/castellans";
    } catch (e2) { toast(`approve failed: ${e.message}`, "err"); }
  }
};

async function viewActivity() {
  const a = await api("/api/v1/audit?tail=100");
  main.innerHTML = `
    <h1>Activity</h1>
    <div class="panel">
      ${a.entries.map((e) => `<div class="logline"><span class="muted">${ago(e.at)}</span> <span class="kind">${esc(e.action)}</span> ${esc(JSON.stringify(e.detail)).slice(0, 200)}</div>`).join("") || '<div class="empty">quiet</div>'}
    </div>`;
}

// ── M2: labels editor ────────────────────────────────────────────────────
window.editLabels = async (daemonId) => {
  const input = window.prompt(
    "Labels as k=v pairs, comma-separated. Prefix with - to remove (e.g. gpu=true,zone=office,-old):"
  );
  if (input == null) return;
  const set = {}, remove = [];
  for (const part of input.split(",").map((x) => x.trim()).filter(Boolean)) {
    if (part.startsWith("-")) remove.push(part.slice(1));
    else {
      const [k, v] = part.split("=").map((x) => x.trim());
      if (k && v != null) set[k] = v;
    }
  }
  try {
    const r = await post(`/api/v1/daemons/${daemonId}/labels`, { set, remove });
    toast("labels updated", "ok");
    route();
  } catch (e) { toast(`labels failed: ${e.message}`, "err"); }
};

// ── M2: create agent (form ⇄ TOML) ──────────────────────────────────────
const DEFAULT_MANIFEST = `name = "my-agent"
harness = { type = "pi", version = "0.84.1" }
model = { provider = "kimi-coding", id = "kimi-for-coding" }

[resources]
vcpu = 2
memory_mib = 2048
disk_mib = 5120

# [[repos]]
# url = "git@github.com:org/repo.git"
# ref = "main"

# [[extensions]]
# source = "npm:@scope/pi-package"        # pi.dev catalog package
# [[extensions]]
# url = "git@github.com:me/ext.git"       # or a pinned git repo
# ref = "v1.2.0"

# [prompt]
# append_system = """
# Extra instructions appended to pi's system prompt (APPEND_SYSTEM.md).
# """

[secrets]
providers = ["kimi-coding"]

# [schedule]
# require = { zone = "office" }
`;

async function viewCreate() {
  const [secrets, catalog, harnessDoc] = await Promise.all([
    api("/api/v1/secrets").catch(() => ({ entries: [] })),
    loadCatalog(),
    loadHarnesses(),
  ]);
  const harnesses = (harnessDoc && harnessDoc.harnesses) || HARNESSES_FALLBACK;
  // Only providers with a configured API key (Secrets page) are offered.
  const configured = secrets.entries.filter((e) => e.kind === "provider").map((e) => e.name);
  const providerOptions = configured.length
    ? configured.map((p) => `<option value="${esc(p)}">${esc(p)}</option>`).join("")
    : `<option value="">(no provider keys configured)</option>`;
  const harnessOptions = Object.entries(harnesses)
    .map(([id, h]) => `<option value="${esc(id)}">${esc(h.label)}</option>`).join("");
  main.innerHTML = `
    <h1>Create agent</h1>
    ${configured.length === 0 ? `<div class="panel" style="border-color:var(--warn)">No LLM provider keys configured — add one under <a href="#/secrets">Secrets</a> first.</div>` : ""}
    <div class="grid2">
      <div class="panel">
        <label>Name</label><input id="f-name" value="my-agent">
        <label>Harness</label><select id="f-harness-type">${harnessOptions}</select>
        <label>Harness version</label><select id="f-harness-version"></select>
        <label>Provider</label>
        <select id="f-provider" ${configured.length ? "" : "disabled"}>${providerOptions}</select>
        <label>Model</label>
        <select id="f-model"></select>
        <input id="f-model-custom" placeholder="model id" style="display:none">
        <div class="grid2">
          <div><label>vCPU</label><input id="f-vcpu" type="number" value="2"></div>
          <div><label>Memory (MiB)</label><input id="f-mem" type="number" value="2048"></div>
          <div><label>Disk (MiB)</label><input id="f-disk" type="number" value="5120"></div>
          <div><label>GPU count</label><input id="f-gpu" type="number" value="0"></div>
          <div><label>VRAM (MiB)</label><input id="f-vram" type="number" value="0"></div>
          <div><label>Daemon pin</label><input id="f-pin" placeholder="optional"></div>
        </div>
        <label>Repos (one per line: url ref)</label><textarea id="f-repos" rows="2" placeholder="git@github.com:org/repo.git main"></textarea>
        <label>Extensions (one per line: url ref)</label><textarea id="f-ext" rows="2"></textarea>
        <label>Pi packages <span class="muted">(from the <a href="https://pi.dev/packages" target="_blank" rel="noopener">pi.dev catalog</a>)</span></label>
        <div id="pkg-chips"></div>
        <div class="btn-row" style="margin:6px 0">
          <button type="button" onclick="togglePkgPicker()">Browse pi.dev catalog</button>
        </div>
        <div id="pkg-picker" style="display:none">
          <div class="btn-row" style="margin-bottom:8px">
            <input id="pkg-q" placeholder="search packages…" style="flex:1">
            <select id="pkg-type">
              <option value="extension" selected>extensions</option>
              <option value="">all types</option>
              <option value="skill">skills</option>
              <option value="prompt">prompts</option>
              <option value="theme">themes</option>
            </select>
          </div>
          <div id="pkg-list" class="pkg-list"><div class="empty">loading…</div></div>
          <div class="btn-row" style="margin-top:8px;align-items:center">
            <button type="button" onclick="pkgPage(-1)">← prev</button>
            <span id="pkg-pageinfo" class="muted" style="font-size:12px"></span>
            <button type="button" onclick="pkgPage(1)">next →</button>
          </div>
        </div>
        <label>Append system prompt <span class="muted">(written to the agent's APPEND_SYSTEM.md)</span></label>
        <textarea id="f-append-system" rows="4" placeholder="Extra instructions appended to pi's system prompt on every run…"></textarea>
        <label>Require labels (k=v, comma-separated)</label><input id="f-require" placeholder="zone=office">
        <div style="margin-top:14px" class="btn-row">
          <button class="primary" onclick="submitCreate()">Create</button>
          <button onclick="syncFormToToml()">form → toml</button>
        </div>
        <div id="create-error"></div>
      </div>
      <div class="panel">
        <label>Manifest TOML (fully editable)</label>
        <textarea id="f-toml" rows="24">${esc(DEFAULT_MANIFEST)}</textarea>
      </div>
    </div>`;
  window.__catalog = catalog;
  window.__harnesses = harnesses;
  const providerSel = $("#f-provider");
  if (configured.includes("kimi-coding")) providerSel.value = "kimi-coding";
  ["f-name","f-vcpu","f-mem","f-disk","f-gpu","f-vram","f-pin","f-repos","f-ext","f-require","f-model-custom","f-append-system"].forEach(
    (id) => $("#" + id).addEventListener("change", syncFormToToml)
  );
  $("#pkg-q").addEventListener("input", debounce(() => { pkgState.q = $("#pkg-q").value.trim(); pkgState.page = 1; loadPkgs(); }, 300));
  $("#pkg-type").addEventListener("change", () => { pkgState.type = $("#pkg-type").value; pkgState.page = 1; loadPkgs(); });
  $("#f-harness-type").addEventListener("change", () => { populateHarnessVersions(); syncFormToToml(); });
  $("#f-harness-version").addEventListener("change", syncFormToToml);
  providerSel.addEventListener("change", () => { populateModels(); syncFormToToml(); });
  $("#f-model").addEventListener("change", syncFormToToml);
  populateHarnessVersions();
  populateModels();
  // Fresh form: clear any package selection left over from a previous visit.
  pkgState.sel.clear();
  pkgState.loaded = false;
  const picker = $("#pkg-picker");
  if (picker) picker.style.display = "none";
  renderPkgChips();
  syncFormToToml();
}

// Harness version dropdown follows the selected harness.
function populateHarnessVersions() {
  const h = (window.__harnesses || HARNESSES_FALLBACK)[$("#f-harness-type").value] || { versions: [] };
  $("#f-harness-version").innerHTML =
    h.versions.map((v) => `<option>${esc(v)}</option>`).join("");
}

// Model dropdown follows the selected provider. Providers missing from the
// catalog (custom ids) fall back to a free-form input.
function populateModels() {
  const provider = $("#f-provider").value;
  const models = (window.__catalog && window.__catalog.providers[provider]?.models) || [];
  const sel = $("#f-model"), custom = $("#f-model-custom");
  if (models.length) {
    sel.style.display = ""; custom.style.display = "none";
    sel.innerHTML = models
      .map((m) => `<option value="${esc(m.id)}">${esc(m.name)} (${esc(m.id)})</option>`)
      .join("");
    if (provider === "kimi-coding" && models.some((m) => m.id === "kimi-for-coding"))
      sel.value = "kimi-for-coding";
  } else {
    sel.style.display = "none"; custom.style.display = "";
    sel.innerHTML = "";
  }
}

const currentModel = () =>
  $("#f-model").style.display === "none" ? $("#f-model-custom").value.trim() : $("#f-model").value;

function linePairs(text) {
  return text.split("\n").map((l) => l.trim()).filter(Boolean).map((l) => {
    const [url, ...rest] = l.split(/\s+/);
    return { url, ref: rest.join(" ") || "main" };
  });
}

// Escape arbitrary text for a TOML multi-line basic string ("""…""").
const tomlMulti = (s) => s.replace(/\\/g, "\\\\").replace(/"""/g, '""\\"');

// ── pi.dev catalog picker ────────────────────────────────────────────────
// Selection persists across picker pages/renders; syncFormToToml turns it
// into [[extensions]] source = "…" entries.
const pkgState = { q: "", type: "extension", page: 1, pages: 1, sel: new Map(), loaded: false };

function debounce(fn, ms) {
  let t;
  return (...args) => { clearTimeout(t); t = setTimeout(() => fn(...args), ms); };
}

window.togglePkgPicker = () => {
  const el = $("#pkg-picker");
  el.style.display = el.style.display === "none" ? "" : "none";
  if (el.style.display !== "none" && !pkgState.loaded) { pkgState.loaded = true; loadPkgs(); }
};

window.pkgPage = (d) => {
  const next = pkgState.page + d;
  if (next < 1 || next > pkgState.pages) return;
  pkgState.page = next;
  loadPkgs();
};

async function loadPkgs() {
  const list = $("#pkg-list");
  if (!list) return;
  list.innerHTML = '<div class="empty">loading…</div>';
  try {
    const params = new URLSearchParams({ page: String(pkgState.page), per_page: "20" });
    if (pkgState.q) params.set("q", pkgState.q);
    if (pkgState.type) params.set("type", pkgState.type);
    const data = await api(`/api/v1/pi-packages?${params}`);
    pkgState.pages = data.pages;
    pkgState.page = data.page;
    $("#pkg-pageinfo").textContent =
      `page ${data.page}/${data.pages} · ${data.total} packages` +
      (data.cache_age_secs > 60 ? ` · catalog ${Math.round(data.cache_age_secs / 60)}m old` : "");
    if (!data.packages.length) {
      list.innerHTML = '<div class="empty">no packages match</div>';
      return;
    }
    list.innerHTML = data.packages.map((p) => {
      const selected = pkgState.sel.has(p.name);
      const installable = !!p.source;
      const badges = (p.types || []).map((t) => `<span class="chip">${esc(t)}</span>`).join("");
      return `<label class="pkg-row ${installable ? "" : "disabled"}">
        <input type="checkbox" ${selected ? "checked" : ""} ${installable ? "" : "disabled"}
          onchange="pkgToggle('${esc(p.name)}', '${esc(p.source || "")}')">
        <div class="pkg-body">
          <div><span class="mono">${esc(p.name)}</span> ${badges}
            <span class="muted" style="font-size:11px">${esc(p.author)} · ${esc(p.downloads)} · ${esc(p.updated)}</span></div>
          <div class="muted pkg-desc">${esc(p.description)}${installable ? "" : " (no install source)"}</div>
        </div>
      </label>`;
    }).join("");
  } catch (e) {
    list.innerHTML = `<div class="empty">catalog unavailable: ${esc(e.message)}</div>`;
  }
}

window.pkgToggle = (name, source) => {
  if (pkgState.sel.has(name)) pkgState.sel.delete(name);
  else pkgState.sel.set(name, { name, source });
  renderPkgChips();
  syncFormToToml();
};

window.pkgRemove = (name) => {
  pkgState.sel.delete(name);
  renderPkgChips();
  // Uncheck if the row is currently visible.
  const cb = document.querySelector(`#pkg-list input[onchange^="pkgToggle('${name}'"]`);
  if (cb) cb.checked = false;
  syncFormToToml();
};

function renderPkgChips() {
  const el = $("#pkg-chips");
  if (!el) return;
  el.innerHTML = [...pkgState.sel.values()].map((p) =>
    `<span class="chip">${esc(p.name)} <a href="javascript:void(0)" onclick="pkgRemove('${esc(p.name)}')" title="remove">×</a></span>`
  ).join("") || '<span class="muted" style="font-size:12px">none selected</span>';
}

function syncFormToToml() {
  const v = (id) => $("#" + id).value.trim();
  const model = currentModel();
  const gpu = parseInt(v("f-gpu") || "0");
  const vram = parseInt(v("f-vram") || "0");
  const repos = linePairs(v("f-repos")).map((r) => `[[repos]]\nurl = "${r.url}"\nref = "${r.ref}"`).join("\n\n");
  const exts = [
    ...linePairs(v("f-ext")).map((r) => `[[extensions]]\nurl = "${r.url}"\nref = "${r.ref}"`),
    ...[...pkgState.sel.values()].map((p) => `[[extensions]]\nsource = "${p.source}"`),
  ].join("\n\n");
  const appendSys = $("#f-append-system").value.replace(/^\n+|\n+$/g, "");
  const requires = v("f-require").split(",").map((x) => x.trim()).filter(Boolean)
    .map((kv) => { const [k, val] = kv.split("=").map((x) => x.trim()); return `${k} = "${val}"`; }).join(", ");
  $("#f-toml").value =
`name = "${v("f-name")}"
harness = { type = "${v("f-harness-type")}", version = "${v("f-harness-version")}" }
model = { provider = "${v("f-provider")}", id = "${model}" }

[resources]
vcpu = ${parseInt(v("f-vcpu") || "2")}
memory_mib = ${parseInt(v("f-mem") || "2048")}
disk_mib = ${parseInt(v("f-disk") || "5120")}
${gpu > 0 ? `\n[resources.gpu]\ncount = ${gpu}${vram > 0 ? `\nvram_mib = ${vram}` : ""}` : ""}
${repos ? "\n" + repos + "\n" : ""}${exts ? "\n" + exts + "\n" : ""}
[secrets]
providers = ["${v("f-provider")}"]
${appendSys ? `\n[prompt]\nappend_system = """\n${tomlMulti(appendSys)}\n"""\n` : ""}
${v("f-pin") || requires ? `\n[schedule]\n${v("f-pin") ? `daemon = "${v("f-pin")}"\n` : ""}${requires ? `require = { ${requires} }` : ""}` : ""}`;
}
window.syncFormToToml = syncFormToToml;

window.submitCreate = async () => {
  const errEl = $("#create-error");
  const btn = document.querySelector('button[onclick="submitCreate()"]');
  errEl.innerHTML = "";
  if (btn) { btn.disabled = true; btn.textContent = "Creating…"; }
  try {
    const r = await post("/api/v1/agents", { manifest_toml: $("#f-toml").value });
    toast(`created ${r.name} — provisioning…`, "ok");
    location.hash = "#/agents";
  } catch (e) {
    if (btn) { btn.disabled = false; btn.textContent = "Create"; }
    errEl.innerHTML = `<div class="panel" style="border-color:var(--err);color:var(--err);white-space:pre-wrap">${esc(e.message)}</div>`;
  }
};

// ── M3: agent session ────────────────────────────────────────────────────
let es = null;

async function viewSession(name) {
  if (es) { es.close(); es = null; }
  const [agent, st] = await Promise.all([
    api(`/api/v1/agents/${name}`),
    api(`/api/v1/agents/${name}/session_state`).catch(() => ({})),
  ]);
  const active = agent.state === "active";
  main.innerHTML = `
    <h1>${esc(name)} ${stateBadge(agent.state)} <span class="muted" style="font-weight:400;font-size:13px">· ${esc(agent.manifest.model.provider)}/${esc(agent.manifest.model.id)}</span>
    <button style="float:right" onclick="location.hash='#/agents/${name}'">details</button></h1>
    ${active ? "" : `<div class="panel" style="border-color:var(--warn)">Agent is ${esc(agent.state)} — chat is read-only; start the agent to send messages.</div>`}
    <div class="statusline"><span id="turn-status">${st.streaming ? '<span class="streaming">streaming…</span>' : "idle"}</span></div>
    <div class="chat" id="chat"></div>
    <div class="composer">
      <textarea id="prompt" rows="4" placeholder="Message ${esc(name)}… (Enter to send, Shift+Enter for newline)" ${active ? "" : "disabled"}></textarea>
      <div class="composer-btns">
        <button class="primary" id="send-btn" onclick="sendPrompt('${name}')" ${active ? "" : "disabled"}>Send</button>
        <button class="danger" id="abort-btn" onclick="abortRun('${name}')" ${active ? "" : "disabled"}>Abort</button>
      </div>
    </div>`;
  $("#prompt").addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); sendPrompt(name); }
  });
  if (active) $("#prompt").focus();

  es = new EventSource(`/api/v1/agents/${name}/session`);
  // History is replayed in full on every (re)connect — reset the chat first
  // so a reconnect doesn't duplicate every line.
  let needReset = true;
  es.addEventListener("history", (e) => {
    if (needReset) { const c = chat(); if (c) c.innerHTML = ""; needReset = false; }
    appendTranscriptItem(JSON.parse(e.data));
  });
  es.addEventListener("history_end", () => { scrollChat(); });
  es.addEventListener("event", (e) => handleLiveEvent(name, JSON.parse(e.data)));
  es.addEventListener("error", () => { needReset = true; setStatus("disconnected (retrying…)"); });
}

const chat = () => $("#chat");
function scrollChat() { const c = chat(); if (c) c.scrollIntoView(false); window.scrollTo(0, document.body.scrollHeight); }
function setStatus(t) { const el = $("#turn-status"); if (el) el.innerHTML = t; }

// System line (crash notices, send failures) rendered between messages.
function sysLine(text, kind = "") {
  if (!chat()) return;
  chat().insertAdjacentHTML("beforeend", `<div class="sysline ${kind}">${esc(text)}</div>`);
  scrollChat();
}

function partHtml(p) {
  switch (p.type) {
    case "text": return `<div class="md">${md(p.text)}</div>`;
    case "thinking": return `<details class="think"><summary>thinking</summary><span class="think-body">${esc(p.text)}</span></details>`;
    case "tool_call": return `<details class="tool" id="tool-${esc(p.id)}"><summary><span class="tname">${esc(p.name)}</span><span class="tstatus">called</span></summary><pre>${esc(fmtArgs(p.arguments))}</pre></details>`;
    case "tool_result": return "";
    default: return "";
  }
}

function fmtArgs(args) {
  if (typeof args === "string") return args.slice(0, 500);
  const s = JSON.stringify(args, null, 1);
  return s.length > 500 ? s.slice(0, 500) + "…" : s;
}

function appendTranscriptItem(item) {
  if (!chat()) return;
  if (item.role === "system") {
    item.parts.forEach((p) => sysLine(p.text || "", "err"));
    return;
  }
  if (item.role === "user" || item.role === "assistant") {
    const textParts = item.parts.filter((p) => p.type === "text");
    // Errored turns produce assistant messages with no text — render the
    // error, not an empty bubble.
    item.parts.filter((p) => p.type === "error").forEach((p) => sysLine(p.text, "err"));
    // Render in reasoning order: thinking → text → tool calls.
    item.parts.filter((p) => p.type === "thinking").forEach((p) => {
      const d = document.createElement("div");
      d.innerHTML = partHtml(p);
      chat().appendChild(d.firstChild || d);
    });
    if (textParts.length) {
      const div = document.createElement("div");
      div.className = `msg ${item.role}`;
      div.innerHTML = `<div class="role">${item.role}</div>` + textParts.map(partHtml).join("");
      chat().appendChild(div);
    }
    item.parts.filter((p) => p.type === "tool_call").forEach((p) => {
      const html = partHtml(p);
      // Replace a live tool block (or a previous render) with the final one.
      const existing = $(`#tool-live-${CSS.escape(p.id)}`) || $(`#tool-${CSS.escape(p.id)}`);
      if (existing) existing.outerHTML = html;
      else {
        const d = document.createElement("div");
        d.innerHTML = html;
        chat().appendChild(d.firstChild || d);
      }
    });
  } else if (item.role === "toolResult") {
    item.parts.forEach((p) => {
      const host = $(`#tool-${CSS.escape(p.tool_call_id)}`) || $(`#tool-live-${CSS.escape(p.tool_call_id)}`);
      const html = `<details class="tool ${p.is_error ? "result-err" : ""}"><summary><span class="tname">${esc(p.name)}</span><span class="tstatus">${p.is_error ? "error" : "done"}</span></summary><pre>${esc((p.text || "").slice(0, 2000))}</pre></details>`;
      if (host) host.outerHTML = html;
      else chat().insertAdjacentHTML("beforeend", html);
    });
  }
}

// live event handling: text deltas, tool lifecycle, turns
let curAssistant = null, curThinking = null, lastEcho = null;
// Elements created from streaming deltas during the current assistant
// message. Replaced by the authoritative final message at message_end.
let liveEls = [];

function handleLiveEvent(name, ev) {
  const t = ev.type;
  if (t === "turn_start") { setStatus('<span class="streaming">streaming…</span>'); }
  if (t === "message_start" && ev.message && ev.message.role === "user") {
    const m = ev.message;
    const text = typeof m.content === "string" ? m.content : (m.content || []).filter((c) => c.type === "text").map((c) => c.text).join("\n");
    // Skip the relayed copy of a message we already echoed optimistically.
    const isEcho = lastEcho && text.trim() === lastEcho.text && Date.now() - lastEcho.at < 30000;
    if (text.trim() && !isEcho) appendTranscriptItem({ role: "user", parts: [{ type: "text", text }] });
    if (isEcho) lastEcho = null;
  }
    // Daemon-side attach notices (prompt rejections, attach errors).
  if (t === "notice") sysLine(`daemon: ${ev.message || ""}`, "err");
  // Crash/VM-level notices, otherwise invisible in the chat.
  if (t === "pi_stderr") sysLine(`pi: ${ev.line || ""}`, "err");
  if (t === "pi_exit") { sysLine(`pi exited (code ${ev.code ?? "?"}) — the agent cannot process messages; check its provider/model config or restart it`, "err"); setStatus("pi exited"); }
  if (t === "driver_died") { sysLine("agent VM driver died — the agent is unavailable", "err"); setStatus("vm gone"); }
  if (t === "message_update") {
    const ame = ev.assistantMessageEvent || {};
    if (ame.type === "text_delta" || ame.type === "thinking_delta") {
      const isThink = ame.type === "thinking_delta";
      if (isThink) {
        if (!curThinking) {
          curThinking = document.createElement("details");
          curThinking.className = "think";
          curThinking.open = true;
          curThinking.innerHTML = "<summary>thinking</summary><span class=\"think-body\"></span>";
          chat().appendChild(curThinking);
          liveEls.push(curThinking);
        }
        curThinking.querySelector(".think-body").textContent += ame.delta || "";
      } else {
        if (!curAssistant) {
          curAssistant = document.createElement("div");
          curAssistant.className = "msg assistant";
          curAssistant.innerHTML = '<div class="role">assistant</div><span class="md"></span>';
          curAssistant.dataset.raw = "";
          chat().appendChild(curAssistant);
          liveEls.push(curAssistant);
        }
        // Re-render markdown as the stream grows.
        curAssistant.dataset.raw += ame.delta || "";
        curAssistant.querySelector("span").innerHTML = md(curAssistant.dataset.raw);
      }
      scrollChat();
    }
  }
  if (t === "message_end") {
    // Replace the streamed partials with the authoritative final message.
    liveEls.forEach((el) => el.remove());
    liveEls = [];
    curAssistant = null; curThinking = null;
    const m = ev.message;
    if (m && (m.role === "assistant" || m.role === "toolResult")) appendTranscriptItem(transcriptFromMessage(m));
  }
  if (t === "tool_execution_start") {
    const id = ev.toolCallId || ev.toolName;
    const host = $(`#tool-${CSS.escape(id)}`) || $(`#tool-live-${CSS.escape(id)}`);
    if (host) host.querySelector(".tstatus").textContent = "running…";
    else chat().insertAdjacentHTML("beforeend", `<details class="tool" id="tool-live-${esc(id)}"><summary><span class="tname">${esc(ev.toolName)}</span><span class="tstatus">running…</span></summary></details>`);
  }
  if (t === "tool_execution_end") {
    const id = ev.toolCallId || ev.toolName;
    const host = $(`#tool-${CSS.escape(id)}`) || $(`#tool-live-${CSS.escape(id)}`);
    if (host) host.querySelector(".tstatus").textContent = ev.result && ev.result.isError ? "error" : "done";
  }
  if (t === "turn_end") {
    chat().insertAdjacentHTML("beforeend", '<div class="turn-sep">turn complete</div>');
    setStatus("idle");
  }
  if (t === "agent_end" || t === "agent_settled") setStatus("idle");
  scrollChat();
}
window.handleLiveEvent = handleLiveEvent; // exposed for tests/debugging

function transcriptFromMessage(m) {
  const parts = [];
  (m.content || []).forEach((c) => {
    if (c.type === "text") parts.push({ type: "text", text: c.text });
    if (c.type === "thinking") parts.push({ type: "thinking", text: c.thinking });
    if (c.type === "toolCall") parts.push({ type: "tool_call", id: c.id, name: c.name, arguments: c.arguments });
  });
  if (m.role === "assistant" && (m.stopReason === "error" || m.stopReason === "aborted")) {
    parts.push({ type: "error", text: m.errorMessage ? `LLM request failed: ${m.errorMessage}` : `turn ended: ${m.stopReason}` });
  }
  if (m.role === "toolResult") {
    return { role: "toolResult", parts: [{ type: "tool_result", tool_call_id: m.toolCallId, name: m.toolName, text: (m.content || []).filter((c) => c.type === "text").map((c) => c.text).join("\n"), is_error: m.isError }] };
  }
  return { role: m.role, parts };
}

window.sendPrompt = async (name) => {
  const ta = $("#prompt");
  const message = ta.value.trim();
  if (!message) return;
  ta.value = "";
  // Echo immediately; the relayed message_start copy is deduped via lastEcho.
  appendTranscriptItem({ role: "user", parts: [{ type: "text", text: message }] });
  lastEcho = { text: message, at: Date.now() };
  scrollChat();
  try {
    await post(`/api/v1/agents/${name}/prompt`, { message, mode: "prompt" });
  } catch (e) {
    lastEcho = null;
    sysLine(`send failed: ${e.message}`, "err");
  }
};

window.abortRun = async (name) => {
  try {
    await post(`/api/v1/agents/${name}/prompt`, { mode: "abort", message: "abort" });
    setStatus("idle");
  } catch (e) { toast(`abort failed: ${e.message}`, "err"); }
};

route();
topbar();
