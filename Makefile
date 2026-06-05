.PHONY: help
help: ## Show this help
	@awk 'BEGIN { printf "\nUsage:\n  make \033[36m<target>\033[0m\n" } \
		/^##@/ { printf "\n\033[1m%s\033[0m\n", substr($$0, 5); next } \
		/^[a-zA-Z0-9_-]+:([^=]|$$)/ { \
			target = $$1; sub(":", "", target); \
			desc = $$0; \
			sub(/.*##[[:space:]]*/, "", desc); \
			if (desc == $$0) desc = ""; \
			printf "  \033[36m%-24s\033[0m %s\n", target, desc; \
		}' $(MAKEFILE_LIST)

# Pinned Python tooling (run without a local install). The Rust toolchain is
# pinned via rust-toolchain.toml.
RUFF ?= uvx ruff@0.15.16
EXT  := --manifest-path editors/zed/Cargo.toml

##@ [Format & Lint]

.PHONY: fmt
fmt: ## Auto-format Rust (server + extension) and Python
	cargo fmt
	cargo fmt $(EXT)
	$(RUFF) format tests/

.PHONY: fmt-check
fmt-check: ## Check formatting without writing (CI)
	cargo fmt -- --check
	cargo fmt $(EXT) -- --check
	$(RUFF) format --check tests/

.PHONY: lint
lint: ## Lint Rust (clippy -D warnings) and Python (ruff check)
	cargo clippy --all-targets -- -D warnings
	cargo clippy $(EXT) --all-targets -- -D warnings
	$(RUFF) check tests/

##@ [Test]

.PHONY: test
test: ## Run Rust unit tests and the Python e2e suites
	cargo test
	cargo build -p ansible-lens-lsp
	python3 tests/smoke_test.py
	python3 tests/watch_test.py
	python3 tests/diagnostics_test.py
	python3 tests/resolution_test.py

##@ [Setup]

.PHONY: git-setup
git-setup: ## Wire local git hooks and the blame-ignore file
	@git config core.hooksPath .githooks
	@git config blame.ignoreRevsFile .git-blame-ignore-revs
	@echo "Configured core.hooksPath=.githooks and blame.ignoreRevsFile"
