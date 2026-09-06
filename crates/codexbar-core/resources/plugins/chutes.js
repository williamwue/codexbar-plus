defineProvider({
  id: "chutes",
  name: "Chutes",
  endpoints: ["https://api.chutes.ai", { setting: "CHUTES_API_URL", policy: "https" }],
  auth: { type: "bearer", secret: "CHUTES_API_KEY" },
  settings: [
    { key: "CHUTES_API_KEY", title: "API key", type: "secure" },
    { key: "CHUTES_API_URL", title: "API URL", type: "plain" },
  ],
  async fetchUsage(ctx) {
    const base = (ctx.settings.get("CHUTES_API_URL") || "https://api.chutes.ai").replace(/\/+$/, "");
    async function get(path, optional) {
      const response = await ctx.http.getJSON(`${base}/${path}`, { timeoutSeconds: 15 });
      if (response.status === 401 || response.status === 403) {
        throw ctx.fail.authenticationExpired("Chutes rejected the API key.");
      }
      if (response.status === 429) throw ctx.fail.rateLimited("Chutes API returned HTTP 429.");
      if (response.status >= 500) {
        if (optional) return null;
        throw ctx.fail.providerUnavailable(`Chutes API returned HTTP ${response.status}.`);
      }
      if (response.status < 200 || response.status >= 300) {
        if (optional) return null;
        throw ctx.fail.apiFailure(`Chutes API returned HTTP ${response.status}.`);
      }
      return response.json;
    }
    function numeric(object, names) {
      for (const name of names) {
        if (typeof object[name] === "number" && Number.isFinite(object[name])) return object[name];
      }
      return null;
    }
    function parseWindow(raw, defaultMinutes) {
      if (!raw || typeof raw !== "object" || Array.isArray(raw)) return null;
      let used = numeric(raw, ["used", "requests", "consumed", "usage"]);
      let limit = numeric(raw, ["limit", "quota", "total"]);
      let remaining = numeric(raw, ["remaining", "remaining_quota"]);
      let percent = numeric(raw, ["usage_percent", "used_percent", "percent_used"]);
      const percentRemaining = numeric(raw, ["percent_remaining", "remaining_percent"]);
      if (percent === null && percentRemaining !== null) percent = 100 - percentRemaining;
      if (limit === null && used !== null && remaining !== null) limit = used + remaining;
      if (used === null && limit !== null && remaining !== null) used = limit - remaining;
      if (percent === null && used !== null && limit !== null && limit > 0) percent = used / limit * 100;
      if (percent === null) return null;
      const unit = typeof raw.unit === "string" && raw.unit.trim() ? ` ${raw.unit.trim()}` : "";
      const amount = (value) => ctx.format.number(value, {
        minimumFractionDigits: Number.isInteger(value) ? 0 : 0,
        maximumFractionDigits: Number.isInteger(value) ? 0 : 2,
      });
      const resetRaw = raw.resets_at || raw.reset_at || raw.reset_time;
      return {
        usedPercent: Math.max(0, Math.min(100, percent)),
        windowMinutes: numeric(raw, ["window_minutes"]) || defaultMinutes || null,
        resetsAt: typeof resetRaw === "string" ? ctx.date.iso(resetRaw) : null,
        resetDescription: used !== null && limit !== null && limit > 0 ? `${amount(used)}/${amount(limit)}${unit}` : null,
      };
    }
    function parseSnapshot(root) {
      const object = root && typeof root === "object" ? root : {};
      const subscription = object.subscription && typeof object.subscription === "object" ? object.subscription : {};
      const rolling = parseWindow(object.rolling_window || object.rollingWindow, 240);
      const monthly = parseWindow(object.monthly || object.monthly_window, 43200);
      let definitions = Array.isArray(object) ? object
        : Array.isArray(object.quotas) ? object.quotas
        : Array.isArray(object.data) ? object.data
        : object.data && Array.isArray(object.data.quotas) ? object.data.quotas : [];
      return {
        rolling,
        monthly,
        fallbacks: definitions.map((item) => parseWindow(item, numeric(item || {}, ["window_minutes"]))).filter(Boolean),
        definitions,
        active: subscription.active === true ? true : subscription.active === false ? false : null,
        plan: typeof subscription.plan_name === "string" ? subscription.plan_name.trim() : "",
        renewsAt: typeof subscription.current_period_end === "string" ? ctx.date.iso(subscription.current_period_end) : null,
      };
    }
    const subscriptionRoot = await get("users/me/subscription_usage", false);
    const subscription = parseSnapshot(subscriptionRoot);
    let quota = { rolling: null, monthly: null, fallbacks: [], definitions: [] };
    if (!subscription.rolling || !subscription.monthly) {
      const quotaRoot = await get("users/me/quotas", true);
      if (quotaRoot) {
        quota = parseSnapshot(quotaRoot);
        if (quota.definitions.length) {
          const enriched = [];
          for (const definition of quota.definitions) {
            const id = definition && (definition.chute_id ?? definition.chuteId ?? definition.id);
            if (id == null) {
              enriched.push(definition);
              continue;
            }
            const usage = await get(`users/me/quota_usage/${encodeURIComponent(String(id))}`, true);
            const payload = usage && typeof usage === "object" ? usage.data || usage.result || usage : null;
            enriched.push(payload && typeof payload === "object" ? { ...definition, ...payload } : definition);
          }
          quota.fallbacks = enriched.map((item) => parseWindow(item, numeric(item || {}, ["window_minutes"]))).filter(Boolean);
        }
      }
    }
    const rolling = subscription.rolling || quota.rolling;
    const monthly = subscription.monthly || quota.monthly;
    const fallbacks = [...quota.fallbacks, ...subscription.fallbacks];
    const primary = rolling || (!monthly ? fallbacks[0] || null : null);
    const secondary = monthly || (rolling ? fallbacks[0] || null : primary ? fallbacks[1] || null : null);
    let loginMethod = subscription.plan || null;
    if (!loginMethod && subscription.active === false) loginMethod = "No active subscription";
    if (!loginMethod && subscription.active === null && !primary && !secondary) loginMethod = "No usage data";
    return {
      primary,
      secondary,
      subscriptionRenewsAt: subscription.renewsAt || (monthly && monthly.resetsAt) || null,
      identity: loginMethod ? { loginMethod } : null,
      dataConfidence: "exact",
    };
  },
});
