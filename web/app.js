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

async function runAction(name, action, confirmText) {
  if (confirmText && !confirmAction(confirmText)) return;
  try {
    await post(`/api/v1/agents/${name}/${action}`);
    toast(`${action} ${name}`, "ok");
    route();
  } catch (e) { toast(`${action} failed: ${e.message}`, "err"); }
}

function confirmAction(text) { return window.confirm(text); }

function actionButtons(a) {
  const btns = [];
  if (a.state === "suspended" || a.state === "failed") btns.push(`<button onclick="runAction('${a.name}','start')">Start</button>`);
  if (a.state === "active") {
    btns.push(`<button onclick="runAction('${a.name}','stop')">Stop</button>`);
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

const esc = (s) => String(s ?? "").replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
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
  castellans: viewCastellans,
  castellan: viewCastellan,
  agents: viewAgents,
  agent: viewAgent,
  create: viewCreate,
  session: viewSession,
  secrets: viewSecrets,
  activity: viewActivity,
};

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
  pollTimer = setTimeout(function loop() {
    render();
    pollTimer = setTimeout(loop, 5000);
  }, 5000);
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
    <h1>Castellans</h1>
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
        <td onclick="event.stopPropagation()">${
          a.state === "active"
            ? `<button onclick="runAction('${a.name}','stop')">Stop</button>`
            : a.state === "suspended" || a.state === "failed"
              ? `<button onclick="runAction('${a.name}','start')">Start</button>`
              : ""
        }</td></tr>`).join("")}
    </table></div>
    ${data.agents.length === 0 ? '<div class="panel empty">No agents yet — create one above.</div>' : ""}`;
}

