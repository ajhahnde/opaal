language 2

type Pair = {
    left: Int,
    right: Int,
}

let Pair { left: left, right: right } = Pair { left: 1, right: 2 }
let [first, ...rest] = [$left, $right]
$first
