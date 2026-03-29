# Peanut Internship Documentation - Module 0 Baseline

## Repositiory Standard (1.1)
- **src/**: Core source code.
- **tests/**: Integration and unit tests.
- **scripts/**: Automation scripts for CI/CD and developer setup.
- **configs/**: Configuration templates (non-secret).
- **docs/**: Technical documentation and training materials.

## Engineering Standard & Safety
1. **Makefile**:
   - `make run` for local execution.
   - `make test` for automated testing.
   - `make lint` for security/best-practice static analysis.
2. **Secret Management**:
   - `.env` is ignored by Git via `.gitignore`.
   - `.env.example` provides the necessary structure.
   - `dotenvy` used for secure configuration loading.
3. **Tests as a Contract**:
   - Deterministic invariants (unit tests).
   - Negative tests (error case handling).
4. **Pre-commit Hooks**:
   - `detect-private-key` for safety.
   - `cargo fmt` and `cargo clippy` for code quality.

## Lab -1 Completion Checklist
- [x] Project structure set up.
- [x] Makefile with `run` / `test`.
- [x] .env.example and secret blocking.
- [x] Placeholder tests for CI integrity.
- [x] Pre-commit hooks configuration.
