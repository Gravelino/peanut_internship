CARGO := cargo
CLIPPY := cargo clippy

.PHONY: all
all: build test

.PHONY: run start
run start:
	$(CARGO) run

.PHONY: test
test:
	$(CARGO) test

.PHONY: lint
lint:
	$(CLIPPY) -- -D warnings

.PHONY: format
format:
	$(CARGO) fmt -- --check

.PHONY: format-fix
format-fix:
	$(CARGO) fmt

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
	@echo "  make clean       - Remove built artifacts"
