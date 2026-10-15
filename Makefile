CARGO ?= cargo
IMAGE ?= ellmephant:dev
CHART ?= charts/ellmephant
HARBOR ?= harbor.vn.linuxguru.net/library
# The chart lives in its own nested repo so its tags never collide with the image tags.
HARBOR_CHARTS ?= $(HARBOR)/charts
# The prebuilt dependency layer (Dockerfile.builder) that the main Dockerfile builds FROM.
#
# The tag is a FINGERPRINT, not a name: the first 8 hex digits of sha256 over exactly these three
# files, in this order -- `Cargo.toml`, `Cargo.lock`, `Dockerfile.builder` -- which are the image's
# only inputs (the dependency set, and the build profile in the Dockerfile). Change any of them and
# the tag moves by itself, so a stale image can never be silently reused and there is nothing to
# remember to bump. `.forgejo/workflows/publish-dev.yml` computes the same hash, from the same
# files in the same order -- keep the two in step.
#
# `make builder-image` publishes it; `make docker-build` consumes it.
BUILDER_PROFILE ?= rust-lean
BUILDER_HASH    := $(shell cat Cargo.toml Cargo.lock Dockerfile.builder | shasum -a 256 | cut -c1-8)
BUILDER_IMAGE   ?= $(HARBOR)/ellmephant-builder:$(BUILDER_PROFILE)-$(BUILDER_HASH)
# Pinned so lint/render do not inherit Helm 3's v1.20.0 default, which the chart's kubeVersion rejects.
KUBE_VERSION ?= 1.34.0

.PHONY: build release test fmt clippy lint doc docker-build docker-run clean \
        chart-lint chart-template chart-package chart-push \
        builder-image coverage coverage-report

build:        ## debug build
	$(CARGO) build

release:      ## optimised single binary
	$(CARGO) build --release

test:         ## unit + integration tests (offline, no model)
	$(CARGO) test --locked

fmt:          ## format
	$(CARGO) fmt --all

clippy:       ## lints, warnings are errors
	$(CARGO) clippy --all-targets --all-features -- -D warnings

lint: fmt clippy

# Coverage. This toolchain is Homebrew's rustc, so there is no rustup `llvm-tools` component to
# add: point llvm-cov at Homebrew's llvm instead. Its version must be >= the compiler's own LLVM
# (`rustc -vV | grep LLVM`) — Xcode's CLT llvm is generally too old for it.
#
# One-time: `cargo install cargo-llvm-cov`. Note `--branch` is deliberately absent: branch
# coverage needs `-Z coverage-options=branch`, i.e. a nightly compiler.
LLVM_BIN  ?= /opt/homebrew/opt/llvm/bin
CARGO_BIN ?= $(HOME)/.cargo/bin

coverage:  ## line/region coverage (a measurement; fmt/clippy/test remain the gates)
	PATH="$(CARGO_BIN):$$PATH" \
	LLVM_COV="$(LLVM_BIN)/llvm-cov" LLVM_PROFDATA="$(LLVM_BIN)/llvm-profdata" \
		$(CARGO) llvm-cov --locked --summary-only
	@echo
	@echo "re-slice without re-running: make coverage-report"
	@echo "WARNING: per-function 'never ran' is unreliable — each function is compiled into two"
	@echo "         crates, so one copy runs while the other reads zero. Trust the per-file line"
	@echo "         percent, and triage a 0% method by grepping for CALLERS before writing a test."

coverage-report:  ## re-render the last coverage run as JSON (no re-run)
	PATH="$(CARGO_BIN):$$PATH" \
	LLVM_COV="$(LLVM_BIN)/llvm-cov" LLVM_PROFDATA="$(LLVM_BIN)/llvm-profdata" \
		$(CARGO) llvm-cov report --json --output-path target/cov.json
	@echo "wrote target/cov.json"

doc:          ## rustdoc
	$(CARGO) doc --no-deps

docker-build: ## build the runtime image (needs the dependency image -- `make builder-image` first)
	docker build --build-arg BUILDER=$(BUILDER_IMAGE) -t $(IMAGE) .

docker-run:   ## run doctor against a named volume
	docker run --rm -it -v ellmephant-data:/data $(IMAGE) doctor

chart-lint:      ## lint the chart (strict)
	helm lint --strict $(CHART) --kube-version $(KUBE_VERSION)

chart-template:  ## render the chart with default values
	helm template ellmephant $(CHART) --kube-version $(KUBE_VERSION)

chart-package:   ## package the chart -> ellmephant-<version>.tgz
	helm package $(CHART)

chart-push: chart-package ## package + push the chart to Harbor as OCI (needs: helm registry login)
	helm push ellmephant-*.tgz "oci://$(HARBOR_CHARTS)"
	@rm -f ellmephant-*.tgz

builder-image:   ## build + push the dependency image (rare, slow; publish-dev builds it when the input hash moves)
	docker build -f Dockerfile.builder -t $(BUILDER_IMAGE) .
	docker push $(BUILDER_IMAGE)

clean:
	$(CARGO) clean
