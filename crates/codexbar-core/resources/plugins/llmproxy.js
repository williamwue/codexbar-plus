defineProvider({
  id: "llmproxy",
  name: "LLMProxy",
  endpoints: [{ setting: "LLM_PROXY_BASE_URL", policy: "https-or-private-network-http" }],
  auth: { type: "bearer", secret: "LLM_PROXY_API_KEY" },
  settings: [
    { key: "LLM_PROXY_API_KEY", title: "API key", type: "secure" },
    { key: "LLM_PROXY_BASE_URL", title: "Proxy base URL", type: "plain" },
  ],
  async fetchUsage(ctx) {
    let base = (ctx.settings.get("LLM_PROXY_BASE_URL") || "").replace(/\/+$/, "");
    if (!base) throw ctx.fail.missingCredential("LLMProxy base URL is required.");
    if (!base.endsWith("/v1")) base += "/v1";
    const response = await ctx.http.getJSON(`${base}/quota-stats`);
    if (response.status === 401 || response.status === 403) {
      throw ctx.fail.authenticationExpired("LLMProxy rejected the API key.");
    }
    if (response.status === 429) throw ctx.fail.rateLimited("LLMProxy API returned HTTP 429.");
    if (response.status >= 500) throw ctx.fail.providerUnavailable(`LLMProxy API returned HTTP ${response.status}.`);
    if (response.status < 200 || response.status >= 300) {
      throw ctx.fail.apiFailure(`LLMProxy API returned HTTP ${response.status}.`);
    }
    const root = response.json;
    if (!root || typeof root !== "object" || !root.providers || typeof root.providers !== "object" || Array.isArray(root.providers)) {
      throw ctx.fail.parseFailure("LLMProxy quota-stats response has an invalid shape.");
    }
    const providers = [];
    let credentials = 0;
    let active = 0;
    let exhausted = 0;
    const remaining = [];
    const resets = [];
    for (const [name, stats] of Object.entries(root.providers)) {
      if (!stats || typeof stats !== "object") continue;
      const integer = (value) => Number.isInteger(value) ? value : 0;
      const tokens = stats.tokens && typeof stats.tokens === "object"
        ? integer(stats.tokens.input_cached) + integer(stats.tokens.input_uncached) + integer(stats.tokens.output)
        : 0;
      providers.push({
        name,
        requests: integer(stats.total_requests),
        tokens,
        cost: typeof stats.approx_cost === "number" && Number.isFinite(stats.approx_cost) ? stats.approx_cost : null,
      });
      credentials += integer(stats.credential_count);
      active += integer(stats.active_count);
      exhausted += integer(stats.exhausted_count);
      const groups = Array.isArray(stats.quota_groups)
        ? stats.quota_groups
        : stats.quota_groups && typeof stats.quota_groups === "object" ? Object.values(stats.quota_groups) : [];
      for (const group of groups) {
        if (group && typeof group.remaining_percent === "number") remaining.push(group.remaining_percent);
        if (group && typeof group.reset_time === "string") {
          try {
            const reset = ctx.date.iso(group.reset_time);
            if (reset.getTime() > ctx.date.now().getTime()) resets.push(reset);
          } catch {}
        }
      }
    }
    providers.sort((left, right) => right.requests - left.requests || left.name.localeCompare(right.name));
    const summary = root.summary && typeof root.summary === "object" ? root.summary : {};
    const totalRequests = Number.isInteger(summary.total_requests)
      ? summary.total_requests : providers.reduce((sum, item) => sum + item.requests, 0);
    const totalTokens = Number.isInteger(summary.total_tokens)
      ? summary.total_tokens : providers.reduce((sum, item) => sum + item.tokens, 0);
    let cost = typeof summary.approx_cost === "number" ? summary.approx_cost : null;
    if (cost === null) {
      const sum = providers.reduce((value, item) => value + (item.cost || 0), 0);
      cost = sum > 0 ? sum : null;
    }
    const integer = (value) => ctx.format.number(value, { maximumFractionDigits: 0 });
    const extraWindows = providers.slice(0, 3).map((provider) => ({
      id: provider.name,
      title: provider.name,
      usedPercent: 0,
      resetDescription: `${integer(provider.requests)} req · ${integer(provider.tokens)} tok${provider.cost == null ? "" : ` · ${ctx.format.usd(provider.cost)}`}`,
    }));
    return {
      primary: remaining.length ? {
        usedPercent: Math.max(0, Math.min(100, 100 - Math.min(...remaining))),
        resetsAt: resets.length ? new Date(Math.min(...resets.map((date) => date.getTime()))) : null,
      } : null,
      secondary: { usedPercent: 0, resetDescription: `${integer(totalRequests)} requests` },
      tertiary: { usedPercent: 0, resetDescription: `${integer(totalTokens)} tokens` },
      extraWindows: extraWindows.length ? extraWindows : null,
      cost: cost == null ? null : { used: cost, limit: 0, currency: "USD", period: "Approx. spend" },
      identity: { organization: `${active}/${credentials} active keys`, loginMethod: "quota-stats" },
      dataConfidence: "estimated",
      details: [{ title: "Credentials", rows: [
        { label: "Providers", value: String(providers.length) },
        { label: "Active", value: String(active) },
        { label: "Exhausted", value: String(exhausted) },
      ] }],
    };
  },
});
