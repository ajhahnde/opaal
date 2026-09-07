#!/usr/bin/env fsh
# Native dependencies: cargo, env, git, jq, rg, rustc, mktemp, mkdir, rm, test,
# printf, cat, sed, sort, tr, xargs, cut, dirname, realpath, uname, wc, and either sha256sum
# or shasum. They provide bounded process, parsing, hashing, and platform
# operations; Flash owns every exercise, expectation, decision, and report.

def exercise_error(message) {
    ^printf '%s\n' $message 1>&2
    exit 1
}

def usage_error(message) {
    ^printf 'run.fsh: %s\n' $message 1>&2
    exit 2
}

def print_help() {
    ^printf '%s\n' \
    'usage: run.fsh [-h] [--profile {smoke,ci,full}] [--record PATH] [--no-build]' \
    '' \
    'Run Flash v1 exercises through assembled host product entry points.' \
    '' \
    'options:' \
    '  -h, --help            show this help message and exit' \
    '  --profile {smoke,ci,full}' \
    '  --record PATH' \
    '  --no-build' || exit 1
    exit 0
}

# xargs invokes this private mode once per candidate source so Flash can build
# the historical path-NUL-bytes-NUL digest preimage without a shell callback.
mut internal_digest = false
mut internal_output = null
mut internal_position = 0
for argument in $args {
    if $argument == '--internal-digest-append' {
        $internal_digest = true
    } else if $internal_digest {
        if $internal_position == 0 {
            $internal_output = $argument
        } else {
            if ^test -f $argument {
                ^printf '%s\0' $argument >> $internal_output || exit 1
                ^cat $argument >> $internal_output || exit 1
                ^printf '\0' >> $internal_output || exit 1
            }
        }
        $internal_position = $internal_position + 1
    }
}
if $internal_digest {
    if $internal_position < 2 {
        usage_error('invalid internal digest invocation')
    }
    exit 0
}

mut rg = env('FLASH_AUTOMATION_RG')
if $rg == null || $rg == '' {
    $rg = 'rg'
}
mut jq = env('FLASH_AUTOMATION_JQ')
if $jq == null || $jq == '' {
    $jq = 'jq'
}

