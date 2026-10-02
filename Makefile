# ============================================================
# rs-webserver Makefile
# ------------------------------------------------------------
# Common commands:
#   make            # same as make build
#   make run        # start the server using config.yml
#   make smoke      # start, then smoke-test a few routes (requires curl)
#   make check      # type check
#   make test       # run unit tests
#   make fmt/clippy # format / lint
#   make clean      # remove build artifacts
# ============================================================

# Overridable via environment variables, e.g.: make run CONFIG=my.yml
CARGO  ?= cargo
CONFIG ?= config.yml
HOST   ?= 127.0.0.1
PORT   ?= 7878

# Make the default goal `build`.
.DEFAULT_GOAL := build

.PHONY: all build run release check test fmt clippy clean smoke help

all: build

## build (debug)
build:
	$(CARGO) build

## run the server (reads config.yml by default; override with CONFIG=...)
run:
	$(CARGO) run -- $(CONFIG)

## build (release, with optimizations)
release:
	$(CARGO) build --release

## type check
check:
	$(CARGO) check --all-targets

## run unit tests
test:
	$(CARGO) test

## format the code
fmt:
	$(CARGO) fmt

## lints (warnings are treated as errors)
clippy:
	$(CARGO) clippy --all-targets -- -D warnings

## remove build artifacts
clean:
	$(CARGO) clean

## smoke test: start the server in the background, hit each route, then shut down
smoke: build
	@echo ">> starting server for smoke test..."
	@$(CARGO) run -- $(CONFIG) & \
	pid=$$!; \
	sleep 1; \
	echo ">> GET /";        curl -sS -o /dev/null -w '  %{http_code}\n' http://$(HOST):$(PORT)/; \
	echo ">> GET /nope";    curl -sS -o /dev/null -w '  %{http_code}\n' http://$(HOST):$(PORT)/nope; \
	echo ">> GET /sleep";   curl -sS -o /dev/null -w '  %{http_code}\n' http://$(HOST):$(PORT)/sleep; \
	kill $$pid 2>/dev/null || true; \
	wait $$pid 2>/dev/null || true

## show this help
help:
	@grep -E '^## ' $(MAKEFILE_LIST) | sed 's/## //'
