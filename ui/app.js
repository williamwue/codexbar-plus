// Popover UI. Talks to the Rust core through Tauri commands; all provider logic,
// including percent semantics and reset math, lives in codexbar-core.

const invoke = window.__TAURI__.core.invoke;
const listen = window.__TAURI__.event.listen;
const { apply: applyI18n, onChange: onLocaleChange, t } = window.CodexBarI18n;

const els = {
  tabs: document.getElementById("tabs"),
  cards: document.getElementById("cards"),
  status: document.getElementById("status"),
};

let providers = [];
let selected = null;

/** Compact countdown, mirroring core's `format::duration_short`. */
function durationShort(msUntil) {
  if (msUntil <= 0) return t("now");
  const minutes = Math.floor(msUntil / 60000);
  const days = Math.floor(minutes / 1440);
  const hours = Math.floor((minutes % 1440) / 60);
  const mins = minutes % 60;
  if (days > 0) return hours > 0 ? `${days}d ${hours}h` : `${days}d`;
  if (hours > 0) return mins > 0 ? `${hours}h ${mins}m` : `${hours}h`;
  if (mins > 0) return `${mins}m`;
  return "<1m";
}

function remaining(window) {
  return Math.max(0, Math.min(100, 100 - window.used_percent));
}

function severity(window) {
  const left = remaining(window);
  if (left <= 10) return "crit";
  if (left <= 30) return "warn";
  return "";
}

function laneHtml(title, window) {
  const left = remaining(window);
  const reset = window.resets_at
    ? t("resetsIn", { value: durationShort(new Date(window.resets_at).getTime() - Date.now()) })
    : window.reset_description || t("resetUnknown");
  return `
    <div class="lane">
      <div class="lane-head">
        <span class="lane-title">${escapeHtml(title)}</span>
        <span class="lane-value">${t("left", { value: left.toFixed(0) })}</span>
      </div>
      <div class="bar ${severity(window)}"><span style="width:${left}%"></span></div>
      <div class="reset">${escapeHtml(reset)}</div>
    </div>`;
}

function rowHtml(label, value, hint) {
  const suffix = hint ? ` <em>(${escapeHtml(hint)})</em>` : "";
  return `<div class="row"><span>${escapeHtml(label)}</span><span>${escapeHtml(value)}${suffix}</span></div>`;
}

function escapeHtml(value) {
  return String(value ?? "").replace(/[&<>"']/g, (c) => ({
    "&": "&amp;",
    "<": "&lt;",
    ">": "&gt;",
    '"': "&quot;",
    "'": "&#39;",
  })[c]);
}

/** Incident banner. Operational providers show nothing — silence is the good state. */
function statusHtml(status) {
  if (!status || status.indicator === "none") return "";
  const severity = status.indicator === "minor" || status.indicator === "maintenance" ? "warn" : "crit";
  const issues = (status.components || []).filter((c) => c.indicator !== "none");
  const detail = issues
    .slice(0, 4)
    .map((c) => `<div class="incident-row">${escapeHtml(c.name)} <span>${escapeHtml(c.indicator)}</span></div>`)
    .join("");
  const summary = status.description || status.indicator;
  return `<div class="incident ${severity}">
      <strong>${escapeHtml(summary)}</strong>
      ${detail}
    </div>`;
}

