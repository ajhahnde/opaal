# Keep directory data structured, then serialize it explicitly for script output.
ls \
| where {|entry| $entry.type == "file"} \
| get name \
| each {|name| "$name"} \
| sort \
| collect \
| to json \
| ^cat
