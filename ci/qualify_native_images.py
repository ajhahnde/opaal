"""Qualify native admission, snapshot faults and bounded descriptor execution."""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import time

if __package__:
    from .qualify_data_processing import command, require
    from .qualify_native_preparation import case_results, host_triple, injected_results, minimal_elf
    from .qualify_native_evidence import prepare_project
else:
    from qualify_data_processing import command, require
    from qualify_native_preparation import case_results, host_triple, injected_results, minimal_elf
    from qualify_native_evidence import prepare_project


PROCESS_CASES = (
    "argv-count-including-zero-exact", "argv-count-including-zero-first-excess",
    "argv-bytes-including-nuls-exact", "argv-bytes-including-nuls-first-excess",
    "argument-bytes-including-nul-exact", "argument-bytes-including-nul-first-excess",
    "environment-encoded-bytes-exact", "environment-encoded-bytes-first-excess",
    "argument-interior-nul", "missing-effect", "missing-grant", "enforced-grant",
    "shared-acquisition-launch-deadline", "shared-eight-attempts", "ninth-attempt-refuses",
    "pure-call", "module-initializer-call", "default-embedding-call",
    "maintained-probe-shares-budget",
)

FRONTEND_CASES = ("image-bytes-exact", "image-bytes-first-excess", "source-symlink",
                  "source-fifo", "source-directory")
LAUNCH_FAILURE_CASES = ("failed-spawn", "missing-interpreter")
FINISH_CASES = ("process-finish-failure", "process-cancel-and-finish-failure")
LINUX_HOSTS = {"x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"}


def finish_launch_census(calls, pid):
    launches = re.findall(r'^(\d+)\s+execve\("([^"]+)", (\[[^\n]*?\]), ([^\n]*)', calls, re.M)
    require(len(launches) == len(re.findall(r'\bexecve\(', calls)) and "execveat(" not in calls,
            "finish observer contains an unknown launch")
    for child, _, _, result in launches:
        require(re.search(r'= 0$', result) or ("<unfinished ...>" in result and re.search(
            rf'^{child}\s+<\.\.\. execve resumed>\)\s+= 0$', calls, re.M)),
            "finish observer contains a failed launch")
    native = [row for row in launches if int(row[0]) == pid]
    require(len(native) == 1 and re.fullmatch(r'/proc/self/fd/[0-9]+', native[0][1]),
            "finish native child launch differs")
    workers = [row for row in launches[1:] if int(row[0]) != pid]
    require(len(workers) <= 1 and len(launches) == 2 + len(workers)
            and int(launches[0][0]) != pid and not launches[0][1].startswith("/proc/self/fd/")
            and all(row[2] == '["opaal", "--opaal-standard-host-worker"]'
                    and re.fullmatch(r'/proc/self/fd/[0-9]+', row[1])
                    and row[0] != launches[0][0] for row in workers)
            and not re.search(r'execve\("[^"\n]*/checker"', calls),
            "finish observer launch census differs")
    for child, _, _, _ in workers:
        require(not Path(f"/proc/{child}").exists()
                and re.search(rf'wait4\({child},.* = {child}', calls), "finish summary worker was not reaped")
    return {"native_launches": 1, "summary_workers": len(workers)}


