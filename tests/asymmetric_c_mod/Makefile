UNAME_S := $(shell uname -s)

ifeq ($(UNAME_S),Darwin)
    TARGET = libasymmetric_c_mod.dylib
    LDFLAGS = -shared -fPIC
else ifeq ($(OS),Windows_NT)
    TARGET = asymmetric_c_mod.dll
    LDFLAGS = -shared
else
    TARGET = libasymmetric_c_mod.so
    LDFLAGS = -shared -fPIC
endif

CFLAGS ?= -O3 -Wall -Wextra -Werror -std=c99 -I../../include

all: $(TARGET)

$(TARGET): mod.c ../../include/bevy_abi_mod.h
	$(CC) $(CFLAGS) $(LDFLAGS) -o $@ $<

clean:
	rm -f $(TARGET)
