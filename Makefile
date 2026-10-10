CARGO ?= cargo
IMAGE ?= ellmephant:dev
CHART ?= charts/ellmephant
HARBOR ?= harbor.vn.linuxguru.net/library
# Pinned so lint/render do not inherit Helm 3's v1.20.0 default, which the chart's kubeVersion rejects.
KUBE_VERSION ?= 1.34.0

.PHONY: build release test fmt clippy lint doc docker-build docker-run clean \
        chart-lint chart-template chart-package chart-push harbor-publish harbor-hooks

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

doc:          ## rustdoc
	$(CARGO) doc --no-deps

docker-build: ## build the runtime image
	docker build -t $(IMAGE) .

docker-run:   ## run doctor against a named volume
	docker run --rm -it -v ellmephant-data:/data $(IMAGE) doctor

chart-lint:      ## lint the chart (strict)
	helm lint --strict $(CHART) --kube-version $(KUBE_VERSION)

chart-template:  ## render the chart with default values
	helm template ellmephant $(CHART) --kube-version $(KUBE_VERSION)

chart-package:   ## package the chart -> ellmephant-<version>.tgz
	helm package $(CHART)

chart-push: chart-package ## package + push the chart to Harbor as OCI (needs: helm registry login)
	helm push ellmephant-*.tgz "oci://$(HARBOR)"
	@rm -f ellmephant-*.tgz

harbor-publish:  ## push this commit to Harbor: chart + image (mirrors the CI-built image)
	scripts/harbor-publish.sh

harbor-hooks:    ## install the repo's git hooks into this clone (core.hooksPath)
	git config core.hooksPath .githooks
	@echo "hooks installed: core.hooksPath -> .githooks"

clean:
	$(CARGO) clean
