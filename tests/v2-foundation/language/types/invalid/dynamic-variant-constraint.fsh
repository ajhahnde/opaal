language 2

enum EqualMaybe[T: Equal] {
    Some(T),
    None,
}

def dynamic(value) {
    $value
}

let callable = {|value| $value}

let maybe = EqualMaybe::Some(dynamic($callable))
$maybe
