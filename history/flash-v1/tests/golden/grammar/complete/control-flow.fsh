if ^git diff --quiet {
    echo clean
} else if $count > 0 {
    echo changed
} else {
    echo unknown
}

while $count < 10 {
    $count = $count + 1
}

for item in $items {
    echo $item
}

match $count {
    0 => { echo zero }
    n if $n > 0 => { echo positive }
    _ => { echo negative }
}
