#!/usr/bin/env python3
"""Read a Truss ledger export bundle from stdin and summarize it.

This is intentionally a consumer, not a runtime plugin: it never opens the Truss
SQLite database and never writes journal entries. Feed it:

    truss --db harness.db ledger export <run-id> | python3 examples/plugin-ledger-consumer.py
"""

import argparse
import json
import sys


def status_name(value):
    if isinstance(value, str):
        return value
    if isinstance(value, dict):
        return value.get("status", "unknown")
    return "unknown"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--fail-on-unverified",
        action="store_true",
        help="exit non-zero when offline verification is not verified",
    )
    args = parser.parse_args()

    bundle = json.load(sys.stdin)
    ledger = bundle.get("ledger", {})
    verdict = bundle.get("verdict", {})
    evidence = bundle.get("evidence", {})
    verification = bundle.get("offline_verification", {})

    status = status_name(verification.get("status"))
    entries = ledger.get("entries", [])
    children = ledger.get("child_sessions", [])
    anchors = ledger.get("anchor_refs", [])

    print(f"schema: {bundle.get('schema_version', 'unknown')}")
    print(f"session: {ledger.get('session_id', 'unknown')}")
    print(f"overall: {verdict.get('overall', 'unknown')}")
    print(f"offline_verification: {status}")
    print(f"entries: {len(entries)}")
    print(f"child_sessions: {len(children)}")
    print(f"anchors: {len(anchors)}")
    print(f"checks: {len(evidence.get('checks', []))}")
    print(f"panel_results: {len(evidence.get('panel', []))}")
    print(f"self_validation: {len(evidence.get('self_validation', []))}")

    if args.fail_on_unverified and status != "verified":
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
