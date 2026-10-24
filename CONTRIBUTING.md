# Contributing to nubi-agent

Thanks for your interest in contributing. This document covers the standards and workflow for contributing to this project.

## Getting Started

### Prerequisites

- Rust 1.70+ (install via [rustup](https://rustup.rs/))
- Git

### Development Setup

```bash
# Clone the repository
git clone https://github.com/your-org/nubi-agent.git
cd nubi-agent

# Build the project
cargo build

# Run tests
cargo test

# Run with clippy lints
cargo clippy -- -D warnings

# Format code
cargo fmt
```

## Code Standards

### Formatting

All code must be formatted with `rustfmt`. Run `cargo fmt` before committing.

### Linting

Code must pass `cargo clippy -- -D warnings` with no warnings.

### Testing

- All new features must include tests
- All bug fixes should include a regression test
- Run `cargo test` before submitting a PR

## Git Workflow

### Branch Naming

```
<type>/<short-description>

Examples:
feat/add-retry-logic
fix/connection-timeout
docs/update-readme
refactor/simplify-polling
```

Types: `feat`, `fix`, `docs`, `refactor`, `test`, `chore`

### Commit Messages

We use [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>(<scope>): <description>

[optional body]

[optional footer]
```

**Types:**
- `feat`: New feature
- `fix`: Bug fix
- `docs`: Documentation only
- `style`: Formatting, no code change
- `refactor`: Code change that neither fixes a bug nor adds a feature
- `test`: Adding or updating tests
- `chore`: Maintenance tasks

**Examples:**
```
feat(polling): add exponential backoff for retries

fix(auth): handle expired tokens gracefully

docs(readme): add installation instructions
```

Keep the subject line under 72 characters. Use the body to explain *what* and *why*, not *how*.

## Pull Requests

### Before Submitting

1. Create an issue first for significant changes
2. Fork the repository and create your branch from `main`
3. Ensure all tests pass: `cargo test`
4. Ensure no clippy warnings: `cargo clippy -- -D warnings`
5. Ensure code is formatted: `cargo fmt --check`
6. Update documentation if needed

### PR Guidelines

- One PR per feature/fix
- Keep PRs focused and reasonably sized
- Write a clear description of what the PR does and why
- Link related issues using `Fixes #123` or `Closes #123`
- Request review from maintainers

### PR Title Format

Follow the same convention as commit messages:
```
feat(scope): add new feature
fix(scope): resolve bug
```

### Review Process

1. Maintainers will review within a few days
2. Address feedback with new commits (don't force-push during review)
3. Once approved, maintainer will merge

## Issues

### Bug Reports

Include:
- Clear description of the bug
- Steps to reproduce
- Expected vs actual behavior
- Rust version and OS
- Relevant logs or error messages

### Feature Requests

Include:
- Clear description of the feature
- Use case / motivation
- Proposed implementation (optional)

## Code of Conduct

Be respectful and constructive. We're all here to build something useful together.

## Questions?

Open an issue with the `question` label or reach out to maintainers.
