from __future__ import annotations

import copy
import importlib.util
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from ci.package_controlled_native_tools import FIXTURES, ROOT, check_cases, check_copies
from ci.package_data_processing import VERSION, package_examples
from ci.qualify_controlled_native_tools import assert_result, corpus, expected_exit, payload
from ci.qualify_native_preparation import asset_observations, case_results, injected_results, snapshot
from ci.qualify_native_evidence import (canonical, cli_evidence_cases, injected_evidence_cases, journal_bytes, mutate_artifact,
                                      passive_case, process_journal, reseal)
from ci.qualify_native_images import (ACQUISITION_CASES, FINISH_CASES, FRONTEND_CASES, POSIX_CASES, PROCESS_CASES, SNAPSHOT_FAULTS,
                                     acquisition_trace_observations, finish_cases, finish_launch_census, posix_image_cases,
                                     permission_cases, qualify_images, snapshot_fault_records)
from ci.qualify_data_processing import QualificationError, unpack

spec = importlib.util.spec_from_file_location("repository_comparison", ROOT / "examples/repository-check/comparison.py")
comparison = importlib.util.module_from_spec(spec)
spec.loader.exec_module(comparison)


class NativeReferenceTests(unittest.TestCase):
    def test_finish_census_distinguishes_native_child_from_owned_summary_worker(self):
        pid, worker = 2147483647, 2147483646
        base = (f'1 execve("/cli", [], 0) = 0\n'
                f'{pid} execve("/proc/self/fd/4", [], 0) = 0\n')
        extra = (f'{worker} execve("/proc/self/fd/65", ["opaal", "--opaal-standard-host-worker"], 0) = 0\n'
                 f'1 wait4({worker}, [], WNOHANG, NULL) = {worker}\n')
        self.assertEqual(finish_launch_census(base, pid), {"native_launches": 1, "summary_workers": 0})
        self.assertEqual(finish_launch_census(base + extra, pid), {"native_launches": 1, "summary_workers": 1})
        resumed = base.replace('[], 0) = 0\n', f'[], 0 <unfinished ...>\n{pid} <... execve resumed>) = 0\n', 1)
        # The resumed return must belong to the PID that entered execve.
        with self.assertRaises(QualificationError):
            finish_launch_census(resumed, pid)
        resumed = (base + extra).replace(f'{worker} execve("/proc/self/fd/65", ["opaal", "--opaal-standard-host-worker"], 0) = 0',
            f'{worker} execve("/proc/self/fd/65", ["opaal", "--opaal-standard-host-worker"], 0 <unfinished ...>\n{worker} <... execve resumed>) = 0')
        self.assertEqual(finish_launch_census(resumed, pid), {"native_launches": 1, "summary_workers": 1})
        for fault in (base + extra + extra, base + base, (base + extra).replace(' = 0', ' = -1 EIO', 1),
                      (base + extra).replace('"--opaal-standard-host-worker"', '"unrecognized"'),
                      (base + extra).replace('/proc/self/fd/65', '/tmp/worker'),
                      (base + extra).replace(f' = {worker}\n', ' = 0\n'),
                      base + '3 execve("/tmp/checker", [], 0) = 0\n',
                      base + '3 execveat(65, "", [], 0, AT_EMPTY_PATH) = 0\n'):
            with self.subTest(trace=fault), self.assertRaises(QualificationError):
                finish_launch_census(fault, pid)

    def test_finish_fault_requires_real_launch_injection_reap_and_primary(self):
        cases = [row for row in check_cases(ROOT / FIXTURES)["image"] if row["name"] in FINISH_CASES]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            executable = root / "tool"
            executable.write_bytes(b"tool")
            for fault in (None, "injection", "fallback", "unreaped", "primary", "audit", "partial", "exit"):
                def seed(binary, work, **options):
                    work.mkdir()
                    self.assertEqual(options, {"native_image": b"compiled"})
                    return "sha256:plan"

                def run(arguments, work, environment):
                    if "-o" in arguments and "-O2" in arguments:
                        arguments[-1].write_bytes(b"compiled")
                        return subprocess.CompletedProcess(arguments, 0, b"", b"")
                    requested = work.name == FINISH_CASES[1]
                    pid = 2147483647
                    (work / "child.pid").write_text(str(pid))
                    trace = (f'1 execve("/cli", [], 0) = 0\n'
                             f'{pid} execve("/proc/self/fd/4", [], 0) = 0\n'
                             f'1 kill(-{pid}, 0) = -1 EIO (INJECTED)\n'
                             f'1 wait4({pid}, [], WNOHANG, NULL) = {pid}\n')
                    if fault == "injection":
                        trace = trace.replace("EIO (INJECTED)", "ESRCH")
                    elif fault == "fallback":
                        trace += '2 execve("/tmp/checker", [], 0) = 0\n'
                    elif fault == "unreaped":
                        trace = trace.replace(f" = {pid}\n", " = 0\n")
                    (work / "syscalls").write_text(trace)
                    primary = {"class": "cancelled" if requested else "error",
                               "cancel_reason": "requested" if requested else None, "partial": fault != "partial"}
                    if fault == "primary":
                        primary["class"] = "success"
                    (work / "receipt.json").write_bytes(canonical({"primary": primary, "journal_state": "complete"}))
                    (work / "audit.json").write_bytes(canonical({"primary": primary,
                        "completeness": "partial" if fault == "audit" else "complete"}))
                    (work / "run.jsonl").write_bytes(canonical({"kind": "effect-before", "payload": {"effect": "process.run"}}))
                    return subprocess.CompletedProcess(arguments, 0 if fault == "exit" else 130 if requested else 2,
                                                       b"", b"safe failure")

                with self.subTest(fault=fault), \
                        patch("ci.qualify_native_images.shutil.which", return_value=str(executable)), \
                        patch("ci.qualify_native_images.prepare_project", side_effect=seed), \
                        patch("ci.qualify_native_images.command", side_effect=run):
                    def observe():
                        rows, metadata = finish_cases(executable, ROOT / FIXTURES, root / str(fault))
                        case_results(cases, rows, [], "x86_64-unknown-linux-gnu", label="image")
                        self.assertEqual(set(metadata["cases"]), set(FINISH_CASES))
                    if fault:
                        with self.assertRaises(QualificationError):
                            observe()
                    else:
                        observe()

    def test_frontend_image_records_require_complete_host_bound_inventory(self):
        cases = check_cases(ROOT / FIXTURES)["image"]
        host = "aarch64-apple-darwin"
        digest = "sha256:" + hashlib.sha256((ROOT / FIXTURES / "image-cases.json").read_bytes()).hexdigest()
        records = [{"name": row["name"], "observed": row["expected"], "host": host,
                    "inventory_digest": digest} for row in cases if row["name"] in FRONTEND_CASES]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            observer = root / "observer"
            observer.write_bytes(b"observer")
            for label, rows in (("complete", records), ("missing", records[:-1]), ("duplicate", records + records[:1]),
                                ("identity", [{**row, "inventory_digest": "old"} for row in records])):
                completed = subprocess.CompletedProcess([], 0, b"".join(
                    b"OPAAL_IMAGE_CASE " + canonical(row) + b"\n" for row in rows), b"")
                with self.subTest(label=label), \
                        patch("ci.qualify_native_images.host_triple", return_value=host), \
                        patch("ci.qualify_native_images.command", return_value=completed):
                    if label != "complete":
                        with self.assertRaises(QualificationError):
                            qualify_images(observer, ROOT / FIXTURES, root / label, cases, frontend_test_binary=observer)
                    else:
                        result = qualify_images(observer, ROOT / FIXTURES, root / label, cases, frontend_test_binary=observer)
                        self.assertEqual({row["name"] for row in result["observed"]}, set(FRONTEND_CASES))

    def test_acquisition_trace_requires_each_boundary_seals_and_no_execution(self):
        blocks = ['1 execve("/observer", [], 0) = 0\n']
        for name in ACQUISITION_CASES:
            block = f'2 write(1<pipe:[1]>, "OPAAL_ACQUISITION_START {name}\\n", 80) = 80\n'
            if name.startswith("copy-") or name == "snapshot-original-stale-before-acquisition":
                reads = []
            else:
                flags = "F_SEAL_SEAL|F_SEAL_SHRINK|F_SEAL_GROW|F_SEAL_WRITE"
                block += f'2 fcntl(4</memfd:opaal-native>(deleted), F_ADD_SEALS, {flags}) = 0\n'
                block += f'2 fcntl(4</memfd:opaal-native>(deleted), F_GET_SEALS) = 0xf ({flags})\n'
                reads = ([65536] if name.startswith("sealed-hash-") else [-1]
                         if name == "snapshot-post-seal-read-failure" else [65536, 65536, 0])
            block += ''.join(f'2 pread64(4</memfd:opaal-native>(deleted), "bytes", 65536, 0) = {read}\n'
                             for read in reads)
            blocks.append(block)
        trace = ''.join(blocks)
        self.assertEqual(set(acquisition_trace_observations(trace)), set(ACQUISITION_CASES))
        header_read = '2 pread64(4</memfd:opaal-native>(deleted), "header", 64, 0) = 64\n'
        program_read = '2 pread64(4</memfd:opaal-native>(deleted), "program", 56, 64) = 56\n'
        arm_trace = trace.replace(blocks[5], blocks[5] + header_read + program_read)
        self.assertEqual(acquisition_trace_observations(arm_trace)['elf-validation-requested'],
                         {'snapshot_read_results': [65536, 65536, 0, 64, 56], 'verified_seals': True})
        for fault in (trace.replace(blocks[1], ''), trace + blocks[1],
                      arm_trace.replace(' = 64\n', ' = 17\n'),
                      arm_trace.replace(program_read, ''),
                      arm_trace + header_read + program_read,
                      trace.replace(' = 65536', ' = 17', 1),
                      trace.replace('F_ADD_SEALS', 'F_GETFD', 1),
                      trace.replace('F_SEAL_WRITE', 'F_SEAL_EXEC', 1),
                      trace.replace('F_GET_SEALS) = 0xf', 'F_GET_SEALS) = -1 EIO', 1),
                      trace + '2 execve("/original", [], 0) = 0\n',
                      trace + '2 execveat(4, "", [], 0, AT_EMPTY_PATH) = 0\n'):
            with self.subTest(trace=fault[-100:]), self.assertRaises(QualificationError):
                acquisition_trace_observations(fault)

    def test_posix_records_require_exact_inventory_and_execution_syscalls(self):
        cases = check_cases(ROOT / FIXTURES)["image"]
        inventory = "sha256:" + hashlib.sha256((ROOT / FIXTURES / "image-cases.json").read_bytes()).hexdigest()
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            observer, tracer = root / "observer", root / "strace"
            observer.write_bytes(b"observer")
            tracer.write_bytes(b"tracer")
            for host, count in (("aarch64-apple-darwin", 1), ("x86_64-unknown-linux-gnu", 21)):
                rows = [{"name": row["name"], "observed": row["expected"], "host": host,
                         "inventory_digest": inventory} for row in cases
                        if row["name"] in POSIX_CASES and host in row["hosts"]]
                self.assertEqual(len(rows), count)
                faults = [None, "missing", "duplicate", "host", "inventory", "result", "failed", "changed"]
                if host.startswith("x86_64"):
                    faults += ["no-tracer", "no-exec", "failed-exec", "fallback", "execveat",
                               "missing-image", "duplicate-image", "invalid-image-digest"]
                for fault in faults:
                    with self.subTest(host=host, fault=fault):
                        emitted = json.loads(json.dumps(rows))
                        if fault == "missing":
                            emitted.pop()
                        elif fault == "duplicate":
                            emitted.append(emitted[0])
                        elif fault in {"host", "inventory"}:
                            emitted[0]["host" if fault == "host" else "inventory_digest"] = "wrong"
                        elif fault == "result":
                            emitted[0]["observed"] = {}

                        def run(arguments, cwd, environment):
                            if host.startswith("x86_64"):
                                self.assertEqual(arguments[0], str(tracer))
                                trace = '123 execve("/proc/self/fd/4", ["literal"], 0) = 0\n' * 17
                                if fault == "no-exec":
                                    trace = ""
                                elif fault == "failed-exec":
                                    trace = trace.replace(" = 0", " = -1 EACCES", 1)
                                elif fault == "fallback":
                                    trace += '123 execve("/tmp/native-image-1/dynamic", [], 0) = 0\n'
                                elif fault == "execveat":
                                    trace += '123 execveat(4, "", [], 0, AT_EMPTY_PATH) = 0\n'
                                (root / "posix.execve").write_text(trace)
                            if fault == "changed":
                                observer.write_bytes(b"changed")
                            images = [{"kind": kind, "digest": "sha256:" + "a" * 64}
                                      for kind in ("static", "dynamic", "lifetime")]
                            if fault == "missing-image":
                                images.pop()
                            elif fault == "duplicate-image":
                                images[-1] = images[0]
                            elif fault == "invalid-image-digest":
                                images[0]["digest"] = "invalid"
                            stdout = b"".join(b"OPAAL_IMAGE_CASE " + canonical(row) + b"\n" for row in emitted)
                            if host.startswith("x86_64"):
                                stdout += b"".join(b"OPAAL_NATIVE_IMAGE " + canonical(row) + b"\n" for row in images)
                            return subprocess.CompletedProcess(arguments, int(fault == "failed"), stdout, b"")

                        with patch("ci.qualify_native_images.shutil.which",
                                   return_value=None if fault == "no-tracer" else str(tracer)), \
                                patch("ci.qualify_native_images.snapshot_fault_records", return_value=([], {})), \
                                patch("ci.qualify_native_images.permission_cases", return_value=[]), \
                                patch("ci.qualify_native_images.command", side_effect=run):
                            if fault:
                                with self.assertRaises(QualificationError):
                                    posix_image_cases(observer, ROOT / FIXTURES, root, cases, host)
                            else:
                                records, metadata = posix_image_cases(observer, ROOT / FIXTURES, root, cases, host)
                                self.assertEqual(len(records), count)
                                if host.startswith("x86_64"):
                                    self.assertEqual(metadata["descriptor_execve_attempts"], 17)
                        observer.write_bytes(b"observer")

    def test_snapshot_faults_require_one_injection_at_the_intended_syscall_and_no_exec(self):
        cases = check_cases(ROOT / FIXTURES)["image"]
        host = "x86_64-unknown-linux-gnu"
        inventory = "sha256:" + hashlib.sha256((ROOT / FIXTURES / "image-cases.json").read_bytes()).hexdigest()
        by_name = {row["name"]: row for row in cases}
        for fault in (None, "missing-injection", "wrong-call", "duplicate-injection", "exec", "wrong-result", "missing-record"):
            def run(test, tracer, work, name, injection):
                syscall = injection.split(":")[0]
                argument = "3, F_ADD_SEALS" if "add-seals" in name else "3, F_GET_SEALS" if syscall == "fcntl" else "3"
                trace = f"123 {syscall}({argument}) = -1 EIO (INJECTED)\n"
                if fault == "missing-injection":
                    trace = ""
                elif fault == "wrong-call":
                    trace = "123 close(3) = -1 EIO (INJECTED)\n"
                elif fault == "duplicate-injection":
                    trace *= 2
                elif fault == "exec":
                    trace += '123 execve("/fallback", [], 0) = -1 ENOENT\n'
                row = {"name": name, "observed": {} if fault == "wrong-result" else by_name[name]["expected"],
                       "host": host, "inventory_digest": inventory}
                stdout = b"" if fault == "missing-record" else b"OPAAL_IMAGE_CASE " + canonical(row) + b"\n"
                return subprocess.CompletedProcess([], 0, stdout, b""), trace

            with self.subTest(fault=fault), patch("ci.qualify_native_images.snapshot_fault_case", side_effect=run):
                arguments = (Path("test"), "strace", ROOT / FIXTURES, Path("output"), cases, host)
                if fault:
                    with self.assertRaises(QualificationError):
                        snapshot_fault_records(*arguments)
                else:
                    records, metadata = snapshot_fault_records(*arguments)
                    self.assertEqual({row["name"] for row in records}, set(SNAPSHOT_FAULTS))
                    self.assertEqual(set(metadata), set(SNAPSHOT_FAULTS))

    def test_original_permission_observers_use_isolated_namespaces_and_exact_records(self):
        cases = check_cases(ROOT / FIXTURES)["image"]
        inventory = "sha256:" + hashlib.sha256((ROOT / FIXTURES / "image-cases.json").read_bytes()).hexdigest()
        host = "x86_64-unknown-linux-gnu"
        by_name = {row["name"]: row for row in cases}
        for uid, fault in ((uid, fault) for uid in (0, 1000)
                           for fault in (None, "failed", "missing", "wrong-host", "wrong-result")):
            with tempfile.TemporaryDirectory() as temporary:
                def run(arguments, cwd, environment):
                    name = cwd.name
                    self.assertEqual(environment["PATH"], "/usr/sbin:/usr/bin:/sbin:/bin")
                    if name == "original-file-capability":
                        self.assertEqual(arguments[:3], ["/usr/bin/unshare", "--user", "--map-root-user"])
                    elif name == "original-noexec-mount":
                        self.assertEqual(arguments[:4], ["/usr/bin/unshare", "--user", "--map-root-user", "--mount"])
                        self.assertEqual(environment["OPAAL_NATIVE_NOEXEC_ROOT"], str(cwd / "noexec"))
                        self.assertTrue((cwd / "noexec").is_dir())
                        self.assertIn('mount -t tmpfs -o noexec tmpfs "$OPAAL_NATIVE_NOEXEC_ROOT"', arguments[6])
                    elif uid == 0:
                        self.assertEqual(arguments[:2], ["/usr/bin/setpriv", "--bounding-set=-dac_override,-dac_read_search"])
                        self.assertEqual(arguments[2], Path("test"))
                    else:
                        self.assertEqual(arguments[0], Path("test"))
                    row = {"name": name, "observed": {} if fault == "wrong-result" else by_name[name]["expected"],
                           "host": "wrong" if fault == "wrong-host" else host, "inventory_digest": inventory}
                    stdout = b"" if fault == "missing" else b"OPAAL_IMAGE_CASE " + canonical(row) + b"\n"
                    return subprocess.CompletedProcess(arguments, int(fault == "failed"), stdout, b"")

                with self.subTest(uid=uid, fault=fault), \
                        patch("ci.qualify_native_images.os.geteuid", return_value=uid), \
                        patch("ci.qualify_native_images.command", side_effect=run):
                    arguments = (Path("test"), ROOT / FIXTURES, Path(temporary), cases, host)
                    if fault:
                        with self.assertRaises(QualificationError):
                            permission_cases(*arguments)
                    else:
                        self.assertEqual(len(permission_cases(*arguments)), 3)

    def test_cli_evidence_records_require_exact_cases_inventory_host_and_candidate(self):
        cases = check_cases(ROOT / FIXTURES)["evidence"]
        inventory = "sha256:" + hashlib.sha256((ROOT / FIXTURES / "evidence-cases.json").read_bytes()).hexdigest()
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary, observer = root / "opaal", root / "observer"
            binary.write_bytes(b"candidate")
            observer.write_bytes(b"observer")
            cli = "sha256:" + hashlib.sha256(binary.read_bytes()).hexdigest()
            for host, count in (("aarch64-apple-darwin", 6), ("x86_64-unknown-linux-gnu", 13)):
                rows = [{"name": row["name"], "observed": row["expected"], "host": host,
                         "inventory_digest": inventory} for row in cases if host in row["hosts"] and (
                         row["input"]["kind"] == "redaction" or (row["input"]["kind"] == "input-race"
                         and row["input"]["phase"] == "after-acceptance-before-read"))]
                self.assertEqual(len(rows), count)
                for fault in (None, "missing", "duplicate", "wrong-host", "wrong-inventory", "wrong-result",
                              "wrong-cli", "missing-cli", "duplicate-cli", "failed", "changed-observer"):
                    with self.subTest(host=host, fault=fault):
                        emitted = json.loads(json.dumps(rows))
                        if fault == "missing":
                            emitted.pop()
                        elif fault == "duplicate":
                            emitted.append(emitted[0])
                        elif fault == "wrong-host":
                            emitted[0]["host"] = "unsupported"
                        elif fault == "wrong-inventory":
                            emitted[0]["inventory_digest"] = "sha256:" + "0" * 64
                        elif fault == "wrong-result":
                            emitted[0]["observed"] = {}
                        identity = (b"" if fault == "missing-cli" else b"OPAAL_EVIDENCE_CLI " +
                                    ("sha256:" + "0" * 64 if fault == "wrong-cli" else cli).encode() + b"\n")
                        if fault == "duplicate-cli":
                            identity *= 2
                        completed = subprocess.CompletedProcess([], int(fault == "failed"), identity + b"".join(
                            b"OPAAL_EVIDENCE_CASE " + canonical(row) + b"\n" for row in emitted), b"")

                        def run(*args):
                            if fault == "changed-observer":
                                observer.write_bytes(b"changed")
                            return completed

                        with patch("ci.qualify_native_evidence.command", side_effect=run):
                            if fault:
                                with self.assertRaises(QualificationError):
                                    cli_evidence_cases(binary, observer, ROOT / FIXTURES, root, cases, host)
                            else:
                                records, metadata = cli_evidence_cases(binary, observer, ROOT / FIXTURES,
                                                                       root, cases, host)
                                self.assertEqual(len(records), count)
                                self.assertEqual(metadata["cli_digest"], cli)
                        observer.write_bytes(b"observer")

    def test_finish_journal_lsp_observers_require_the_exact_frozen_rows(self):
        cases = check_cases(ROOT / FIXTURES)["evidence"]
        inventory = "sha256:" + hashlib.sha256((ROOT / FIXTURES / "evidence-cases.json").read_bytes()).hexdigest()
        host = "aarch64-apple-darwin"
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            observer = root / "observer"
            for kind, count in (("finish", 26), ("journal", 3), ("lsp", 1)):
                rows = [{"name": row["name"], "observed": row["expected"], "host": host,
                         "inventory_digest": inventory} for row in cases if (
                         kind == "finish" and (row["input"]["kind"] == "finish-fault"
                         or row["name"] in {"requested-cancellation", "deadline-cancellation"}))
                         or (kind == "journal" and row["input"]["kind"] == "journal-bound")
                         or (kind == "lsp" and row["name"] == "passive-lsp")]
                self.assertEqual(len(rows), count)
                for fault in (None, "missing", "duplicate", "wrong-host", "wrong-inventory", "wrong-result",
                              "failed", "changed-binary"):
                    with self.subTest(kind=kind, fault=fault):
                        observer.write_bytes(b"observer")
                        emitted = copy.deepcopy(rows)
                        if fault == "missing":
                            emitted.pop()
                        elif fault == "duplicate":
                            emitted.append(emitted[0])
                        elif fault == "wrong-host":
                            emitted[0]["host"] = "unsupported"
                        elif fault == "wrong-inventory":
                            emitted[0]["inventory_digest"] = "sha256:" + "0" * 64
                        elif fault == "wrong-result":
                            emitted[0]["observed"] = {}
                        completed = subprocess.CompletedProcess([], int(fault == "failed"), b"".join(
                            b"OPAAL_EVIDENCE_CASE " + canonical(row) + b"\n" for row in emitted), b"")

                        def run(*args):
                            if fault == "changed-binary":
                                observer.write_bytes(b"changed")
                            return completed

                        with patch("ci.qualify_native_evidence.command", side_effect=run):
                            if fault:
                                with self.assertRaises(QualificationError):
                                    injected_evidence_cases(observer, ROOT / FIXTURES, root, cases, host, kind)
                            else:
                                records, metadata = injected_evidence_cases(observer, ROOT / FIXTURES, root,
                                                                           cases, host, kind)
                                self.assertEqual(len(records), count)
                                self.assertTrue(metadata["scope"])

    def test_native_version_mutations_follow_each_artifacts_closed_shape(self):
        for document in ("check", "plan", "audit"):
            for mutation, field, expected in (("native-required-version", "required_version", "1.0.0"),
                                               ("native-locked-version", "locked_version", "1.0.0"),
                                               ("native-version-verified", "version_verified", True)):
                with self.subTest(document=document, mutation=mutation):
                    value = {"schema": "opaal." + document + ".v5", "schema_version": 5}
                    if document != "audit":
                        value["tools"] = [{"adapter": "native", "required_version": None,
                                           "locked_version": None, "version_verified": False}]
                    mutate_artifact(value, document, mutation)
                    self.assertEqual(value[field] if document == "audit" else value["tools"][0][field], expected)
                    self.assertEqual(reseal(dict(value)), value)
                    self.assertEqual("tools" in value, document != "audit")
        with self.assertRaisesRegex(QualificationError, "audit unexpectedly contains tool rows"):
            mutate_artifact({"tools": []}, "audit", "native-locked-version")

    def test_passive_help_and_prompt_require_views_without_evaluation(self):
        def seed(binary, work, **options):
            work.mkdir()
            return subprocess.CompletedProcess([], 0, b"", b"")

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for observer, output in (("help", b"Usage:\n"), ("interactive-prompt", b">> ")):
                for fault in (None, "empty", "initializer", "action", "launch"):
                    with self.subTest(observer=observer, fault=fault):
                        work = root / (observer + str(fault))

                        def view(arguments, cwd, environment, *, stdin=None):
                            self.assertEqual(arguments, [Path("opaal"), "--help"] if observer == "help"
                                             else [Path("opaal")])
                            self.assertEqual(stdin, b"exit\n" if observer == "interactive-prompt" else None)
                            if fault == "launch":
                                (cwd / "checker.launches").write_bytes(b"launch\n")
                            data = (b"" if fault == "empty" else output +
                                    (b"SENSITIVE_PASSIVE_" + fault.upper().encode()
                                     if fault in {"initializer", "action"} else b""))
                            return subprocess.CompletedProcess(arguments, 0, data, b"")

                        with patch("ci.qualify_native_evidence.project", side_effect=seed), \
                                patch("ci.qualify_native_evidence.command", side_effect=view):
                            if fault in {"empty", "initializer", "action"}:
                                with self.assertRaises(QualificationError):
                                    passive_case(Path("opaal"), work, observer)
                            else:
                                observed = passive_case(Path("opaal"), work, observer)
                                self.assertEqual(observed, {"launches": int(fault == "launch"),
                                                            "evaluated_initializers": 0,
                                                            "evaluated_actions": 0})

    def test_process_observer_records_cover_only_explicit_inventory_cases(self):
        cases = check_cases(ROOT / FIXTURES)["image"]
        host = "aarch64-apple-darwin"
        digest = "sha256:" + hashlib.sha256((ROOT / FIXTURES / "image-cases.json").read_bytes()).hexdigest()
        records = [{"name": row["name"], "observed": row["expected"], "host": host,
                    "inventory_digest": digest} for row in cases
                   if row["name"] in PROCESS_CASES and host in row["hosts"]]
        names = {row["name"] for row in records}
        self.assertEqual(len(records), 15)
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "candidate"
            binary.write_bytes(b"candidate")
            observer = root / "observer"
            observer.write_bytes(b"observer")
            for label, emitted in (("complete", records), ("missing", records[:-1]),
                                   ("duplicate", records + records[:1])):
                completed = subprocess.CompletedProcess([], 0, b"".join(
                    b"OPAAL_IMAGE_CASE " + canonical(row) + b"\n" for row in emitted), b"")
                with patch("ci.qualify_native_images.host_triple", return_value=host), \
                        patch("ci.qualify_native_images.command", return_value=completed):
                    if label == "complete":
                        result = qualify_images(binary, ROOT / FIXTURES, root / label, cases,
                                                process_test_binary=observer)
                        self.assertEqual({row["name"] for row in result["observed"]}, names)
                        self.assertTrue(names.isdisjoint(result["pending"]))
                    else:
                        with self.assertRaisesRegex(QualificationError, "observer case set differs"):
                            qualify_images(binary, ROOT / FIXTURES, root / label, cases,
                                           process_test_binary=observer)

    def test_journal_mutations_reseal_links_without_repairing_lifecycle_faults(self):
        node = "sha256:" + "1" * 64 + "#000000"
        events = [{"kind": "header", "payload": {}},
                  {"kind": "action-start", "payload": {"action_node_id": node}},
                  {"kind": "action-end", "payload": {"outcome": {"class": "success"}}},
                  {"kind": "terminal", "payload": {}}]
        original = canonical(events)
        process = process_journal(b"\n".join(canonical(event) for event in events))
        process[2]["payload"]["attempt"] = 1
        process[3]["payload"]["attempt"] = 1
        encoded = journal_bytes(process)
        self.assertEqual(canonical(events), original)
        self.assertNotIn("digest", process[0])
        decoded = [json.loads(line) for line in encoded.splitlines()]
        self.assertEqual([event["seq"] for event in decoded], list(range(6)))
        self.assertIsNone(decoded[0]["previous"])
        for index, event in enumerate(decoded):
            digest = event.pop("digest")
            self.assertEqual(digest, "sha256:" + hashlib.sha256(canonical(event)).hexdigest())
            if index + 1 < len(decoded):
                self.assertEqual(decoded[index + 1]["previous"], digest)
        self.assertEqual(decoded[2]["payload"]["attempt"], 1)
        self.assertEqual(decoded[3]["payload"]["attempt"], 1)
        self.assertIsNone(decoded[2]["payload"]["operation"]["status"])
        self.assertEqual(decoded[3]["payload"]["operation"]["status"], 0)

    def test_image_and_evidence_accounting_rejects_missing_duplicate_and_mistyped_results(self):
        inventories = check_cases(ROOT / FIXTURES)
        for label in ("image", "evidence"):
            cases = inventories[label]
            for host in ("aarch64-apple-darwin", "x86_64-unknown-linux-gnu"):
                applicable = [row for row in cases if host in row["hosts"]]
                observations = [{"name": row["name"], "observed": row["expected"]} for row in applicable]
                result = case_results(cases, observations, [], host, label=label)
                self.assertEqual(len(result["observed"]) + len(result["inapplicable"]), len(cases))
                mistyped = {**observations[0], "observed": {**observations[0]["observed"]}}
                key = next(key for key, value in mistyped["observed"].items() if type(value) is bool)
                mistyped["observed"][key] = int(mistyped["observed"][key])
                for changed in (observations[:-1], observations + observations[:1],
                                [mistyped] + observations[1:]):
                    with self.subTest(label=label, host=host):
                        with self.assertRaises(QualificationError):
                            case_results(cases, changed, [], host, label=label)

    def test_image_api_records_are_explicit_and_bound_to_inventory_and_host(self):
        case = check_cases(ROOT / FIXTURES)["image"][0]
        record = {"name": case["name"], "observed": case["expected"],
                  "host": "aarch64-apple-darwin", "inventory_digest": "sha256:inventory"}
        completed = subprocess.CompletedProcess([], 0, b"OPAAL_IMAGE_CASE " + canonical(record) + b"\n", b"")
        args = (completed, [case], [case["name"]], record["host"], record["inventory_digest"])
        self.assertEqual(len(injected_results(*args, marker=b"OPAAL_IMAGE_CASE ", label="image")), 1)
        with self.assertRaises(QualificationError):
            injected_results(*args)

    def test_image_producer_keeps_mac_format_rows_pending_without_an_api_observer(self):
        cases = check_cases(ROOT / FIXTURES)["image"]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "candidate"
            binary.write_bytes(b"candidate identity")
            with patch("ci.qualify_native_images.host_triple", return_value="aarch64-apple-darwin"), \
                    patch("ci.qualify_native_images.command") as invoke:
                result = qualify_images(binary, ROOT / FIXTURES, root / "results", cases)
            invoke.assert_not_called()
            self.assertEqual(result["observed"], [])
            self.assertIsNone(result["process_observer"])
            self.assertTrue({row["name"] for row in cases
                             if row["name"] in PROCESS_CASES and "aarch64-apple-darwin" in row["hosts"]}
                            <= set(result["pending"]))
            self.assertTrue({row["name"] for row in cases if row["input"]["kind"] == "format"}
                            <= set(result["pending"]))

    def test_image_cli_refusal_uses_the_existing_setup_diagnostic(self):
        case = next(row for row in check_cases(ROOT / FIXTURES)["image"] if row["name"] == "empty-image")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "candidate"
            binary.write_bytes(b"candidate identity")
            for code in ("INIT001", "INIT002"):
                with patch("ci.qualify_native_images.host_triple", return_value="x86_64-unknown-linux-gnu"), \
                        patch("ci.qualify_native_images.command", return_value=subprocess.CompletedProcess(
                            [], 2, b"", ("opaal: " + code + ": setup refused\n").encode())):
                    if code == "INIT001":
                        result = qualify_images(binary, ROOT / FIXTURES, root / code, [case])
                        self.assertEqual(result["observed"][0]["observed"], {"admitted": False})
                    else:
                        with self.assertRaisesRegex(QualificationError, "refusal diagnostic differs"):
                            qualify_images(binary, ROOT / FIXTURES, root / code, [case])

    def test_independent_resealing_uses_canonical_bytes_and_replaces_only_digest(self):
        value = {"z": [True, None, 7], "a": "é", "digest": "old"}
        result = reseal(value)
        self.assertEqual(result, {"a": "é", "z": [True, None, 7],
                                 "digest": "sha256:" + hashlib.sha256(b'{"a":"\xc3\xa9","z":[true,null,7]}').hexdigest()})
        self.assertEqual(reseal(result), result)

    @unittest.skipUnless(shutil.which("cc"), "child input observer requires a C compiler")
    def test_child_input_observer_records_changed_bytes_and_restores_only_when_requested(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            checker, script = root / "checker", root / "script"
            subprocess.run([shutil.which("cc"), "-O2", "-Wall", "-Wextra", "-Werror",
                            ROOT / FIXTURES / "child-input-checker.c", "-o", checker], check=True,
                           capture_output=True, timeout=30)
            original = b"#!/bin/sh\nexit 0\n"
            for mode in (0, 1, 2):
                script.write_bytes(original)
                (root / "checker.change").write_text(str(mode))
                result = subprocess.run([checker, "--norc", "--format=json1", "--shell=sh",
                                         "--severity=style", "--", script], cwd=root, env={"LC_ALL": "C"},
                                        capture_output=True, timeout=10)
                self.assertEqual((result.returncode, result.stdout, result.stderr), (0, b'{"comments":[]}', b""))
                self.assertEqual((root / "checker.saw").read_bytes() == original, mode == 0)
                self.assertEqual(script.read_bytes() == original, mode != 1)
            self.assertEqual((root / "checker.launches").read_bytes(), b"launch\n" * 3)

    def test_injected_results_bind_inventory_host_success_and_exact_fault_set(self):
        cases = check_cases(ROOT / FIXTURES)["prepare"]
        case = next(row for row in cases if row["name"] == "failure-file-sync")
        row = {"name": case["name"], "observed": case["expected"],
               "host": "aarch64-apple-darwin", "inventory_digest": "sha256:bound-inventory"}

        def completed(rows, code=0):
            stdout = b"runner output\n" + b"".join(
                b"OPAAL_PREPARATION_CASE " + json.dumps(item).encode() + b"\n" for item in rows)
            return subprocess.CompletedProcess([], code, stdout, b"")

        def consume(result):
            return injected_results(result, cases, [case["name"]], row["host"], row["inventory_digest"])

        self.assertEqual(consume(completed([row])), [{"name": case["name"], "observed": case["expected"]}])
        for result in (completed([]), completed([row, row]), completed([row], code=1),
                       completed([{**row, "name": "unknown"}]),
                       completed([{**row, "observed": {}}]),
                       completed([{**row, "host": "x86_64-unknown-linux-gnu"}]),
                       completed([{**row, "inventory_digest": "old"}])):
            with self.assertRaises(QualificationError):
                consume(result)

    def test_preparation_results_require_exact_case_and_expectation_sets(self):
        cases = check_cases(ROOT / FIXTURES)["prepare"]
        host = "aarch64-apple-darwin"
        applicable = [row for row in cases if host in row["hosts"]]
        observations = [{"name": row["name"], "observed": row["expected"]} for row in applicable]
        result = case_results(cases, observations, [], host)
        self.assertEqual(len(result["observed"]) + len(result["inapplicable"]), 49)
        for changed, pending in ((observations[:-1], []), (observations + observations[:1], []),
                                 (observations, [observations[0]["name"]]),
                                 (observations + [{"name": "unlisted", "observed": {}}], []),
                                 ([{**observations[0], "observed": {}}] + observations[1:], []),
                                 (observations[1:], [row["name"] for row in cases if host not in row["hosts"]])):
            with self.subTest(changed=changed[:1], pending=pending):
                with self.assertRaises(QualificationError):
                    case_results(cases, changed, pending, host)
        result = case_results(cases, observations[1:], [observations[0]["name"]], host)
        self.assertEqual(result["pending"], [observations[0]["name"]])

    def test_preparation_preservation_observer_does_not_open_fifo_or_follow_links(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            os.mkfifo(root / "fifo")
            (root / "link").symlink_to("missing")
            (root / "file").write_bytes(b"preserve")
            original = snapshot(root)
            self.assertEqual(snapshot(root), original)
            (root / "file").write_bytes(b"changed")
            self.assertNotEqual(snapshot(root), original)
            self.assertIsNone(snapshot(root / "missing"))

    def test_preparation_asset_observer_rejects_missing_changed_and_extra_files(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            reference = ROOT / FIXTURES / "reference"
            for name in ("README.md", "policy.opaal", "policy-tests.opaal", "source.opaal",
                         "saved-tasks.opaal", "tasks.opaal", "offline-authority.toml"):
                shutil.copyfile(reference / name, root / name)
            shutil.copytree(reference / "fixtures", root / "fixtures")
            shutil.copyfile(reference / "offline.toml", root / "opaal.toml")
            shutil.copyfile(reference / "offline-tools-macos.toml", root / "offline-tools.toml")
            self.assertTrue(asset_observations(root, reference, "aarch64-apple-darwin"))
            (root / "unexpected").write_bytes(b"")
            self.assertFalse(asset_observations(root, reference, "aarch64-apple-darwin"))
            (root / "unexpected").unlink()
            (root / "README.md").write_bytes(b"changed")
            self.assertFalse(asset_observations(root, reference, "aarch64-apple-darwin"))
            (root / "README.md").unlink()
            self.assertFalse(asset_observations(root, reference, "aarch64-apple-darwin"))

    def test_fault_inventories_are_frozen_before_producers_and_packaging(self):
        inventories = check_cases(ROOT / FIXTURES)
        self.assertEqual({name: len(rows) for name, rows in inventories.items()},
                         {"image": 115, "prepare": 49, "evidence": 130})
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for name in inventories:
                shutil.copyfile(ROOT / FIXTURES / (name + "-cases.json"), root / (name + "-cases.json"))
            for name in inventories:
                path = root / (name + "-cases.json")
                original = path.read_bytes()
                for mutation in ("omitted", "changed-expectation"):
                    with self.subTest(inventory=name, mutation=mutation):
                        changed = json.loads(original)
                        if mutation == "omitted":
                            changed["cases"].pop()
                        else:
                            changed["cases"][0]["expected"] = {}
                        path.write_text(json.dumps(changed))
                        with self.assertRaisesRegex(ValueError, "native case inventory differs"):
                            check_cases(root)
                path.write_bytes(original)

    def test_frozen_inventory_and_independent_saved_expectations(self):
        rows, inventory = corpus(ROOT / FIXTURES)
        self.assertEqual((len(rows), len(inventory["saved_cases"]), len(inventory["live_cases"])), (64, 60, 56))
        self.assertEqual(inventory["real_checker_cases"], ["clean", "findings", "negative-manifest", "invalid-toml"])
        for row in rows:
            if row["name"] not in inventory["saved_cases"]:
                continue
            with self.subTest(case=row["name"]):
                if expected_exit(row) == 2:
                    with self.assertRaisesRegex(ValueError, "^Saved repository input failed\\.$"):
                        comparison.saved(payload(row, "saved"))
                else:
                    actual = comparison.saved(payload(row, "saved"))
                    self.assertEqual(json.dumps(actual, separators=(",", ":"), sort_keys=True), row["report"])

    def test_inventory_omission_is_rejected_before_any_producer(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for name in ("reference-cases.json", "comparison-cases.json"):
                shutil.copyfile(ROOT / FIXTURES / name, root / name)
            document = json.loads((root / "reference-cases.json").read_bytes())
            document["cases"].pop()
            (root / "reference-cases.json").write_text(json.dumps(document))
            with self.assertRaisesRegex(QualificationError, "inventory differs"):
                corpus(root)

    def test_case_names_cannot_escape_fresh_evidence_directory(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            reference = {"schema_version": 1, "cases": [{"name": "../escape"}]}
            comparison = {"schema_version": 1, "reference_cases": ["../escape"]}
            (root / "reference-cases.json").write_text(json.dumps(reference))
            (root / "comparison-cases.json").write_text(json.dumps(comparison))
            with self.assertRaisesRegex(QualificationError, "safe evidence basename"):
                corpus(root)

    def test_safe_report_oracle_rejects_wrong_bytes_exits_and_payload_leaks(self):
        rows, _ = corpus(ROOT / FIXTURES)
        clean = rows[0]
        for code, stdout, stderr in ((1, clean["report"].encode(), b""),
                                     (0, clean["report"].encode() + b"\n", b""),
                                     (0, clean["report"].encode(), b"SENSITIVE_CANARY")):
            with self.subTest(code=code, stdout=stdout, stderr=stderr):
                with self.assertRaises(QualificationError):
                    assert_result(subprocess.CompletedProcess([], code, stdout, stderr), clean, "oracle")

    def test_comparison_cli_bounds_saved_bytes_and_uses_fixed_safe_errors(self):
        for source in (b"\xffSENSITIVE_CANARY", b" " * (comparison.SAVED_LIMIT + 1)):
            result = subprocess.run([sys.executable, ROOT / "examples/repository-check/comparison.py"],
                                    input=source, capture_output=True, timeout=10)
            self.assertEqual((result.returncode, result.stdout, result.stderr),
                             (2, b"", b"Saved repository input failed.\n"))

    def test_live_capture_bounds_each_stream_with_safe_acquisition_failure(self):
        with tempfile.TemporaryDirectory() as temporary:
            tool = Path(temporary) / "checker"
            for stream in ("", " >&2"):
                tool.write_text("#!/bin/sh\nprintf '%0100d' 0" + stream + "\n")
                tool.chmod(0o700)
                with patch.object(comparison, "OUTPUT_LIMIT", 64):
                    with self.assertRaisesRegex(ValueError, "^Checker acquisition failed\\.$"):
                        comparison.live(tool, tool, tool)

    def test_live_files_reject_fifo_and_first_excess_without_blocking(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "input"
            path.write_bytes(b"a" * comparison.FILE_LIMIT)
            self.assertEqual(len(comparison.read_file(path)), comparison.FILE_LIMIT)
            path.write_bytes(b"a" * (comparison.FILE_LIMIT + 1))
            with self.assertRaises(ValueError):
                comparison.read_file(path)
            path.unlink()
            os.mkfifo(path)
            with self.assertRaises(ValueError):
                comparison.read_file(path)

    def test_copies_and_deterministic_bundle_preserve_all_inputs(self):
        check_copies(ROOT)
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            shutil.copytree(ROOT / FIXTURES, root / FIXTURES, ignore=shutil.ignore_patterns("__pycache__"))
            shutil.copytree(ROOT / "examples/repository-check", root / "examples/repository-check",
                            ignore=shutil.ignore_patterns("__pycache__"))
            check_copies(root)
            paths = sorted(path.relative_to(root / FIXTURES).as_posix()
                           for path in (root / FIXTURES).rglob("*") if path.is_file())
            archive = package_examples(root, paths, "a" * 40, directory=FIXTURES, name="controlled-native-tools")
            original = archive.read_bytes()
            package_examples(root, paths, "a" * 40, directory=FIXTURES, name="controlled-native-tools")
            self.assertEqual(archive.read_bytes(), original)
            copied, _ = unpack(archive, root / "unpacked", kind="native-fixtures", version=VERSION,
                               source="a" * 40, platform="portable")
            for name in paths:
                self.assertEqual((copied / name).read_bytes(), (root / FIXTURES / name).read_bytes())
            (root / "examples/repository-check/policy.opaal").write_text("changed")
            with self.assertRaisesRegex(ValueError, "differ"):
                check_copies(root)


if __name__ == "__main__":
    unittest.main()
