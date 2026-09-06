// Settings window: provider toggles, plugin settings and API keys.
// Secrets are write-only from the UI's point of view — the backend stores them through
// DPAPI and only ever reports whether one exists.

const invoke = window.__TAURI__.core.invoke;
const listen = window.__TAURI__.event.listen;
const {
  apply: applyI18n,
  getLocale,
  onChange: onLocaleChange,
  setLocale,
  t,
} = window.CodexBarI18n;

const els = {
  list: document.getElementById("list"),
  detail: document.getElementById("detail"),
  filter: document.getElementById("filter"),
  summary: document.getElementById("summary"),
  paths: document.getElementById("paths"),
  autostart: document.getElementById("autostart"),
  language: document.getElementById("language"),
};

let providers = [];
let selected = null;

function escapeHtml(value) {
  return String(value ?? "").replace(/[&<>"']/g, (c) => ({
    "&": "&amp;",
    "<": "&lt;",
    ">": "&gt;",
    '"': "&quot;",
    "'": "&#39;",
  })[c]);
}

function stateLabel(provider) {
  if (!provider.enabled) return t("off");
  if (provider.ready) return t("ready");
  if (provider.requires_cookies) return t("cookies");
  return t("setup");
}

function renderList() {
  const needle = els.filter.value.trim().toLowerCase();
  const visible = providers.filter(
    (p) => !needle || p.id.includes(needle) || p.display_name.toLowerCase().includes(needle),
  );
  els.list.innerHTML = visible
    .map(
      (p) => `<li><button type="button" data-id="${escapeHtml(p.id)}" aria-current="${p.id === selected}">
        <span class="dot" style="background:${escapeHtml(p.accent)}"></span>
        <span>${escapeHtml(p.display_name)}</span>
        <span class="state">${stateLabel(p)}</span>
      </button></li>`,
    )
    .join("");

  const ready = providers.filter((p) => p.ready).length;
  els.summary.textContent = t("readyCount", { ready, total: providers.length });
}

function fieldHtml(provider, setting) {
  const id = `f-${provider.id}-${setting.key}`;
  if (setting.secure) {
    const badge = setting.configured
      ? `<span class="badge ok">${t("stored")}</span>`
      : `<span class="badge warn">${t("missing")}</span>`;
    return `
      <div class="field">
        <label for="${id}">${escapeHtml(setting.title)} ${badge}</label>
        <div class="field-row">
          <input id="${id}" type="password" placeholder="${setting.configured ? "••••••" : t("pasteKey")}"
                 autocomplete="off" data-key="${escapeHtml(setting.key)}" data-secure="1" />
          <button type="button" data-save="${escapeHtml(setting.key)}">${t("save")}</button>
          ${setting.configured ? `<button type="button" data-clear="${escapeHtml(setting.key)}">${t("clear")}</button>` : ""}
        </div>
        <div class="hint">${escapeHtml(setting.key)} · encrypted with Windows DPAPI, never written to config.json</div>
      </div>`;
  }
  return `
    <div class="field">
      <label for="${id}">${escapeHtml(setting.title)}</label>
      <div class="field-row">
        <input id="${id}" type="text" value="${escapeHtml(setting.value || "")}"
               autocomplete="off" data-key="${escapeHtml(setting.key)}" />
        <button type="button" data-save="${escapeHtml(setting.key)}">${t("save")}</button>
      </div>
      <div class="hint">${escapeHtml(setting.key)}</div>
    </div>`;
}

