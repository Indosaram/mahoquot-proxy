#!/usr/bin/env python3
"""
Run cross-repository gates for Task 17:
- In proxy: cargo test --workspace, bash scripts/verify-catalog.sh
- In quotio-rs:
    - cargo check -p mahoquot-monitor-ui
    - cargo test --workspace
    - bun run test (in crates/monitor-ui/frontend)
    - bun run build (in crates/monitor-ui/frontend)
    - bun run typecheck (in crates/monitor-ui/frontend)
    - bun run lint (in crates/monitor-ui/frontend)
Saves evidence to:
- <proxy-root>/.omo/evidence/model-registry/task-17-gates.txt
- <desktop-root>/.omo/evidence/model-registry/task-17-gates.txt
"""

import argparse
import json
import os
import tomllib
import subprocess
import sys
from pathlib import Path

PROXY_ROOT = Path(__file__).resolve().parents[1]

def parse_roots():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--desktop-root", type=Path,
                        default=Path(os.environ.get("MAHOQUOT_DESKTOP_DIR",
                                                    PROXY_ROOT.parent / "mahoquot")))
    parser.add_argument("--dry-run", action="store_true",
                        help="Print resolved paths as JSON without executing or writing evidence")
    args = parser.parse_args()
    desktop_root = args.desktop_root.expanduser().resolve()
    for label, root, member, name in (
        ("proxy", PROXY_ROOT, "crates/gateway", "mahoquot-gateway"),
        ("desktop", desktop_root, "crates/monitor-ui", "mahoquot-monitor-ui"),
    ):
        try:
            with (root / "Cargo.toml").open("rb") as manifest:
                workspace = tomllib.load(manifest)
            with (root / member / "Cargo.toml").open("rb") as manifest:
                package = tomllib.load(manifest)
            valid = (member in workspace.get("workspace", {}).get("members", [])
                     and package.get("package", {}).get("name") == name)
        except (OSError, ValueError) as exc:
            parser.error(f"invalid {label} workspace at {root}: {exc}")
        if not valid:
            parser.error(f"invalid {label} workspace at {root}: expected {name}")
    return args, desktop_root


def main():
    args, QUOTIO_ROOT = parse_roots()
    gates = [
        {
            "name": "proxy: cargo test --workspace",
            "cwd": PROXY_ROOT,
            "cmd": ["cargo", "test", "--workspace"]
        },
        {
            "name": "proxy: bash scripts/verify-catalog.sh",
            "cwd": PROXY_ROOT,
            "cmd": ["bash", "scripts/verify-catalog.sh"]
        },
        {
            "name": "quotio-rs: cargo check -p mahoquot-monitor-ui",
            "cwd": QUOTIO_ROOT,
            "cmd": ["cargo", "check", "-p", "mahoquot-monitor-ui"]
        },
        {
            "name": "quotio-rs: cargo test --workspace",
            "cwd": QUOTIO_ROOT,
            "cmd": ["cargo", "test", "--workspace"]
        },
        {
            "name": "quotio-rs: bun run test",
            "cwd": QUOTIO_ROOT / "crates/monitor-ui/frontend",
            "cmd": ["bun", "run", "test"]
        },
        {
            "name": "quotio-rs: bun run build",
            "cwd": QUOTIO_ROOT / "crates/monitor-ui/frontend",
            "cmd": ["bun", "run", "build"]
        },
        {
            "name": "quotio-rs: bun run typecheck",
            "cwd": QUOTIO_ROOT / "crates/monitor-ui/frontend",
            "cmd": ["bun", "run", "typecheck"]
        },
        {
            "name": "quotio-rs: bun run lint",
            "cwd": QUOTIO_ROOT / "crates/monitor-ui/frontend",
            "cmd": ["bun", "run", "lint"]
        },
    ]

    plan = {
        "proxy_root": str(PROXY_ROOT),
        "desktop_root": str(QUOTIO_ROOT),
        "gates": gates,
        "evidence": [str(root / ".omo/evidence/model-registry/task-17-gates.txt")
                     for root in (PROXY_ROOT, QUOTIO_ROOT)],
    }
    if args.dry_run:
        print(json.dumps(plan, default=str))
        return 0
    print(f"Proxy root: {PROXY_ROOT}\nDesktop root: {QUOTIO_ROOT}")

    output_lines = []
    output_lines.append("=== TASK 17 CROSS-REPOSITORY GATES VERIFICATION REPORT ===")
    output_lines.append("Timestamp: " + subprocess.check_output(["date", "-u"]).decode().strip())
    output_lines.append("")

    all_ok = True

    for gate in gates:
        name = gate["name"]
        cwd = gate["cwd"]
        header = f"--- GATE: {name} (cwd: {cwd}) ---"
        print(header)
        output_lines.append(header)
        res = subprocess.run(gate["cmd"], cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        output_lines.append(f"Exit status: {res.returncode}")
        output_lines.append(res.stdout)
        output_lines.append("")
        if res.returncode != 0:
            all_ok = False
            print(f"FAILED: {name}")
        else:
            print(f"PASSED: {name}")

    output_lines.append("=== SUMMARY ===")
    if all_ok:
        output_lines.append("ALL 8 CROSS-REPOSITORY GATES PASSED (EXIT CODE 0).")
        print("\nALL 8 GATES PASSED!")
    else:
        output_lines.append("SOME GATES FAILED.")
        print("\nSOME GATES FAILED!")

    report_text = "\n".join(output_lines)

    proxy_dest = PROXY_ROOT / ".omo" / "evidence" / "model-registry" / "task-17-gates.txt"
    quotio_dest = QUOTIO_ROOT / ".omo" / "evidence" / "model-registry" / "task-17-gates.txt"

    proxy_dest.parent.mkdir(parents=True, exist_ok=True)
    quotio_dest.parent.mkdir(parents=True, exist_ok=True)

    with open(proxy_dest, "w") as f:
        f.write(report_text)
    with open(quotio_dest, "w") as f:
        f.write(report_text)

    print(f"Wrote gates report to:\n  {proxy_dest}\n  {quotio_dest}")
    return 0 if all_ok else 1


if __name__ == "__main__":
    sys.exit(main())