def assembled_exercises(binary, status_fixture, stream_fixture) {
    let stream_name = 'flash-e2e-stream-fixture'
    return [
    {
        id: 'language-values',
        summary: 'Values, access, operators, ranges, loops, matching, and interpolation execute in one script.',
        program: $binary,
        arguments: [],
        expected_code: 0,
        stdout_contains: [],
        stderr_contains: [],
        source: "let project = 'Flash'\nlet values = [1, 2, 3]\nlet record = {name: \"\${\$project}\", count: 3}\nmut total = 0\nfor value in \$values { \$total = \$total + \$value }\nmut chosen = 'bad'\nmatch \$total {\n    6 if \$record.name == 'Flash' => { \$chosen = 'ok' }\n    _ => { \$chosen = 'bad' }\n}\nif \$chosen == 'ok' && 3 in 1..=3 && !(4 in 1..4) {\n    exit 0\n} else {\n    exit 91\n}\n",
    },
    {
        id: 'invalid-language',
        summary: 'A chained comparison is rejected through the script frontend.',
        program: $binary,
        arguments: [],
        expected_code: 1,
        stdout_contains: [],
        stderr_contains: ['comparison operators are non-associative'],
        source: 'let invalid = 1 < 2 < 3
',
    },
    {
        id: 'language-composition',
        summary: 'Bindings, functions, closure capture, conditions, and live status compose through fsh.',
        program: $binary,
        arguments: [],
        expected_code: 0,
        stdout_contains: [],
        stderr_contains: [],
        source: "mut base = 2\ndef add(left: Int, right: Int) -> Int { return \$left + \$right }\nlet offset = 3\nlet advance = {|value| \$value + \$offset}\nif add(\$base, 3) == 5 && \$advance(2) == 5 {\n    ^$status_fixture exit 0\n} else {\n    exit 92\n}\nif \$status.ok { exit 0 } else { exit 93 }\n",
    },
    {
        id: 'functions-and-modules',
        summary: 'Named imports initialize once and expose typed functions to the root module.',
        program: $binary,
        arguments: [],
        expected_code: 0,
        stdout_contains: [],
        stderr_contains: [],
        source: "import { answer, add } from './math.fsh'\nif \$answer == 42 && add(2, 3) == 5 { exit 0 } else { exit 94 }\n",
    },
    {
        id: 'invalid-modules',
        summary: 'A missing imported export is rejected before program execution.',
        program: $binary,
        arguments: [],
        expected_code: 1,
        stdout_contains: [],
        stderr_contains: ['is not exported'],
        source: "import { missing } from './library.fsh'\n",
    },
    {
        id: 'commands-and-capture',
        summary: 'Dynamic argv, explicit external execution, text capture, byte capture, and status reach real processes.',
        program: $binary,
        arguments: [],
        expected_code: 0,
        stdout_contains: [],
        stderr_contains: [],
        source: "let program = '$stream_name'\nlet text = \$(command \$program source 2 0)\nlet bytes = \$(bytes: ^$stream_name source 2 0)\nlet repeated = \$(bytes: ^$stream_name source 2 0)\nif \$text == 'xx' && \$bytes == \$repeated { exit 0 } else { exit 95 }\n",
    },
    {
        id: 'invalid-command-boundary',
        summary: 'An implicit structured-to-byte carrier edge is rejected before execution.',
        program: $binary,
        arguments: [],
        expected_code: 1,
        stdout_contains: [],
        stderr_contains: ['incompatible pipeline edge'],
        source: "^$stream_name source 1 0 | from json | ^$stream_name sink 0 0\n",
    },
    {
        id: 'pipelines-and-files',
        summary: 'External, structured, mixed, and file pipelines cross explicit representation boundaries.',
        program: $binary,
        arguments: [],
        expected_code: 0,
        stdout_contains: [],
        stderr_contains: [],
        source: "^$stream_name source 7 0 > input.txt\nopen input.txt | save copy.txt\nopen copy.txt | ^$stream_name sink 7 0\n^$stream_name source 7 0 | decode bytes | encode bytes | ^$stream_name sink 7 0\n",
    },
    {
        id: 'structured-errors',
        summary: 'Throw, catch, rethrow metadata, and rollback remain distinct from process status.',
        program: $binary,
        arguments: [],
        expected_code: 0,
        stdout_contains: [],
        stderr_contains: [],
        source: "mut state = 'before'\ntry {\n    \$state = 'discarded'\n    throw 'caught'\n} catch error {\n    if \$error.message == 'caught' && \$state == 'before' { exit 0 } else { exit 97 }\n}\n",
    },
    {
        id: 'uncaught-error',
        summary: 'An uncaught structured error terminates the script with an anchored diagnostic.',
        program: $binary,
        arguments: [],
        expected_code: 1,
        stdout_contains: [],
        stderr_contains: ['uncaught'],
        source: "throw 'uncaught'\n",
    },
    {
        id: 'intrinsics',
        summary: 'All four v1 intrinsics execute through one assembled script.',
        program: $binary,
        arguments: [],
        expected_code: 0,
        stdout_contains: [],
        stderr_contains: [],
        source: "let from_env = env('FLASH_V1_EXERCISE')\nlet as_int = int(3.75)\nlet as_float = float(3)\nlet matches = glob('*.fsh')\nif \$from_env != 'present' { exit 91 }\nif \$as_int != 3 { exit 92 }\nif \$as_float != 3.0 { exit 93 }\nif \$matches[0] == glob('*.fsh')[0] { exit 0 } else { exit 94 }\n",
    },
    {
        id: 'standard-builtins',
        summary: 'The exact standard namespace is discoverable through assembled help and which paths.',
        program: $binary,
        arguments: [],
        expected_code: 0,
        stdout_contains: [],
        stderr_contains: [],
        source: "let names = [\n    'bg', 'cd', 'check', 'collect', 'command', 'decode', 'each', 'encode',\n    'exit', 'fg', 'first', 'from', 'get', 'help', 'jobs', 'kill', 'last',\n    'length', 'lines', 'ls', 'open', 'pwd', 'save', 'select', 'sort', 'to',\n    'update', 'wait', 'where', 'which',\n]\nlet discovered = \"\$(which ...\$names\n    | where {|entry| \$entry.kind == 'internal'}\n    | length\n    | to json)\"\nif \$discovered == '30' { help pwd > help.txt; exit 0 } else { exit 99 }\n",
    },
    {
        id: 'invalid-builtin-contracts',
        summary: 'Built-in arity and option misuse is rejected at the user boundary.',
        program: $binary,
        arguments: [],
        expected_code: 1,
        stdout_contains: [],
        stderr_contains: ['expects'],
        source: 'pwd unexpected
',
    },
    {
        id: 'launcher-version',
        summary: 'The assembled executable reports the exact Flash 1.0.0 package version.',
        program: $binary,
        arguments: ['--version'],
        expected_code: 0,
        stdout_contains: ['fsh 1.0.0'],
        stderr_contains: [],
        source: null,
    },
    {
        id: 'processes-and-jobs',
        summary: 'Foreground, pipeline, and background processes complete and are reaped by the script session.',
        program: $binary,
        arguments: [],
        expected_code: 0,
        stdout_contains: [],
        stderr_contains: [],
        source: "^$status_fixture exit 0 && ^$stream_name source 4 0 | ^$stream_name sink 4 0\n^$status_fixture late 10 late.marker 0 &\nwait\nlet marker = \"\$(open late.marker | decode utf8)\"\nif \$marker == 'late' { exit 0 } else { exit 90 }\n",
    },
    ]
}

