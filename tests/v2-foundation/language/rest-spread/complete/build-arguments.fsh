language 2

def build_arguments(items: List[String]) -> List[String] {
    match $items {
        [] => { return [] }
        [first, ...rest] => { return $rest }
    }
}

let transformed = build_arguments($args)
$transformed
