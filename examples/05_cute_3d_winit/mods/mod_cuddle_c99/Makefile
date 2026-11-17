UNAME_S := $(shell uname -s)

ifeq ($(UNAME_S),Darwin)
    TARGET := libmod_cuddle.dylib
    LDFLAGS = -shared -fPIC -lm
else ifeq ($(OS),Windows_NT)
    TARGET := mod_cuddle.dll
    LDFLAGS = -shared
else
    TARGET := libmod_cuddle.so
    LDFLAGS = -shared -fPIC -lm
endif

override CFLAGS += -std=c99 -Wall -Wextra -Wpedantic -Werror -I../../../../include -O3

all: $(TARGET)

$(TARGET): mod.c ../../../../include/bevy_abi_mod.h
	$(CC) $(CFLAGS) $(LDFLAGS) -o $@ $<

clean:
	rm -f $(TARGET)
	rm -rf *.dSYM

.PHONY: all clean
