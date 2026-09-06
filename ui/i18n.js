(() => {
  const STORAGE_KEY = "codexbar.locale";
  const messages = {
    en: {
      settings: "Settings", refresh: "Refresh", refreshNow: "Refresh now", close: "Close", quit: "Quit",
      providers: "Providers", loading: "Loading…", filterProviders: "Filter providers", launchAtLogin: "Launch at login",
      shortcut: "Shortcut: Ctrl+Shift+Space", checkUpdates: "Check for updates", noProviders: "No providers enabled.",
      notConnected: "Not connected", signInHint: "Sign in with the provider's own CLI, then refresh.", connected: "Connected",
      session: "Session", weekly: "Weekly", model: "Model", today: "Today", last30d: "Last 30d", credits: "Credits",
      cost: "Cost", source: "Source", localScan: "local scan", resetUnknown: "Reset time unknown", now: "now",
      left: "{value}% left", resetsIn: "Resets in {value}", ago: "{value} ago", connectedCount: "{ready}/{total} connected",
      refreshing: "Refreshing…", readyCount: "{ready} of {total} providers ready", selectProvider: "Select a provider.",
      enabled: "Enabled", save: "Save", clear: "Clear", stored: "stored", missing: "missing", pasteKey: "Paste key",
      cookieSource: "Cookie source", cookieHeader: "Cookie header", domains: "Domains", nativeProvider: "native provider",
      jsPlugin: "JavaScript plugin", enterValue: "Enter a value first.", pasteCookie: "Paste a cookie header first.",
      language: "Language", english: "English", simplifiedChinese: "简体中文", traditionalChinese: "繁體中文",
      autostartOn: "Launch at login enabled", autostartOff: "Launch at login disabled", checkingUpdates: "Checking for updates…",
      updateNotConfigured: "Version {version} · this development build has no update feed", upToDate: "Version {version} is up to date",
      updateTo: "Update CodexBar to {version}?", rollbackTo: "Roll back CodexBar to {version}?", updateCancelled: "Update cancelled",
      rollbackCancelled: "Rollback cancelled", downloadingUpdate: "Update package is downloading…", downloadingRollback: "Rollback package is downloading…",
      feedChanged: "The release feed changed; no update is available", autostartError: "Could not update launch at login",
      updateError: "Update check failed", refreshRequested: "Refresh requested",
      off: "off", ready: "ready", cookies: "cookies", setup: "setup"
    },
    "zh-Hans": {
      settings: "设置", refresh: "刷新", refreshNow: "立即刷新", close: "关闭", quit: "退出",
      providers: "服务商", loading: "加载中…", filterProviders: "筛选服务商", launchAtLogin: "登录时启动",
      shortcut: "快捷键：Ctrl+Shift+Space", checkUpdates: "检查更新", noProviders: "未启用服务商。",
      notConnected: "未连接", signInHint: "请先使用服务商自己的 CLI 登录，然后刷新。", connected: "已连接",
      session: "会话", weekly: "每周", model: "模型", today: "今天", last30d: "最近 30 天", credits: "余额",
      cost: "费用", source: "来源", localScan: "本地扫描", resetUnknown: "重置时间未知", now: "现在",
      left: "剩余 {value}%", resetsIn: "{value} 后重置", ago: "{value}前", connectedCount: "已连接 {ready}/{total}",
      refreshing: "正在刷新…", readyCount: "{total} 个服务商中 {ready} 个就绪", selectProvider: "请选择服务商。",
      enabled: "启用", save: "保存", clear: "清除", stored: "已保存", missing: "缺失", pasteKey: "粘贴密钥",
      cookieSource: "Cookie 来源", cookieHeader: "Cookie 请求头", domains: "域名", nativeProvider: "原生服务商",
      jsPlugin: "JavaScript 插件", enterValue: "请先输入值。", pasteCookie: "请先粘贴 Cookie 请求头。",
      language: "语言", english: "English", simplifiedChinese: "简体中文", traditionalChinese: "繁體中文",
      autostartOn: "已启用登录时启动", autostartOff: "已关闭登录时启动", checkingUpdates: "正在检查更新…",
      updateNotConfigured: "版本 {version} · 此开发构建未配置更新源", upToDate: "版本 {version} 已是最新",
      updateTo: "将 CodexBar 更新到 {version}？", rollbackTo: "将 CodexBar 回滚到 {version}？", updateCancelled: "已取消更新",
      rollbackCancelled: "已取消回滚", downloadingUpdate: "正在下载更新包…", downloadingRollback: "正在下载回滚包…",
      feedChanged: "发布源已变化，当前没有可用更新", autostartError: "无法更新登录启动设置",
      updateError: "更新检查失败", refreshRequested: "已请求刷新",
      off: "关闭", ready: "就绪", cookies: "Cookie", setup: "待配置"
    },
    "zh-Hant": {
      settings: "設定", refresh: "重新整理", refreshNow: "立即重新整理", close: "關閉", quit: "結束",
      providers: "服務商", loading: "載入中…", filterProviders: "篩選服務商", launchAtLogin: "登入時啟動",
      shortcut: "快速鍵：Ctrl+Shift+Space", checkUpdates: "檢查更新", noProviders: "未啟用服務商。",
      notConnected: "未連線", signInHint: "請先使用服務商自己的 CLI 登入，然後重新整理。", connected: "已連線",
      session: "工作階段", weekly: "每週", model: "模型", today: "今天", last30d: "最近 30 天", credits: "餘額",
      cost: "費用", source: "來源", localScan: "本機掃描", resetUnknown: "重設時間未知", now: "現在",
      left: "剩餘 {value}%", resetsIn: "{value} 後重設", ago: "{value}前", connectedCount: "已連線 {ready}/{total}",
      refreshing: "正在重新整理…", readyCount: "{total} 個服務商中 {ready} 個就緒", selectProvider: "請選擇服務商。",
      enabled: "啟用", save: "儲存", clear: "清除", stored: "已儲存", missing: "缺少", pasteKey: "貼上金鑰",
      cookieSource: "Cookie 來源", cookieHeader: "Cookie 標頭", domains: "網域", nativeProvider: "原生服務商",
      jsPlugin: "JavaScript 外掛", enterValue: "請先輸入值。", pasteCookie: "請先貼上 Cookie 標頭。",
      language: "語言", english: "English", simplifiedChinese: "简体中文", traditionalChinese: "繁體中文",
      autostartOn: "已啟用登入時啟動", autostartOff: "已關閉登入時啟動", checkingUpdates: "正在檢查更新…",
      updateNotConfigured: "版本 {version} · 此開發組建未設定更新來源", upToDate: "版本 {version} 已是最新",
      updateTo: "將 CodexBar 更新到 {version}？", rollbackTo: "將 CodexBar 回復到 {version}？", updateCancelled: "已取消更新",
      rollbackCancelled: "已取消回復", downloadingUpdate: "正在下載更新套件…", downloadingRollback: "正在下載回復套件…",
      feedChanged: "發佈來源已變更，目前沒有可用更新", autostartError: "無法更新登入啟動設定",
      updateError: "更新檢查失敗", refreshRequested: "已要求重新整理",
      off: "關閉", ready: "就緒", cookies: "Cookie", setup: "待設定"
    }
  };

  function normalize(value) {
    const locale = String(value || "").toLowerCase();
    if (/^zh-(tw|hk|mo|hant)/.test(locale)) return "zh-Hant";
    if (locale.startsWith("zh")) return "zh-Hans";
    return "en";
  }

  let locale = normalize(localStorage.getItem(STORAGE_KEY) || navigator.languages?.[0] || navigator.language);

  function t(key, values = {}) {
    const template = messages[locale][key] ?? messages.en[key] ?? key;
    return template.replace(/\{(\w+)\}/g, (_, name) => String(values[name] ?? ""));
  }

  function apply(root = document) {
    document.documentElement.lang = locale;
    root.querySelectorAll("[data-i18n]").forEach((node) => { node.textContent = t(node.dataset.i18n); });
    root.querySelectorAll("[data-i18n-title]").forEach((node) => { node.title = t(node.dataset.i18nTitle); });
    root.querySelectorAll("[data-i18n-placeholder]").forEach((node) => { node.placeholder = t(node.dataset.i18nPlaceholder); });
    root.querySelectorAll("[data-i18n-label]").forEach((node) => { node.setAttribute("aria-label", t(node.dataset.i18nLabel)); });
  }

  function setLocale(value) {
    const next = normalize(value);
    if (next === locale) return;
    locale = next;
    localStorage.setItem(STORAGE_KEY, locale);
    window.dispatchEvent(new CustomEvent("codexbar-locale-changed", { detail: locale }));
  }

  function onChange(listener) {
    window.addEventListener("codexbar-locale-changed", listener);
    window.addEventListener("storage", (event) => {
      if (event.key === STORAGE_KEY) {
        locale = normalize(event.newValue);
        listener();
      }
    });
  }

  window.CodexBarI18n = { apply, getLocale: () => locale, normalize, onChange, setLocale, t };
})();
