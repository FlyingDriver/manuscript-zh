PREFIX ?= $(HOME)/.local
DESTDIR ?=
BINDIR = $(DESTDIR)$(PREFIX)/bin
DATADIR = $(DESTDIR)$(PREFIX)/share
LICENSEDIR = $(DATADIR)/licenses/manuscript
APP_ID = io.gitlab.ilshat_apps.manuscript

.PHONY: build build-debug install uninstall clean run help

build:
	cargo build --release

build-debug:
	cargo build

run:
	cargo run -- $(ARGS)

install: build
	install -Dm755 target/release/manuscript $(BINDIR)/manuscript
	install -Dm644 data/$(APP_ID).desktop $(DATADIR)/applications/$(APP_ID).desktop
	install -Dm644 data/$(APP_ID).metainfo.xml $(DATADIR)/metainfo/$(APP_ID).metainfo.xml
	install -Dm644 data/icons/hicolor/scalable/apps/$(APP_ID).svg $(DATADIR)/icons/hicolor/scalable/apps/$(APP_ID).svg
	install -Dm644 LICENSE $(LICENSEDIR)/LICENSE
	@echo "Installed to $(DESTDIR)$(PREFIX). Make sure $(BINDIR) is in your PATH."

uninstall:
	rm -f $(BINDIR)/manuscript
	rm -f $(DATADIR)/applications/$(APP_ID).desktop
	rm -f $(DATADIR)/metainfo/$(APP_ID).metainfo.xml
	rm -f $(DATADIR)/icons/hicolor/scalable/apps/$(APP_ID).svg
	rm -rf $(LICENSEDIR)

clean:
	cargo clean

help:
	@echo "Usage: make [target]"
	@echo ""
	@echo "Targets:"
	@echo "  build        Build release binary"
	@echo "  build-debug  Build debug binary"
	@echo "  run          Run debug build (ARGS=file.md)"
	@echo "  install      Build and install to PREFIX (default: ~/.local)"
	@echo "  uninstall    Remove installed files"
	@echo "  clean        Remove build artifacts"
	@echo "  help         Show this help"
	@echo ""
	@echo "Variables:"
	@echo "  PREFIX       Installation prefix (default: $(HOME)/.local; packagers: /usr)"
	@echo "  DESTDIR      Staging dir for packagers (default: empty)"
	@echo "  ARGS         Arguments for 'make run' (e.g. ARGS=test.md)"
