# Unified Hi-Fi Control - Build helpers

# Pin Tailwind version for reproducible builds (update manually when needed)
TAILWIND_VERSION := v4.1.18

# Detect OS and architecture for Tailwind CLI download
UNAME_S := $(shell uname -s)
UNAME_M := $(shell uname -m)

ifeq ($(UNAME_S),Darwin)
    ifeq ($(UNAME_M),arm64)
        TAILWIND_BINARY := tailwindcss-macos-arm64
    else
        TAILWIND_BINARY := tailwindcss-macos-x64
    endif
else ifeq ($(UNAME_S),Linux)
    ifeq ($(UNAME_M),aarch64)
        TAILWIND_BINARY := tailwindcss-linux-arm64
    else
        TAILWIND_BINARY := tailwindcss-linux-x64
    endif
else
    $(warning Unsupported OS: $(UNAME_S). Tailwind CLI download may fail.)
    TAILWIND_BINARY := tailwindcss-linux-x64
endif

.PHONY: help setup-tailwind css css-watch synology-test qnap-test clean

help:
	@echo "Available targets:"
	@echo "  setup-tailwind  - Download Tailwind CSS standalone CLI"
	@echo "  css             - Build Tailwind CSS"
	@echo "  css-watch       - Watch and rebuild Tailwind CSS"
	@echo "  synology-test   - Validate SPK structure and unprivileged lifecycle"
	@echo "  qnap-test       - Validate QNAP x86_64 package contract"
	@echo "  clean           - Remove generated files"

setup-tailwind:
	@if [ ! -f ./tailwindcss ]; then \
		echo "Downloading Tailwind CSS $(TAILWIND_VERSION) standalone CLI..."; \
		curl -sLO https://github.com/tailwindlabs/tailwindcss/releases/download/$(TAILWIND_VERSION)/$(TAILWIND_BINARY); \
		chmod +x $(TAILWIND_BINARY); \
		mv $(TAILWIND_BINARY) tailwindcss; \
		echo "Done! Tailwind CLI $(TAILWIND_VERSION) installed."; \
	else \
		echo "Tailwind CLI already installed."; \
	fi

css: setup-tailwind
	./tailwindcss -i src/input.css -o public/tailwind.css --content "src/app/**/*.rs"

css-watch: setup-tailwind
	./tailwindcss -i src/input.css -o public/tailwind.css --content "src/app/**/*.rs" --watch

synology-test:
	tests/synology_package_contract.sh
	tests/synology_package_lifecycle.sh

qnap-test:
	tests/qnap_package_contract.sh

clean:
	rm -f public/tailwind.css