async function viewAgent(name) {
  const a = await api(`/api/v1/agents/${name}`);
  const logs = await api(`/api/v1/agents/${name}/logs?tail=60`);
  const d = a.daemon || {};
  main.innerHTML = `
    <h1>${esc(a.name)} ${stateBadge(a.state)}</h1>
    <div class="panel">${actionButtons(a)}
      <button class="primary" onclick="location.hash='#/session/${a.name}'" style="margin-top:8px">Open session</button>
    </div>
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
  const s = await api("/api/v1/secrets");
  main.innerHTML = `
    <h1>Secrets</h1>
    ${!s.store_present ? '<div class="panel empty">No secrets store configured — create secrets.sops.yaml via sops (see README).</div>' : ""}
    <div class="panel"><table>
      <tr><th>kind</th><th>name</th><th>used by</th></tr>
      ${s.entries.map((e) => `<tr><td class="muted">${esc(e.kind)}</td><td>${esc(e.name)}</td><td class="muted">${e.used_by || "—"}</td></tr>`).join("")}
    </table></div>
    <p class="muted">Values are masked everywhere. Editing arrives in the next milestone.</p>`;
}

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

[secrets]
providers = ["kimi-coding"]

# [schedule]
# require = { zone = "office" }
`;

async function viewCreate() {
  const secrets = await api("/api/v1/secrets").catch(() => ({ entries: [] }));
  const providers = secrets.entries.filter((e) => e.kind === "provider").map((e) => e.name);
  main.innerHTML = `
    <h1>Create agent</h1>
    <div class="grid2">
      <div class="panel">
        <label>Name</label><input id="f-name" value="my-agent">
        <label>Provider</label>
        <select id="f-provider">${providers.map((p) => `<option>${esc(p)}</option>`).join("")}<option>kimi-coding</option></select>
        <label>Model</label><input id="f-model" value="kimi-for-coding">
        <label>Harness version</label><input id="f-harness" value="0.84.1">
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
  ["f-name","f-provider","f-model","f-harness","f-vcpu","f-mem","f-disk","f-gpu","f-vram","f-pin","f-repos","f-ext","f-require"].forEach(
    (id) => $("#" + id).addEventListener("change", syncFormToToml)
  );
}

function linePairs(text) {
  return text.split("\n").map((l) => l.trim()).filter(Boolean).map((l) => {
    const [url, ...rest] = l.split(/\s+/);
    return { url, ref: rest.join(" ") || "main" };
  });
}

function syncFormToToml() {
  const v = (id) => $("#" + id).value.trim();
  const gpu = parseInt(v("f-gpu") || "0");
  const vram = parseInt(v("f-vram") || "0");
  const repos = linePairs(v("f-repos")).map((r) => `[[repos]]\nurl = "${r.url}"\nref = "${r.ref}"`).join("\n\n");
  const exts = linePairs(v("f-ext")).map((r) => `[[extensions]]\nurl = "${r.url}"\nref = "${r.ref}"`).join("\n\n");
  const requires = v("f-require").split(",").map((x) => x.trim()).filter(Boolean)
    .map((kv) => { const [k, val] = kv.split("=").map((x) => x.trim()); return `${k} = "${val}"`; }).join(", ");
  $("#f-toml").value =
`name = "${v("f-name")}"
harness = { type = "pi", version = "${v("f-harness")}" }
model = { provider = "${v("f-provider")}", id = "${v("f-model")}" }

[resources]
vcpu = ${parseInt(v("f-vcpu") || "2")}
memory_mib = ${parseInt(v("f-mem") || "2048")}
disk_mib = ${parseInt(v("f-disk") || "5120")}
${gpu > 0 ? `\n[resources.gpu]\ncount = ${gpu}${vram > 0 ? `\nvram_mib = ${vram}` : ""}` : ""}
${repos ? "\n" + repos + "\n" : ""}${exts ? "\n" + exts + "\n" : ""}
[secrets]
providers = ["${v("f-provider")}"]
${v("f-pin") || requires ? `\n[schedule]\n${v("f-pin") ? `daemon = "${v("f-pin")}"\n` : ""}${requires ? `require = { ${requires} }` : ""}` : ""}`;
}
window.syncFormToToml = syncFormToToml;

window.submitCreate = async () => {
  const errEl = $("#create-error");
  errEl.innerHTML = "";
  try {
    const r = await post("/api/v1/agents", { manifest_toml: $("#f-toml").value });
    toast(`created ${r.name} — provisioning…`, "ok");
    location.hash = `#/agents/${r.name}`;
  } catch (e) {
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
  main.innerHTML = `
    <h1>${esc(name)} ${stateBadge(agent.state)} <span class="muted" style="font-weight:400;font-size:13px">· ${esc(agent.manifest.model.provider)}/${esc(agent.manifest.model.id)}</span>
    <button style="float:right" onclick="location.hash='#/agents/${name}'">details</button></h1>
    <div class="statusline"><span id="turn-status">${st.streaming ? '<span class="streaming">streaming…</span>' : "idle"}</span></div>
    <div class="chat" id="chat"></div>
    <div class="composer">
      <textarea id="prompt" placeholder="Message ${esc(name)}… (Enter to send, Shift+Enter for newline)"></textarea>
      <select id="mode"><option value="prompt">prompt</option><option value="steer">steer</option><option value="follow_up">follow-up</option></select>
      <button class="primary" id="send-btn" onclick="sendPrompt('${name}')">Send</button>
      <button class="danger" id="abort-btn" onclick="abortRun('${name}')">Abort</button>
    </div>`;
  $("#prompt").addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); sendPrompt(name); }
  });

  es = new EventSource(`/api/v1/agents/${name}/session`);
  es.addEventListener("history", (e) => appendTranscriptItem(JSON.parse(e.data)));
  es.addEventListener("history_end", () => { scrollChat(); });
  es.addEventListener("event", (e) => handleLiveEvent(name, JSON.parse(e.data)));
  es.addEventListener("error", () => setStatus("disconnected (retrying…)"));
}

const chat = () => $("#chat");
function scrollChat() { const c = chat(); if (c) c.scrollIntoView(false); window.scrollTo(0, document.body.scrollHeight); }
function setStatus(t) { const el = $("#turn-status"); if (el) el.innerHTML = t; }