/** Cookie controls, shown only for providers that declare cookie domains. */
function cookieHtml(provider) {
  if (!provider.cookie_domains || !provider.cookie_domains.length) return "";
  const badge = provider.has_cookie_header
    ? `<span class="badge ok">${t("stored")}</span>`
    : `<span class="badge warn">${t("missing")}</span>`;
  const options = ["auto", "manual", "off"]
    .map(
      (value) =>
        `<option value="${value}" ${provider.cookie_source === value ? "selected" : ""}>${value}</option>`,
    )
    .join("");
  return `
    <div class="field">
      <label for="cookie-source">${t("cookieSource")}</label>
      <div class="field-row">
        <select id="cookie-source">${options}</select>
      </div>
      <div class="hint">auto = pasted header first, then a browser import · manual = pasted only · off = never</div>
    </div>
    <div class="field">
      <label for="cookie-header">${t("cookieHeader")} ${badge}</label>
      <div class="field-row">
        <input id="cookie-header" type="password" autocomplete="off"
               placeholder="${provider.has_cookie_header ? "••••••" : "name=value; name2=value2"}" />
        <button type="button" id="cookie-save">${t("save")}</button>
        ${provider.has_cookie_header ? `<button type="button" id="cookie-clear">${t("clear")}</button>` : ""}
      </div>
      <div class="hint">
        ${t("domains")}: ${escapeHtml(provider.cookie_domains.join(", "))} · encrypted with DPAPI.
        Chrome/Edge cookies newer than Chrome 127 use app-bound encryption and cannot be imported;
        copy the request header from DevTools instead.
      </div>
    </div>`;
}

function renderDetail() {
  const provider = providers.find((p) => p.id === selected);
  if (!provider) {
    els.detail.innerHTML = `<p class="muted">${t("selectProvider")}</p>`;
    return;
  }

  const kind = provider.plugin ? t("jsPlugin") : t("nativeProvider");
  const notices = [];
  if (provider.cookie_domains && provider.cookie_domains.length && !provider.has_cookie_header) {
    notices.push(
      "This provider authenticates with browser cookies. Firefox and pre-Chrome-127 profiles import automatically; " +
        "newer Chrome/Edge profiles are app-bound encrypted, so paste the header above.",
    );
  }
  if (!provider.plugin) {
    notices.push(
      "Credentials come from the provider's own CLI (for example ~/.codex/auth.json), so there is nothing to enter here.",
    );
  }

  els.detail.innerHTML = `
    <h2>${escapeHtml(provider.display_name)}</h2>
    <p class="muted">${escapeHtml(provider.id)} · ${kind} · ${escapeHtml(provider.strategies.join(", "))}</p>

    <div class="field">
      <label class="toggle">
        <input id="enabled" type="checkbox" ${provider.enabled ? "checked" : ""} />
        ${t("enabled")}
      </label>
    </div>

    ${provider.settings.map((setting) => fieldHtml(provider, setting)).join("")}
    ${cookieHtml(provider)}
    ${notices.map((n) => `<div class="notice">${escapeHtml(n)}</div>`).join("")}
    <div id="status-line"></div>`;

  const enabled = document.getElementById("enabled");
  enabled.addEventListener("change", async () => {
    await invoke("set_provider_enabled", { id: provider.id, enabled: enabled.checked });
    await load(`${provider.display_name} ${enabled.checked ? "enabled" : "disabled"}`);
  });

  els.detail.querySelectorAll("[data-save]").forEach((button) => {
    button.addEventListener("click", async () => {
      const key = button.dataset.save;
      const input = els.detail.querySelector(`input[data-key="${key}"]`);
      const secure = input.dataset.secure === "1";
      const value = input.value;
      if (secure && !value.trim()) {
        status(t("enterValue"));
        return;
      }
      if (secure) {
        await invoke("set_provider_secret", { id: provider.id, key, value });
        input.value = "";
      } else {
        await invoke("set_provider_setting", { id: provider.id, key, value });
      }
      await load(`Saved ${key}`);
    });
  });

  const cookieSource = document.getElementById("cookie-source");
  if (cookieSource) {
    cookieSource.addEventListener("change", async () => {
      await invoke("set_provider_cookie_source", { id: provider.id, source: cookieSource.value });
      await load(`Cookie source set to ${cookieSource.value}`);
    });
  }
  const cookieSave = document.getElementById("cookie-save");
  if (cookieSave) {
    cookieSave.addEventListener("click", async () => {
      const input = document.getElementById("cookie-header");
      if (!input.value.trim()) {
        status(t("pasteCookie"));
        return;
      }
      await invoke("set_provider_cookie", { id: provider.id, header: input.value });
      input.value = "";
      await load("Saved cookie header");
    });
  }
  const cookieClear = document.getElementById("cookie-clear");
  if (cookieClear) {
    cookieClear.addEventListener("click", async () => {
      await invoke("set_provider_cookie", { id: provider.id, header: "" });
      await load("Cleared cookie header");
    });
  }

  els.detail.querySelectorAll("[data-clear]").forEach((button) => {
    button.addEventListener("click", async () => {
      const key = button.dataset.clear;
      await invoke("set_provider_secret", { id: provider.id, key, value: "" });
      await load(`Cleared ${key}`);
    });
  });
}

