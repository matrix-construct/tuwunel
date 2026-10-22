.DEFAULT_GOAL := help

CARGO ?= cargo
COMPLEMENT_TESTS ?= $(or $(t),$(p),.*)

.PHONY: help format fmt format-check fmt-check check lint test doc-test \
	complement-build complement complement-all

help: ## Show available targets
	@awk 'BEGIN { FS = ":.*##" } /^[a-zA-Z_-]+:.*##/ { printf "%-18s %s\n", $$1, $$2 }' $(MAKEFILE_LIST)

format: ## Format Rust sources with the repository's nightly rustfmt
	$(CARGO) +nightly fmt

fmt: format ## Alias for format

format-check: ## Check Rust formatting with nightly rustfmt
	$(CARGO) +nightly fmt --check

fmt-check: format-check ## Alias for format-check

check: ## Run cargo check for the workspace
	$(CARGO) +nightly check --workspace

lint: ## Run Clippy for the workspace and deny warnings
	$(CARGO) +nightly clippy --workspace --all-targets -- -D warnings

test: ## Run Rust tests; optionally filter by test name with p=FILTER
	$(CARGO) +nightly test $(if $(p),-- $(p),)

doc-test: ## Run Rust documentation tests
	$(CARGO) +nightly test --doc

complement-build: ## Build the Complement tester and Tuwunel testee images
	docker/bake.sh complement-tester complement-testee

complement: ## Run Complement; select tests with t=REGEX (or p=REGEX)
	docker/complement.sh '$(COMPLEMENT_TESTS)'

complement-all: ## Build images, then run Complement
	$(MAKE) complement-build
	$(MAKE) complement t='$(COMPLEMENT_TESTS)'