def finish_cases(binary, inputs, output):
    """Fail the real supervisor's first group check, preserving its primary."""
    output.mkdir()
    tracer, compiler = shutil.which("strace"), shutil.which("cc")
    require(tracer is not None and compiler is not None, "finish qualification requires strace and cc")
    image = output / "finish"
    built = command([compiler, "-O2", "-Wall", "-Wextra", "-Werror", inputs / "finish.c", "-o", image],
                    output, {"PATH": "/usr/bin:/bin"})
    (output / "build.stdout").write_bytes(built.stdout)
    (output / "build.stderr").write_bytes(built.stderr)
    require(built.returncode == 0 and not built.stderr, "finish observer compilation failed")
    observations, traces = [], {}
    for name in FINISH_CASES:
        work = output / name
        digest = prepare_project(binary, work, native_image=image.read_bytes())
        requested = name == "process-cancel-and-finish-failure"
        if requested:
            (work / "request-cancel").write_bytes(b"requested")
        trace = work / "syscalls"
        completed = command([tracer, "-f", "-qq", "-s", "256", "-e",
                             "trace=kill,tgkill,wait4,execve,execveat", "-e", "inject=kill:error=EIO:when=1",
                             "-o", trace, binary, "task", "run", "--plan", "plan.json", "--accept", digest,
                             "--require-true", "--journal", "run.jsonl", "--receipt-out", "receipt.json",
                             "--audit-out", "audit.json"], work, {})
        (work / "stdout").write_bytes(completed.stdout)
        (work / "stderr").write_bytes(completed.stderr)
        calls = trace.read_text()
        pid = int((work / "child.pid").read_text())
        injected = [line for line in calls.splitlines() if "(INJECTED)" in line]
        require(len(injected) == 1 and re.search(rf'kill\(-{pid}, 0\)\s+= -1 EIO', injected[0]),
                "finish fault did not hit the owned group check")
        census = finish_launch_census(calls, pid)
        require(not Path(f"/proc/{pid}").exists() and re.search(rf'wait4\({pid},.* = {pid}', calls),
                "finish observer child was not reaped")
        receipt = json.loads((work / "receipt.json").read_bytes())
        audit = json.loads((work / "audit.json").read_bytes())
        primary = receipt["primary"]
        require(primary["class"] == ("cancelled" if requested else "error")
                and primary["cancel_reason"] == ("requested" if requested else None),
                "finish fault replaced the primary cause")
        require(audit["primary"] == primary and audit["completeness"] == "complete"
                and receipt["journal_state"] == "complete",
                "finish receipt/audit primary differs")
        events = [json.loads(line) for line in (work / "run.jsonl").read_bytes().splitlines()]
        effects = [event for event in events if event["kind"] == "effect-before"]
        require(len(effects) == 1 and effects[0]["payload"]["effect"] == "process.run",
                "finish observer probed or retried the native tool")
        require(not completed.stdout, "finish failure unexpectedly reported success")
        observations.append({"name": name, "observed": {"exit": completed.returncode,
                             "passed": completed.returncode == 0, "partial": primary["partial"]},
                             "route": "candidate-cli-with-real-native-child-and-injected-group-check"})
        traces[name] = {"trace_sha256": hashlib.sha256(trace.read_bytes()).hexdigest(),
                        "primary_class": primary["class"], "child_reaped": True, **census}
    return observations, {"native_image_sha256": hashlib.sha256(image.read_bytes()).hexdigest(),
                          "source_sha256": hashlib.sha256((inputs / "finish.c").read_bytes()).hexdigest(),
                          "tracer_sha256": hashlib.sha256(Path(tracer).read_bytes()).hexdigest(),
                          "compiler_sha256": hashlib.sha256(Path(compiler).read_bytes()).hexdigest(),
                          "cases": traces}

ACQUISITION_CASES = (
    "copy-requested", "copy-deadline", "sealed-hash-requested", "sealed-hash-deadline",
    "elf-validation-requested", "elf-validation-deadline", "before-launch-requested",
    "before-launch-deadline", "snapshot-original-stale-before-acquisition",
    "snapshot-preseal-mutation-mismatches-lock", "snapshot-post-seal-read-failure",
)


def acquisition_trace_observations(trace):
    starts = list(re.finditer(r'write\(1(?:<[^>]*>)?, "OPAAL_ACQUISITION_START ([a-z-]+)\\n",', trace))
    require(len(starts) == len(ACQUISITION_CASES)
            and {match[1] for match in starts} == set(ACQUISITION_CASES),
            "acquisition trace case set differs")
    require(len(re.findall(r'\bexecve\(', trace)) == 1 and "execveat(" not in trace
            and "execve(" in trace[:starts[0].start()], "acquisition trace execution census differs")
    observations = {}
    for index, start in enumerate(starts):
        name = start[1]
        block = trace[start.end():starts[index + 1].start() if index + 1 < len(starts) else len(trace)]
        require(not re.search(r'\bexecve(?:at)?\(', block), "acquisition refusal attempted execution")
        reads = [int(value) for value in re.findall(r'pread64\([^\n]*?/memfd:opaal-native[^\n]*? = (-?\d+)', block)]
        if name.startswith("copy-") or name == "snapshot-original-stale-before-acquisition":
            expected, seals = [], 0
        elif name.startswith("sealed-hash-"):
            expected, seals = [65536], 1
        elif name == "snapshot-post-seal-read-failure":
            expected, seals = [-1], 1
        else:
            expected, seals = [65536, 65536, 0], 1
        # AArch64 also uses pread64 for the ELF and program headers.
        headers = name.startswith(("elf-validation-", "before-launch-")) and reads == expected + [64, 56]
        require(reads == expected or headers, "acquisition snapshot read boundary differs: " + name)
        for operation in ("F_ADD_SEALS", "F_GET_SEALS"):
            calls = [line for line in block.splitlines() if "/memfd:opaal-native" in line and operation in line]
            require(len(calls) == seals and all(" = -1 " not in line for line in calls),
                    "acquisition seal boundary differs: " + name)
            require(all(all(flag in line for flag in ("F_SEAL_SEAL", "F_SEAL_SHRINK", "F_SEAL_GROW", "F_SEAL_WRITE"))
                        for line in calls), "acquisition seals differ: " + name)
        observations[name] = {"snapshot_read_results": reads, "verified_seals": bool(seals)}
    return observations

