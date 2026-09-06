defineProvider({
  id: "moonshot",
  name: "Moonshot / Kimi",
  endpoints: ["https://api.moonshot.ai", "https://api.moonshot.cn"],
  auth: { type: "bearer", secret: "MOONSHOT_API_KEY" },
  settings: [
    { key: "MOONSHOT_API_KEY", title: "API key", type: "secure" },
    { key: "MOONSHOT_REGION", title: "Region (international or china)", type: "plain" },
  ],
  async fetchUsage(ctx) {
    const region = (ctx.settings.get("MOONSHOT_REGION") || "international").trim().toLowerCase();
    const origin = region === "china" ? "https://api.moonshot.cn" : "https://api.moonshot.ai";
    const response = await ctx.http.getJSON(`${origin}/v1/users/me/balance`, { timeoutSeconds: 15 });
    if (response.status === 401 || response.status === 403) {
      throw ctx.fail.authenticationExpired("Moonshot rejected the API key.");
    }
    if (response.status === 429) throw ctx.fail.rateLimited("Moonshot API returned HTTP 429.");
    if (response.status >= 500) throw ctx.fail.providerUnavailable(`Moonshot API returned HTTP ${response.status}.`);
    if (response.status !== 200) throw ctx.fail.apiFailure(`Moonshot API returned HTTP ${response.status}.`);
    const root = response.json;
    if (!root || typeof root !== "object" || root.code !== 0 || root.status !== true || !root.data) {
      throw ctx.fail.parseFailure("Moonshot balance response has an invalid shape.");
    }
    const data = root.data;
    for (const field of ["available_balance", "voucher_balance", "cash_balance"]) {
      if (typeof data[field] !== "number" || !Number.isFinite(data[field])) {
        throw ctx.fail.parseFailure(`Moonshot balance field ${field} is invalid.`);
      }
    }
    const balance = ctx.format.usd(data.available_balance);
    const loginMethod = data.cash_balance < 0
      ? `Balance: ${balance} · ${ctx.format.usd(Math.abs(data.cash_balance))} in deficit`
      : `Balance: ${balance}`;
    return {
      identity: { loginMethod },
      details: [{
        title: "Balance",
        rows: [
          { label: "Available", value: balance },
          { label: "Voucher", value: ctx.format.usd(data.voucher_balance) },
          { label: "Cash", value: ctx.format.usd(data.cash_balance) },
        ],
      }],
    };
  },
});
