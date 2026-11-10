UNAME_S := $(shell uname -s)

ifeq ($(UNAME_S),Darwin)
    TARGET = libc_mod.dylib
    LDFLAGS = -shared -fPIC
else ifeq ($(OS),Windows_NT)
    TARGET = c_mod.dll
    LDFLAGS = -shared
else
    TARGET = libc_mod.so
    LDFLAGS = -shared -fPIC
endif

# Mandatory C99 compile flags (override prevents environment CFLAGS clobbering)
override CFLAGS += -std=c99 -Wall -Wextra -Werror -I../../../include -O3

all: $(TARGET)

$(TARGET): mod.c ../../../include/bevy_abi_mod.h
	$(CC) $(CFLAGS) $(LDFLAGS) -o $@ $<

clean:
	rm -f $(TARGET)

.PHONY: all clean