POSIX_CASES = (
    "static-sealed-execution", "dynamic-sealed-execution", "post-seal-source-inplace-mutation",
    "post-seal-path-replacement", "snapshot-descriptors-close-on-exec", "empty-argument-list",
    "original-nonexec", "original-setuid", "original-setgid", "image-first-excess",
    "image-growth-during-copy", "literal-argv-environment-cwd", "nonzero-status-is-data",
    "signal-status-is-data", "stdout-exact", "stdout-first-excess", "stderr-exact",
    "stderr-first-excess", "running-child-requested", "running-child-deadline",
    "term-resistant-group", "macos-live-refusal",
)

SNAPSHOT_FAULTS = {
    "effective-access-unavailable": ("faccessat2:error=ENOSYS:when=1", r"faccessat2\("),
    "capability-read-failure": ("fgetxattr:error=EIO:when=1", r"fgetxattr\("),
    "snapshot-create-failure": ("memfd_create:error=ENOSYS:when=1", r"memfd_create\("),
    "snapshot-mode-failure": ("fchmod:error=EPERM:when=1", r"fchmod\("),
    "snapshot-copy-read-failure": ("pread64:error=EIO:when=1", r"pread64\("),
    "snapshot-copy-write-failure": ("write:error=ENOSPC:when=1", r"write\("),
    "snapshot-add-seals-failure": ("fcntl:error=EIO:when=1", r"fcntl\(.*F_ADD_SEALS"),
    "snapshot-get-seals-failure": ("fcntl:error=EIO:when=2", r"fcntl\(.*F_GET_SEALS"),
    "snapshot-missing-seals": ("fcntl:retval=0:when=2", r"fcntl\(.*F_GET_SEALS"),
}


def permission_cases(test_binary, inputs, output, cases, host):
    records = []
    inventory = "sha256:" + hashlib.sha256((inputs / "image-cases.json").read_bytes()).hexdigest()
    for name, test in (("original-acl-denies-effective-user", "image_original_acl_refuses"),
                       ("original-file-capability", "image_original_capabilities_refuse"),
                       ("original-noexec-mount", "image_original_noexec_mount_refuses")):
        work = output / name
        work.mkdir()
        environment = {"PATH": "/usr/sbin:/usr/bin:/sbin:/bin"}
        arguments = [test_binary, test, "--exact", "--ignored", "--nocapture", "--test-threads=1"]
        if name == "original-acl-denies-effective-user" and os.geteuid() == 0:
            arguments = ["/usr/bin/setpriv", "--bounding-set=-dac_override,-dac_read_search", *arguments]
        elif name == "original-file-capability":
            arguments = ["/usr/bin/unshare", "--user", "--map-root-user", *arguments]
        elif name == "original-noexec-mount":
            mount = work / "noexec"
            mount.mkdir()
            environment["OPAAL_NATIVE_NOEXEC_ROOT"] = str(mount)
            arguments = ["/usr/bin/unshare", "--user", "--map-root-user", "--mount", "/bin/sh", "-c",
                         'mount -t tmpfs -o noexec tmpfs "$OPAAL_NATIVE_NOEXEC_ROOT" && exec "$@"',
                         "native-noexec", *arguments]
        completed = command(arguments, work, environment)
        (work / "stdout").write_bytes(completed.stdout)
        (work / "stderr").write_bytes(completed.stderr)
        records.extend(injected_results(completed, cases, [name], host, inventory,
                                        marker=b"OPAAL_IMAGE_CASE ", label="original permission"))
    return records


