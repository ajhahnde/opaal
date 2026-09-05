language 2

import '../complete/support/model.fsh' as model
import '../complete/support/shadow.fsh' as shadow

def dynamic(value) {
    $value
}

let shadowed = shadow::Box { value: 1 }
let model::Box { value: value } = dynamic($shadowed)
$value
