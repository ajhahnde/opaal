language 2

def tail(items) {
    let [first, ...rest] = $items
    return $rest
}

tail("not a list")
