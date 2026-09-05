language 2

import './support/model.fsh' as model

def inspect(box: model::Box[Int]) -> Int {
    let model::Box { value: value } = $box
    return $value
}

let box = model::Box { value: 9 }
let value = inspect($box)
let inferred = model::unwrap($box)
let maybe = model::Maybe::Some($inferred)

match $maybe {
    model::Maybe::Some(selected) => { $selected }
    model::Maybe::None => { $value }
}
