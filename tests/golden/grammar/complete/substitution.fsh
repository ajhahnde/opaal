let branch = $(^git branch --show-current)
let explicit = $(text: ^git branch --show-current)
let binary = $(bytes: ^program --binary)
echo "branch: $(^git branch --show-current || echo detached)"
