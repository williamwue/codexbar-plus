defineProvider({
  id: "zenmux",
  name: "ZenMux",
  endpoints: ["https://zenmux.ai"],
  auth: { type: "bearer", secret: "ZENMUX_MANAGEMENT_API_KEY" },
  settings: [
    { key: "ZENMUX_MANAGEMENT_API_KEY", title: "Management API key", type: "secure" },
    { key: "ZENMUX_INCLUDE_PAYG", title: "Include PAYG balance (1 or 0)", type: "plain" },
  ],
  async fetchUsage(ctx) {
    const root = "https://zenmux.ai/api/v1/management";
    async function get(path, optional) {
      const response = await ctx.http.getJSON(`${root}/${path}`, { timeoutSeconds: 15 });
      if (response.status === 401 || response.status === 403) {
        throw ctx.fail.authenticationExpired("ZenMux rejected the Management API key.");
      }
      if (response.status === 429) throw ctx.fail.rateLimited("ZenMux API returned HTTP 429.");
      if (response.status >= 500) {
        if (optional) return null;
        throw ctx.fail.providerUnavailable(`ZenMux API returned HTTP ${response.status}.`);
      }
      if (response.status < 200 || response.status >= 300) {
        if (optional) return null;
        throw ctx.fail.apiFailure(`ZenMux API returned HTTP ${response.status}.`);
      }
      return response.json;
    }
    const subscription = await get("subscription/detail", false);
    if (!subscription || subscription.success !== true || !subscription.data) {
      throw ctx.fail.parseFailure("ZenMux subscription response has an invalid shape.");
    }
    const data = subscription.data;
    function quota(raw, field, minutes) {
      if (!raw || typeof raw !== "object") throw ctx.fail.parseFailure(`ZenMux ${field} is missing.`);
      for (const name of ["usage_percentage", "max_flows", "used_flows", "remaining_flows"]) {
        if (typeof raw[name] !== "number" || !Number.isFinite(raw[name])) {
          throw ctx.fail.parseFailure(`ZenMux ${field}.${name} is invalid.`);
        }
      }
      const amount = (value) => ctx.format.number(value, {
        minimumFractionDigits: Number.isInteger(value) ? 0 : 2,
        maximumFractionDigits: Number.isInteger(value) ? 0 : 2,
      });
      return {
        usedPercent: Math.max(0, Math.min(100, raw.usage_percentage * 100)),
        windowMinutes: minutes,
        resetsAt: raw.resets_at ? ctx.date.iso(raw.resets_at) : null,
        resetDescription: `${amount(raw.used_flows)} / ${amount(raw.max_flows)} flows`,
      };
    }
    const plan = data.plan && typeof data.plan.tier === "string" ? data.plan.tier.trim() : "";
    const status = typeof data.account_status === "string" ? data.account_status.trim() : "";
    let loginMethod = plan ? `${plan.replace(/\b\w/g, (letter) => letter.toUpperCase())} plan` : "";
    if (status && status.toLowerCase() !== "healthy") {
      loginMethod += `${loginMethod ? " · " : ""}${status.replace(/\b\w/g, (letter) => letter.toUpperCase())}`;
    }
    let cost = null;
    if (ctx.settings.get("ZENMUX_INCLUDE_PAYG") !== "0") {
      const balance = await get("payg/balance", true);
      if (balance && balance.success === true && balance.data &&
          String(balance.data.currency).trim().toLowerCase() === "usd" &&
          typeof balance.data.total_credits === "number") {
        cost = {
          used: balance.data.total_credits,
          limit: 0,
          currency: "USD",
          period: "ZenMux PAYG balance",
        };
      }
    }
    return {
      primary: quota(data.quota_5_hour, "quota_5_hour", 300),
      secondary: quota(data.quota_7_day, "quota_7_day", 10080),
      cost,
      subscriptionExpiresAt: data.plan && data.plan.expires_at ? ctx.date.iso(data.plan.expires_at) : null,
      identity: loginMethod ? { loginMethod } : null,
      dataConfidence: "exact",
    };
  },
});
