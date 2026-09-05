language 2

enum Choice {
    A,
    B,
}

let choice = Choice::A

match $choice {
    Choice::A => { 1 }
    Choice::A => { 2 }
    Choice::B => { 3 }
}
