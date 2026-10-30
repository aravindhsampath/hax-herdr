PREFIX ?= $(HOME)/.local
HERDR_PROGRAMS ?= $(HOME)/.config/herdr-app/programs.json
# How Herdr shows hax and starts it from its + menu; layered over Herdr's built-in table.
ENTRY := {"name": "hax", "run": ["hax-herdr"]}

.PHONY: build check e2e install uninstall

build:
	cargo build --release --locked

check:
	cargo fmt --check
	cargo clippy --all-targets --locked -- -D warnings
	cargo nextest run --locked
	shellcheck scripts/*.sh

# Needs hc (herdr-core), hax and jq; uses hax's mock provider and a throwaway state folder.
e2e: build
	scripts/e2e.sh target/release/hax-herdr

install: build
	install -d $(PREFIX)/bin
	install -m 755 target/release/hax-herdr $(PREFIX)/bin/hax-herdr
	mkdir -p $(dir $(HERDR_PROGRAMS))
	[ -s $(HERDR_PROGRAMS) ] || echo '{}' > $(HERDR_PROGRAMS)
	jq --argjson e '$(ENTRY)' '.programs.hax = $$e' $(HERDR_PROGRAMS) > $(HERDR_PROGRAMS).tmp
	mv $(HERDR_PROGRAMS).tmp $(HERDR_PROGRAMS)

uninstall:
	rm -f $(PREFIX)/bin/hax-herdr
	[ ! -s $(HERDR_PROGRAMS) ] || { jq 'del(.programs.hax)' $(HERDR_PROGRAMS) > $(HERDR_PROGRAMS).tmp && mv $(HERDR_PROGRAMS).tmp $(HERDR_PROGRAMS); }
