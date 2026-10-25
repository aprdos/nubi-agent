#!/bin/sh
# Setup git hooks for nubi-agent
# Run this once after cloning the repo

REPO_ROOT="$(git rev-parse --show-toplevel)"

echo "Installing git hooks..."
cp "$REPO_ROOT/.githooks/pre-commit" "$REPO_ROOT/.git/hooks/pre-commit"
chmod +x "$REPO_ROOT/.git/hooks/pre-commit"

echo "Done! Pre-commit hook installed."