def command_exercises(cargo) {
    return [
    {
        id: 'launcher-frontends',
        summary: 'Launcher help, version, checker, planner, and formatter use their public executable modes.',
        program: $cargo,
        arguments: ['test', '--locked', '-p', 'flash-cli', '--test', 'checker_e2e', '--test', 'formatter_e2e', '--test', 'planner_e2e'],
        expected_code: 0,
        stdout_contains: [],
        stderr_contains: [],
        source: null,
    },
    {
        id: 'invalid-launcher-options',
        summary: 'Launcher and frontend misuse paths retain distinct statuses and channels.',
        program: $cargo,
        arguments: ['test', '--locked', '-p', 'flash-cli', 'cli::tests', '--lib'],
        expected_code: 0,
        stdout_contains: [],
        stderr_contains: [],
        source: null,
    },
    {
        id: 'interactive-config',
        summary: 'All six config settings and their refusal paths reach a real interactive session.',
        program: $cargo,
        arguments: ['test', '--locked', '-p', 'flash-cli', '--test', 'config_startup'],
        expected_code: 0,
        stdout_contains: [],
        stderr_contains: [],
        source: null,
    },
    {
        id: 'language-server',
        summary: 'The assembled stdio server executes lifecycle, synchronization, diagnostics, queries, and refusals.',
        program: $cargo,
        arguments: ['test', '--locked', '-p', 'flash-lsp', '--test', 'server_e2e'],
        expected_code: 0,
        stdout_contains: [],
        stderr_contains: [],
        source: null,
    },
    {
        id: 'interactive-editor',
        summary: 'The real PTY editor executes prompts, Unicode, multiline input, completion, history, cancellation, and restoration.',
        program: $cargo,
        arguments: ['test', '--locked', '-p', 'flash-cli', '--test', 'pty', 'draws_the_primary_prompt_and_runs_a_command'],
        expected_code: 0,
        stdout_contains: [],
        stderr_contains: [],
        source: null,
    },
    {
        id: 'interactive-jobs',
        summary: 'Real PTY job built-ins execute stop, list, background, foreground, wait, and signal paths.',
        program: $cargo,
        arguments: ['test', '--locked', '-p', 'flash-cli', '--test', 'pty', 'job_builtins_'],
        expected_code: 0,
        stdout_contains: [],
        stderr_contains: [],
        source: null,
    },
    {
        id: 'platform-user-paths',
        summary: 'Portable and selected-adapter operation contracts execute, including explicit target signal withholding.',
        program: $cargo,
        arguments: ['test', '--locked', '-p', 'flash-platform-posix', '-p', 'flash-platform-flashos'],
        expected_code: 0,
        stdout_contains: [],
        stderr_contains: [],
        source: null,
    },
    {
        id: 'documentation-examples',
        summary: 'Language and scripting examples are exercised by their assembled script and frontend owners.',
        program: $cargo,
        arguments: ['test', '--locked', '-p', 'flash-cli', '--test', 'e2e'],
        expected_code: 0,
        stdout_contains: [],
        stderr_contains: [],
        source: null,
    },
    ]
}

