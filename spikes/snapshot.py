"""Immutable private baseline capture and exact diff CLI."""
import argparse
import json
from pathlib import Path
from common import snapshot, diff, save_private

def main():
    p = argparse.ArgumentParser()
    sub = p.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("save"); s.add_argument("path")
    d = sub.add_parser("diff"); d.add_argument("before"); d.add_argument("after")
    a = p.parse_args()
    if a.cmd == "save":
        save_private(a.path, snapshot())
        print(f"Saved private baseline: {a.path}")
        return 0
    changes = diff(json.loads(Path(a.before).read_text()), json.loads(Path(a.after).read_text()))
    print("\n".join(changes) if changes else "No differences")
    return int(bool(changes))

if __name__ == "__main__":
    raise SystemExit(main())
