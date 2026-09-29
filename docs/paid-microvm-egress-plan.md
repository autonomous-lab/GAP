# Paid MicroVM outbound access: rollout and abuse controls

This is a proposal, not the current production policy. Claimed free VMs still
use the restricted package proxy. Do not switch their QEMU `restrict=on` flag
until the containment path below is implemented and tested.

## Why a policy toggle is insufficient

- `NodeState::microvm_approval` returns the `trial` tier with
  `network_restricted=true` for a claimed free VM.
- The runner validates that pairing and stores it in VM metadata.
- `MicroVMs.command` refuses to start a `free-vm-v2` guest without the restricted
  egress proxy. The guest image itself installs proxy settings for apt, Docker
  and login shells. A policy transition therefore needs a stopped-VM upgrade,
  proxy cleanup in the guest, and a network configuration change together.
- Today QEMU user-mode NAT would put unrestricted traffic on a node's shared
  public IP. An abuse complaint or IP block could affect every tenant there.

## Proposed tiers

1. **Anonymous:** keep the existing package-registry allowlist and rate limits.
2. **Claimed with promotional credits, no verified payment:** allow useful
   development web traffic, but retain strong connection and volume budgets.
   Email verification alone is not a trustworthy abuse deposit.
3. **Paid and payment-verified:** permit general outbound connectivity through
   a separate egress IP pool, while retaining network-safety exclusions and
   per-VM volume, packet and connection-rate budgets. Grant dedicated egress IPs
   to higher-trust workloads when available. Never use the control plane or
   ingress IP as a shared unrestricted egress address.

The precise port, rate, geography and payment criteria need product and
provider-policy approval before launch. At minimum, deny host/control-plane,
RFC1918/link-local/metadata targets and direct SMTP port 25; provide a relay
for legitimate transactional mail.

## Containment before opening egress

- Move paid guests to host-controlled TAP/netns networking (or equivalent)
  with per-VM firewall identity and SNAT through a dedicated gateway/IP pool.
  Guest root must not be able to alter host egress rules.
- Record short-lived flow metadata keyed to VM, project and outbound IP;
  monitor new destinations, ports, packets, bandwidth, scan patterns and
  provider abuse notices. Avoid indiscriminate content inspection.
- Apply rate limits at the host/gateway. On a high-confidence network alert,
  block that VM's egress immediately and notify the operator and customer.
  Provide a reversible quarantine, human review and appeal path.
- Maintain a monitored abuse contact and an incident runbook. For reported
  illegal content, disable the affected public route promptly, preserve only
  necessary evidence under an applicable legal process, and escalate to the
  relevant specialist hotline/authority. Do not have general operators inspect
  suspected child sexual abuse material or copy it into ordinary tickets.
- Keep public preview/tenant domains operationally separate from GAP's control
  domain so one abusive tenant cannot readily damage the main service's
  reputation.

## Release gate

Prove attribution of every external flow to one VM; simulate port scans,
high-rate traffic and a takedown report; show that quarantine affects only that
VM; verify the node's control IP never appears as the unrestricted source; test
rollback to the restricted proxy and user notification. Only then enable a
paid-network policy on one pilot node before a fleet rollout.

References: [AWS EC2 egress controls](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/ec2-security-groups.html),
[GuardDuty network findings](https://docs.aws.amazon.com/guardduty/latest/ug/guardduty_finding-types-ec2.html),
[Cloudflare abuse-report obligations](https://developers.cloudflare.com/fundamentals/reference/report-abuse/abuse-report-obligations/).
