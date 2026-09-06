defineProvider({
  id: "litellm",
  name: "LiteLLM",
  endpoints: [{ setting: "LITELLM_BASE_URL", policy: "https-or-private-network-http" }],
  auth: { type: "bearer", secret: "LITELLM_API_KEY" },
  settings: [
    { key: "LITELLM_API_KEY", title: "Virtual API key", type: "secure" },
    { key: "LITELLM_BASE_URL", title: "Proxy base URL", type: "plain" },
  ],
  async fetchUsage(ctx) {
    let base = (ctx.settings.get("LITELLM_BASE_URL") || "").replace(/\/+$/, "");
    if (!base) throw ctx.fail.missingCredential("LiteLLM proxy base URL is required.");
    if (base.endsWith("/v1")) base = base.slice(0, -3);
    async function get(path) {
      const response = await ctx.http.getJSON(`${base}${path}`);
      if (response.status === 401 || response.status === 403) {
        throw ctx.fail.authenticationExpired("LiteLLM rejected the virtual key.");
      }
      if (response.status === 429) throw ctx.fail.rateLimited("LiteLLM API returned HTTP 429.");
      if (response.status >= 500) throw ctx.fail.providerUnavailable(`LiteLLM API returned HTTP ${response.status}.`);
      if (response.status < 200 || response.status >= 300) {
        throw ctx.fail.apiFailure(`LiteLLM API returned HTTP ${response.status}.`);
      }
      return response.json;
    }
    const keyRoot = await get("/key/info");
    const info = keyRoot && keyRoot.info;
    if (!info || typeof info !== "object") throw ctx.fail.parseFailure("LiteLLM /key/info response is invalid.");
    const userID = typeof info.user_id === "string" && info.user_id.trim() ? info.user_id.trim() : null;
    const teamID = typeof info.team_id === "string" && info.team_id.trim() ? info.team_id.trim() : null;
    if (!userID && !teamID) throw ctx.fail.parseFailure("LiteLLM key has neither a user_id nor a team_id.");
    let user = null;
    let team = null;
    let email = null;
    if (userID) {
      const root = await get(`/user/info?user_id=${encodeURIComponent(userID)}`);
      user = root && root.user_info;
      if (!user || typeof user !== "object") throw ctx.fail.parseFailure("LiteLLM /user/info response is invalid.");
      const returned = user.user_id || root.user_id;
      if (returned && returned !== userID) throw ctx.fail.parseFailure("LiteLLM user_id did not match /key/info.");
      email = user.user_email || user.user_alias || (user.metadata && user.metadata.preferred_username) || null;
      if (Array.isArray(root.teams) && root.teams.length) {
        team = root.teams.find((candidate) => candidate && candidate.team_id === teamID) || root.teams[0];
      }
    } else {
      const root = await get(`/team/info?team_id=${encodeURIComponent(teamID)}`);
      team = root && root.team_info;
      if (!team || typeof team !== "object") throw ctx.fail.parseFailure("LiteLLM /team/info response is invalid.");
      const returned = team.team_id || root.team_id;
      if (returned && returned !== teamID) throw ctx.fail.parseFailure("LiteLLM team_id did not match /key/info.");
    }
    const finite = (value, fallback) => typeof value === "number" && Number.isFinite(value) ? value : fallback;
    const date = (value) => typeof value === "string" && value.trim() ? ctx.date.iso(value) : null;
    const personalSpend = user ? finite(user.spend, 0) : 0;
    const personalBudget = user && finite(user.max_budget, null);
    const teamSpend = team ? finite(team.spend, 0) : 0;
    const teamBudget = team && finite(team.max_budget, null);
    const teamAlias = team && typeof team.team_alias === "string" ? team.team_alias : null;
    function window(spend, budget, reset, prefix) {
      if (!(typeof budget === "number" && budget > 0)) return null;
      return {
        usedPercent: ctx.pct(spend, budget),
        resetsAt: date(reset),
        resetDescription: `${prefix}${ctx.format.usd(spend)} / ${ctx.format.usd(budget)}`,
      };
    }
    const costSpend = userID ? personalSpend : teamSpend;
    const costBudget = userID ? personalBudget : teamBudget;
    const costReset = userID ? user && user.budget_reset_at : team && team.budget_reset_at;
    return {
      primary: window(personalSpend, personalBudget, user && user.budget_reset_at, ""),
      secondary: team ? window(teamSpend, teamBudget, team.budget_reset_at, `Team${teamAlias ? ` ${teamAlias}` : ""}: `) : null,
      cost: costSpend > 0 || (costBudget || 0) > 0 ? {
        used: costSpend,
        limit: Math.max(0, costBudget || 0),
        currency: "USD",
        period: `${userID ? "Personal" : "Team"} ${(costBudget || 0) > 0 ? "budget" : "spend"}`,
        resetsAt: date(costReset),
      } : null,
      subscriptionExpiresAt: date(info.expires),
      identity: {
        email,
        organization: teamAlias,
        loginMethod: "api",
        accountID: userID || teamID,
      },
    };
  },
});
