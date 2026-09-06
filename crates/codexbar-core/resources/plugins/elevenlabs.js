defineProvider({
  id: "elevenlabs",
  name: "ElevenLabs",
  endpoints: ["https://api.elevenlabs.io", { setting: "ELEVENLABS_API_URL", policy: "https" }],
  auth: { type: "header", header: "xi-api-key", secret: "ELEVENLABS_API_KEY" },
  settings: [
    { key: "ELEVENLABS_API_KEY", title: "API key", type: "secure" },
    { key: "ELEVENLABS_API_URL", title: "API URL", type: "plain" },
  ],
  async fetchUsage(ctx) {
    let base = (ctx.settings.get("ELEVENLABS_API_URL") || "https://api.elevenlabs.io").replace(/\/+$/, "");
    const url = base.endsWith("/v1") ? `${base}/user/subscription` : `${base}/v1/user/subscription`;
    const response = await ctx.http.getJSON(url, { timeoutSeconds: 15 });
    if (response.status === 401 || response.status === 403) {
      throw ctx.fail.authenticationExpired("ElevenLabs rejected the API key.");
    }
    if (response.status === 429) throw ctx.fail.rateLimited("ElevenLabs API returned HTTP 429.");
    if (response.status >= 500) throw ctx.fail.providerUnavailable(`ElevenLabs API returned HTTP ${response.status}.`);
    if (response.status !== 200) throw ctx.fail.apiFailure(`ElevenLabs API returned HTTP ${response.status}.`);
    const data = response.json;
    if (!data || typeof data !== "object" || !Number.isInteger(data.character_count) || !Number.isInteger(data.character_limit)) {
      throw ctx.fail.parseFailure("ElevenLabs subscription response has an invalid shape.");
    }
    const count = data.character_count;
    const limit = data.character_limit;
    const resetsAt = data.next_character_count_reset_unix == null
      ? null
      : ctx.date.unixSeconds(data.next_character_count_reset_unix);
    const number = (value) => ctx.format.number(value, { maximumFractionDigits: 0 });
    const extraWindows = [];
    function addWindow(id, title, used, maximum) {
      if (Number.isInteger(used) && Number.isInteger(maximum) && maximum > 0) {
        extraWindows.push({
          id,
          title,
          usedPercent: ctx.pct(used, maximum),
          resetDescription: `${used} / ${maximum}`,
        });
      }
    }
    addWindow("voice-slots", "Voice slots", data.voice_slots_used, data.voice_limit);
    addWindow(
      "professional-voices",
      "Professional voices",
      data.professional_voice_slots_used,
      data.professional_voice_limit,
    );
    const tier = typeof data.tier === "string" ? data.tier.trim() : "";
    const status = typeof data.status === "string" ? data.status.trim() : "";
    const titleCase = (value) => value.replace(/_/g, " ").replace(/\b\w/g, (letter) => letter.toUpperCase());
    const loginMethod = tier
      ? `${titleCase(tier)}${status && status.toLowerCase() !== "active" ? ` · ${status}` : ""}`
      : status || null;
    return {
      primary: {
        usedPercent: limit > 0 ? ctx.pct(count, limit) : 0,
        resetsAt,
        resetDescription: `${number(count)} / ${number(limit)} credits`,
      },
      extraWindows: extraWindows.length ? extraWindows : null,
      identity: loginMethod ? { loginMethod } : null,
    };
  },
});