function partHtml(p) {
  switch (p.type) {
    case "text": return `<div>${esc(p.text)}</div>`;
    case "thinking": return `<details class="think"><summary>thinking</summary>${esc(p.text)}</details>`;
    case "tool_call": return `<div class="tool" id="tool-${esc(p.id)}"><span class="tname">${esc(p.name)}</span><span class="tstatus">called</span><pre>${esc(fmtArgs(p.arguments))}</pre></div>`;
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
  if (item.role === "user" || item.role === "assistant") {
    const div = document.createElement("div");
    div.className = `msg ${item.role}`;
    div.innerHTML = `<div class="role">${item.role}</div>` + item.parts.filter((p) => p.type === "text").map(partHtml).join("");
    chat().appendChild(div);
    item.parts.filter((p) => p.type === "thinking").forEach((p) => {
      const d = document.createElement("div");
      d.innerHTML = partHtml(p);
      chat().appendChild(d.firstChild || d);
    });
    item.parts.filter((p) => p.type === "tool_call").forEach((p) => {
      const d = document.createElement("div");
      d.innerHTML = partHtml(p);
      chat().appendChild(d.firstChild || d);
    });
  } else if (item.role === "toolResult") {
    item.parts.forEach((p) => {
      const host = $(`#tool-${CSS.escape(p.tool_call_id)}`);
      const html = `<div class="tool ${p.is_error ? "result-err" : ""}"><span class="tname">${esc(p.name)}</span><span class="tstatus">${p.is_error ? "error" : "done"}</span><pre>${esc((p.text || "").slice(0, 500))}</pre></div>`;
      if (host) host.outerHTML = html;
      else chat().insertAdjacentHTML("beforeend", html);
    });
  }
}

// live event handling: text deltas, tool lifecycle, turns
let curAssistant = null, curThinking = null;

function handleLiveEvent(name, ev) {
  const t = ev.type;
  if (t === "turn_start") { setStatus('<span class="streaming">streaming…</span>'); }
  if (t === "message_start" && ev.message && ev.message.role === "user") {
    const m = ev.message;
    const text = typeof m.content === "string" ? m.content : (m.content || []).filter((c) => c.type === "text").map((c) => c.text).join("\n");
    if (text.trim()) appendTranscriptItem({ role: "user", parts: [{ type: "text", text }] });
  }
  if (t === "message_update") {
    const ame = ev.assistantMessageEvent || {};
    if (ame.type === "text_delta" || ame.type === "thinking_delta") {
      const isThink = ame.type === "thinking_delta";
      if (isThink) {
        if (!curThinking) {
          curThinking = document.createElement("details");
          curThinking.className = "think";
          curThinking.innerHTML = "<summary>thinking</summary><span></span>";
          chat().appendChild(curThinking);
        }
        curThinking.querySelector("span").textContent += ame.delta || "";
      } else {
        if (!curAssistant) {
          curAssistant = document.createElement("div");
          curAssistant.className = "msg assistant";
          curAssistant.innerHTML = '<div class="role">assistant</div><span></span>';
          chat().appendChild(curAssistant);
        }
        curAssistant.querySelector("span").textContent += ame.delta || "";
      }
      scrollChat();
    }
  }
  if (t === "message_end") {
    curAssistant = null; curThinking = null;
    const m = ev.message;
    if (m && (m.role === "assistant" || m.role === "toolResult")) appendTranscriptItem(transcriptFromMessage(m));
  }
  if (t === "tool_execution_start") {
    const html = `<div class="tool" id="tool-live-${esc(ev.toolCallId || ev.toolName)}"><span class="tname">${esc(ev.toolName)}</span><span class="tstatus">running…</span></div>`;
    chat().insertAdjacentHTML("beforeend", html);
  }
  if (t === "tool_execution_end") {
    const host = $(`#tool-live-${CSS.escape(ev.toolCallId || ev.toolName)}`);
    if (host) host.querySelector(".tstatus").textContent = ev.result && ev.result.isError ? "error" : "done";
  }
  if (t === "turn_end") {
    chat().insertAdjacentHTML("beforeend", '<div class="turn-sep">turn complete</div>');
    setStatus("idle");
  }
  if (t === "agent_end" || t === "agent_settled") setStatus("idle");
  scrollChat();
}

function transcriptFromMessage(m) {
  const parts = [];
  (m.content || []).forEach((c) => {
    if (c.type === "text") parts.push({ type: "text", text: c.text });
    if (c.type === "thinking") parts.push({ type: "thinking", text: c.thinking });
    if (c.type === "toolCall") parts.push({ type: "tool_call", id: c.id, name: c.name, arguments: c.arguments });
  });
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
  const mode = $("#mode").value;
  try {
    await post(`/api/v1/agents/${name}/prompt`, { message, mode });
  } catch (e) { toast(`send failed: ${e.message}`, "err"); }
};

window.abortRun = async (name) => {
  try {
    await post(`/api/v1/agents/${name}/prompt`, { mode: "abort", message: "abort" });
    setStatus("idle");
  } catch (e) { toast(`abort failed: ${e.message}`, "err"); }
};

route();
topbar();
