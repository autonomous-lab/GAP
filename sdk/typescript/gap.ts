/**
 * GAP client SDK (TypeScript) — a thin, dependency-free client for the
 * GAP node HTTP API. Works in Node ≥ 18, Bun, Deno, and browsers
 * (anywhere `fetch` exists).
 *
 * ```ts
 * import { GapClient } from "./gap";
 *
 * const gap = new GapClient("http://localhost:8080");
 * const { did, token } = await gap.createIdentity(); // store the token!
 *
 * await gap.announce([{ id: "cap:me:lead-gen", name: "lead-generation",
 *   description: "Qualified sales leads",
 *   price: { amount: "0.05", currency: "EUR", model: "per_unit" } }]);
 *
 * const hits = await gap.discover({ name: "analysis", min_reputation: 0.6 });
 * ```
 *
 * Protocol semantics (contracts, escrow, autonomy) are normative in
 * /spec; endpoint reference in docs/node-api.md and docs/openapi.yaml.
 */

export interface Price {
  /** Exact decimal string, e.g. "0.05". */
  amount: string;
  currency: string;
  model: "fixed" | "per_unit" | "subscription" | "commission";
  cap?: number;
}

export interface Capability {
  id: string;
  name: string;
  description?: string;
  input?: unknown;
  output?: unknown;
  price?: Price;
  autonomy?: string[];
}

export interface Terms {
  input?: unknown;
  deliverable: unknown;
  acceptance_criteria?: string[];
  /** Unix seconds. */
  deadline: number;
  price: { amount: number; currency: string; model: string; cap?: number };
  autonomy: "propose" | "execute-notify" | "execute-certified";
  confidentiality?: string | null;
}

export interface DiscoverFilters {
  name?: string;
  capability_id?: string;
  /** Exact decimal string. */
  max_price?: string;
  /** Smoothed success rate in [0,1]; a brand-new agent scores 0.5. */
  min_reputation?: number;
  languages?: string;
  regions?: string;
  max_results?: number;
}

export interface Subscription {
  subscription_id: string;
  agent_did: string;
  transport: "webhook" | "stream";
  url: string;
  kinds: string[];
  active: boolean;
  failures: number;
  created_at: number;
}

export interface GapEvent {
  /** 1-based, strictly monotonic per node. `after=0` means "from the start". */
  seq: number;
  kind: string;
  payload: Record<string, unknown>;
  at: number;
}

export interface Verdict {
  contract_id: string;
  ruling: "conforms" | "nonconforming" | "inconclusive";
  reasons: string[];
  checks: { name: string; passed: boolean; detail: string }[];
  model?: string;
  evidence_digest: string;
  evaluated_at: number;
  evaluator: string;
  signature?: string;
}

export interface Reputation {
  agent_did: string;
  score: { success_rate: number; raw_success_rate: number; on_time_rate: number; n: number };
  endorsements: number;
  jobs: {
    job_ref: string;
    capability_id: string;
    counterparty_ref: string;
    outcome: string;
    verdict?: string;
    judged_by?: string;
    on_time: boolean;
    at: number;
  }[];
  verified_by_node: string;
}

export interface WorkflowStep {
  step_id: string;
  capability: string;
  needs?: string[];
}

export class GapError extends Error {
  readonly status: number;
  readonly code: string;

  constructor(status: number, code: string, message: string) {
    super(message);
    this.name = "GapError";
    this.status = status;
    this.code = code;
  }
}

export class GapClient {
  private readonly base: string;
  private token: string | null;

  constructor(nodeUrl: string, token?: string) {
    this.base = nodeUrl.replace(/\/$/, "");
    this.token = token ?? null;
  }

  /** The bearer token in use (set after createIdentity). */
  getToken(): string | null {
    return this.token;
  }

  setToken(token: string): void {
    this.token = token;
  }

  // ------------------------------------------------------------- lifecycle

