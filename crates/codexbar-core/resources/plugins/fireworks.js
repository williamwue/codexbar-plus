defineProvider({
  id: "fireworks",
  name: "Fireworks AI",
  endpoints: ["https://api.fireworks.ai"],
  auth: { type: "bearer", secret: "FIREWORKS_API_KEY" },
  settings: [
    { key: "FIREWORKS_API_KEY", title: "API key", type: "secure" },
    { key: "FIREWORKS_ACCOUNT_SLUG", title: "Account slug", type: "plain" },
  ],
  async fetchUsage(ctx) {
    async function get(url) {
      const response = await ctx.http.getJSON(url, { timeoutSeconds: 15 });
      if (response.status === 401 || response.status === 403) {
        throw ctx.fail.authenticationExpired("Fireworks rejected the API key.");
      }
      if (response.status === 429) throw ctx.fail.rateLimited("Fireworks API returned HTTP 429.");
      if (response.status >= 500) throw ctx.fail.providerUnavailable(`Fireworks API returned HTTP ${response.status}.`);
      if (response.status < 200 || response.status >= 300) {
        throw ctx.fail.apiFailure(`Fireworks API returned HTTP ${response.status}.`);
      }
      return response.json;
    }
    async function discoverAccounts() {
      const slugs = new Set();
      let pageToken = null;
      do {
        const url = `https://api.fireworks.ai/v1/accounts${pageToken ? `?pageToken=${encodeURIComponent(pageToken)}` : ""}`;
        const page = await get(url);
        if (!page || typeof page !== "object" || (page.accounts != null && !Array.isArray(page.accounts))) {
          throw ctx.fail.parseFailure("Fireworks accounts response has an invalid shape.");
        }
        for (const account of page.accounts || []) {
          if (!account || typeof account !== "object") continue;
          const raw = account.accountId || account.id || account.name;
          if (typeof raw !== "string") continue;
          const slug = raw.trim().split("/").pop();
          if (/^[A-Za-z0-9._-]+$/.test(slug)) slugs.add(slug);
        }
        pageToken = typeof page.nextPageToken === "string" && page.nextPageToken.trim()
          ? page.nextPageToken.trim() : null;
      } while (pageToken);
      return Array.from(slugs).sort();
    }
    let slug = (ctx.settings.get("FIREWORKS_ACCOUNT_SLUG") || "").trim();
    if (slug && !/^[A-Za-z0-9._-]+$/.test(slug)) {
      throw ctx.fail.apiFailure("Fireworks account slug contains invalid characters.");
    }
    if (!slug) {
      const slugs = await discoverAccounts();
      if (!slugs.length) throw ctx.fail.apiFailure("No Fireworks accounts are visible to this API key.");
      if (slugs.length > 1) {
        throw ctx.fail.apiFailure(`Multiple Fireworks accounts are visible (${slugs.join(", ")}); configure an account slug.`);
      }
      slug = slugs[0];
    }
    const end = ctx.date.now();
    const start = new Date(end.getTime() - 30 * 24 * 60 * 60 * 1000);
    const summary = await get(
      `https://api.fireworks.ai/v1/accounts/${slug}/billing/summary?startTime=${encodeURIComponent(start.toISOString())}&endTime=${encodeURIComponent(end.toISOString())}`,
    );
    if (!summary || typeof summary !== "object" || (summary.lineItems != null && !Array.isArray(summary.lineItems))) {
      throw ctx.fail.parseFailure("Fireworks billing summary has an invalid shape.");
    }
    let currency = null;
    let total = 0;
    for (const item of summary.lineItems || []) {
      const money = item && item.totalCost;
      if (!money || typeof money !== "object" || typeof money.units !== "string" || !Number.isInteger(money.nanos) ||
          typeof money.currencyCode !== "string" || !money.currencyCode.trim()) continue;
      const units = Number(money.units);
      if (!Number.isFinite(units)) continue;
      const code = money.currencyCode.trim();
      if (currency === null) currency = code;
      if (code === currency) total += units + money.nanos / 1000000000;
    }
    if (currency === null) {
      return { identity: { organization: slug, loginMethod: "No rated spend in the last 30 days" } };
    }
    return {
      cost: { used: total, limit: 0, currency, period: "Last 30 days" },
      identity: { organization: slug, loginMethod: "api" },
      dataConfidence: "exact",
    };
  },
});
