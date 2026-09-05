language 2

let [first, ...rest] = $args
match $rest {
    ["--target", "x86_64-unknown-linux-gnu", "Grüße 🌍"] => {}
    _ => { throw "unexpected build arguments" }
}
$rest
