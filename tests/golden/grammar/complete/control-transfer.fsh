def first_value(items: list[string]) {
    for item in $items {
        if $item == "stop" { break }
        if $item == "skip" { continue }
        return $item
    }
    return null
}
