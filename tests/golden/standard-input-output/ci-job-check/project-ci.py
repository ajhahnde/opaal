"""Map a validated successful action's domain result to a CI exit code."""
import json
from pathlib import Path
import subprocess
import sys

try:
    acquired = subprocess.run([sys.executable, "producer.py"], capture_output=True, timeout=35)
except (OSError, subprocess.TimeoutExpired):
    sys.stderr.write("acquisition failed\n")
    sys.exit(1)
if acquired.returncode != 0:
    sys.stderr.write("acquisition failed\n")
    sys.exit(acquired.returncode if 0 < acquired.returncode < 256 else 1)
try:
    base = ["--project", "opaal.toml", "--task", "sample", "--environment", "ci"]
    subprocess.run(["opaal", "check", *base], capture_output=True, check=True, timeout=35)
    subprocess.run(["opaal", "plan", *base, "--expires-in", "900s", "--out", "ci.plan.json"],
                   capture_output=True, check=True, timeout=35)
    plan = json.loads(Path("ci.plan.json").read_bytes())
    run_id = "0123456789abcdef0123456789abcdef"
    result = subprocess.run(["opaal", "execute", "--plan", "ci.plan.json", "--accept", plan["digest"],
                             "--run-id", run_id, "--journal", "ci.run.jsonl", "--receipt-out", "ci.receipt.json"],
                            input=acquired.stdout, capture_output=True, timeout=35)
    receipt = json.loads(Path("ci.receipt.json").read_bytes())
    if (result.returncode != 0 or result.stderr or receipt["schema"] != "opaal.execution-receipt.v1"
            or receipt["schema_version"] != 1 or receipt["run_id"] != run_id
            or receipt["plan_digest"] != plan["digest"] or receipt["primary"]["class"] != "success"
            or receipt["primary"]["value_digest"] is not None or receipt["secondary"]
            or receipt["journal_state"] != "complete" or receipt["omitted_secondary_count"] != 0):
        raise ValueError("execution evidence is unsuccessful")
    assessment = json.loads(result.stdout)
    if type(assessment["passed"]) is not bool:
        raise ValueError("assessment must contain a Bool")
except (OSError, subprocess.SubprocessError, ValueError, KeyError, TypeError):
    sys.stderr.write("project check failed; inspect the journal and receipt\n")
    sys.exit(1)
sys.stdout.buffer.write(result.stdout)
sys.exit(0 if assessment["passed"] else 1)
