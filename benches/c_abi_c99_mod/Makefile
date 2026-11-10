UNAME_S := $(shell uname -s)

ifeq ($(UNAME_S),Darwin)
    TARGET_DEBUG   = libc_abi_c99_mod_debug.dylib
    TARGET_RELEASE = libc_abi_c99_mod_release.dylib
    LDFLAGS = -shared -fPIC -lm
else ifeq ($(OS),Windows_NT)
    TARGET_DEBUG   = c_abi_c99_mod_debug.dll
    TARGET_RELEASE = c_abi_c99_mod_release.dll
    LDFLAGS = -shared
else
    TARGET_DEBUG   = libc_abi_c99_mod_debug.so
    TARGET_RELEASE = libc_abi_c99_mod_release.so
    LDFLAGS = -shared -fPIC -lm
endif

BASE_CFLAGS = -std=c99 -Wall -Wextra -Werror -I../../include

all: $(TARGET_DEBUG) $(TARGET_RELEASE)

$(TARGET_DEBUG): mod.c ../../include/bevy_abi_mod.h
	$(CC) $(BASE_CFLAGS) -O0 -g $(LDFLAGS) -o $@ $<

$(TARGET_RELEASE): mod.c ../../include/bevy_abi_mod.h
	$(CC) $(BASE_CFLAGS) -O3 $(LDFLAGS) -o $@ $<

clean:
	rm -f $(TARGET_DEBUG) $(TARGET_RELEASE)
	rm -rf *.dSYM

.PHONY: all clean
