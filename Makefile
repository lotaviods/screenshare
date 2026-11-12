# Sharescreen development
#
# Generic targets:
#   make dist          build the portable package for the current OS
#   make dist-windows  build dist/sharescreen-windows(.zip)
#   make dist-linux    build dist/sharescreen-linux.tar.gz
#
# The desktop app cannot be cross-compiled from Linux: it links native
# Wayland/X11, Vulkan, and FFmpeg libraries. Build on the target OS.
SHELL := /bin/bash
APP := sharescreen-ui

# Windows cross-compilation target. Needs `rustup target add` and the mingw
# linker: x86_64-w64-mingw32-gcc.
WINDOWS_TARGET ?= x86_64-pc-windows-gnu

.DEFAULT_GOAL := help

.PHONY: help run dev build test fmt clippy check dist dist-windows dist-linux dist-clean windows windows-target windows-server windows-check

help: ## show targets
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}'

run: ## run the desktop app
	cargo run -p $(APP)

dev: run ## alias for run

build: ## build the desktop app (release)
	cargo build -p $(APP) --release

test: ## run all Rust tests
	cargo test --workspace

fmt: ## format all Rust
	cargo fmt --all

clippy: ## lint all Rust
	cargo clippy --workspace --all-targets -- -D warnings

check: ## type-check all Rust
	cargo check --workspace

dist: ## build the portable package for the current OS
	@case "$$(uname -s)" in \
		MINGW*|MSYS*|CYGWIN*) \
			powershell -NoProfile -ExecutionPolicy Bypass -File scripts/build.ps1 ;; \
		*) \
			bash scripts/build.sh ;; \
	esac

dist-windows: ## build the portable Windows package
	powershell -NoProfile -ExecutionPolicy Bypass -File scripts/build.ps1

dist-linux: ## build the portable Linux package
	bash scripts/build.sh

dist-clean: ## remove generated dist packages
	rm -rf dist

windows: windows-server ## alias for windows-server

windows-target: ## install the Windows target and check the mingw linker
	@command -v x86_64-w64-mingw32-gcc >/dev/null || { echo "missing x86_64-w64-mingw32-gcc (install mingw-w64)"; exit 1; }
	rustup target add $(WINDOWS_TARGET)

windows-server: windows-target ## cross-compile the signaling server for Windows
	cargo build -p sharescreen-server --release --target $(WINDOWS_TARGET)

windows-check: windows-target ## type-check the crates with no native Windows deps
	cargo check --target $(WINDOWS_TARGET) \
		-p sharescreen-core -p sharescreen-config \
		-p sharescreen-signaling -p sharescreen-network