def materialize_case_files(identifier, directory) {
    if $identifier == 'functions-and-modules' {
        ^printf '%s' 'let answer: Int = 42
def add(left: Int, right: Int) -> Int { return $left + $right }
export { answer, add }
' > "$directory/math.fsh" || exit 1
    } else if $identifier == 'invalid-modules' {
        ^printf '%s' 'let present = 1
export { present }
' > "$directory/library.fsh" || exit 1
    }
}

def stabilize(source, destination, repository_root, temporary) {
    let resolved_repository = "$(^realpath $repository_root)"
    let resolved_temporary = "$(^realpath $temporary)"
    ^sed -e "s|$resolved_repository|<repository>|g" -e "s|$repository_root|<repository>|g" -e "s|$resolved_temporary|<temporary>|g" -e "s|$temporary|<temporary>|g" $source > $destination || exit 1
}

def write_json_strings(values, destination, jq) {
    if $values == [] {
        ^printf '%s\n' '[]' > $destination || exit 1
    } else {
        ^printf '%s\n' ...$values | ^env $jq --raw-input --slurp 'split("\n")[:-1]' > $destination || exit 1
    }
}

def execute_case(exercise, flash_root, repository_root, suite_temporary, result_path, rg, jq) {
    let identifier = $exercise.id
    let summary = $exercise.summary
    let program = $exercise.program
    let source = $exercise.source
    let expected_code = $exercise.expected_code
    let stdout_contains = $exercise.stdout_contains
    let stderr_contains = $exercise.stderr_contains
    let directory = "$suite_temporary/case-$identifier"
    ^mkdir -p $directory || exit 1
    let stdout_path = "$directory/stdout"
    let stderr_path = "$directory/stderr"
    let stable_stdout_path = "$directory/stable-stdout"
    let stable_stderr_path = "$directory/stable-stderr"
    let input_path = "$directory/exercise.fsh"
    let stable_input_path = "$directory/stable-input"
    let action_path = "$directory/action.json"
    let expected_stdout_path = "$directory/expected-stdout.json"
    let expected_stderr_path = "$directory/expected-stderr.json"

    materialize_case_files($identifier, $directory)
    mut arguments = $exercise.arguments
    if $source != null {
        ^printf '%s' $source > $input_path || exit 1
        $arguments = [$input_path]
        cd $directory
    } else {
        ^printf '%s' 'acceptance owner selected by action' > $input_path || exit 1
        cd $flash_root
    }

    if $identifier == 'intrinsics' {
        export FLASH_V1_EXERCISE = 'present'
    }
    ^env $program ...$arguments > $stdout_path 2> $stderr_path
    let observed_status = $status
    mut observed_code = $observed_status.code
    if $observed_code == null {
        if $observed_status.signal == null {
            $observed_code = 1
        } else {
            $observed_code = -$observed_status.signal.number
        }
    }
    if $identifier == 'intrinsics' {
        unset FLASH_V1_EXERCISE
    }
    cd $repository_root

    mut passed = $observed_code == $expected_code
    for expected in $stdout_contains {
        if ^env $rg --fixed-strings --quiet -- $expected $stdout_path {
            let found = true
        } else {
            $passed = false
        }
    }
    for expected in $stderr_contains {
        if ^env $rg --fixed-strings --quiet -- $expected $stderr_path {
            let found = true
        } else {
            $passed = false
        }
    }

    stabilize($stdout_path, $stable_stdout_path, $repository_root, $directory)
    stabilize($stderr_path, $stable_stderr_path, $repository_root, $directory)
    stabilize($input_path, $stable_input_path, $repository_root, $directory)
    ^printf '%s\n' $program ...$arguments \
    | ^sed -e "s|$repository_root|<repository>|g" -e "s|$directory|<temporary>|g" \
    | ^env $jq --raw-input --slurp 'split("\n")[:-1]' > $action_path || exit 1
    write_json_strings($stdout_contains, $expected_stdout_path, $jq)
    write_json_strings($stderr_contains, $expected_stderr_path, $jq)

    mut result = 'fail'
    if $passed {
        $result = 'pass'
    }
    ^env $jq --null-input \
    --arg id $identifier \
    --arg summary $summary \
    --slurpfile action $action_path \
    --rawfile input $stable_input_path \
    --argjson expected_code $expected_code \
    --slurpfile stdout_contains $expected_stdout_path \
    --slurpfile stderr_contains $expected_stderr_path \
    --argjson observed_code $observed_code \
    --rawfile stdout $stable_stdout_path \
    --rawfile stderr $stable_stderr_path \
    --arg result $result \
    '{id: $id, summary: $summary, action: $action[0], input: $input, expected: {exit_code: $expected_code, stdout_contains: $stdout_contains[0], stderr_contains: $stderr_contains[0]}, observed: {exit_code: $observed_code, stdout: $stdout, stderr: $stderr}, result: $result}' \
    >> $result_path || exit 1
    return $passed
}

