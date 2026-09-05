language 2

def dynamic(value) {
    $value
}

def identity[T](value: T) -> T {
    return $value
}

identity(dynamic("value"))
