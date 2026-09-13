#!/usr/bin/env python3
"""Read-only fleet placement planner.

The planner evaluates public node admission, pricing and aggregate headroom.
It never creates a VM and never calls a reservation endpoint.
"""
import argparse
import json
import math
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request


DEFAULT_NODES = (
    "node-01=https://gap.geta.team",
    "node-02=https://gap-node-02-u3.vm.elestio.app",
    "node-03=https://gap-node-03-u3.vm.elestio.app",
)
NODE_ID = re.compile(r"[A-Za-z0-9_.:-]{1,128}\Z")


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


def parse_node(value):
    node, separator, origin = value.partition("=")
    parsed = urllib.parse.urlsplit(origin)
    if not separator or not NODE_ID.fullmatch(node) or parsed.scheme != "https" or not parsed.netloc:
        raise ValueError("invalid node origin; use node-id=https://host")
    if parsed.username or parsed.password or parsed.query or parsed.fragment or parsed.path not in ("", "/"):
        raise ValueError("node origin must contain only an HTTPS host")
    return node, origin.rstrip("/")


def fetch(origin, timeout=5):
    request = urllib.request.Request(origin + "/v1/public-node", headers={"User-Agent": "GAP-Placement/1.0"})
    with urllib.request.build_opener(NoRedirect()).open(request, timeout=timeout) as response:
        if response.status != 200:
            raise ValueError("node returned HTTP %s" % response.status)
        raw = response.read(131073)
    if len(raw) > 131072:
        raise ValueError("node response too large")
    value = json.loads(raw)
    if not isinstance(value, dict):
        raise ValueError("node response is not an object")
    return value


def number(value, integer=False):
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value):
        return None
    if integer and type(value) is not int:
        return None
    return value


def evaluate(node_id, origin, value, request, error=None):
    if error:
        return {"node_id": node_id, "origin": origin, "status": "unavailable", "reasons": [error]}
    microvm = value.get("microvm") if isinstance(value.get("microvm"), dict) else {}
    headroom = microvm.get("headroom") if isinstance(microvm.get("headroom"), dict) else {}
    pricing = microvm.get("pricing") if isinstance(microvm.get("pricing"), dict) else {}
    reasons = []
    if value.get("node_id") != node_id:
        reasons.append("node_identity_mismatch")
    if microvm.get("admission_ready") is not True:
        reasons.append("admission_not_ready")
    if microvm.get("available") is not True:
        reasons.append("microvm_unavailable")
    if pricing.get("available") is not True or pricing.get("mode") != "enforced":
        reasons.append("pricing_unavailable_or_not_enforced")
    dimensions = {
        "vcpus": (number(headroom.get("vcpus")), request["vcpus"]),
        "memory_mib": (number(headroom.get("memory_mib"), integer=True), request["memory_mib"]),
        "disk_gib": (number(headroom.get("disk_gib"), integer=True), request["disk_gib"]),
    }
    for name, (available, needed) in dimensions.items():
        if available is None:
            reasons.append("headroom_%s_unknown" % name)
        elif available < needed:
            reasons.append("insufficient_%s" % name)
    if request["region"] and value.get("region") != request["region"]:
        reasons.append("region_mismatch")
    result = {
        "node_id": node_id,
        "origin": origin,
        "status": "eligible" if not reasons else "rejected",
        "reasons": reasons,
        "region": value.get("region"),
        "country": value.get("country"),
        "headroom": {name: available for name, (available, _) in dimensions.items() if available is not None},
        "pricing_version": pricing.get("tariff", {}).get("version") if isinstance(pricing.get("tariff"), dict) else None,
    }
    if result["status"] == "eligible":
        result["_score"] = (
            1 if request["region"] and value.get("region") == request["region"] else 0,
            result["headroom"].get("vcpus", 0) - request["vcpus"],
            result["headroom"].get("memory_mib", 0) - request["memory_mib"],
            result["headroom"].get("disk_gib", 0) - request["disk_gib"],
            node_id,
        )
    return result


def choose(candidates):
    eligible = [item for item in candidates if item["status"] == "eligible"]
    if not eligible:
        return None
    return max(eligible, key=lambda item: item["_score"])


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--node", action="append", dest="nodes", metavar="NODE=HTTPS_ORIGIN",
                        help="node origin; repeatable (defaults to the three GAP nodes)")
    parser.add_argument("--vcpus", type=float, default=1.0)
    parser.add_argument("--memory-mib", type=int, default=1024)
    parser.add_argument("--disk-gib", type=int, default=10)
    parser.add_argument("--region", default="", help="optional preferred region")
    args = parser.parse_args(argv)
    if not math.isfinite(args.vcpus) or args.vcpus <= 0 or args.memory_mib <= 0 or args.disk_gib <= 0:
        parser.error("resource requests must be positive")
    nodes = [parse_node(value) for value in (args.nodes or DEFAULT_NODES)]
    request = {"vcpus": args.vcpus, "memory_mib": args.memory_mib, "disk_gib": args.disk_gib, "region": args.region}
    candidates = []
    for node_id, origin in nodes:
        try:
            value = fetch(origin)
            candidates.append(evaluate(node_id, origin, value, request))
        except (OSError, ValueError, TypeError, urllib.error.URLError) as error:
            candidates.append(evaluate(node_id, origin, None, request, "node_unreachable_or_invalid_response"))
    selected = choose(candidates)
    for item in candidates:
        item.pop("_score", None)
    result = {
        "mode": "dry-run",
        "mutates": False,
        "checked_at": int(time.time()),
        "request": request,
        "selected_node_id": selected["node_id"] if selected else None,
        "candidates": candidates,
    }
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0 if selected else 2


if __name__ == "__main__":
    raise SystemExit(main())
