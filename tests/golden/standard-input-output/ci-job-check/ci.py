"""Acquire a complete export successfully before launching the processor."""
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
    processed = subprocess.run(["opaal", "source.opaal"], input=acquired.stdout,
                               capture_output=True, timeout=35)
except (OSError, subprocess.TimeoutExpired):
    sys.stderr.write("processor failed\n")
    sys.exit(1)
sys.stdout.buffer.write(processed.stdout)
sys.stderr.buffer.write(processed.stderr)
sys.exit(processed.returncode if 0 <= processed.returncode < 256 else 1)
