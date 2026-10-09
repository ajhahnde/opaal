#![cfg(any(target_os = "macos", target_os = "linux"))]
#![forbid(unsafe_code)]
#[path = "support/data_processing.rs"]
mod support;

use std::io::{Read, Write};
use std::path::Path;
use std::process::Stdio;
use support::{ReportDir, failure, success};

fn controlled_fixture(source: &str) -> ReportDir {
    let work = ReportDir::new();
    work.write(
        "opaal.toml",
        r#"schema_version = 1
[project]
name = "sample"
root_module = "tasks.opaal"
required_opaal = ">=1.2.0,<2.0.0"
[paths]
root = "."
evidence = "result.json"
[environments.ci]
authority = "authority.toml"
tool_lock = "tools.toml"
"#,
    );
    work.write("tasks.opaal", source);
    work.write(
        "authority.toml",
        r#"schema_version = 1
project = "sample"
environment = "ci"
[[rules]]
decision = "grant"
effect = "entropy.system"
scope = "evaluation"
required_enforcement = "enforced"
[[rules]]
decision = "grant"
effect = "filesystem.read"
scope = "project.root"
required_enforcement = "enforced"
"#,
    );
    let platform = if cfg!(target_os = "macos") {
        "aarch64-apple-darwin"
    } else {
        "x86_64-unknown-linux-gnu"
    };
    work.write(
        "tools.toml",
        format!(
            r#"schema_version = 1
project = "sample"
environment = "ci"
platform = "{platform}"
tools = []
[child_environment]
inherit = []
"#
        ),
    );
    work
}

fn controlled_plan(work: &ReportDir) -> opaal_runtime::workflow::PlanArtifact {
    success(&work.run(&[
        "plan",
        "--project",
        "opaal.toml",
        "--task",
        "sample",
        "--environment",
        "ci",
        "--expires-in",
        "900s",
        "--out",
        "sample.plan.json",
    ]));
    let inspected = work.run(&["plan", "inspect", "sample.plan.json"]);
    success(&inspected);
    assert!(String::from_utf8_lossy(&inspected.stdout).contains("entropy.system evaluation"));
    opaal_runtime::workflow::PlanArtifact::parse(
        &std::fs::read(work.0.join("sample.plan.json")).unwrap(),
    )
    .unwrap()
}

fn controlled_execute(
    work: &ReportDir,
    plan: &opaal_runtime::workflow::PlanArtifact,
) -> std::process::Output {
    work.run(&[
        "execute",
        "--plan",
        "sample.plan.json",
        "--accept",
        plan.digest(),
        "--run-id",
        "0123456789abcdef0123456789abcdef",
        "--journal",
        "sample.run.jsonl",
    ])
}

