language 2

def choose[T: Equal](left: T, right: T) -> T {
    return $left
}

def dynamic(value) {
    $value
}

let inferred = choose(1, 2)
let explicit = choose[Int](dynamic(3), 4)
[$inferred, $explicit]