def format_image(row, host):
    image = bytearray.fromhex(row["input"]["image_hex"])
    if len(image) >= 20:
        machine = 183 if host == "aarch64-unknown-linux-gnu" else 62
        if row["name"] == "foreign-machine":
            machine = 62 if machine == 183 else 183
        image[18:20] = machine.to_bytes(2, "little")
    return bytes(image)


def snapshot_fault_case(test_binary, tracer, work, name, injection):
    """Attach after test startup, so loader/harness calls cannot consume the fault."""
    work.mkdir()
    trace = work / "syscalls"
    environment = {"PATH": "/usr/bin:/bin", "OPAAL_NATIVE_FAULT_ROOT": str(work),
                   "OPAAL_NATIVE_FAULT_CASE": name}
    test = subprocess.Popen([str(test_binary), "image_snapshot_syscall_fault", "--exact", "--ignored",
                             "--nocapture", "--test-threads=1"], cwd=work, env=environment,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    observer = None
    try:
        deadline = time.monotonic() + 5
        while not (work / "ready").exists():
            require(test.poll() is None and time.monotonic() < deadline, "snapshot fault test did not become ready")
            time.sleep(0.01)
        threads = [path for path in Path(f"/proc/{test.pid}/task").iterdir() if path.name != str(test.pid)]
        require(len(threads) == 1, "snapshot observer requires one test thread")
        thread = threads[0]
        observer = subprocess.Popen([tracer, "-qq", "-yy", "-s", "256", "-e",
                                     "trace=faccessat2,fgetxattr,memfd_create,fchmod,pread64,write,fcntl,%process",
                                     "-e", "inject=" + injection, "-o", str(trace), "-p", thread.name],
                                    cwd=work, env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        while True:
            require(observer.poll() is None and test.poll() is None and time.monotonic() < deadline,
                    "snapshot syscall observer did not attach")
            if re.search(r"^TracerPid:\s+[1-9][0-9]*$", (thread / "status").read_text(), re.MULTILINE):
                break
            time.sleep(0.01)
        (work / "release").write_bytes(b"release")
        stdout, stderr = test.communicate(timeout=15)
        observer_stdout, observer_stderr = observer.communicate(timeout=5)
        (work / "stdout").write_bytes(stdout)
        (work / "stderr").write_bytes(stderr)
        (work / "tracer.stdout").write_bytes(observer_stdout)
        (work / "tracer.stderr").write_bytes(observer_stderr)
        require(observer.returncode == 0, "snapshot syscall observer failed")
        return subprocess.CompletedProcess(test.args, test.returncode, stdout, stderr), trace.read_text()
    finally:
        for child in (test, observer):
            if child is not None and child.poll() is None:
                child.kill()
                child.communicate(timeout=5)


def snapshot_fault_records(test_binary, tracer, inputs, output, cases, host):
    records, traces = [], {}
    inventory = "sha256:" + hashlib.sha256((inputs / "image-cases.json").read_bytes()).hexdigest()
    for name, (injection, call) in SNAPSHOT_FAULTS.items():
        completed, trace = snapshot_fault_case(test_binary, tracer, output / name, name, injection)
        injected = [line for line in trace.splitlines() if "(INJECTED)" in line]
        require(len(injected) == 1 and re.search(call, injected[0]), "snapshot fault syscall differs: " + name)
        require(not re.search(r"\b(?:execve|execveat|clone|clone3|fork|vfork)\(", trace),
                "snapshot failure attempted execution")
        records.extend(injected_results(completed, cases, [name], host, inventory,
                                        marker=b"OPAAL_IMAGE_CASE ", label="snapshot fault"))
        traces[name] = {"injection": injection, "trace_sha256": hashlib.sha256(trace.encode()).hexdigest()}
    return records, traces


def posix_image_cases(test_binary, inputs, output, cases, host):
    digest = hashlib.sha256(test_binary.read_bytes()).hexdigest()
    arguments = [test_binary, "native_frozen_", "--nocapture", "--test-threads=1"]
    trace = output / "posix.execve"
    if host in LINUX_HOSTS:
        tracer = shutil.which("strace")
        require(tracer is not None, "POSIX image qualification requires strace")
        arguments = [tracer, "-f", "-qq", "-s", "4096", "-e", "trace=execve,execveat",
                     "-o", trace, *arguments]
    completed = command(arguments, output, {"PATH": "/usr/bin:/bin",
                                          "OPAAL_NATIVE_INHERIT_CANARY": "must-not-reach-child"})
    (output / "posix.stdout").write_bytes(completed.stdout)
    (output / "posix.stderr").write_bytes(completed.stderr)
    names = [row["name"] for row in cases if row["name"] in POSIX_CASES and host in row["hosts"]]
    records = injected_results(completed, cases, names, host,
                               "sha256:" + hashlib.sha256((inputs / "image-cases.json").read_bytes()).hexdigest(),
                               marker=b"OPAAL_IMAGE_CASE ", label="POSIX image")
    metadata = {"binary_sha256": digest, "scope": "POSIX snapshot/process adapter and compiled C observers"}
    if host in LINUX_HOSTS:
        images = [json.loads(line.partition(b"OPAAL_NATIVE_IMAGE ")[2])
                  for line in completed.stdout.splitlines() if b"OPAAL_NATIVE_IMAGE " in line]
        require(len(images) == 3 and {row.get("kind") for row in images} == {"static", "dynamic", "lifetime"}
                and all(set(row) == {"kind", "digest"} and re.fullmatch(r"sha256:[0-9a-f]{64}", row["digest"])
                        for row in images), "POSIX native fixture identities differ")
        metadata["native_images"] = {row["kind"]: row["digest"] for row in images}
        calls = trace.read_text()
        launches = re.findall(r'execve\("/proc/self/fd/[0-9]+",.*? = (.*)', calls)
        require(len(launches) == 17 and all(result == "0" for result in launches),
                "POSIX descriptor execution syscall census differs")
        require(not re.search(r'execve\("[^"\n]*/native-(?:image|lifetime)-', calls)
                and "execveat(" not in calls, "POSIX image observer used an unexpected execution route")
        metadata.update({"descriptor_execve_attempts": len(launches), "pathname_fallback_attempts": 0,
                         "trace_sha256": hashlib.sha256(trace.read_bytes()).hexdigest(),
                         "tracer_sha256": hashlib.sha256(Path(tracer).read_bytes()).hexdigest()})
        fault_records, metadata["snapshot_faults"] = snapshot_fault_records(test_binary, tracer, inputs, output, cases, host)
        records.extend(fault_records)
        records.extend(permission_cases(test_binary, inputs, output, cases, host))
    require(hashlib.sha256(test_binary.read_bytes()).hexdigest() == digest,
            "POSIX image test binary changed")
    return records, metadata


def qualify_images(binary, inputs, output, cases, *, test_binary=None, process_test_binary=None,
                   posix_test_binary=None, frontend_test_binary=None):
    host = host_triple()
    require(host is not None, "unsupported image qualification host")
    output.mkdir()
    observations, pending, image_hashes = [], [], {}
    for row in cases:
        if host not in row["hosts"]:
            continue
        spec = row["input"]
        supported = (spec["kind"] in {"format", "source-open"}
                     or (spec["kind"] == "request-bound" and spec["subject"] == "image-bytes"))
        # CLI preparation admits Linux images only; macOS format validation needs an API observer.
        if host not in LINUX_HOSTS or not supported:
            pending.append(row["name"])
            continue
        work = output / row["name"]
        work.mkdir()
        tool = work / "image"
        image = format_image(row, host) if spec["kind"] == "format" else minimal_elf()
        tool.write_bytes(image)
        if spec["kind"] == "request-bound":
            with tool.open("r+b") as handle:
                handle.truncate(spec["value"])
        image_hashes[row["name"]] = hashlib.sha256(tool.read_bytes()).hexdigest()
        tool.chmod(0o700)
        if spec["kind"] == "source-open":
            tool.rename(work / "original")
            if spec["source"] == "symlink":
                tool.symlink_to(work / "original")
            elif spec["source"] == "fifo":
                os.mkfifo(tool)
            else:
                require(spec["source"] == "directory", "unknown image source case")
                tool.mkdir()
        destination = work / "project"
        result = command([binary, "project", "init", "--example", "repository-check",
                          "--destination", destination, "--environment", "ci", "--tool", tool], work, {})
        (work / "stdout").write_bytes(result.stdout)
        (work / "stderr").write_bytes(result.stderr)
        admitted = result.returncode == 0
        require(result.returncode in {0, 2}, "image preparation exit differs: " + row["name"])
        require(destination.exists() == admitted, "image preparation partial destination differs")
        if admitted:
            require(not result.stderr and (destination / "live-tools.toml").is_file(),
                    "admitted image has no lock")
        else:
            require(not result.stdout and b"INIT001" in result.stderr, "image refusal diagnostic differs")
        observed = {"admitted": admitted}
        if spec["kind"] == "source-open":
            # This route performs preparation only, so no process execution is requested.
            observed["attempts"] = 0
        observations.append({"name": row["name"], "observed": observed,
                             "route": "cli-preparation-no-execution-request"})
    api = None
    if test_binary:
        digest = hashlib.sha256(test_binary.read_bytes()).hexdigest()
        completed = command([test_binary, "image_frozen_format_cases_have_independent_expected_admission",
                             "--nocapture", "--test-threads=1"], output, {})
        (output / "api.stdout").write_bytes(completed.stdout)
        (output / "api.stderr").write_bytes(completed.stderr)
        names = [row["name"] for row in cases if row["input"]["kind"] == "format"]
        records = injected_results(completed, cases, names, host,
                                   "sha256:" + hashlib.sha256((inputs / "image-cases.json").read_bytes()).hexdigest(),
                                   marker=b"OPAAL_IMAGE_CASE ", label="image")
        by_name = {row["name"]: row for row in observations}
        for row in records:
            if row["name"] in by_name:
                require(by_name[row["name"]]["observed"] == row["observed"], "CLI and API admission differ")
                by_name[row["name"]]["route"] = "cli-preparation-and-runtime-api"
            else:
                pending.remove(row["name"])
                observations.append({**row, "route": "runtime-api"})
            image = format_image(next(case for case in cases if case["name"] == row["name"]), host)
            image_hashes[row["name"]] = hashlib.sha256(image).hexdigest()
        require(hashlib.sha256(test_binary.read_bytes()).hexdigest() == digest, "image API test binary changed")
        api = {"binary_sha256": digest, "scope": "public runtime structural image validation; no snapshot or launch"}
    frontend = None
    if frontend_test_binary:
        digest = hashlib.sha256(frontend_test_binary.read_bytes()).hexdigest()
        completed = command([frontend_test_binary, "image_frozen_source_open_and_image_byte_bounds",
                             "--nocapture", "--test-threads=1"], output, {})
        (output / "frontend.stdout").write_bytes(completed.stdout)
        (output / "frontend.stderr").write_bytes(completed.stderr)
        records = injected_results(completed, cases, list(FRONTEND_CASES), host,
                                   "sha256:" + hashlib.sha256((inputs / "image-cases.json").read_bytes()).hexdigest(),
                                   marker=b"OPAAL_IMAGE_CASE ", label="frontend image")
        by_name = {row["name"]: row for row in observations}
        for row in records:
            if row["name"] in by_name:
                require(by_name[row["name"]]["observed"] == row["observed"], "CLI and frontend admission differ")
                by_name[row["name"]]["route"] = "cli-preparation-and-frontend-source-api"
            else:
                pending.remove(row["name"])
                observations.append({**row, "route": "frontend-source-api-no-execution-request"})
        require(hashlib.sha256(frontend_test_binary.read_bytes()).hexdigest() == digest,
                "frontend image test binary changed")
        frontend = {"binary_sha256": digest, "scope": "shared no-follow regular-file open and bounded read; no launch"}
    process = None
    if process_test_binary:
        digest = hashlib.sha256(process_test_binary.read_bytes()).hexdigest()
        arguments = [process_test_binary, "native_frozen_", "--nocapture", "--test-threads=1"]
        tracer = None
        trace = output / "acquisition.syscalls"
        if host in LINUX_HOSTS:
            tracer = shutil.which("strace")
            require(tracer is not None, "runtime acquisition qualification requires strace")
            arguments = [tracer, "-f", "-qq", "-yy", "-s", "256", "-e",
                         "trace=pread64,fcntl,write,execve,execveat", "-o", trace, *arguments]
        completed = command(arguments, output, {"PATH": "/usr/bin:/bin"})
        (output / "process.stdout").write_bytes(completed.stdout)
        (output / "process.stderr").write_bytes(completed.stderr)
        names = [row["name"] for row in cases if row["name"] in PROCESS_CASES + ACQUISITION_CASES and host in row["hosts"]]
        records = injected_results(completed, cases, names, host,
                                   "sha256:" + hashlib.sha256((inputs / "image-cases.json").read_bytes()).hexdigest(),
                                   marker=b"OPAAL_IMAGE_CASE ", label="image process")
        for row in records:
            pending.remove(row["name"])
            observations.append({**row, "route": "runtime-request-api-with-fake-adapter"})
        require(hashlib.sha256(process_test_binary.read_bytes()).hexdigest() == digest,
                "image process test binary changed")
        process = {"binary_sha256": digest,
                   "scope": "runtime request/authority/attempt validation with fake adapters; Linux acquisition cases use real POSIX snapshots and a fake deadline clock",
                   "exit_observation": "runtime ninth-attempt refusal maps to task failure status2; no CLI fault injection"}
        if tracer:
            process.update({"acquisition": acquisition_trace_observations(trace.read_text()),
                            "trace_sha256": hashlib.sha256(trace.read_bytes()).hexdigest(),
                            "tracer_sha256": hashlib.sha256(Path(tracer).read_bytes()).hexdigest()})
            launch_trace = output / "launch-failures.syscalls"
            completed = command([tracer, "-f", "-qq", "-s", "256", "-e", "trace=execve,execveat,chdir",
                                 "-o", launch_trace, process_test_binary,
                                 "image_frozen_real_spawn_and_missing_loader_fail_without_fallback",
                                 "--nocapture", "--test-threads=1"], output, {"PATH": "/usr/bin:/bin"})
            (output / "launch-failures.stdout").write_bytes(completed.stdout)
            (output / "launch-failures.stderr").write_bytes(completed.stderr)
            records = injected_results(completed, cases, list(LAUNCH_FAILURE_CASES), host,
                                       "sha256:" + hashlib.sha256((inputs / "image-cases.json").read_bytes()).hexdigest(),
                                       marker=b"OPAAL_IMAGE_CASE ", label="launch failure")
            calls = launch_trace.read_text()
            launches = re.findall(r'execve\("/proc/self/fd/[0-9]+",.* = (-?\d+)', calls)
            require(launches == ["-1"] and "ENOENT" in calls and "execveat(" not in calls
                    and len(re.findall(r'\bexecve\(', calls)) == 2
                    and re.search(r'chdir\("[^"\n]*/missing-directory"\)\s+= -1 ENOENT', calls),
                    "launch failure syscall census differs")
            for row in records:
                pending.remove(row["name"])
                observations.append({**row, "route": "runtime-with-real-posix-spawn-failure"})
            require(hashlib.sha256(process_test_binary.read_bytes()).hexdigest() == digest,
                    "launch failure test binary changed")
            process["launch_failures"] = {"trace_sha256": hashlib.sha256(launch_trace.read_bytes()).hexdigest(),
                                          "descriptor_execve_failures": 1, "pathname_fallback_attempts": 0}
    posix = None
    if posix_test_binary:
        records, posix = posix_image_cases(posix_test_binary, inputs, output, cases, host)
        for row in records:
            pending.remove(row["name"])
            observations.append({**row, "route": "posix-adapter-with-compiled-native-observer"})
    finish = None
    if host in LINUX_HOSTS and all(name in pending for name in FINISH_CASES):
        records, finish = finish_cases(binary, inputs, output / "finish")
        for row in records:
            pending.remove(row["name"])
            observations.append(row)
    result = {"host": host, "producer_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              "cli_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "inventory_sha256": hashlib.sha256((inputs / "image-cases.json").read_bytes()).hexdigest(),
              "image_sha256": image_hashes, "api_observer": api,
              "process_observer": process,
              "frontend_observer": frontend,
              "finish_observer": finish,
              "posix_observer": posix,
              **case_results(cases, observations, pending, host, label="image"),
              "limits": ["Preparation observes structural admission, not snapshot acquisition or execution.",
                         "CLI preparation attempts denote no execution request, not a syscall launch census."]}
    (output / "image-results.json").write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    return result
