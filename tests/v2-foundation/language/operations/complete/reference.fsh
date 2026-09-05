language 2

import './support/facade.fsh' as api
import std::value as value

def length(items: List[Int]) -> Int {
    return 99
}

def expression_count() -> Int {
    return value::length([1, 2])
}

def pipeline_count() -> Int {
    ["one", "two", "three"] | api::value::length
}

if expression_count() != 2 {
    throw "qualified expression operation returned the wrong result"
}

if value::length[Int]([]) != 0 {
    throw "explicit operation type arguments did not resolve an empty list"
}

if pipeline_count() != 3 {
    throw "pipeline operation returned the wrong result"
}

if length([1]) != 99 {
    throw "a local function no longer has its own callable identity"
}

pipeline_count()