fn journal_records(work: &ReportDir) -> Vec<serde_json::Value> {
    let bytes = std::fs::read(work.0.join("sample.run.jsonl")).unwrap();
    let audit = opaal_runtime::workflow::audit_journal(&bytes).unwrap();
    assert!(audit.is_complete());
    assert_eq!(audit.value()["schema"], "opaal.audit.v3");
    success(&work.run(&[
        "audit",
        "--project",
        "opaal.toml",
        "--journal",
        "sample.run.jsonl",
        "--out",
        "sample.audit.json",
    ]));
    let inspected = work.run(&["audit", "inspect", "sample.audit.json"]);
    success(&inspected);
    let text = String::from_utf8(inspected.stdout).unwrap();
    for canary in [
        "path-canary-4729",
        "payload-canary-4729",
        "error-canary-4729",
        "http-payload-canary",
    ] {
        assert!(!text.contains(canary));
        assert!(!text.contains(&opaal_runtime::workflow::digest_bytes(canary.as_bytes())));
    }
    std::str::from_utf8(&bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn controlled_native_calls_bind_selected_grants_and_emit_only_progress() {
    let work = controlled_fixture(
        r#"import std::random as random
action draw() -> Int effects { entropy.system(); } { return random::int(0, 4) }
action sample() -> Record effects { entropy.system(); } {
    let shard = draw()
    let fraction = random::float()
    let identifier = random::bytes(3)
    let empty = random::bytes(0)
    let fixed = random::int(7, 8)
    return ({ shard: shard, fraction: fraction, identifier: identifier, fixed: fixed })
}
task sample = sample
"#,
    );
    let checked = work.run(&[
        "check",
        "--project",
        "opaal.toml",
        "--task",
        "sample",
        "--environment",
        "ci",
        "--format",
        "json",
    ]);
    success(&checked);
    let checked = opaal_runtime::workflow::CheckArtifact::parse(&checked.stdout).unwrap();
    assert_eq!(checked.value()["schema"], "opaal.check.v3");
    assert!(!work.0.join("sample.run.jsonl").exists());
    let plan = controlled_plan(&work);
    assert_eq!(plan.value()["schema"], "opaal.plan.v3");
    assert_eq!(
        plan.value()["standard_host"],
        checked.value()["standard_host"]
    );
    let executed = controlled_execute(&work, &plan);
    assert!(
        executed.status.success(),
        "{executed:?}\n{}",
        std::fs::read_to_string(work.0.join("sample.run.jsonl")).unwrap()
    );
    let records = journal_records(&work);
    let effects = records
        .iter()
        .filter(|record| record["kind"] == "effect-after")
        .collect::<Vec<_>>();
    assert_eq!(effects.len(), 5);
    for (effect, bytes) in effects.iter().zip([8, 8, 3, 0, 0]) {
        assert_eq!(effect["payload"]["operation"]["confirmed_bytes"], bytes);
        assert_eq!(effect["payload"]["operation"]["admitted_bytes"], bytes);
        assert_eq!(
            effect["payload"]["operation"]["uncertain_bytes_upper_bound"],
            0
        );
        assert!(effect["payload"]["outcome"]["value_digest"].is_null());
    }
    for record in records {
        for field in ["outcome", "primary"] {
            if let Some(outcome) = record["payload"].get(field) {
                assert_eq!(outcome["message"], "metadata-only outcome");
                assert!(outcome["value_digest"].is_null());
            }
        }
    }
}

#[test]
fn untaken_entropy_declaration_projects_sibling_paths_payloads_and_errors() {
    for fail in [false, true] {
        let work = controlled_fixture(&format!(
            r#"import std::random as random
import std::filesystem as fs
import std::path as path
import std::data as data
import std::string as string
import project::context as project
action sibling(suffix: Int) -> Bytes effects {{ filesystem.read(project::root); }} {{
    let name = string::join(['path-canary-', string::decode_utf8(data::json_encode(suffix))], '')
    return fs::read(path::join(project::root, name), 64)
}}
action sample() -> Bytes effects {{ entropy.system(); filesystem.read(project::root); }} {{
    if false {{ let ignored = random::float() }}
    let min = random::int(4729, 4730)
    let suffix = random::int(min, min + 1)
    let bytes = sibling(suffix)
    {}
    return bytes
}}
task sample = sample
"#,
            if fail {
                "throw 'error-canary-4729'"
            } else {
                ""
            }
        ));
        work.write("path-canary-4729", "payload-canary-4729");
        let plan = controlled_plan(&work);
        let executed = controlled_execute(&work, &plan);
        if fail {
            failure(&executed);
        } else {
            success(&executed);
        }
        let records = journal_records(&work);
        let bytes = std::fs::read(work.0.join("sample.run.jsonl")).unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        for canary in [
            "path-canary-4729",
            "payload-canary-4729",
            "error-canary-4729",
        ] {
            assert!(!text.contains(canary));
            assert!(!text.contains(&opaal_runtime::workflow::digest_bytes(canary.as_bytes())));
        }
        let effect = records
            .iter()
            .find(|record| {
                record["kind"] == "effect-after" && record["payload"]["effect"] == "filesystem.read"
            })
            .unwrap();
        assert!(effect["payload"]["operation"]["relative_path"].is_null());
        assert!(effect["payload"]["operation"]["evidence_digest"].is_null());
        for record in &records {
            if record["payload"]["effect"] == "entropy.system" {
                let operation = record["payload"]["operation"].as_object().unwrap();
                assert!(!operation.contains_key("min"));
                assert!(!operation.contains_key("max"));
            }
        }
        assert_eq!(
            records
                .iter()
                .filter(|record| record["kind"] == "effect-after"
                    && record["payload"]["effect"] == "entropy.system")
                .count(),
            2
        );
        assert_eq!(
            records.last().unwrap()["payload"]["primary"]["class"],
            if fail { "error" } else { "success" }
        );
    }
}

#[test]
fn nested_random_derived_http_methods_bodies_and_errors_never_enter_evidence() {
    for fail in [false, true] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let work = controlled_fixture(&format!(
            r#"import std::random as random
import std::http as http
import project::endpoints as endpoints
action draw() -> Int effects {{ entropy.system(); }} {{ return random::int(0, 2) }}
action sibling(method: String, body: Bytes) -> Bytes effects {{ network.http(endpoints::sample); }} {{
    let response = http::request(endpoints::sample, method, {{}}, null, body, 64)
    return response.body
}}
action sample() -> Bytes effects {{ entropy.system(); network.http(endpoints::sample); }} {{
    let selector = draw()
    mut method = 'GET'
    if selector == 1 {{ method = 'POST' }}
    let body = random::bytes(16)
    let response = sibling(method, body)
    {}
    return response
}}
task sample = sample
"#,
            if fail { "throw method" } else { "" }
        ));
        let manifest = std::fs::read_to_string(work.0.join("opaal.toml")).unwrap();
        work.write("opaal.toml", format!("{manifest}\n[endpoints.sample]\nurl = \"http://127.0.0.1:{port}/\"\nmethods = [\"GET\", \"POST\"]\nsecret_headers = []\ntls = false\n"));
        let authority = std::fs::read_to_string(work.0.join("authority.toml")).unwrap();
        work.write("authority.toml", format!("{authority}\n[[rules]]\ndecision = \"grant\"\neffect = \"network.http\"\nscope = \"endpoint.sample\"\nrequired_enforcement = \"enforced\"\n"));
        let plan = controlled_plan(&work);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        let server = std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(started.elapsed() < std::time::Duration::from_secs(5));
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let method = String::from_utf8(request)
                .unwrap()
                .split(' ')
                .next()
                .unwrap()
                .to_owned();
            let mut body = [0; 16];
            stream.read_exact(&mut body).unwrap();
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 19\r\nConnection: close\r\n\r\nhttp-payload-canary").unwrap();
            (method, body)
        });
        let executed = controlled_execute(&work, &plan);
        let (method, body) = server.join().unwrap();
        if fail {
            failure(&executed);
        } else {
            success(&executed);
        }
        let records = journal_records(&work);
        let bytes = std::fs::read(work.0.join("sample.run.jsonl")).unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(!text.contains("http-payload-canary"));
        for payload in [
            method.as_bytes(),
            body.as_slice(),
            b"http-payload-canary".as_slice(),
        ] {
            assert!(!text.contains(&opaal_runtime::workflow::digest_bytes(payload)));
        }
        for record in &records {
            if (record["kind"] == "effect-before" || record["kind"] == "effect-after")
                && record["payload"]["effect"] == "network.http"
            {
                assert_eq!(
                    record["payload"]["scope"].get("method"),
                    Some(&serde_json::Value::Null)
                );
                assert_eq!(
                    record["payload"]["operation"].get("method"),
                    Some(&serde_json::Value::Null)
                );
                assert_eq!(
                    record["payload"]["operation"].get("evidence_digest"),
                    Some(&serde_json::Value::Null)
                );
            }
            for field in ["outcome", "primary"] {
                if let Some(outcome) = record["payload"].get(field) {
                    assert!(outcome["value_digest"].is_null());
                    assert_eq!(outcome["message"], "metadata-only outcome");
                }
            }
        }
        assert_eq!(
            records
                .iter()
                .filter(|record| record["kind"] == "effect-before"
                    && record["payload"]["effect"] == "network.http")
                .count(),
            1
        );
        assert_eq!(
            records
                .iter()
                .filter(|record| record["kind"] == "effect-after"
                    && record["payload"]["effect"] == "network.http")
                .count(),
            1
        );
        assert_eq!(
            records.last().unwrap()["payload"]["primary"]["class"],
            if fail { "error" } else { "success" }
        );
    }
}

