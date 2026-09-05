language 2

import './support/facade.fsh' as api
import std::value as value

def direct() -> Int {
    return value::length(["one", "two"])
}

def reexported() -> Int {
    return api::value::length(["one", "two"])
}

direct()
