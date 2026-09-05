language 2

type Box = {
    value: Int,
}

type Other = {
    value: Int,
}

def inspect(Box { value: value }: Other) -> Int {
    return $value
}
