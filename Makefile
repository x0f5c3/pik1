CC       ?= gcc
CFLAGS   := -O2 -std=c11 -Wall -Wextra -D_GNU_SOURCE \
             -ffunction-sections -fdata-sections
LDFLAGS  := -Wl,--gc-sections
SRCS     := src/serialmux.c src/channels.c
HDRS     := src/serialmux.h

STATIC   := -static
BUILD    := build

# ── Cross toolchains ─────────────────────────────────────────────────────────
# 'make toolchain' downloads musl.cc prebuilts into .toolchain/.
# Override any *_CC / *_STRIP on the command line to use a different compiler.
TOOLCHAIN_DIR  := $(CURDIR)/.toolchain
MUSL_CC_BASE   := https://musl.cc

MIPSEL_TRIPLE  := mipsel-linux-musl
AARCH64_TRIPLE := aarch64-linux-musl
ARMV7_TRIPLE   := arm-linux-musleabihf

MIPSEL_CC    ?= $(TOOLCHAIN_DIR)/$(MIPSEL_TRIPLE)-cross/bin/$(MIPSEL_TRIPLE)-gcc
MIPSEL_STRIP ?= $(TOOLCHAIN_DIR)/$(MIPSEL_TRIPLE)-cross/bin/$(MIPSEL_TRIPLE)-strip
AARCH64_CC   ?= $(TOOLCHAIN_DIR)/$(AARCH64_TRIPLE)-cross/bin/$(AARCH64_TRIPLE)-gcc
AARCH64_STRIP ?= $(TOOLCHAIN_DIR)/$(AARCH64_TRIPLE)-cross/bin/$(AARCH64_TRIPLE)-strip
ARMV7_CC     ?= $(TOOLCHAIN_DIR)/$(ARMV7_TRIPLE)-cross/bin/$(ARMV7_TRIPLE)-gcc
ARMV7_STRIP  ?= $(TOOLCHAIN_DIR)/$(ARMV7_TRIPLE)-cross/bin/$(ARMV7_TRIPLE)-strip

.PHONY: all native mipsel aarch64 armv7 toolchain clean distclean

all: native

native: $(BUILD)/serialmux

$(BUILD)/serialmux: $(SRCS) $(HDRS) | $(BUILD)
	$(CC) $(CFLAGS) $(LDFLAGS) -o $@ $(SRCS)

mipsel: $(BUILD)/serialmux.mipsel
$(BUILD)/serialmux.mipsel: $(SRCS) $(HDRS) | $(BUILD)
	$(MIPSEL_CC) $(CFLAGS) $(LDFLAGS) $(STATIC) -o $@ $(SRCS)
	-$(MIPSEL_STRIP) $@

aarch64: $(BUILD)/serialmux.aarch64
$(BUILD)/serialmux.aarch64: $(SRCS) $(HDRS) | $(BUILD)
	$(AARCH64_CC) $(CFLAGS) $(LDFLAGS) $(STATIC) -o $@ $(SRCS)
	-$(AARCH64_STRIP) $@

armv7: $(BUILD)/serialmux.armv7
$(BUILD)/serialmux.armv7: $(SRCS) $(HDRS) | $(BUILD)
	$(ARMV7_CC) $(CFLAGS) $(LDFLAGS) $(STATIC) -o $@ $(SRCS)
	-$(ARMV7_STRIP) $@

$(BUILD):
	mkdir -p $@

# Download toolchain tarballs from musl.cc into .toolchain/
# (GNU Make pattern rules only allow one %, so these are explicit)
define fetch_toolchain
$(TOOLCHAIN_DIR)/$(1)-cross/bin/$(1)-gcc:
	mkdir -p $(TOOLCHAIN_DIR)
	curl -fL --progress-bar $(MUSL_CC_BASE)/$(1)-cross.tgz | tar -xz -C $(TOOLCHAIN_DIR)
endef
$(eval $(call fetch_toolchain,$(MIPSEL_TRIPLE)))
$(eval $(call fetch_toolchain,$(AARCH64_TRIPLE)))
$(eval $(call fetch_toolchain,$(ARMV7_TRIPLE)))

toolchain: \
	$(TOOLCHAIN_DIR)/$(MIPSEL_TRIPLE)-cross/bin/$(MIPSEL_TRIPLE)-gcc \
	$(TOOLCHAIN_DIR)/$(AARCH64_TRIPLE)-cross/bin/$(AARCH64_TRIPLE)-gcc \
	$(TOOLCHAIN_DIR)/$(ARMV7_TRIPLE)-cross/bin/$(ARMV7_TRIPLE)-gcc

clean:
	rm -rf $(BUILD)

distclean: clean
	rm -rf $(TOOLCHAIN_DIR)