def tool_version(program, arguments, error_path) {
    let output = "$(^env $program ...$arguments 2> $error_path)"
    if $status.ok {
        return $output
    }
    let error = "$(open $error_path | decode utf8)"
    return "unavailable ($error)"
}

mut profile = 'full'
mut record_path = null
mut no_build = false
mut expecting_profile = false
mut expecting_record = false
for argument in $args {
    if $expecting_profile {
        if $argument in ['smoke', 'ci', 'full'] {
            $profile = $argument
        } else {
            usage_error("invalid profile: $argument")
        }
        $expecting_profile = false
    } else if $expecting_record {
        $record_path = $argument
        $expecting_record = false
    } else if $argument == '--profile' {
        $expecting_profile = true
    } else if $argument == '--profile=smoke' {
        $profile = 'smoke'
    } else if $argument == '--profile=ci' {
        $profile = 'ci'
    } else if $argument == '--profile=full' {
        $profile = 'full'
    } else if $argument == '--record' {
        $expecting_record = true
    } else if '--record=' in $argument {
        if ^printf '%s' $argument | ^env $rg --quiet '^--record=' {
            $record_path = "$(^printf '%s' $argument | ^sed 's/^--record=//')"
        } else {
            usage_error("unrecognized argument: $argument")
        }
    } else if $argument == '--no-build' {
        $no_build = true
    } else if $argument in ['-h', '--help'] {
        print_help()
    } else {
        usage_error("unrecognized argument: $argument")
    }
}
if $expecting_profile {
    usage_error('--profile requires a value')
}
if $expecting_record {
    usage_error('--record requires a path')
}

# The supported public invocation starts in the repository root. This avoids
# making project-root discovery another hidden host-language responsibility.
let repository_root = "$(pwd)"
let flash_root = "$repository_root/components/flash"
let contract_path = "$flash_root/exercises/v1.toml"
let host_cases_path = "$flash_root/exercises/host-cases-v1.json"
let runner_path = "$flash_root/exercises/run.fsh"
if ^test -f $contract_path && ^test -f $host_cases_path {
    let root_is_valid = true
} else {
    exercise_error('run.fsh must be invoked from the repository root')
}

mut temporary_parent = env('TMPDIR')
if $temporary_parent == null {
    $temporary_parent = '/tmp'
}
let suite_temporary = "$(^mktemp -d "$temporary_parent/flash-v1-exercises.XXXXXX")"
if !$status.ok {
    exercise_error('cannot create the exercise temporary directory')
}
let results_path = "$suite_temporary/results.jsonl"
^printf '%s' '' > $results_path || exit 1

