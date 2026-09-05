language 2

type Box[T: Equal] = {
    value: T,
}

enum Maybe[T: Equal] {
    Some(T),
    None,
}

def unwrap[T: Equal](Box { value: value }: Box[T]) -> T {
    return $value
}

export { Box, Maybe, unwrap }
