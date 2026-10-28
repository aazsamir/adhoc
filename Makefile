CARGO ?= cargo
BIN := target/release/adhoc

.PHONY: help all build release install install-service remove-service test test-repeat e2e e2e-root e2e-nonroot e2e-coverage e2e-local \
        fmt fmt-check clippy lint doc verify clean ci ci-full

help: ## show this help
	@awk 'BEGIN {FS = ":.*##"} /^[a-zA-Z0-9_-]+:.*##/ {printf "  %-12s %s\n", $$1, $$2}' $(MAKEFILE_LIST)

all: build

build: ## debug build
	$(CARGO) build

release: ## optimised build
	$(CARGO) build --release

install: release ## install into ~/.cargo/bin
	$(CARGO) install --path .

SERVICE_DIR := $(HOME)/.config/systemd/user
SERVICE := adhoc.service
CONFIG ?= $(HOME)/.config/adhoc.yaml

install-service: release ## install adhoc.service as a systemd user unit (CONFIG=... to override)
	mkdir -p $(SERVICE_DIR)
	if [ ! -f $(CONFIG) ]; then cp config.yaml $(CONFIG); echo "created $(CONFIG)"; fi
	sed -e 's|@BIN@|$(HOME)/.cargo/bin/adhoc|' -e 's|@CONFIG@|$(CONFIG)|' systemd/$(SERVICE) > $(SERVICE_DIR)/$(SERVICE)
	systemctl --user daemon-reload
	systemctl --user enable $(SERVICE)
	@echo "started: systemctl --user start adhoc"

remove-service: ## remove the adhoc systemd user unit
	systemctl --user disable --now $(SERVICE) 2>/dev/null || true
	rm -f $(SERVICE_DIR)/$(SERVICE)
	systemctl --user daemon-reload

test: ## unit + integration tests
	$(CARGO) test

test-repeat: ## the suite repeatedly, to shake out timing-dependent failures
	@for i in 1 2 3; do echo "run $$i"; $(CARGO) test --quiet || exit 1; done

e2e: ## end-to-end suite in docker: root pass + rootless pass
	./e2e/run.sh

e2e-root: ## end-to-end suite as root in docker
	./e2e/run.sh --root

e2e-nonroot: ## end-to-end suite as uid 1000 in docker (the rootless user-namespace path)
	./e2e/run.sh --nonroot

e2e-coverage: ## instrumented e2e run + line-coverage report (fails below MIN_COVERAGE, default 70)
	./e2e/run.sh --coverage

e2e-local: ## same suite on this machine; needs unshare + python3 (pasta optional)
	$(CARGO) test --test lifecycle -- --nocapture

fmt: ## format the sources
	$(CARGO) fmt

fmt-check: ## fail if the sources are not rustfmt'd
	$(CARGO) fmt -- --check

clippy: ## lints, including tests
	$(CARGO) clippy --all-targets -- -D warnings

lint: fmt-check clippy ## what CI runs before the tests

doc: ## rustdoc for the crate
	$(CARGO) doc --no-deps

ci: lint test ## what CI runs on every change
ci-full: ci e2e ## CI plus the docker end-to-end suite

verify: release ## check the default config against this machine
	./$(BIN) verify --config config.yaml

clean: ## remove build artifacts
	$(CARGO) clean
