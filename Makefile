# Lintel — developer workflow targets.
#
#   make run     build the .app, sign it, and launch it in the BACKGROUND (no terminal)
#   make dev     run in the foreground (holds the terminal; Ctrl-C to quit)
#   make bundle  build a signed Lintel.app at target/bundle/Lintel.app
#   make logs    tail the log (populated when launched via `open`)
#   make sign    (re)sign the bundle with a stable identity
#   make fmt / make lint / make clean

CARGO ?= cargo
APP_NAME := Lintel
BIN := lintel
BUNDLE_DIR := target/bundle
APP := $(BUNDLE_DIR)/$(APP_NAME).app
PLIST := resources/Info.plist

# Stable code-signing identity for LOCAL dev. macOS ties the Accessibility (TCC)
# grant to the app's code identity; an ad-hoc build gets a NEW identity every
# rebuild, so the grant never sticks. Signing with a stable self-signed cert
# keeps the identity constant. Create it once: `make cert`. Override: SIGN_IDENTITY=...
SIGN_IDENTITY ?= Lintel Dev

.PHONY: build dev run bundle sign cert logs fmt lint clean

build:
	$(CARGO) build

# Foreground / dev loop (output stays in the terminal).
dev:
	$(CARGO) run

# Build + sign the .app, kill any old instance, and launch it detached.
# Pass extra args to the app with RUN_ARGS, e.g. `make run RUN_ARGS=--settings` to open Settings
# on launch (handy while iterating, since a restart otherwise closes the Settings window).
run: bundle
	@killall $(BIN) 2>/dev/null || true
	@sleep 0.3
	open $(APP) $(if $(RUN_ARGS),--args $(RUN_ARGS),)
	@echo "==> Lintel launched in the background (menu-bar icon > Quit Lintel). Logs: make logs"

# Build a signed Lintel.app that runs headless (LSUIElement) with a menu-bar icon.
bundle:
	$(CARGO) build --release
	@rm -rf $(APP)
	@mkdir -p $(APP)/Contents/MacOS $(APP)/Contents/Resources
	@cp target/release/$(BIN) $(APP)/Contents/MacOS/$(BIN)
	@cp $(PLIST) $(APP)/Contents/Info.plist
	@printf 'APPL????' > $(APP)/Contents/PkgInfo
	@plutil -lint $(APP)/Contents/Info.plist >/dev/null
	@$(MAKE) sign
	@echo "==> bundle ready at $(APP)"

# Sign with the STABLE identity so the Accessibility grant persists across rebuilds;
# fall back to ad-hoc (re-prompts each rebuild) if that identity isn't set up.
sign:
	@test -d $(APP) || (echo "no bundle at $(APP); run 'make bundle'"; exit 1)
	@if codesign --force --deep --sign "$(SIGN_IDENTITY)" $(APP) 2>/dev/null; then \
		echo "==> codesigned with '$(SIGN_IDENTITY)' — Accessibility grant persists across rebuilds"; \
	else \
		echo "!!  signing identity '$(SIGN_IDENTITY)' not found — run 'make cert' once to create it,"; \
		echo "!!  then re-grant Lintel in System Settings > Privacy & Security > Accessibility."; \
		echo "==> falling back to ad-hoc signature (re-prompts each rebuild)"; \
		codesign --force --deep --sign - $(APP); \
	fi

# One-time: create the stable self-signed "Lintel Dev" code-signing identity.
cert:
	bash scripts/make-signing-cert.sh "$(SIGN_IDENTITY)"

# Tail the log file (stdout is discarded when launched via `open`, so run mode tees
# to ~/Library/Logs/Lintel/lintel.log when it has no terminal). Ctrl-C to stop.
logs:
	@f="$$HOME/Library/Logs/Lintel/lintel.log"; test -f "$$f" || f="/tmp/lintel.log"; \
		echo "==> tailing $$f"; touch "$$f"; tail -f "$$f"

fmt:
	$(CARGO) fmt

lint:
	$(CARGO) clippy --all-targets

clean:
	$(CARGO) clean
	@rm -rf $(BUNDLE_DIR)
