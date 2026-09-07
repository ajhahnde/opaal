^printf "a\nb\n" | ^grep b
collect diagnostics | normalize |& save combined.log
