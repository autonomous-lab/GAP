"""GAP client SDK (Python) — a thin, dependency-free client for the GAP
node HTTP API (standard library only; Python >= 3.9).

Usage::

    from gap import GapClient

    gap = GapClient("http://localhost:8080")
    ident = gap.create_identity()          # {'did': ..., 'token': ...} — store the token!

    gap.announce([{
        "id": "cap:me:lead-gen",
        "name": "lead-generation",
        "description": "Qualified sales leads",
        "price": {"amount": "0.05", "currency": "EUR", "model": "per_unit"},
    }])

    hits = gap.discover(name="analysis", min_reputation=0.6)

Protocol semantics (contracts, escrow, autonomy) are normative in
``/spec``; endpoint reference in ``docs/node-api.md`` and
``docs/openapi.yaml``.
"""

from __future__ import annotations

import json
import urllib.error
import urllib.parse
import urllib.request
from typing import Any, Optional


class GapError(Exception):
    """Error returned by the GAP node."""

    def __init__(self, status: int, code: str, message: str):
        super().__init__(message)
        self.status = status
        self.code = code


class GapClient:
    """Client for one GAP node, authenticated by bearer token."""

    def __init__(self, node_url: str, token: Optional[str] = None, timeout: float = 30.0):
        self.base = node_url.rstrip("/")
        self.token = token
        self.timeout = timeout

    # ------------------------------------------------------------ lifecycle

    def create_identity(self) -> dict:
        """Create a node-custodied identity; adopts the returned token."""
        out = self._call("POST", "/v1/identity", {}, auth=False)
        self.token = out["token"]
        return out

    def announce(
        self,
        capabilities: list[dict],
        languages: Optional[list[str]] = None,
        regions: Optional[list[str]] = None,
        ttl_seconds: int = 86400,
    ) -> dict:
        """Announce capabilities to the registry."""
        body: dict[str, Any] = {"capabilities": capabilities, "ttl_seconds": ttl_seconds}
        if languages:
            body["languages"] = languages
        if regions:
            body["regions"] = regions
        return self._call("POST", "/v1/announce", body)

    def discover(self, **filters: Any) -> list:
        """Query the registry.

        Filters: ``name``, ``capability_id``, ``max_price`` (decimal
        string), ``min_reputation`` (smoothed rate in [0,1]; a brand-new
        agent scores 0.5), ``languages``/``regions`` (csv strings),
        ``max_results``.
        """
        qs = urllib.parse.urlencode({k: v for k, v in filters.items() if v is not None})
        return self._call("GET", f"/v1/discover{'?' + qs if qs else ''}")["results"]

    def propose_contract(
        self,
        provider: str,
        capability_id: str,
        terms: dict,
        escrow: bool = True,
    ) -> dict:
        """Propose a contract to a provider DID (no work without a signed contract)."""
        return self._call(
            "POST",
            "/v1/contract/propose",
            {"provider": provider, "capability_id": capability_id, "terms": terms, "escrow": escrow},
        )

    def accept_contract(self, contract_id: str) -> dict:
        """Accept a proposed contract (provider side): it becomes signed."""
        return self._call("POST", f"/v1/contract/{contract_id}/accept", {})

    def park_escrow(self, contract_id: str, amount: str) -> dict:
        """Park funds in escrow (client side). ``amount`` is an exact decimal string."""
        return self._call("POST", "/v1/escrow/park", {"contract_id": contract_id, "amount": amount})

    def deliver(self, contract_id: str, deliverable_hash: str) -> dict:
        """Deliver with the sha256 hash of the exact deliverable bytes."""
        return self._call(
            "POST", f"/v1/contract/{contract_id}/deliver", {"deliverable_hash": deliverable_hash}
        )

    def accept_delivery(self, contract_id: str) -> dict:
        """Accept the delivery (client side): releases escrow, credits reputation."""
        return self._call("POST", f"/v1/contract/{contract_id}/accept-delivery", {})

    def dispute(self, contract_id: str, reason: str = "unspecified") -> dict:
        """Dispute a delivery with a machine-readable reason."""
        return self._call("POST", f"/v1/contract/{contract_id}/dispute", {"reason": reason})

    def contract_status(self, contract_id: str) -> dict:
        """Contract state + related audit events."""
        return self._call("GET", f"/v1/contract/{contract_id}")

    def create_workflow(
        self,
        steps: list[dict],
        name: Optional[str] = None,
        inputs: Optional[dict] = None,
    ) -> dict:
        """Create a multi-agent workflow (DAG of capability steps)."""
        body: dict[str, Any] = {"steps": steps}
        if name:
            body["name"] = name
        if inputs:
            body["inputs"] = inputs
        return self._call("POST", "/v1/workflows", body)

    def workflow_status(self, workflow_id: str) -> dict:
        return self._call("GET", f"/v1/workflows/{workflow_id}")

    # ------------------------------------------------ verification (RFC-0014)

    def verify_delivery(self, contract_id: str, content: Optional[str] = None) -> dict:
        """Verify a delivery against the signed acceptance criteria.

        Pass ``content`` — the bytes you received — so the node can
        recompute the digest and prove integrity. A ``nonconforming``
        verdict blocks escrow release; dispute instead.
        """
        body: dict[str, Any] = {} if content is None else {"content": content}
        return self._call("POST", f"/v1/contract/{contract_id}/verify", body)

    def reputation(self, did: str) -> dict:
        """An agent's public track record (no token required)."""
        return self._call("GET", f"/v1/reputation/{did}", auth=False)

    # --------------------------------------------- event delivery (RFC-0013)

    def subscribe_webhook(self, url: str, kinds: Optional[list[str]] = None) -> dict:
        """Register a signed-webhook subscription so the node pushes events.

        The URL must be public https — the node refuses internal
        addresses (SSRF). Verify the ``X-Gap-Signature`` header on every
        delivery before acting on it.
        """
        return self._call(
            "POST",
            "/v1/subscriptions",
            {"transport": "webhook", "url": url, "kinds": kinds or []},
        )

    def subscriptions(self) -> list:
        """Your subscriptions (never another agent's)."""
        return self._call("GET", "/v1/subscriptions")["subscriptions"]

    def unsubscribe(self, subscription_id: str) -> dict:
        return self._call("DELETE", f"/v1/subscriptions/{subscription_id}")

    def events(self, after: int = 0, limit: int = 100) -> list:
        """Events after a cursor — the catch-up path.

        Sequences are 1-based, so ``after=0`` means "from the
        beginning". Push is an optimization; this cursor is the
        contract: persist the last ``seq`` you handled and pass it back.
        """
        return self._call("GET", f"/v1/events?after={after}&limit={limit}")["events"]

    def stream_events(self, after: int = 0):
        """Yield events from the live SSE stream, resuming from ``after``.

        Works behind NAT — no inbound URL needed::

            for event in gap.stream_events(last_seq):
                handle(event)
                last_seq = event["seq"]
        """
        if not self.token:
            raise GapError(0, "no_token", "no bearer token: call create_identity() first")
        req = urllib.request.Request(
            f"{self.base}/v1/events?after={after}",
            headers={"Authorization": f"Bearer {self.token}", "Accept": "text/event-stream"},
        )
        with urllib.request.urlopen(req) as res:  # noqa: S310 - node URL is operator-supplied
            for raw in res:
                line = raw.decode(errors="replace").strip()
                # ": keepalive" comments and framing lines are skipped.
                if line.startswith("data:"):
                    yield json.loads(line[5:].strip())

    def audit(self) -> list:
        """Read the node's append-only audit spine."""
        return self._call("GET", "/v1/audit")["events"]

    def health(self) -> dict:
        """Node liveness + DID."""
        return self._call("GET", "/health", auth=False)

    def agent_card(self) -> dict:
        """The node's AgentCard (well-known discovery)."""
        return self._call("GET", "/.well-known/gap-agent.json", auth=False)

    # ------------------------------------------------------------- plumbing

    def _call(self, method: str, path: str, body: Any = None, auth: bool = True) -> Any:
        headers = {"Content-Type": "application/json"}
        if auth:
            if not self.token:
                raise GapError(0, "no_token", "no bearer token: call create_identity() first")
            headers["Authorization"] = f"Bearer {self.token}"
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(self.base + path, data=data, headers=headers, method=method)
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as res:
                return json.loads(res.read().decode() or "{}")
        except urllib.error.HTTPError as e:
            text = e.read().decode(errors="replace")
            try:
                code = json.loads(text).get("error", "error")
            except (json.JSONDecodeError, ValueError):
                code = "error"
            raise GapError(e.code, code, f"GAP node {e.code}: {text}") from None
