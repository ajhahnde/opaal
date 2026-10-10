from pathlib import Path
import sys

sys.stdout.buffer.write(Path("jobs.json").read_bytes())
sys.exit(0)