mut cargo = env('CARGO')
if $cargo == null {
    $cargo = 'cargo'
}
if !$no_build {
    let build_stdout = "$suite_temporary/build-stdout"
    let build_stderr = "$suite_temporary/build-stderr"
    let build_arguments = ['build', '--workspace', '--bins', '--locked']
    cd $flash_root
    ^env $cargo ...$build_arguments > $build_stdout 2> $build_stderr
    let build_ok = $status.ok
    cd $repository_root
    if !$build_ok {
        if ^test -s $build_stderr {
            ^cat $build_stderr 1>&2
        } else {
            ^cat $build_stdout 1>&2
        }
        ^rm -rf $suite_temporary
        exit 1
    }
}

let binary = "$flash_root/target/debug/fsh"
let fixture_directory = "$flash_root/target/debug"
let status_fixture = "$fixture_directory/flash-e2e-status-fixture"
let stream_fixture = "$fixture_directory/flash-e2e-stream-fixture"
if ^test -f $binary {
    let binary_exists = true
} else {
    ^rm -rf $suite_temporary
    exercise_error("assembled fsh is missing: $binary")
}
mut path = env('PATH')
if $path == null {
    $path = ''
}
export PATH = "$fixture_directory:$path"

mut count = 0
let assembled = assembled_exercises($binary, $status_fixture, $stream_fixture)
for exercise in $assembled {
    let identifier = $exercise.id
    ^printf 'Flash v1 exercise: %s\n' $identifier || exit 1
    let passed = execute_case($exercise, $flash_root, $repository_root, $suite_temporary, $results_path, $rg, $jq)
    $count = $count + 1
    if !$passed {
        ^env $jq --slurp '.[-1]' $results_path
        ^rm -rf $suite_temporary
        exercise_error("Flash v1 exercise failed: $identifier")
    }
}
if $profile != 'smoke' {
    let commands = command_exercises($cargo)
    for exercise in $commands {
        let identifier = $exercise.id
        ^printf 'Flash v1 exercise: %s\n' $identifier || exit 1
        let passed = execute_case($exercise, $flash_root, $repository_root, $suite_temporary, $results_path, $rg, $jq)
        $count = $count + 1
        if !$passed {
            ^env $jq --slurp '.[-1]' $results_path
            ^rm -rf $suite_temporary
            exercise_error("Flash v1 exercise failed: $identifier")
        }
    }
}

let suite_version = "$(^env $rg --only-matching '^suite_version = [0-9]+$' $contract_path | ^cut -d ' ' -f 3)"
if !$status.ok || $suite_version == '' {
    ^rm -rf $suite_temporary
    exercise_error('cannot read suite_version from exercises/v1.toml')
}
mut commit = "$(^git rev-parse HEAD)"
if !$status.ok {
    $commit = 'unavailable'
}
mut tree = "$(^git rev-parse 'HEAD^{tree}')"
if !$status.ok {
    $tree = 'unavailable'
}
let worktree_state = "$(^git status --porcelain)"
mut worktree = 'dirty'
if $status.ok && $worktree_state == '' {
    $worktree = 'clean'
}

let candidates_path = "$suite_temporary/candidates"
let candidates_nul_path = "$suite_temporary/candidates-nul"
let candidates_unfiltered_path = "$suite_temporary/candidates-unfiltered"
let digest_input_path = "$suite_temporary/source-digest-input"
^git ls-files -co --exclude-standard -z > $candidates_nul_path
if !$status.ok {
    ^rm -rf $suite_temporary
    exercise_error('cannot enumerate candidate sources')
}
let nul_count = "$(^tr -cd '\0' < $candidates_nul_path | ^wc -c | ^tr -d ' ')"
^tr '\0' '\n' < $candidates_nul_path > $candidates_unfiltered_path || exit 1
let line_count = "$(^wc -l < $candidates_unfiltered_path | ^tr -d ' ')"
if $nul_count != $line_count {
    ^rm -rf $suite_temporary
    exercise_error('candidate source paths containing newlines cannot be represented by the frozen-v1 digest iterator')
}
^env $rg --invert-match '^(components/flash/target/|components/flash/exercises/evidence/host-v1\.json)$' $candidates_unfiltered_path \
| ^env LC_ALL=C sort > $candidates_path
if !$status.ok {
    ^rm -rf $suite_temporary
    exercise_error('cannot filter candidate sources')
}
^printf '%s' '' > $digest_input_path || exit 1
mut digest_runner = env('FLASH_V1_BOOTSTRAP_FSH')
if $digest_runner == null {
    $digest_runner = $binary
}
let digest_runner_version = "$(^env $digest_runner --version)"
if !$status.ok || $digest_runner_version != 'fsh 1.0.0' {
    ^rm -rf $suite_temporary
    exercise_error('the source-digest helper requires an explicitly selected Flash 1.0.0 runtime')
}
^tr '\n' '\0' < $candidates_path \
| ^xargs -0 -n 64 $digest_runner $runner_path --internal-digest-append $digest_input_path
if !$status.ok {
    ^rm -rf $suite_temporary
    exercise_error('cannot assemble the candidate source digest')
}