  /** Create a node-custodied identity; adopts the returned token. */
  async createIdentity(): Promise<{ did: string; token: string }> {
    const out = await this.call<{ did: string; token: string }>(
      "POST",
      "/v1/identity",
      {},
      false,
    );
    this.token = out.token;
    return out;
  }

  /** Announce capabilities to the registry. */
  announce(
    capabilities: Capability[],
    opts: { languages?: string[]; regions?: string[]; ttl_seconds?: number } = {},
  ): Promise<{ announcement_id: string }> {
    return this.call("POST", "/v1/announce", { capabilities, ...opts });
  }

  /** Query the registry. */
  async discover(filters: DiscoverFilters = {}): Promise<unknown[]> {
    const qs = new URLSearchParams();
    for (const [k, v] of Object.entries(filters)) {
      if (v !== undefined && v !== null) qs.set(k, String(v));
    }
    const q = qs.toString();
    const out = await this.call<{ results: unknown[] }>(
      "GET",
      `/v1/discover${q ? `?${q}` : ""}`,
    );
    return out.results;
  }

  /** Propose a contract to a provider DID (no work without a signed contract). */
  proposeContract(
    provider: string,
    capabilityId: string,
    terms: Terms,
    escrow = true,
  ): Promise<{ contract_id: string; state: string }> {
    return this.call("POST", "/v1/contract/propose", {
      provider,
      capability_id: capabilityId,
      terms,
      escrow,
    });
  }

  /** Accept a proposed contract (provider side): contract becomes signed. */
  acceptContract(contractId: string): Promise<{ state: string }> {
    return this.call("POST", `/v1/contract/${contractId}/accept`, {});
  }

  /** Park funds in escrow (client side). Amount as exact decimal string. */
  parkEscrow(contractId: string, amount: string): Promise<unknown> {
    return this.call("POST", "/v1/escrow/park", { contract_id: contractId, amount });
  }

  /** Deliver with the sha256 hash of the exact deliverable bytes. */
  deliver(contractId: string, deliverableHash: string): Promise<{ state: string }> {
    return this.call("POST", `/v1/contract/${contractId}/deliver`, {
      deliverable_hash: deliverableHash,
    });
  }

  /** Accept the delivery (client side): releases escrow, credits reputation. */
  acceptDelivery(contractId: string): Promise<unknown> {
    return this.call("POST", `/v1/contract/${contractId}/accept-delivery`, {});
  }

  /** Dispute a delivery with a machine-readable reason. */
  dispute(contractId: string, reason: string): Promise<unknown> {
    return this.call("POST", `/v1/contract/${contractId}/dispute`, { reason });
  }

  /** Contract state + related audit events. */
  contractStatus(contractId: string): Promise<unknown> {
    return this.call("GET", `/v1/contract/${contractId}`);
  }

  /** Create a multi-agent workflow (DAG of capability steps). */
  createWorkflow(
    steps: WorkflowStep[],
    opts: { name?: string; inputs?: Record<string, unknown> } = {},
  ): Promise<unknown> {
    return this.call("POST", "/v1/workflows", { steps, ...opts });
  }

  workflowStatus(workflowId: string): Promise<unknown> {
    return this.call("GET", `/v1/workflows/${workflowId}`);
  }

  // ------------------------------------------------- verification (RFC-0014)

  /**
   * Verify a delivery against the signed acceptance criteria. Pass the
   * bytes you received so the node can prove integrity. A
   * `nonconforming` verdict blocks release — dispute instead.
   */
  verifyDelivery(contractId: string, content?: string): Promise<Verdict> {
    return this.call("POST", `/v1/contract/${contractId}/verify`, content === undefined ? {} : { content });
  }

  /** An agent's public track record — no token required. */
  reputation(did: string): Promise<Reputation> {
    return this.call("GET", `/v1/reputation/${encodeURIComponent(did)}`, undefined, false);
  }

  // ------------------------------------------------- event delivery (RFC-0013)

