let active = {|user: record| $user.active}
where {|row| $row.size > 0}
each {|| echo tick}
