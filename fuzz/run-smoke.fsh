#!/usr/bin/env fsh

def status_code(completion: Status) -> Int {
    if $completion.code != null {
        return $completion.code
    }

    if $completion.signal != null {
        if $completion.signal.number != null {
            return 128 + $completion.signal.number
        }
    }

    1
}

def cleanup_and_exit(directory: String, completion: Status) {
    let result = status_code($completion)
    ^rm -rf $directory
    if !$status.ok {
        exit
    }
    exit $result
}

let repository = "$(pwd)"
let root = "$repository/components/flash"

mut runs = '1000'
if $args != [] {
    if $args[0] != '' {
        $runs = $args[0]
    }
}

if ^printf %s $runs | ^grep -Eq '^[0-9]+$' {
} else {
    ^printf '%s\n' 'run count must be a nonnegative integer' 1>&2 || exit
    exit 2
}

let nightly_cargo = "$(^rustup which --toolchain nightly cargo)"
if !$status.ok {
    exit
}
let nightly_directory = "$(^dirname -- $nightly_cargo)"
if !$status.ok {
    exit
}
mut inherited_path = env('PATH')
if $inherited_path == null {
    $inherited_path = ''
}
export PATH = "$nightly_directory:$inherited_path"

mut temporary_parent = env('TMPDIR')
if $temporary_parent == null {
    $temporary_parent = '/tmp'
}
let work = "$(^mktemp -d "$temporary_parent/flash-fuzz.XXXXXX")"
if !$status.ok {
    exit
}

for target in ['lexer', 'parser', 'expander'] {
    let corpus = "$work/$target"
    ^mkdir $corpus
    if !$status.ok {
        cleanup_and_exit($work, $status)
    }

    ^cargo fuzz run \
    --fuzz-dir "$root/fuzz" \
    $target \
    $corpus \
    "$root/tests/golden/grammar/complete" \
    "$root/tests/golden/grammar/incomplete" \
    "$root/tests/golden/grammar/invalid" \
    "$root/tests/golden/lexical/complete" \
    "$root/tests/golden/lexical/incomplete" \
    "$root/tests/golden/lexical/invalid" \
    -- \
    "-runs=$runs" \
    -max_len=4096 \
    -timeout=10 \
    -rss_limit_mb=2048
    if !$status.ok {
        cleanup_and_exit($work, $status)
    }
}

^rm -rf $work || exit
