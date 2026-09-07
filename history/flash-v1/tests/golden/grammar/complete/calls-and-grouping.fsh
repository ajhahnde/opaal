let files = (glob "src/**/*.rs")
render($files[0].name)
let clean = (^git diff --quiet || false)
