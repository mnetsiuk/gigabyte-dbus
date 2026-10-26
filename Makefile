DEBUG ?= 0
# Packages install the rule under /usr/lib/udev/rules.d; /etc is for a local `make install`.
UDEVRULESDIR ?= /etc/udev/rules.d
ifeq ($(DEBUG),0)
	ARGS += --release
	TARGET = release
else
	TARGET = debug
endif
BIN = ./target/$(TARGET)/gigabyted

all: build

clean:
	cargo clean

install:
	install -Dm755 $(BIN) $(DESTDIR)/usr/bin/gigabyted
	install -Dm644 gigabyted.rules $(DESTDIR)$(UDEVRULESDIR)/gigabyted.rules
	install -Dm644 gigabyted.service $(DESTDIR)/usr/lib/systemd/system/gigabyted.service
	install -Dm644 gigabyted.conf $(DESTDIR)/usr/share/dbus-1/system.d/gigabyted.conf

uninstall:
	rm -f $(DESTDIR)/usr/bin/gigabyted
	rm -f $(DESTDIR)$(UDEVRULESDIR)/gigabyted.rules
	rm -f $(DESTDIR)/usr/lib/systemd/system/gigabyted.service
	rm -f $(DESTDIR)/usr/share/dbus-1/system.d/gigabyted.conf

build:
	cargo build $(ARGS)
ifeq ($(DEBUG),0)
	strip -s $(BIN)
endif

.PHONY: all clean install uninstall build
