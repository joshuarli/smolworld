SMOLWORLD_DIR := $(abspath $(dir $(lastword $(MAKEFILE_LIST))))
SMOLVM_DIR ?= $(abspath $(SMOLWORLD_DIR)/../smolvm)
LIBKRUN_DIR ?= $(abspath $(SMOLVM_DIR)/libkrun)

.PHONY: test test-smolworld test-smolvm test-libkrun lint

# Run the ordinary unit and documentation suites in dependency order. Live VM
# acceptance tests remain opt-in because they require prepared guest artifacts
# and host networking services.
test:
	@set -eu; \
	echo "==> libkrun"; \
	$(MAKE) --no-print-directory test-libkrun; \
	echo "==> smolvm"; \
	$(MAKE) --no-print-directory test-smolvm; \
	echo "==> smolworld"; \
	$(MAKE) --no-print-directory test-smolworld

test-libkrun:
	$(MAKE) --no-print-directory -C "$(LIBKRUN_DIR)" unit-test

test-smolvm:
	cargo test --manifest-path "$(SMOLVM_DIR)/Cargo.toml"

test-smolworld:
	cargo test

lint:
	cargo fmt --all
	cargo clippy --fix --allow-dirty --all-targets --all-features -- --deny warnings
