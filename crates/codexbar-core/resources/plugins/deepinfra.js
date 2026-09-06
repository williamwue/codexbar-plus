defineProvider({
  id: "deepinfra",
  name: "DeepInfra",
  endpoints: ["https://api.deepinfra.com"],
  auth: { type: "bearer", secret: "DEEPINFRA_API_KEY" },
  settings: [{ key: "DEEPINFRA_API_KEY", title: "API key", type: "secure" }],
  async fetchUsage(ctx) {
    async function get(url) {
      const response = await ctx.http.getJSON(url, { timeoutSeconds: 30 });
      if (response.status === 401 || response.status === 403) {
        throw ctx.fail.authenticationExpired(`DeepInfra rejected billing access (HTTP ${response.status}).`);
      }
      if (response.status === 429) throw ctx.fail.rateLimited("DeepInfra API returned HTTP 429.");
      if (response.status >= 500) throw ctx.fail.providerUnavailable(`DeepInfra API returned HTTP ${response.status}.`);
      if (response.status !== 200) throw ctx.fail.apiFailure(`DeepInfra API returned HTTP ${response.status}.`);
      return response.json;
    }
    const checklist = await get("https://api.deepinfra.com/payment/checklist?compute_owed=true");
    const usage = await get("https://api.deepinfra.com/payment/usage?from=current");
    if (!checklist || typeof checklist !== "object" || typeof checklist.stripe_balance !== "number" ||
        typeof checklist.recent !== "number" || typeof checklist.suspended !== "boolean" ||
        !usage || typeof usage !== "object" || !Array.isArray(usage.months)) {
      throw ctx.fail.parseFailure("DeepInfra billing response has an invalid shape.");
    }
    const recent = Math.max(0, checklist.recent);
    const latest = usage.months.length ? usage.months[usage.months.length - 1] : null;
    if (latest && typeof latest.total_cost !== "number") {
      throw ctx.fail.parseFailure("DeepInfra monthly total_cost is invalid.");
    }
    const monthCost = latest ? Math.max(0, latest.total_cost / 100) : recent;
    const netBalance = checklist.stripe_balance + recent;
    const available = Math.max(0, -netBalance);
    const owed = Math.max(0, netBalance);
    const suspended = checklist.suspended;
    const reason = typeof checklist.suspend_reason === "string" ? checklist.suspend_reason.trim() : "";
    const balanceText = owed > 0 ? `${ctx.format.usd(owed)} owed` : `${ctx.format.usd(available)} available`;
    const spendingText = `${ctx.format.usd(monthCost)} spent this month`;
    const detail = suspended
      ? `${reason ? `Suspended: ${reason}` : "Suspended"} · ${balanceText} · ${spendingText}`
      : `${balanceText} · ${spendingText}`;
    const limit = typeof checklist.limit === "number" && checklist.limit > 0 ? checklist.limit : null;
    return {
      primary: {
        usedPercent: suspended || owed > 0 || available <= 0 ? 100 : 0,
        resetDescription: detail,
      },
      cost: limit == null ? null : {
        used: recent,
        limit,
        currency: "USD",
        period: "Billing cycle",
      },
      dataConfidence: "exact",
      details: [{
        title: "Billing",
        rows: [
          { label: "Available", value: ctx.format.usd(available) },
          { label: "Current month", value: ctx.format.usd(monthCost) },
        ],
      }],
    };
  },
});
