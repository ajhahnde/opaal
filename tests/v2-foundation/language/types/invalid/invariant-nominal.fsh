language 2

type Box[T] = {
    value: T,
}

def integer(box: Box[Int]) -> Int {
    let Box { value: value } = $box
    return $value
}

let text = Box { value: "value" }
integer($text)
