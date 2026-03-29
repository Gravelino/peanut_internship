# Project Automation Scripts

- `scripts/install-hooks.ps1`: (Work-In-Progress) Installs Git hooks for local environment matching CI standards. 
- Use the `Makefile` as the primary interface for running and testing the project.

## CI Workflow (Github Actions / GitLab CI)
Automation scripts should:
1) Check formatting (`make format`)
2) Lint code (`make lint`)
3) Run security checks (pre-commit)
4) Run all tests (`make test`)
5) Ensure one-command deployment (`make start`)
