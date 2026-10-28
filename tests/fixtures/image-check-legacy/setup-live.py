#!/usr/bin/env python3
"""Bind independently qualified Linux Docker facts in a fresh reference copy."""
import argparse
import base64
import hashlib
import json
import os
import platform
import stat
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--docker", type=Path, default=Path("/usr/bin/docker"))
    parser.add_argument("--socket", type=Path, default=Path("/run/docker.sock"))
    args = parser.parse_args()
    profile = json.loads(Path("docker-cli-profile.json").read_text())
    if (platform.system(), platform.machine()) != ("Linux", "x86_64"):
        raise ValueError("live inspection requires Linux x86_64")
    version = Path("qualified-docker-version.txt").read_bytes()
    if version != (profile["cli_version"] + "\n").encode("ascii"):
        raise ValueError("qualified version differs from the fixed profile")
    executable = args.docker
    socket = args.socket
    if any(not path.is_absolute() for path in (executable, socket)):
        raise ValueError("qualified executable and socket require absolute paths")
    for path in (executable, socket):
        if any(parent.is_symlink() for parent in (path, *path.parents)):
            raise ValueError("qualified path has a symlink component")
    if not executable.is_file() or not os.access(executable, os.X_OK):
        raise ValueError("qualified executable is unavailable")
    if not stat.S_ISSOCK(socket.stat().st_mode):
        raise ValueError("qualified socket is unavailable")
    names = ("docker-empty", "empty-path", "sandbox-home", "sandbox-tmp")
    if any(Path(name).exists() or Path(name).is_symlink()
           for name in (*names, "live.toml", "live-tools.toml", "opaal.toml")):
        raise ValueError("setup requires fresh exclusive paths")
    digest = hashlib.sha256(executable.read_bytes()).hexdigest()
    encode = lambda path: base64.urlsafe_b64encode(bytes(path)).decode().rstrip("=")
    manifest = f'''schema_version = 1
[project]
name = "image_check"
root_module = "tasks.opaal"
required_opaal = ">=1.9.0,<2.0.0"
[paths]
root = "."
evidence = "output"
[tools.docker]
adapter = "docker"
version = "={profile['cli_version']}"
[targets.ci]
adapter = "docker"
tool = "docker"
socket = {{ encoding = "base64url-nopad", platform = "unix", value = "{encode(socket)}" }}
config_directory = "docker-empty"
api_version = "1.48"
[environments.ci]
authority = "live-authority.toml"
tool_lock = "live-tools.toml"
'''
    lock = f'''schema_version = 1
project = "image_check"
environment = "ci"
platform = "x86_64-unknown-linux-gnu"
[child_environment]
inherit = []
[[tools]]
id = "docker"
adapter = "docker"
path = {{ encoding = "base64url-nopad", platform = "unix", value = "{encode(executable)}" }}
version = "{profile['cli_version']}"
digest = "sha256:{digest}"
'''
    for name in names:
        Path(name).mkdir(mode=0o700)
    for name, text in (("live.toml", manifest), ("opaal.toml", manifest), ("live-tools.toml", lock)):
        with Path(name).open("x") as output:
            output.write(text)


if __name__ == "__main__":
    main()