function cardHtml(provider) {
  if (provider.error) {
    return `
      <section class="card">
        <h2>${escapeHtml(provider.display_name)}</h2>
        <div class="sub">${t("notConnected")}</div>
        <div class="error">${escapeHtml(provider.error)}</div>
        <div class="hint">${t("signInHint")}</div>
      </section>`;
  }

  const snapshot = provider.snapshot;
  if (!snapshot) {
    return `<section class="card"><h2>${escapeHtml(provider.display_name)}</h2>
      <div class="sub">${t("loading")}</div></section>`;
  }

  const identity = snapshot.identity || {};
  const subtitleParts = [identity.account, identity.plan, identity.organization].filter(Boolean);
  const lanes = [];
  if (snapshot.primary) lanes.push(laneHtml(t("session"), snapshot.primary));
  if (snapshot.secondary) lanes.push(laneHtml(t("weekly"), snapshot.secondary));
  if (snapshot.tertiary) lanes.push(laneHtml(t("model"), snapshot.tertiary));
  for (const extra of snapshot.extra_rate_windows || []) {
    lanes.push(laneHtml(extra.title, extra));
  }

  const money = (value, currency) => {
    const code = currency || "USD";
    return code === "USD" ? `$${value.toFixed(2)}` : `${value.toFixed(2)} ${code}`;
  };

  const rows = [];
  if (typeof provider.cost_today === "number") {
    rows.push(rowHtml(t("today"), money(provider.cost_today, "USD"), t("localScan")));
  }
  if (typeof provider.cost_month === "number") {
    rows.push(rowHtml(t("last30d"), money(provider.cost_month, "USD"), t("localScan")));
  }
  if (snapshot.credits) {
    rows.push(rowHtml(t("credits"), money(snapshot.credits.remaining, snapshot.credits.currency)));
  }
  if (snapshot.cost) {
    const cost = snapshot.cost;
    const value = cost.limit
      ? `${money(cost.used, cost.currency)} / ${money(cost.limit, cost.currency)}`
      : money(cost.used, cost.currency);
    rows.push(rowHtml(cost.period || t("cost"), value));
  }
  if (snapshot.cost_usage) {
    const history = snapshot.cost_usage;
    const total = (history.entries || []).reduce((sum, entry) => sum + entry.cost, 0);
    rows.push(rowHtml(`Last ${history.history_days}d`, money(total, history.currency)));
  }
  if (provider.source) {
    const when = provider.fetched_at ? durationShort(Date.now() - new Date(provider.fetched_at).getTime()) : "";
    rows.push(rowHtml(t("source"), when ? `${provider.source} · ${t("ago", { value: when })}` : provider.source));
  }

  // Plugin providers group their rows into titled sections; native ones use one section.
  const sections = (snapshot.details || [])
    .map((section) => {
      const sectionRows = (section.rows || []).map((row) => rowHtml(row.label, row.value, row.hint));
      if (!sectionRows.length) return "";
      const title = section.title ? `<div class="section-title">${escapeHtml(section.title)}</div>` : "";
      return `<div class="rows">${title}${sectionRows.join("")}</div>`;
    })
    .join("");

  return `
    <section class="card">
      <h2>${escapeHtml(provider.display_name)}</h2>
      <div class="sub">${escapeHtml(subtitleParts.join(" · ") || t("connected"))}</div>
      ${statusHtml(provider.status)}
      ${lanes.join("")}
      ${sections}
      ${rows.length ? `<div class="rows">${rows.join("")}</div>` : ""}
    </section>`;
}

function tabHtml(provider) {
  const isSelected = provider.id === selected;
  const color = provider.error ? "var(--crit)" : provider.accent;
  return `<button class="tab" role="tab" aria-selected="${isSelected}" data-id="${escapeHtml(provider.id)}">
      <span class="dot" style="background:${escapeHtml(color)}"></span>${escapeHtml(provider.display_name)}
    </button>`;
}

function render() {
  if (!providers.length) {
    els.cards.innerHTML = `<section class="card"><div class="sub">${t("noProviders")}</div></section>`;
    return;
  }
  if (!providers.some((p) => p.id === selected)) {
    selected = providers[0].id;
  }

  els.tabs.innerHTML = providers.map(tabHtml).join("");
  const current = providers.find((p) => p.id === selected);
  els.cards.innerHTML = cardHtml(current);

  const connected = providers.filter((p) => p.snapshot).length;
  els.status.textContent = t("connectedCount", { ready: connected, total: providers.length });
}

async function load() {
  providers = await invoke("snapshots");
  const current = await invoke("selected_provider");
  if (current && !selected) selected = current.id;
  render();
}

els.tabs.addEventListener("click", async (event) => {
  const tab = event.target.closest(".tab");
  if (!tab) return;
  selected = tab.dataset.id;
  render();
  await invoke("select_provider", { id: selected });
});

document.getElementById("refresh").addEventListener("click", async () => {
  els.status.textContent = t("refreshing");
  await invoke("refresh");
});

document
  .getElementById("settings")
  .addEventListener("click", () => invoke("open_settings", { provider: selected }));
document.getElementById("close").addEventListener("click", () => invoke("hide_popover"));
document.getElementById("quit").addEventListener("click", () => invoke("quit"));

document.addEventListener("keydown", (event) => {
  if (event.key === "Escape") invoke("hide_popover");
});

listen("providers-updated", (event) => {
  providers = event.payload;
  render();
});

// The backend's first fetch can finish before this webview attaches its listener, so pull
// state as well as pushing it. Polling is a cheap in-process command, not a network call.
// Answer liveness pings so the app can tell a live webview from a dead one.
listen("ping", () => invoke("ui_ready", { label: "main" }));

applyI18n();
onLocaleChange(() => {
  applyI18n();
  render();
});

load();
setInterval(load, 5000);
document.addEventListener("visibilitychange", () => {
  if (!document.hidden) load();
});
