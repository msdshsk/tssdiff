# tssdiff Makefile

.PHONY: all build build-dev install install-gui installer test lint format clean check help

# Build targets
all: build

build:
	cargo build --release

build-dev:
	cargo build

install:
	cargo install --path crates/tssdiff-tui

install-gui:
	cargo install --path crates/tssdiff-gui

# NSIS installer for the desktop GUI (requires cargo-tauri:
#   cargo install tauri-cli --locked)
installer:
	cd crates/tssdiff-gui && cargo tauri build

# Development targets
test:
	cargo test --all-features

lint:
	cargo clippy --all-targets --all-features -- -D warnings

format:
	cargo fmt --all

clean:
	cargo clean

check:
	cargo check --all-features

# Example config
example-config:
	@echo "Creating example config at ~/.config/tssdiff/config.yaml"
	@mkdir -p ~/.config/tssdiff
	@cp config.example.yaml ~/.config/tssdiff/config.yaml
	@echo "Edit ~/.config/tssdiff/config.yaml to customize"

# Help
help:
	@echo "tssdiff - read-only diff viewer (TUI + desktop GUI)"
	@echo ""
	@echo "Build targets:"
	@echo "  build             Build all release binaries (tssdiff, tssdiff-gui)"
	@echo "  build-dev         Build debug binaries"
	@echo "  install           Install the TUI (cargo install)"
	@echo "  install-gui       Install the desktop GUI"
	@echo "  installer         Build the Windows NSIS installer for the GUI"
	@echo ""
	@echo "Development:"
	@echo "  test              Run tests (all features)"
	@echo "  lint              Run clippy (all targets/features)"
	@echo "  format            Format code"
	@echo "  check             Check compilation"
	@echo "  clean             Clean build artifacts"
	@echo ""
	@echo "Usage:"
	@echo "  tssdiff                            # TUI: working directory changes"
	@echo "  tssdiff --gui                      # Desktop GUI on the current repo"
	@echo "  tssdiff-gui <path>                 # Desktop GUI on a repo"
