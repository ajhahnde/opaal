#!/usr/bin/env python3
"""Compare report processing and callable construction with unchanged wall/CPU/RSS gates."""
import argparse, base64, hashlib, importlib.util, json, os, platform, re, statistics, subprocess, sys, time
from pathlib import Path

ROOT = None
PROJECT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(PROJECT / "ci"))
from qualify_data_processing import MEASURE

def sha(data): return hashlib.sha256(data).hexdigest()

def measured(command, cwd):
    worker = subprocess.run([sys.executable, "-c", MEASURE, json.dumps([str(x) for x in command])], cwd=cwd,
                            env={"PATH": "/usr/bin:/bin", "LC_ALL": "C"}, capture_output=True, timeout=120)
    assert worker.returncode == 0, worker.stderr
    result = json.loads(worker.stdout)
    out = base64.b64decode(result.pop("stdout"))
    err = base64.b64decode(result.pop("stderr"))
    assert result.pop("returncode") == 0 and not err, err
    result["stdout_sha256"] = sha(out)
    destination = ROOT/"outputs"/(sha(out)+".stdout")
    if not destination.exists(): destination.write_bytes(out)
    return result, out

def row(name, commands, cwd, expected, large=False):
    def canonical(output):
        if name.startswith("restore-") or name.startswith("retained-") or name.startswith("callback-"):
            match = re.search(rb"PHASE (?:restore padding=\d+|first-use mode=[a-z-]+) repeats=\d+\n", output)
            assert match, output
            return match.group(0)
        return output
    warmups, samples = {k:[] for k in commands}, {k:[] for k in commands}
    for _ in range(3):
        for engine, command in commands.items():
            sample, output = measured(command, cwd)
            assert canonical(output) == expected, (name, engine, output[:200], expected[:200])
            warmups[engine].append(sample)
    order = []
    for pair in range(7):
        engines = ["baseline", "candidate"] if pair % 2 == 0 else ["candidate", "baseline"]
        order.append(engines)
        for engine in engines:
            sample, output = measured(commands[engine], cwd)
            assert canonical(output) == expected, (name, engine)
            sample["cpu_seconds"] = sample["user_seconds"] + sample["system_seconds"]
            samples[engine].append(sample)
    summary = {}
    for engine, values in samples.items():
        summary[engine] = {"median_wall_seconds": statistics.median(s["wall_seconds"] for s in values),
                           "median_cpu_seconds": statistics.median(s["cpu_seconds"] for s in values),
                           "max_peak_rss_bytes": max(s["peak_rss_bytes"] for s in values)}
    gates = {}
    for metric in ["wall_seconds", "cpu_seconds"]:
        before = [s[metric] for s in samples["baseline"]]
        after = [s[metric] for s in samples["candidate"]]
        median = statistics.median(before)
        if large:
            tolerance = max(.05 * median, max(before)-min(before))
            gates[metric] = {"required_each_pair_gain_seconds": tolerance,
                             "paired_gains_seconds": [a-b for a,b in zip(before,after)],
                             "pass": all(a-b > tolerance for a,b in zip(before,after))}
        else:
            tolerance = max(.05 * median, .002)
            regression = statistics.median(after)-median
            gates[metric] = {"median_regression_seconds": regression, "tolerance_seconds": tolerance,
                             "pass": regression <= tolerance}
    rss = summary["baseline"]["max_peak_rss_bytes"]
    ceiling = rss + max(.05 * rss, 1024*1024)
    gates["rss"] = {"ceiling_bytes": ceiling, "pass": summary["candidate"]["max_peak_rss_bytes"] <= ceiling}
    result = {"case": name, "commands": {k:[str(x) for x in v] for k,v in commands.items()},
              "expected_sha256": sha(expected), "warmups": warmups, "pair_order": order,
              "samples": samples, "summary": summary, "gates": gates,
              "pass": all(v["pass"] for v in gates.values()),
              "load_observations": os.getloadavg()}
    print(json.dumps({"case":name, "summary":summary, "pass":result["pass"]}), flush=True)
    return result

