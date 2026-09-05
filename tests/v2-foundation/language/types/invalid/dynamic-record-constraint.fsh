language 2

type EqualBox[T: Equal] = {
    value: T,
}

def dynamic(value) {
    $value
}

let callable = {|value| $value}

let box = EqualBox {
    value: dynamic($callable),
}
$box