#[test]
fn altered_accepted_limits_and_denied_grants_refuse_before_journal_creation() {
    let source = "import std::random as random\naction sample() -> Int effects { entropy.system(); } { return random::int(7, 8) }\ntask sample = sample\n";
    let work = controlled_fixture(source);
    let plan = controlled_plan(&work);
    let mut changed = plan.value().clone();
    changed["standard_host"]["max_host_bytes"] = serde_json::json!(1);
    changed.as_object_mut().unwrap().remove("digest");
    let changed = opaal_runtime::workflow::PlanArtifact::seal(changed).unwrap();
    work.write("sample.plan.json", changed.bytes());
    failure(&controlled_execute(&work, &changed));
    assert!(!work.0.join("sample.run.jsonl").exists());
    let authority = std::fs::read_to_string(work.0.join("authority.toml")).unwrap();
    work.write(
        "authority.toml",
        authority
            .replacen("decision = \"grant\"", "decision = \"deny\"", 1)
            .replacen("required_enforcement = \"enforced\"\n", "", 1),
    );
    let checked = work.run(&[
        "check",
        "--project",
        "opaal.toml",
        "--task",
        "sample",
        "--environment",
        "ci",
        "--format",
        "json",
    ]);
    failure(&checked);
    let checked = opaal_runtime::workflow::CheckArtifact::parse(&checked.stdout).unwrap();
    assert_eq!(checked.value()["schema"], "opaal.check.v3");
    assert!(!checked.is_executable());
}

