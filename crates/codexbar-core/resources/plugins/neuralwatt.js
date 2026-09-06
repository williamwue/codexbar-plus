defineProvider({
  id: "neuralwatt",
  name: "Neuralwatt",
  endpoints: ["https://api.neuralwatt.com", { setting: "NEURALWATT_API_URL", policy: "https" }],
  auth: { type: "bearer", secret: "NEURALWATT_API_KEY" },
  settings: [
    { key: "NEURALWATT_API_KEY", title: "API key", type: "secure" },
    { key: "NEURALWATT_API_URL", title: "API URL", type: "plain" },
  ],
  async fetchUsage(ctx) {
    let base = (ctx.settings.get("NEURALWATT_API_URL") || "https://api.neuralwatt.com").replace(/\/+$/, "");
    const url = base.endsWith("/v1") ? `${base}/quota` : `${base}/v1/quota`;
    const response = await ctx.http.getJSON(url, { timeoutSeconds: 15 });
    if (response.status === 401 || response.status === 403) {
      throw ctx.fail.authenticationExpired("Neuralwatt rejected the API key.");
    }
    if (response.status === 429) throw ctx.fail.rateLimited("Neuralwatt API returned HTTP 429.");
    if (response.status >= 500) throw ctx.fail.providerUnavailable(`Neuralwatt API returned HTTP ${response.status}.`);
    if (response.status !== 200) throw ctx.fail.apiFailure(`Neuralwatt API returned HTTP ${response.status}.`);
    const data = response.json;
    const balance = data && data.balance;
    if (!balance || typeof balance !== "object") throw ctx.fail.parseFailure("Neuralwatt balance object is missing.");
    const validNonNegative = (value) => typeof value === "number" && Number.isFinite(value) && value >= 0 ? value : null;
    const validPositive = (value) => typeof value === "number" && Number.isFinite(value) && value > 0 ? value : null;
    const rawRemaining = validNonNegative(balance.credits_remaining_usd);
    const rawUsed = validNonNegative(balance.credits_used_usd);
    const rawTotal = validPositive(balance.total_credits_usd);
    if (rawRemaining === null && rawUsed === null && rawTotal === null) {
      throw ctx.fail.parseFailure("Neuralwatt credit balance fields are missing.");
    }
    const total = rawTotal !== null ? rawTotal
      : rawRemaining !== null && rawUsed !== null && rawRemaining + rawUsed > 0 ? rawRemaining + rawUsed : null;
    const used = rawUsed !== null ? rawUsed
      : total !== null && rawRemaining !== null ? Math.max(0, total - rawRemaining) : null;
    const remaining = rawRemaining !== null ? rawRemaining
      : total !== null && used !== null ? Math.max(0, total - used) : null;
    const subscription = data.subscription && typeof data.subscription === "object" ? data.subscription : null;
    let primary = null;
    if (subscription) {
      const included = validPositive(subscription.kwh_included);
      const subUsed = validNonNegative(subscription.kwh_used);
      const subRemaining = validNonNegative(subscription.kwh_remaining);
      const subTotal = included !== null ? included
        : subUsed !== null && subRemaining !== null && subUsed + subRemaining > 0 ? subUsed + subRemaining : null;
      const effectiveUsed = subUsed !== null ? subUsed
        : subTotal !== null && subRemaining !== null ? Math.max(0, subTotal - subRemaining) : null;
      if (subTotal !== null && effectiveUsed !== null) {
        const start = subscription.current_period_start ? ctx.date.iso(subscription.current_period_start) : null;
        const end = subscription.current_period_end ? ctx.date.iso(subscription.current_period_end) : null;
        const digits = (value) => ctx.format.number(value, {
          minimumFractionDigits: Number.isInteger(value) ? 0 : 2,
          maximumFractionDigits: Number.isInteger(value) ? 0 : 2,
        });
        primary = {
          usedPercent: ctx.pct(effectiveUsed, subTotal),
          windowMinutes: start && end && end > start ? Math.max(1, Math.floor((end - start) / 60000)) : null,
          resetsAt: end,
          resetDescription: `${digits(effectiveUsed)} / ${digits(subTotal)} kWh`,
        };
      }
    }
    const allowance = data.key && data.key.allowance;
    const extraWindows = [];
    if (allowance && typeof allowance === "object") {
      const limit = validPositive(allowance.limit_usd);
      const spent = validNonNegative(allowance.spent_usd);
      if (allowance.blocked === true || (limit !== null && spent !== null)) {
        const period = typeof allowance.period === "string" && allowance.period ? allowance.period : "allowance";
        extraWindows.push({
          id: "key-allowance",
          title: `Key ${period.replace(/\b\w/g, (letter) => letter.toUpperCase())}`,
          usedPercent: allowance.blocked === true ? 100 : ctx.pct(spent, limit),
        });
      }
    }
    const method = subscription && typeof subscription.plan === "string" && subscription.plan.trim()
      ? `${subscription.plan.replace(/_/g, " ").replace(/\b\w/g, (letter) => letter.toUpperCase())} plan`
      : typeof balance.accounting_method === "string" ? balance.accounting_method.replace(/\b\w/g, (letter) => letter.toUpperCase()) : null;
    return {
      primary,
      extraWindows: extraWindows.length ? extraWindows : null,
      cost: remaining === null ? null : {
        used: remaining,
        limit: 0,
        currency: "USD",
        period: "Neuralwatt prepaid balance",
      },
      subscriptionRenewsAt: subscription && subscription.auto_renew !== false && subscription.current_period_end
        ? ctx.date.iso(subscription.current_period_end) : null,
      identity: method ? { loginMethod: method } : null,
      dataConfidence: "exact",
    };
  },
});
