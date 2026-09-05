language 2

type Pair[T: Equal] = {
    left: T,
    right: T,
}

enum Selection[T] {
    Selected(T),
    Empty,
}

def first[T: Equal](Pair { left: left, right: right }: Pair[T]) -> T {
    return $left
}

let pair = Pair {
    left: 1,
    right: 2,
}

let inferred = first($pair)
let selection = Selection::Selected(first[Int]($pair))

match $selection {
    Selection::Selected(value) if true => { $value }
    Selection::Selected(_) => { 0 }
    Selection::Empty => { $inferred }
}
