language 2

enum Choice {
    Yes,
    No,
}

let choice = Choice::Yes

match $choice {
    Choice::Yes if 1 => { true }
    Choice::No => { false }
}
