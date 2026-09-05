// Server-side example. Never ship GAP_PROJECT_TOKEN to a browser.
export function createRealtimeTokenHandler({
  gapUrl = "https://gap.geta.team",
  projectId,
  projectToken,
  authorize,
}) {
  if (!projectId || !projectToken || typeof authorize !== "function") {
    throw new TypeError("projectId, projectToken and authorize are required");
  }
  return async function issueToken(request) {
    const grant = await authorize(request);
    if (!grant) return new Response("Unauthorized", { status: 401 });
    if (!Array.isArray(grant.channels) || grant.channels.length === 0 ||
        !grant.channels.every(channel => typeof channel === "string") ||
        (grant.permissions && (!Array.isArray(grant.permissions) ||
          !grant.permissions.every(permission => ["subscribe", "publish"].includes(permission))))) {
      return new Response("Invalid realtime grant", { status: 403 });
    }
    const response = await fetch(`${gapUrl}/v1/cloud/projects/${encodeURIComponent(projectId)}/realtime/tokens`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${projectToken}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({
        channels: grant.channels,
        permissions: grant.permissions || ["subscribe"],
        subject: grant.subject,
      }),
    });
    return new Response(await response.text(), {
      status: response.status,
      headers: { "content-type": "application/json", "cache-control": "no-store" },
    });
  };
}
