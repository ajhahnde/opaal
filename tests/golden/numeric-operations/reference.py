"""Independent scalar report calculation for the offline examples."""
import argparse
import json
import math

parser = argparse.ArgumentParser()
parser.add_argument("--text", action="store_true")
parser.add_argument("--caught", action="store_true")
args = parser.parse_args()
report = {
    "percent": min(max(float(math.floor(37.0 / 40.0 * 100.0 + 0.5)), 0.0), 100.0),
    "delta": abs(-3), "scale": math.sqrt(81.0), "low": min(4, 9),
    "high": max(4, 9), "floor": float(math.floor(2.7)), "ceil": float(math.ceil(2.1)),
}
if args.caught:
    try:
        math.sqrt(-1.0)
    except ValueError:
        print("fallback=0.0")
elif args.text:
    print("percent={percent} delta={delta} scale={scale}".format(**report))
else:
    print(json.dumps(report, sort_keys=True, separators=(",", ":")), end="")
