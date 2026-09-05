language 2

import '../complete/support/model.fsh' as model
import '../complete/support/shadow.fsh' as shadow

def inspect(box: model::Box[Int]) -> Int {
    let model::Box { value: value } = $box
    return $value
}

let shadowed = shadow::Box { value: 1 }
inspect($shadowed)
