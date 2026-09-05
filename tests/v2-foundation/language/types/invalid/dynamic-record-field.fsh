language 2

type Pair[T] = {
    left: T,
    right: T,
}

def dynamic(value) {
    $value
}

let pair = Pair {
    left: dynamic("wrong"),
    right: 1,
}
$pair
