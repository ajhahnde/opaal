language 2

import './support/facade.fsh' as api

def inspect(box: api::model::Box[Int]) -> Int {
    let api::model::Box { value: value } = $box
    return $value
}

let box = api::model::Box { value: 11 }
let value = api::model::unwrap($box)
let maybe = api::model::Maybe::Some($value)

match $maybe {
    api::model::Maybe::Some(selected) => { inspect($box) + $selected }
    api::model::Maybe::None => { 0 }
}