def main():
    global ROOT
    parser = argparse.ArgumentParser(description=__doc__)
    for engine in ["before", "after"]:
        parser.add_argument(f"--{engine}", type=Path, required=True, help="optimized CLI")
        parser.add_argument(f"--{engine}-raw", type=Path, required=True, help="optimized public-API harness")
        parser.add_argument(f"--{engine}-phases", type=Path, required=True, help="optimized uninstrumented phase harness")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if (platform.system(), platform.machine()) not in [("Darwin", "arm64"), ("Linux", "x86_64")]:
        parser.error("qualification requires macOS arm64 or Linux x86_64")
    output = args.output.resolve()
    if output.exists():
        parser.error("output already exists; retain each attempt separately")
    ROOT = output.parent / (output.stem + "-artifacts")
    ROOT.mkdir(parents=True, exist_ok=False)
    (ROOT / "outputs").mkdir()
    cli = {"baseline":args.before.resolve(), "candidate":args.after.resolve()}
    raw = {"baseline":args.before_raw.resolve(), "candidate":args.after_raw.resolve()}
    phases = {"baseline":args.before_phases.resolve(), "candidate":args.after_phases.resolve()}
    binaries = list(cli.values()) + list(raw.values()) + list(phases.values())
    if not all(p.is_file() for p in binaries):
        parser.error("all six optimized binaries must exist")
    work = ROOT / "inputs"
    work.mkdir()
    fixture = PROJECT / "tests/golden/data-processing"
    for name in ["report.opaal", "json-report.opaal", "text-report.opaal", "reference.py"]:
        (work/name).write_bytes((fixture/name).read_bytes())
    result = {"schema":"opaal-report-performance-v1", "host":platform.platform(), "machine":platform.machine(), "python":sys.version,
              "protocol":"3 discarded warmups + 7 alternating pairs, independent process workers",
              "binaries_sha256":{str(p):sha(p.read_bytes()) for p in binaries},
              "fixtures_sha256":{name:sha((work/name).read_bytes()) for name in ["report.opaal", "json-report.opaal", "text-report.opaal", "reference.py"]},
              "started":time.time(), "rows":[], "invalid_cases":[],
              "evidence_scope":"One paired same-host optimized candidate measurement; source/build identities and the other host are separate required evidence."}
    contract_path = PROJECT / "benchmarks/report-contract-v1.json"
    contract = json.loads(contract_path.read_text())
    result["contract_sha256"] = sha(contract_path.read_bytes())
    recipes = {item["case"]:item for item in contract["cases"]}
    seen = set()
    def checked_row(name, commands, cwd, expected, large=False):
        recipe = recipes[name]
        assert name not in seen and sha(expected) == recipe["expected_sha256"], name
        seen.add(name)
        for engine, command in commands.items():
            identities = {str(cli[engine]):"<cli>", str(raw[engine]):"<raw>", str(phases[engine]):"<phases>"}
            assert [identities.get(str(token), str(token)) for token in command] == recipe["commands"][engine], name
        value = row(name, commands, cwd, expected, large)
        assert value["pair_order"] == recipe["pair_order"]
        return value
    def save(): output.write_text(json.dumps(result,indent=2)+"\n")
    for count in [0,1,100,1000,10000]:
        jobs = [{"name":f"job/{index % 101:03}/{index:05}","status":"completed","conclusion":"failure" if index % 3 == 0 else "success"} for index in range(count)]
        inputs = json.dumps({"jobs":jobs}, separators=(",", ":")).encode()
        (work/"jobs.json").write_bytes(inputs)
        (work/f"jobs-{count}.json").write_bytes(inputs)
        expected = subprocess.check_output([sys.executable, work/"reference.py", work/"jobs.json"],cwd=work)
        commands = {e:[p,"json-report.opaal"] for e,p in cli.items()}
        value = checked_row(f"report-{count}", commands, work, expected,large=count>=1000)
        (ROOT/"outputs"/f"report-{count}-expected.json").write_bytes(expected)
        value["input_sha256"] = sha(inputs)
        result["rows"].append(value); save()
    (work/"empty.opaal").write_text("")
    result["rows"].append(checked_row("fresh-empty-startup", {e:[p,"empty.opaal"] for e,p in cli.items()},work,b"")); save()
    for name, count, padding, repeats in [("raw-one-unused",1,0,2000), ("raw-many-unused",128,0,10),
                                        ("raw-large-many-unused",128,32768,10), ("raw-large-source",1,262144,20)]:
        commands = {e:[p,"construct",count,padding,repeats] for e,p in raw.items()}
        expected = f"constructed={count} padding={padding} repeats={repeats}\n".encode()
        result["rows"].append(checked_row(name, commands,work,expected)); save()
    for padding in [0,131072]:
        repeats = 500
        result["rows"].append(checked_row(f"raw-repeated-closure-{padding}", {e:[p,"closure",padding,repeats] for e,p in raw.items()},
                                  work,f"closure padding={padding} repeats={repeats}\n".encode())); save()
    for padding,repeats in [(0,2000),(32768,200)]:
        commands = {}
        for e in cli:
            binary = str(phases[e])
            commands[e]=["/usr/bin/env",f"REPORT_PADDING={padding}",f"REPORT_REPEATS={repeats}",binary,
                         "eval::tests::callable_classification::measure_restore_phase","--exact","--ignored","--nocapture","--test-threads=1"]
        result["rows"].append(checked_row(f"restore-{padding}",commands,work,f"PHASE restore padding={padding} repeats={repeats}\n".encode())); save()
    for stage,padding,repeats in [("cold",0,2000),("cold",131072,100),("warm",131072,500)]:
        result["rows"].append(checked_row(f"call-{stage}-{padding}",{e:[p,"call",stage,padding,repeats] for e,p in raw.items()},work,f"call {stage} padding={padding} repeats={repeats}\n".encode()));save()
    for name,mode,repeats in [("callback-empty-preparation","empty-preparation",2000),("callback-repeated-preparation","nonempty-preparation",2000),("retained-originals","originals",50000),("retained-clones","clones",50000)]:
        commands={}
        for e in cli:
            binary = str(phases[e])
            commands[e]=["/usr/bin/env",f"REPORT_MODE={mode}",f"REPORT_REPEATS={repeats}",binary,"eval::tests::callable_classification::measure_first_use_phase","--exact","--ignored","--nocapture","--test-threads=1"]
        result["rows"].append(checked_row(name,commands,work,f"PHASE first-use mode={mode} repeats={repeats}\n".encode()));save()
    for path in sorted((fixture/"invalid").glob("*.json")):
        (work/"jobs.json").write_bytes(path.read_bytes())
        outcomes=[]
        for e,p in cli.items():
            completed=subprocess.run([p,"json-report.opaal"],cwd=work,env={"PATH":"/usr/bin:/bin"},capture_output=True,timeout=20)
            outcomes.append((completed.returncode,completed.stdout,completed.stderr))
        for engine,outcome in zip(cli,outcomes):
            (ROOT/"outputs"/f"{engine}-{path.stem}.stdout").write_bytes(outcome[1])
            (ROOT/"outputs"/f"{engine}-{path.stem}.stderr").write_bytes(outcome[2])
        assert outcomes[0]==outcomes[1] and outcomes[0][0]!=0, path
        result["invalid_cases"].append({"case":path.name,"input_sha256":sha(path.read_bytes()),"exit":outcomes[0][0],
                                         "stdout_sha256":sha(outcomes[0][1]),"stderr_sha256":sha(outcomes[0][2])}); save()
    for name in ["empty", "pending", "conclusions", "stable-prefix", "escaped", "producer-shaped"]:
        (work/"jobs.json").write_bytes((fixture/"valid"/f"{name}.json").read_bytes())
        for script,suffix in [("json-report.opaal","json"),("text-report.opaal","txt")]:
            expected=(fixture/"valid"/f"{name}.expected.{suffix}").read_bytes()
            for e,p in cli.items():
                completed=subprocess.run([p,script],cwd=work,env={"PATH":"/usr/bin:/bin"},capture_output=True,timeout=20)
                (ROOT/"outputs"/f"{e}-{name}.{suffix}").write_bytes(completed.stdout)
                assert completed.returncode==0 and not completed.stderr and completed.stdout==expected,(name,e,script)
    result["valid_json_text_cases"] = 24
    result["finished"] = time.time()
    assert seen == set(recipes), "incomplete performance case coverage"
    result["all_measured_gates_pass"] = all(r["pass"] for r in result["rows"])
    save()
    if not result["all_measured_gates_pass"]:
        raise SystemExit("performance qualification failed; retain this attempt")

if __name__ == "__main__": main()