#[test]
fn public_source_uses_the_real_same_image_worker_and_checks_domains() {
    let work = ReportDir::new();
    std::os::unix::fs::symlink("/usr/bin/printf", work.0.join("bin/printf")).unwrap();
    work.write(
        "random.opaal",
        std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/golden/random-values/source.opaal"),
        )
        .unwrap(),
    );
    success(&work.run(&["check", "random.opaal"]));
    success(&work.run(&["format", "--check", "random.opaal"]));
    let output = work.run(&["random.opaal"]);
    success(&output);
    let text = std::str::from_utf8(&output.stdout).unwrap();
    let parts = text.trim().split(' ').collect::<Vec<_>>();
    assert_eq!(parts.len(), 2);
    let shard: i64 = parts[0].strip_prefix("shard=").unwrap().parse().unwrap();
    let fraction: f64 = parts[1].strip_prefix("fraction=").unwrap().parse().unwrap();
    assert!((0..4).contains(&shard));
    assert!((0.0..1.0).contains(&fraction));
    assert_eq!(
        fraction * 2.0_f64.powi(53),
        (fraction * 2.0_f64.powi(53)).trunc()
    );
}

#[test]
fn native_calls_share_the_cumulative_host_byte_ceiling() {
    let work = ReportDir::new();
    let mut source = String::from("import std::random as random\n");
    for index in 0..8 {
        source.push_str(&format!("let bytes_{index} = random::bytes(1048576)\n"));
    }
    work.write("exact.opaal", &source);
    success(&work.run(&["exact.opaal"]));
    source.push_str("random::bytes(1)\n");
    work.write("excess.opaal", source);
    let output = work.run(&["excess.opaal"]);
    failure(&output);
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("evaluation exceeded its resource budget")
    );
}

#[test]
fn domain_errors_pure_wrappers_and_imported_initializers_fail_without_output() {
    let work = ReportDir::new();
    for (body, code) in [
        ("random::int(4, 4)", "RANDOM001"),
        ("random::bytes(-1)", "RANDOM001"),
        ("random::bytes(1048577)", "RANDOM001"),
        (
            "def pure() -> Int { return random::int(7, 8) }\npure()",
            "refused[unsupported]",
        ),
        (
            "let callback = {|| random::bytes(0)}\ncallback()",
            "refused[unsupported]",
        ),
    ] {
        work.write(
            "invalid.opaal",
            format!("import std::random as random\n{body}\n"),
        );
        let output = work.run(&["invalid.opaal"]);
        failure(&output);
        assert!(output.stdout.is_empty());
        assert!(std::str::from_utf8(&output.stderr).unwrap().contains(code));
    }
    work.write(
        "dependency.opaal",
        "import std::random as random\nlet sample = random::bytes(0)\nexport { sample }\n",
    );
    work.write(
        "root.opaal",
        "import './dependency.opaal' as dependency\n42\n",
    );
    failure(&work.run(&["root.opaal"]));
}

#[test]
fn retained_cells_get_fresh_entropy_and_recover_after_domain_errors() {
    let work = ReportDir::new();
    let mut child = work
        .command()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"import std::random as random\nlet shard = random::int(0, 4)\nshard\nlet fraction = random::float()\nfraction\nlet identifier = random::bytes(3)\nidentifier\nrandom::int(4, 4)\nrandom::bytes(-1)\nrandom::int(7, 8)\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let text = std::str::from_utf8(&output.stdout).unwrap();
    let lines = text.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 5, "{text}");
    let values = lines[..4]
        .iter()
        .zip([">> >> >> ", ">> >> ", ">> >> ", ">> >> >> "])
        .map(|(line, prefix)| line.strip_prefix(prefix).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(lines[4], ">> ");
    assert!((0..4).contains(&values[0].parse::<i64>().unwrap()));
    assert!((0.0..1.0).contains(&values[1].parse::<f64>().unwrap()));
    let mut encoded = values[2].bytes();
    let mut count = 0;
    while let Some(byte) = encoded.next() {
        if byte == b'\\' {
            match encoded.next().unwrap() {
                b'x' => {
                    assert!(encoded.next().unwrap().is_ascii_hexdigit());
                    assert!(encoded.next().unwrap().is_ascii_hexdigit());
                }
                b'"' | b'\\' => {}
                byte => panic!("unexpected byte escape {byte}"),
            }
        }
        count += 1;
    }
    assert_eq!(count, 3);
    assert_eq!(values[3], "7");
    assert_eq!(
        std::str::from_utf8(&output.stderr)
            .unwrap()
            .matches("RANDOM001")
            .count(),
        2
    );
}

