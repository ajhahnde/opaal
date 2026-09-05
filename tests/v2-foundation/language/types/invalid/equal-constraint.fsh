language 2

def equal_only[T: Equal](value: T) -> T {
    return $value
}

equal_only({|value| $value})
