#!/usr/bin/env fsh

def duration_error() {
    ^printf '%s\n' 'duration must be a positive integer number of seconds' 1>&2 || exit
    exit 2
}

def usage_error() {
    ^printf '%s\n' 'usage: components/flash/fuzz/run-campaign.fsh [seconds [result-directory]]' 1>&2 || exit
    exit 2
}

let repository_root = "$(pwd)"
let root = "$repository_root/components/flash"
let argument_shape = "$(^printf '.%.0s' marker ...$args)"
if $status {
} else {
    exit
}

mut seconds = '600'
if $argument_shape != '.' {
    if $args[0] != '' {
        $seconds = $args[0]
    }
}

let duration_residue = "$(^printf '%sX' $seconds | ^tr -d '0-9')"
if $status {
} else {
    exit
}
if $seconds == '0' || $duration_residue != 'X' {
    duration_error()
}

mut campaign = ''
if $argument_shape in ['.', '..'] {
    ^mkdir -p "$root/fuzz/campaigns" || exit

    let timestamp = "$(^date -u +%Y%m%dT%H%M%SZ)"
    if $status {
    } else {
        exit
    }

    $campaign = "$(^mktemp -d "$root/fuzz/campaigns/$timestamp.XXXXXX")"
    if $status {
    } else {
        exit
    }
} else if $argument_shape == '...' {
    $campaign = $args[1]
    if $campaign == '' {
        ^printf '%s\n' 'result directory must not be empty' 1>&2 || exit
        exit 2
    }

    if ^test -e $campaign {
        ^printf '%s\n' "result directory must not already exist: $campaign" 1>&2 || exit
        exit 2
    }

    let campaign_parent = "$(^dirname -- $campaign)"
    if $status {
    } else {
        exit
    }
    ^mkdir -p $campaign_parent || exit
    ^mkdir $campaign || exit
} else {
    usage_error()
}

let nightly_cargo = "$(^rustup which --toolchain nightly cargo)"
if $status {
} else {
    exit
}
let nightly_directory = "$(^dirname -- $nightly_cargo)"
if $status {
} else {
    exit
}
let inherited_path = env('PATH')
export PATH = "$nightly_directory:$inherited_path"

^mkdir "$campaign/corpus" "$campaign/artifacts" || exit
^printf '%s\n' "campaign directory: $campaign" || exit

for target in ['lexer', 'parser', 'expander', 'migration', 'resources'] {
    let corpus = "$campaign/corpus/$target"
    let artifacts = "$campaign/artifacts/$target"
    ^mkdir -p $corpus $artifacts || exit
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
    "$root/tests/v2-foundation/language/grammar/complete" \
    "$root/tests/v2-foundation/language/grammar/incomplete" \
    "$root/tests/v2-foundation/language/grammar/invalid" \
    "$root/tests/v2-foundation/language/grammar/repl" \
    "$root/tests/v2-foundation/language/lexical" \
    "$root/tests/v2-foundation/language/modules/complete" \
    "$root/tests/v2-foundation/language/modules/invalid" \
    -- \
    "-max_total_time=$seconds" \
    -max_len=4096 \
    -timeout=10 \
    -rss_limit_mb=2048 \
    "-artifact_prefix=$artifacts/" || exit
}