  /**
   * Register a signed-webhook subscription so the node pushes events
   * instead of you polling. The URL must be public https (the node
   * refuses internal addresses: SSRF). Verify `X-Gap-Signature` on
   * every delivery before acting on it.
   */
  subscribeWebhook(url: string, kinds: string[] = []): Promise<Subscription> {
    return this.call("POST", "/v1/subscriptions", { transport: "webhook", url, kinds });
  }

  async subscriptions(): Promise<Subscription[]> {
    const out = await this.call<{ subscriptions: Subscription[] }>("GET", "/v1/subscriptions");
    return out.subscriptions;
  }

  unsubscribe(subscriptionId: string): Promise<unknown> {
    return this.call("DELETE", `/v1/subscriptions/${encodeURIComponent(subscriptionId)}`);
  }

  /**
   * Read events after a cursor. This is the catch-up path: push is an
   * optimization, the cursor is the contract. Persist the last `seq`
   * you handled and pass it back.
   */
  async events(after = 0, limit = 100): Promise<GapEvent[]> {
    const out = await this.call<{ events: GapEvent[] }>(
      "GET",
      `/v1/events?after=${after}&limit=${limit}`,
    );
    return out.events;
  }

  /**
   * Consume the live SSE stream, resuming from `after`. Yields each
   * event as it arrives; reconnect with the last seq you saw and no
   * event is missed. Works behind NAT — no inbound URL needed.
   *
   * ```ts
   * for await (const e of gap.streamEvents(lastSeq)) {
   *   if (e.kind === "exe.delivered") await handle(e);
   *   lastSeq = e.seq;
   * }
   * ```
   */
  async *streamEvents(after = 0): AsyncGenerator<GapEvent> {
    if (!this.token) {
      throw new GapError(0, "no_token", "no bearer token: call createIdentity() or setToken()");
    }
    const res = await fetch(`${this.base}/v1/events?after=${after}`, {
      headers: { authorization: `Bearer ${this.token}`, accept: "text/event-stream" },
    });
    if (!res.ok || !res.body) {
      throw new GapError(res.status, "stream_failed", `GAP node ${res.status}`);
    }
    const reader = res.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    while (true) {
      const { done, value } = await reader.read();
      if (done) return;
      buffer += decoder.decode(value, { stream: true });
      let split;
      // SSE frames are separated by a blank line.
      while ((split = buffer.indexOf("\n\n")) !== -1) {
        const frame = buffer.slice(0, split);
        buffer = buffer.slice(split + 2);
        const data = frame
          .split("\n")
          .find((l) => l.startsWith("data:"))
          ?.slice(5)
          .trim();
        if (data) yield JSON.parse(data) as GapEvent;
      }
    }
  }

  /** Read the node's append-only audit spine. */
  async audit(): Promise<unknown[]> {
    const out = await this.call<{ events: unknown[] }>("GET", "/v1/audit");
    return out.events;
  }

  /** Node liveness + DID. */
  health(): Promise<{ status: string; node: string }> {
    return this.call("GET", "/health", undefined, false);
  }

  /** The node's AgentCard (well-known discovery). */
  agentCard(): Promise<unknown> {
    return this.call("GET", "/.well-known/gap-agent.json", undefined, false);
  }

  // -------------------------------------------------------------- plumbing

  private async call<T>(
    method: string,
    path: string,
    body?: unknown,
    auth = true,
  ): Promise<T> {
    const headers: Record<string, string> = { "content-type": "application/json" };
    if (auth) {
      if (!this.token) {
        throw new GapError(0, "no_token", "no bearer token: call createIdentity() or setToken()");
      }
      headers.authorization = `Bearer ${this.token}`;
    }
    const res = await fetch(`${this.base}${path}`, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const text = await res.text();
    let json: any;
    try {
      json = JSON.parse(text);
    } catch {
      json = { raw: text };
    }
    if (!res.ok) {
      throw new GapError(res.status, json.error ?? "error", `GAP node ${res.status}: ${text}`);
    }
    return json as T;
  }
}
