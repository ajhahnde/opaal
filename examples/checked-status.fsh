# A nonzero process status remains a value until check turns it into an error.
try {
    ^false | check
} catch error {
    let category = $error.category
    ^printf 'caught %s\n' $category
}
