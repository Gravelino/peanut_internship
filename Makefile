CARGO := cargo
CLIPPY := cargo clippy

.PHONY: all
all: build test

.PHONY: run start
run start:
	$(CARGO) run

.PHONY: test
test:
	$(CARGO) test --all

.PHONY: lint
lint:
	$(CLIPPY) -- -D warnings
	$(CLIPPY) --tests -- -A dead_code -D warnings

.PHONY: format
format:
	$(CARGO) fmt --all -- --check

.PHONY: format-fix
format-fix:
	$(CARGO) fmt --all

.PHONY: ci
ci: format lint test
	@echo "CI checks passed"

.PHONY: pre-commit
pre-commit: lint format test

.PHONY: clean
clean:
	$(CARGO) clean

.PHONY: help
help:
	@echo "Available commands:"
	@echo "  make run         - Run the project"
	@echo "  make test        - Run all tests"
	@echo "  make lint        - Run clippy linting"
	@echo "  make format      - Check formatting"
	@echo "  make format-fix  - Automatically format code"
	@echo "  make ci          - Run full CI pipeline (fmt + clippy + test)"
	@echo "  make clean       - Remove built artifacts"