function status(message) {
  const line = document.getElementById("status-line");
  if (line) line.textContent = message;
}

/** Provider id from the URL fragment, so the app can deep-link into a provider. */
function requestedProvider() {
  const hash = decodeURIComponent((location.hash || "").replace(/^#/, "")).trim();
  return hash || null;
}

async function load(message) {
  const [payload, autostart] = await Promise.all([
    invoke("provider_settings"),
    invoke("autostart_enabled"),
  ]);
  providers = payload.providers;
  els.autostart.checked = autostart;
  els.language.value = getLocale();
  const requested = requestedProvider();
  if (requested && providers.some((p) => p.id === requested)) {
    selected = requested;
  }
  els.paths.textContent = `config: ${payload.config_path || "unresolved"} · secrets: ${payload.secrets_path || "unresolved"}`;
  if (!providers.some((p) => p.id === selected)) {
    selected = providers[0]?.id ?? null;
  }
  renderList();
  renderDetail();
  if (message) status(message);
}

els.list.addEventListener("click", (event) => {
  const button = event.target.closest("button[data-id]");
  if (!button) return;
  selected = button.dataset.id;
  renderList();
  renderDetail();
});

els.filter.addEventListener("input", renderList);

els.autostart.addEventListener("change", async () => {
  const enabled = els.autostart.checked;
  els.autostart.disabled = true;
  try {
    await invoke("set_autostart_enabled", { enabled });
    status(enabled ? t("autostartOn") : t("autostartOff"));
  } catch (error) {
    els.autostart.checked = !enabled;
    status(`${t("autostartError")}: ${error}`);
  } finally {
    els.autostart.disabled = false;
  }
});

document.getElementById("check-update").addEventListener("click", async (event) => {
  const button = event.currentTarget;
  button.disabled = true;
  status(t("checkingUpdates"));
  try {
    const update = await invoke("check_for_updates");
    if (update.state === "notConfigured") {
      status(t("updateNotConfigured", { version: update.currentVersion }));
    } else if (update.state === "emptyFeed" || update.state === "upToDate") {
      status(t("upToDate", { version: update.currentVersion }));
    } else {
      const rollback = update.state === "rollbackAvailable";
      const prompt = rollback
        ? t("rollbackTo", { version: update.availableVersion })
        : t("updateTo", { version: update.availableVersion });
      if (window.confirm(prompt)) {
        status(rollback ? t("downloadingRollback") : t("downloadingUpdate"));
        const scheduled = await invoke("install_update");
        if (!scheduled) status(t("feedChanged"));
      } else {
        status(rollback ? t("rollbackCancelled") : t("updateCancelled"));
      }
    }
  } catch (error) {
    status(`${t("updateError")}: ${error}`);
  } finally {
    button.disabled = false;
  }
});

els.language.addEventListener("change", () => setLocale(els.language.value));

document.getElementById("refresh").addEventListener("click", async () => {
  status(t("refreshing"));
  await invoke("refresh");
  await load(t("refreshRequested"));
});

document.getElementById("close").addEventListener("click", () => invoke("hide_settings"));
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape") invoke("hide_settings");
});

window.addEventListener("hashchange", () => {
  const requested = requestedProvider();
  if (requested && providers.some((p) => p.id === requested)) {
    selected = requested;
    els.filter.value = "";
    renderList();
    renderDetail();
  }
});

// Answer liveness pings so the app can tell a live webview from a dead one.
listen("ping", () => invoke("ui_ready", { label: "settings" }));
applyI18n();
els.language.value = getLocale();
onLocaleChange(() => {
  applyI18n();
  els.language.value = getLocale();
  renderList();
  renderDetail();
});


load();
