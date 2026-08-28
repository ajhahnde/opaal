# External bytes become values only at the explicit JSON boundary.
^printf '[{"name":"build","active":true},{"name":"deploy","active":false}]' \
| from json array \
| where {|item| $item.active} \
| select name \
| collect \
| to json \
| ^cat
