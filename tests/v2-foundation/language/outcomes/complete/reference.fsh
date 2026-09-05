language 2

import std::outcome as outcome
import './support/facade.fsh' as api

def select(result: outcome::Result[Int, String]) -> Int {
    match $result {
        outcome::Result::Ok(value) => { return $value }
        outcome::Result::Err(_) => { return 0 }
    }
}

def option_value(option: api::outcome::Option[Int]) -> Int {
    match $option {
        api::outcome::Option::Some(value) => { return $value }
        api::outcome::Option::None => { return 0 }
    }
}

let success = outcome::Result::Ok[Int, String](2)
let present = api::outcome::Option::Some[Int](2)
let absent: api::outcome::Option[Int] = api::outcome::Option::None

if option_value($present) != 2 {
    throw "Some did not preserve its payload"
}

if option_value($absent) != 0 {
    throw "None did not remain distinct"
}

select($success)
