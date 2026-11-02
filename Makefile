# Sharescreen development + bundling
#
# Targets:
#   make setup    install frontend deps + Tauri CLI (first run only)
#   make dev      run the Tauri app with hot reload
#   make bundle   build a production bundle (AppImage on Linux)
#   make windows-server  cross-compile the signaling server for Windows
#
# The desktop app cannot be cross-compiled from Linux: it links WebView2 and
# the Windows GStreamer runtime. Build `make bundle` on a Windows host.
SHELL := /bin/bash
APP := frontend
TAURI_DIR := $(APP)/src-tauri

# Windows cross-compilation target. Needs `rustup target add` and the mingw
# linker: x86_64-w64-mingw32-gcc.
WINDOWS_TARGET ?= x86_64-pc-windows-gnu

.DEFAULT_GOAL := help

.PHONY: help setup icons dev bundle build test fmt clippy check windows windows-target windows-server windows-check

help: ## show targets
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}'

setup: ## install frontend deps + Tauri CLI
	pnpm --dir $(APP) install

icons: ## regenerate Tauri icon set from $(TAURI_DIR)/app-icon.png
	cd $(TAURI_DIR) && convert app-icon.png -resize 32x32 PNG32:icons/32x32.png
	cd $(TAURI_DIR) && convert app-icon.png -resize 128x128 PNG32:icons/128x128.png
	cd $(TAURI_DIR) && convert app-icon.png -resize 256x256 PNG32:icons/128x128@2x.png
	cd $(TAURI_DIR) && convert app-icon.png -define icon:auto-resize=256,128,64,48,32,16 icons/icon.ico

dev: ## run Tauri dev with hot reload
	pnpm --dir $(APP) tauri dev

bundle: ## build a production bundle
	pnpm --dir $(APP) tauri build

build: bundle ## alias for bundle

test: ## run all Rust tests
	cargo test --workspace

fmt: ## format all Rust
	cargo fmt --all

clippy: ## lint all Rust
	cargo clippy --workspace --all-targets -- -D warnings

check: ## type-check all Rust
	cargo check --workspace

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
