PREFIX  ?= $(HOME)/.local
BIN     := target/release/ramster
VERSION := $(shell sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -n1)

# Binaries that go into the app bundle: target/release (host arch, default)
# or target/universal (set by `make dist`).
BINDIR  ?= target/release
APP     := dist/ramster.app

# Code signing: ad-hoc by default. With a Developer ID, sign with the hardened
# runtime and a secure timestamp (required for notarization):
#   make app SIGN_ID="Developer ID Application: Your Name (TEAMID)"
SIGN_ID ?= -
ifeq ($(SIGN_ID),-)
SIGN_FLAGS := --force --sign -
else
SIGN_FLAGS := --force --sign "$(SIGN_ID)" --options runtime --timestamp
endif

# Notarization: credentials stored once with
#   xcrun notarytool store-credentials ramster --apple-id ... --team-id ... --password <app-specific>
NOTARY_PROFILE  ?= ramster
NOTARY_KEYCHAIN ?=
NOTARY_ARGS     := --keychain-profile "$(NOTARY_PROFILE)" $(if $(NOTARY_KEYCHAIN),--keychain "$(NOTARY_KEYCHAIN)")
# $(call notarize_zip,file.zip): submit, wait, and fail unless Accepted
# (notarytool exits 0 even for an Invalid submission).
notarize_zip = xcrun notarytool submit $(1) $(NOTARY_ARGS) --wait --output-format json > $(1).json; \
	cat $(1).json; echo; \
	grep -Eq '"status" *: *"Accepted"' $(1).json || { echo "notarization of $(1) was not accepted; see xcrun notarytool log <id>"; exit 1; }; \
	rm -f $(1) $(1).json

TARGETS   := aarch64-apple-darwin x86_64-apple-darwin
UNIVERSAL := target/universal
CLI_TGZ   := dist/ramster-$(VERSION)-macos-universal.tar.gz
APP_ZIP   := dist/ramster-$(VERSION).app.zip

.PHONY: build install universal app install-app icon notarize dist test lint clean

build:
	cargo build --release

install: build
	install -d $(PREFIX)/bin
	install -m 755 $(BIN) $(PREFIX)/bin/ramster
	@echo "installed $(PREFIX)/bin/ramster"

# arm64 + x86_64 release builds, merged with lipo into target/universal/.
universal:
	@if command -v rustup >/dev/null; then rustup target add $(TARGETS); fi
	for t in $(TARGETS); do cargo build --release --target $$t || exit 1; done
	mkdir -p $(UNIVERSAL)
	for b in ramster ramster-bar; do \
		lipo -create -output $(UNIVERSAL)/$$b $(foreach t,$(TARGETS),target/$(t)/release/$$b) || exit 1; \
		lipo -info $(UNIVERSAL)/$$b; \
	done

# Menu bar app bundle (contains the CLI too, used for the background sampler).
# Inner binaries are signed first, then the bundle.
app: $(if $(filter $(UNIVERSAL),$(BINDIR)),universal,build)
	rm -rf $(APP)
	mkdir -p $(APP)/Contents/MacOS $(APP)/Contents/Resources
	cp packaging/Info.plist $(APP)/Contents/
	plutil -replace CFBundleShortVersionString -string "$(VERSION)" $(APP)/Contents/Info.plist
	plutil -replace CFBundleVersion -string "$(VERSION)" $(APP)/Contents/Info.plist
	cp packaging/ramster.icns $(APP)/Contents/Resources/
	cp $(BINDIR)/ramster-bar $(BINDIR)/ramster $(APP)/Contents/MacOS/
	codesign $(SIGN_FLAGS) --identifier dev.ramster.cli $(APP)/Contents/MacOS/ramster
	codesign $(SIGN_FLAGS) $(APP)
	codesign --verify --strict --verbose=2 $(APP)
	@echo "built $(APP) $(VERSION) (signed: $(SIGN_ID))"

install-app: app
	rm -rf $(HOME)/Applications/ramster.app
	mkdir -p $(HOME)/Applications
	cp -R $(APP) $(HOME)/Applications/
	@echo "installed ~/Applications/ramster.app"

# Regenerate packaging/ramster.icns (committed; only needed when the design changes).
# ICON_PREVIEW=path.png also writes a 512 px preview.
ICON_PREVIEW ?=
icon:
	rm -rf target/ramster.iconset
	swift packaging/make-icon.swift target/ramster.iconset $(ICON_PREVIEW)
	iconutil -c icns target/ramster.iconset -o packaging/ramster.icns
	@echo "wrote packaging/ramster.icns"

# Notarize and staple the already built, Developer ID signed app:
#   make app notarize SIGN_ID="Developer ID Application: ..." NOTARY_PROFILE=ramster
notarize:
	@test -d $(APP) || { echo "no $(APP); run make app SIGN_ID=... first"; exit 1; }
	@codesign -dv --verbose=2 $(APP) 2>&1 | grep -q "Authority=Developer ID Application" || \
		{ echo "$(APP) is not Developer ID signed; rebuild with SIGN_ID=\"Developer ID Application: ...\""; exit 1; }
	rm -f dist/notarize-app.zip
	ditto -c -k --keepParent $(APP) dist/notarize-app.zip
	$(call notarize_zip,dist/notarize-app.zip)
	xcrun stapler staple $(APP)
	xcrun stapler validate $(APP)

# Release artifacts: universal CLI tarball + app zip, with checksums.
# NOTARIZE=1 (needs a Developer ID SIGN_ID) also notarizes the app (stapled) and
# the CLI (a bare binary can't be stapled; Gatekeeper checks the ticket online).
dist:
	$(MAKE) app BINDIR=$(UNIVERSAL)
	$(if $(filter 1,$(NOTARIZE)),$(MAKE) notarize)
	rm -rf dist/cli $(CLI_TGZ) $(APP_ZIP) dist/SHA256SUMS
	mkdir -p dist/cli
	cp $(UNIVERSAL)/ramster README.md dist/cli/
	codesign $(SIGN_FLAGS) --identifier dev.ramster.cli dist/cli/ramster
	$(if $(filter 1,$(NOTARIZE)),ditto -c -k dist/cli/ramster dist/notarize-cli.zip)
	$(if $(filter 1,$(NOTARIZE)),$(call notarize_zip,dist/notarize-cli.zip))
	COPYFILE_DISABLE=1 tar -czf $(CLI_TGZ) -C dist/cli ramster README.md
	# No resource forks or xattrs: plain unzip (Homebrew) would write them as ._ files into
	# the bundle and break its signature.
	ditto -c -k --norsrc --noextattr --noqtn --noacl --keepParent $(APP) $(APP_ZIP)
	rm -rf dist/cli
	cd dist && shasum -a 256 $(notdir $(CLI_TGZ)) $(notdir $(APP_ZIP)) | tee SHA256SUMS

test:
	cargo test

lint:
	cargo fmt --check
	cargo clippy --release -- -D warnings

clean:
	cargo clean
