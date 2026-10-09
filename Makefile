CARGO ?= cargo
IMAGE ?= llmphant:dev

.PHONY: build release test fmt clippy lint doc docker-build docker-run clean

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
	docker run --rm -it -v llmphant-data:/data $(IMAGE) doctor

clean:
	$(CARGO) clean
