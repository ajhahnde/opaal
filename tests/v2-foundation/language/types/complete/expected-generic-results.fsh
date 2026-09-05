language 2

enum Maybe[T: Equal] {
    Some(T),
    None,
}

def empty[T]() -> List[T] {
    return []
}

def identity[T: Equal](value: T) -> T {
    return $value
}

def accept[T](values: List[T]) -> List[T] {
    return $values
}

def empty_equal[T: Equal]() -> List[T] {
    return []
}

def combine[T: Equal](value: T, values: List[T]) -> List[T] {
    return $values
}

def nothing[T: Equal]() -> Maybe[T] {
    return Maybe::None
}

let numbers: List[Int] = empty()
let nested = accept[Int](empty())
let sequential = combine(1, empty_equal())
let inferred: Maybe[Int] = Maybe::None
let explicit = Maybe::None[String]()
let returned = nothing[Int]()
let selected = Maybe::Some(7)

match $selected {
    Maybe::Some(value) => { identity($value) }
    Maybe::None => { $numbers[0] }
}
