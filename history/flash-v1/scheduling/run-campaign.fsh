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

let repository = "$(pwd)"
let root = "$repository/components/flash"

mut cases = "64"
if $args != [] {
    if $args[0] != "" {
        $cases = $args[0]
    }
}

if ^printf %s $cases | ^grep -Eq '^[0-9]+$' {
    if ^printf %s $cases | ^grep -Eq '^0+$' {
        ^printf '%s\n' "case count must be a positive integer" 1>&2 || exit
        exit 2
    }
} else {
    ^printf '%s\n' "case count must be a positive integer" 1>&2 || exit
    exit 2
}

if ^printf %s $cases | ^grep -Eq '^0*([0-9]{1,3}|[1-3][0-9]{3}|40([0-8][0-9]|9[0-6]))$' {
} else {
    ^printf '%s\n' "case count must not exceed 4096" 1>&2 || exit
    exit 2
}

mut campaign = ""
mut seed = "0"
mut generate_seed = true

if $args == [] || $args == [$args[0]] {
    ^mkdir -p "$root/scheduling/campaigns" || exit
    let stamp = "$(^date -u +%Y%m%dT%H%M%SZ)"
    $campaign = "$(^mktemp -d "$root/scheduling/campaigns/$stamp.XXXXXX")"
    if !$status.ok {
        exit
    }
} else if $args == [$args[0], $args[1]] {
    $campaign = $args[1]
    if $campaign == "" {
        ^printf '%s\n' "result directory must not be empty" 1>&2 || exit
        exit 2
    }
    if ^test -e $campaign {
        ^printf '%s\n' "result directory must not already exist: $campaign" 1>&2 || exit
        exit 2
    }
    let parent = "$(^dirname -- $campaign)"
    ^mkdir -p $parent || exit
    ^mkdir $campaign || exit
} else {
    if $args == [$args[0], $args[1], $args[2]] {
        $campaign = $args[1]
        $seed = $args[2]
        $generate_seed = false
        if $campaign == "" {
            ^printf '%s\n' "result directory must not be empty" 1>&2 || exit
            exit 2
        }
        if ^test -e $campaign {
            ^printf '%s\n' "result directory must not already exist: $campaign" 1>&2 || exit
            exit 2
        }
        let parent = "$(^dirname -- $campaign)"
        ^mkdir -p $parent || exit
        ^mkdir $campaign || exit
    } else {
        ^printf '%s\n' "usage: components/flash/scheduling/run-campaign.fsh [cases [result-directory [campaign-seed]]]" 1>&2 || exit
        exit 2
    }
}

if $generate_seed {
    while $seed == "0" {
        let raw_seed = "$(^od -An -N8 -tu8 /dev/urandom)"
        if !$status.ok {
            exit
        }
        $seed = "$(^printf %s $raw_seed | ^tr -d '[:space:]')"
        if !$status.ok {
            exit
        }
    }
}

let manifest = "$campaign/manifest.txt"
let output = "$campaign/output.log"
let replay = "FLASH_PTY_STRESS_CAMPAIGN_SEED=$seed FLASH_PTY_STRESS_CASES=$cases cargo test -p flash-cli --test pty stress_ -- --nocapture --test-threads=1"
let started_utc = "$(^date -u +%Y-%m-%dT%H:%M:%SZ)"
let host = "$(^uname -a)"
let rustc_version = "$(^rustc --version)"
let cargo_version = "$(^cargo --version)"

^printf '%s\n' "Flash scheduling stress campaign" > $manifest || exit
^printf '%s\n' "started_utc=$started_utc" >> $manifest || exit
^printf '%s\n' "campaign_seed=$seed" >> $manifest || exit
^printf '%s\n' "cases_per_scenario=$cases" >> $manifest || exit
^printf '%s\n' "scenarios=4" >> $manifest || exit
^printf '%s\n' "host=$host" >> $manifest || exit
^printf '%s\n' "rustc=$rustc_version" >> $manifest || exit
^printf '%s\n' "cargo=$cargo_version" >> $manifest || exit
^printf '%s\n' "workspace=$root" >> $manifest || exit
^printf '%s\n' "replay=$replay" >> $manifest || exit

^printf '%s\n' "campaign directory: $campaign" || exit
^printf '%s\n' "campaign seed: $seed" || exit
^printf '%s\n' "cases per scenario: $cases" || exit

cd $root || exit
^env "FLASH_PTY_STRESS_CAMPAIGN_SEED=$seed" "FLASH_PTY_STRESS_CASES=$cases" cargo test -p flash-cli --test pty stress_ -- --nocapture --test-threads=1 2>&1 | ^tee $output

let cargo_status = $status.stages[0]
let tee_status = $status.stages[1]
mut result = 0
mut result_name = "passed"

if !$tee_status.ok {
    $result = status_code($tee_status)
    $result_name = "failed"
} else if !$cargo_status.ok {
    $result = status_code($cargo_status)
    $result_name = "failed"
}

let finished_utc = "$(^date -u +%Y-%m-%dT%H:%M:%SZ)"
^printf '%s\n' "finished_utc=$finished_utc" >> $manifest || exit
^printf '%s\n' "result=$result_name" >> $manifest || exit

^printf '%s\n' "campaign result: $result_name" || exit
^printf '%s\n' "manifest: $manifest" || exit
^printf '%s\n' "complete output: $output" || exit
exit $result
