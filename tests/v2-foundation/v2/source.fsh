language 2

import std::value as value

export { Selection, tail_count }

enum Selection[T] {
    Selected(T),
    Empty,
}

def tail[T](items: List[T]) -> Selection[List[T]] {
    match $items {
        [] => { return Selection::Empty }
        [first, ...rest] => { return Selection::Selected($rest) }
    }
}

def tail_values[T](items: List[T]) -> List[T] {
    match tail($items) {
        Selection::Selected(rest) => { return $rest }
        Selection::Empty => { return [] }
    }
}

def tail_count[T](items: List[T]) -> Int {
    return value::length(tail_values($items))
}

tail_values($args) | value::length
