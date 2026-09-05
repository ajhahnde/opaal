language 2

def dynamic(value) {
    $value
}

def identity[T](value: T) -> T {
    dynamic("wrong")
}

let result = identity[Int](1)
