defineProvider({
  id: "aiand",
  name: "ai&",
  endpoints: ["https://api.aiand.com"],
  auth: { type: "bearer", secret: "AIAND_API_KEY" },
  settings: [{ key: "AIAND_API_KEY", title: "API key", type: "secure" }],
  async fetchUsage(ctx) {
    let after = null;
    let afterID = null;
    let complete = false;
    const rows = [];
    for (let page = 0; page < 10; page += 1) {
      let url = "https://api.aiand.com/logs?range=30days&limit=100";
      if (after !== null) url += `&after=${encodeURIComponent(after)}`;
      if (afterID !== null) url += `&after_id=${encodeURIComponent(afterID)}`;
      const response = await ctx.http.getJSON(url, { timeoutSeconds: 15 });
      if (response.status === 401 || response.status === 403) {
        throw ctx.fail.authenticationExpired("ai& rejected the API key.");
      }
      if (response.status === 402) throw ctx.fail.apiFailure("ai& reports insufficient credits.");
      if (response.status === 429) throw ctx.fail.rateLimited("ai& API returned HTTP 429.");
      if (response.status >= 500) throw ctx.fail.providerUnavailable(`ai& API returned HTTP ${response.status}.`);
      if (response.status < 200 || response.status >= 300) {
        throw ctx.fail.apiFailure(`ai& API returned HTTP ${response.status}.`);
      }
      const body = response.json;
      if (!body || typeof body !== "object" || !Array.isArray(body.data)) {
        throw ctx.fail.parseFailure("ai& logs response has an invalid shape.");
      }
      rows.push(...body.data);
      if (body.has_more !== true) {
        complete = true;
        break;
      }
      if (typeof body.next_after !== "string" || typeof body.next_after_id !== "string") break;
      after = body.next_after;
      afterID = body.next_after_id;
    }
    let currency = null;
    let total = 0;
    for (const row of rows) {
      if (!row || typeof row !== "object" || typeof row.cost !== "string" || typeof row.currency !== "string") continue;
      const cost = Number(row.cost);
      const code = row.currency.trim().toLowerCase();
      if (!Number.isFinite(cost) || !code) continue;
      if (currency === null) currency = code;
      if (code === currency) total += cost;
    }
    if (currency === null) {
      return {
        identity: { loginMethod: complete ? "No priced requests in the last 30 days" : "No priced requests in partial history" },
        dataConfidence: complete ? "exact" : "estimated",
      };
    }
    return {
      cost: {
        used: total,
        limit: 0,
        currency: currency.toUpperCase(),
        period: complete ? "Last 30 days" : "Last 30 days (partial)",
      },
      dataConfidence: complete ? "exact" : "estimated",
    };
  },
});
