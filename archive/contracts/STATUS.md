# Contract protocol archive

The pre-Cloud product is frozen at Git commit `20a015e`. No historical data is
deleted, no balance is converted to Cloud credits, and no contract is settled
by this change. Do not resume a historical binary against the live Cloud store:
its settlement and migration workers could mutate the preserved records.

## Archive inventory

- `README.md`, `AGENTS.md`, `env.example` here preserve the former documentation.
- `/spec`, `/docs/rfcs`, `/contracts`, `/demo`, and commerce examples/benchmarks
  are historical references in the repository, not supported Cloud features.
- `/sdk/typescript`, `/sdk/python`, `/adapters/mcp` target the retired commerce
  API. `/sdk/realtime.js` and `/sdk/realtime-token-handler.js` remain active Cloud helpers.
- Shared protocol Rust modules and their regression tests remain in the tree
  during extraction. The production binary uses a Cloud-only HTTP boundary
  and Cloud-only startup; calling legacy library functions is not a supported
  Cloud deployment mode.

## Operator migration

Back up persistent state and secrets before upgrading. The current node keeps
agent identities, Cloud project IDs, SQLite project stores, custom-domain
mappings, realtime secrets and balances of **Cloud credits** unchanged.

Old contract/discovery/escrow/balance/events/workflow endpoints and legacy
pages return HTTP 410 on the node origin. The activity SSE feed, commerce
webhook delivery worker, expiry settlement worker and on-chain relayer startup
are removed from the production binary. No public contract withdrawal route
remains; historical financial reconciliation must be handled separately by
the operator, not by restarting a historical server on production data.

Custom domains keep serving their assigned sites and `/_gap/` aliases.
Cloud function security judges, outbound HTTP and x402 response handling are
not the old commerce gateway and remain supported.
