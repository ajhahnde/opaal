language 2

import std::outcome as outcome

def accept_status(value: Status) -> Status {
    return $value
}

accept_status(outcome::Result::Ok[Int, String](2))
