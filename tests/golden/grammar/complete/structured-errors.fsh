try {
    throw "failed"
} catch error {
    let category: String = $error.category
    throw $error
}
