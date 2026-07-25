let branch = $(^git branch --show-current)
echo "branch: $(^git branch --show-current || echo detached)"