let system = "$(^uname -s | ^tr '[:upper:]' '[:lower:]')"
if !$status.ok || $system == '' {
    ^rm -rf $suite_temporary
    exercise_error('cannot identify the host system')
}
let architecture = "$(^uname -m | ^tr '[:upper:]' '[:lower:]')"
if !$status.ok || $architecture == '' {
    ^rm -rf $suite_temporary
    exercise_error('cannot identify the host architecture')
}
mut source_sha256 = ''
if $system == 'darwin' {
    $source_sha256 = "$(^shasum -a 256 $digest_input_path | ^cut -d ' ' -f 1)"
} else {
    $source_sha256 = "$(^sha256sum $digest_input_path | ^cut -d ' ' -f 1)"
}
if !$status.ok || $source_sha256 == '' {
    ^rm -rf $suite_temporary
    exercise_error('cannot hash candidate sources')
}

cd $flash_root
let rustc_version = tool_version('rustc', ['--version'], "$suite_temporary/rustc-version-errors")
let cargo_version = tool_version($cargo, ['--version'], "$suite_temporary/cargo-version-errors")
cd $repository_root
let flash_version = $digest_runner_version
let contract_cases_path = "$suite_temporary/contract-cases.json"
^env $jq '.owners' $host_cases_path > $contract_cases_path || exit 1

let report_path = "$suite_temporary/report.json"
^env $jq --slurp \
--argjson schema_version 1 \
--argjson suite_version $suite_version \
--arg commit $commit \
--arg tree $tree \
--arg source_sha256 $source_sha256 \
--arg worktree $worktree \
--arg system $system \
--arg architecture $architecture \
--arg flash $flash_version \
--arg rustc $rustc_version \
--arg cargo $cargo_version \
--arg profile $profile \
--slurpfile contract_cases $contract_cases_path \
'{schema_version: $schema_version, suite_version: $suite_version, candidate: {commit: $commit, tree: $tree, source_sha256: $source_sha256, worktree: $worktree}, environment: {id: "host-posix", system: $system, architecture: $architecture, flash: $flash, rustc: $rustc, cargo: $cargo}, profile: $profile, contract_cases: $contract_cases[0], results: ., limitations: ["Host results do not establish FlashOS target behavior.", "Physical-device execution remains identification- and approval-gated.", "Flash v1 has no guaranteed scope-exit cleanup; interruption or a runtime adapter failure can leave the owned temporary directory for inspection."]}' \
$results_path | ^env $jq --sort-keys '.' > $report_path
if !$status.ok {
    ^rm -rf $suite_temporary
    exercise_error('cannot render exercise evidence')
}

if $record_path != null {
    mut output_path = $record_path
    if $record_path[0] != '/' {
        $output_path = "$repository_root/$record_path"
    }
    let output_directory = "$(^dirname $output_path)"
    ^mkdir -p $output_directory || exit 1
    ^cat $report_path > $output_path || exit 1
} else {
    ^cat $report_path
    let report_written = $status.ok
    if !$report_written {
        ^rm -rf $suite_temporary
        exit 1
    }
}
^printf 'Flash v1 exercises: %s assembled host cases passed\n' $count || exit 1
^rm -rf $suite_temporary
