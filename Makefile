PREFIX ?= $(HOME)/.local
BIN    := target/release/ramster

.PHONY: build install app install-app test lint clean

build:
	cargo build --release

install: build
	install -d $(PREFIX)/bin
	install -m 755 $(BIN) $(PREFIX)/bin/ramster
	@echo "installed $(PREFIX)/bin/ramster"

APP := dist/ramster.app

# Menu bar app bundle (contains the CLI too, used for the background sampler).
app: build
	rm -rf $(APP)
	mkdir -p $(APP)/Contents/MacOS
	cp packaging/Info.plist $(APP)/Contents/
	cp target/release/ramster-bar target/release/ramster $(APP)/Contents/MacOS/
	codesign --force --sign - $(APP)
	@echo "built $(APP)"

install-app: app
	rm -rf $(HOME)/Applications/ramster.app
	mkdir -p $(HOME)/Applications
	cp -R $(APP) $(HOME)/Applications/
	@echo "installed ~/Applications/ramster.app"

test:
	cargo test

lint:
	cargo fmt --check
	cargo clippy --release -- -D warnings

clean:
	cargo clean
