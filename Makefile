.DEFAULT_GOAL := help

CARGO ?= cargo
COMPLEMENT_TESTS ?= $(or $(t),$(p),.*)


# ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
# Format, check, lint
# ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
.PHONY: format
format: ## Format Rust sources with the repository's nightly rustfmt
	$(CARGO) +nightly fmt

.PHONY: check
check: ## Run cargo check for the workspace
	$(CARGO) +nightly check --workspace

.PHONY: lint
lint: ## Run Clippy for the workspace and deny warnings
	$(CARGO) +nightly clippy --workspace --all-targets -- -D warnings

# ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
# Test and docs
# ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

.PHONY: test
test: ## Run Rust tests; optionally filter by test name with var p
	$(CARGO) +nightly test $(if $(p),-- $(p),)

.PHONY: docs
docs: ## Build and test docs
	$(CARGO) +nightly test --doc
	$(CARGO) doc --no-deps


# ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
# Complement
# ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

.PHONY: complement/build
complement/build: ## Build the Complement tester and Tuwunel testee images
	docker/bake.sh complement-tester complement-testee

.PHONY: complement/run
complement/run: ## Run Complement; set var t or v for REGEX filter/select
	docker/complement.sh '$(COMPLEMENT_TESTS)'

.PHONY: complement/all
complement/all: ## Build images, then run Complement
	$(MAKE) complement-build
	$(MAKE) complement t='$(COMPLEMENT_TESTS)'


# ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
# Help and extras
# ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

.PHONY:
help: ## Show available targets
	@awk 'BEGIN { FS = ":.*##" } /^[a-zA-Z_-]+:.*##/ { printf "%-18s %s\n", $$1, $$2 }' $(MAKEFILE_LIST)

.PHONY: extras/cloc
extras/cloc:	## Count lines of code
	cloc --git HEAD