#[test]
fn interactive_help_resolves_random_aliases_without_an_entropy_call() {
    let work = ReportDir::new();
    let mut child = work
        .command()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"import std::random as draw\nhelp draw::int\nhelp draw::float\nhelp draw::bytes\nexit 0\n").unwrap();
    let output = child.wait_with_output().unwrap();
    success(&output);
    let text = std::str::from_utf8(&output.stdout).unwrap();
    for signature in [
        "std::random::int(min: Int, max: Int) -> Int",
        "std::random::float() -> Float",
        "std::random::bytes(count: Int) -> Bytes",
    ] {
        assert!(text.contains(signature), "{text}");
    }
    assert_eq!(
        text.matches("effect: entropy.system (evaluation)").count(),
        3
    );
}

#[cfg(target_os = "linux")]
#[test]
fn native_process_receives_random_argv_but_audit_omits_payloads_and_digests() {
    use std::os::unix::fs::PermissionsExt;
    let work = controlled_fixture(
        r#"import std::random as random
import std::data as data
import std::string as string
import std::process as process
import project::tools as tools
action sample() -> Bytes effects { entropy.system(); process.run(tools::git); } {
    let sample = random::int(987654321, 987654325)
    let argument = string::decode_utf8(data::json_encode(sample))
    let result = process::run(tools::git, [argument])
    return result.stdout
}
task sample = sample
"#,
    );
    let marker = work.0.join("argv.txt");
    let tool = work.0.join("locked-git");
    work.write("locked-git", format!("#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf 'git version 2.50.0\\n'; else printf '%s' \"$1\" > '{}'; printf 'process-payload-canary'; fi\n", marker.display()));
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o700)).unwrap();
    let manifest = std::fs::read_to_string(work.0.join("opaal.toml")).unwrap();
    work.write(
        "opaal.toml",
        format!("{manifest}\n[tools.git]\nadapter = \"git\"\nversion = \">=2.50.0,<3.0.0\"\n"),
    );
    let authority = std::fs::read_to_string(work.0.join("authority.toml")).unwrap();
    work.write("authority.toml", format!("{authority}\n[[rules]]\ndecision = \"grant\"\neffect = \"process.run\"\nscope = \"tool.git\"\nrequired_enforcement = \"acknowledge-unenforced\"\n"));
    work.write(
        "tools.toml",
        format!(
            r#"schema_version = 1
project = "sample"
environment = "ci"
platform = "x86_64-unknown-linux-gnu"
[child_environment]
inherit = []
[[tools]]
id = "git"
adapter = "git"
path = {{ encoding = "base64url-nopad", platform = "unix", value = "{}" }}
version = "2.50.0"
digest = "{}"
"#,
            opaal_runtime::workflow::native_path(&tool)["value"]
                .as_str()
                .unwrap(),
            opaal_runtime::workflow::digest_bytes(&std::fs::read(&tool).unwrap())
        ),
    );
    let plan = controlled_plan(&work);
    assert!(!marker.exists());
    success(&controlled_execute(&work, &plan));
    let argument = std::fs::read_to_string(&marker).unwrap();
    assert!((987654321..987654325).contains(&argument.parse::<i64>().unwrap()));
    let records = journal_records(&work);
    let process = records
        .iter()
        .find(|row| row["kind"] == "effect-after" && row["payload"]["effect"] == "process.run")
        .unwrap();
    assert_eq!(
        process["payload"]["operation"]["argv"],
        serde_json::json!([])
    );
    for field in ["argv_digest", "evidence_digest"] {
        assert_eq!(
            process["payload"]["operation"].get(field),
            Some(&serde_json::Value::Null)
        );
    }
    let journal = std::fs::read_to_string(work.0.join("sample.run.jsonl")).unwrap();
    let inspected = work.run(&["audit", "inspect", "sample.audit.json"]);
    success(&inspected);
    for text in [
        journal.as_str(),
        std::str::from_utf8(&inspected.stdout).unwrap(),
    ] {
        for canary in [argument.as_str(), "process-payload-canary"] {
            assert!(!text.contains(canary));
            assert!(!text.contains(&opaal_runtime::workflow::digest_bytes(canary.as_bytes())));
        }
    }
}
