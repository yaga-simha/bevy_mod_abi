UNAME_S := $(shell uname -s)
CXX ?= c++

ifeq ($(UNAME_S),Darwin)
    TARGET = libcpp17_mod.dylib
    LDFLAGS = -shared -fPIC
else ifeq ($(OS),Windows_NT)
    TARGET = cpp17_mod.dll
    LDFLAGS = -shared
else
    TARGET = libcpp17_mod.so
    LDFLAGS = -shared -fPIC
endif

# Strict ISO C++17 compile flags (override prevents environment CXXFLAGS clobbering)
override CXXFLAGS += -std=c++17 -Wall -Wextra -Wpedantic -Werror -I../../../include -O3

all: $(TARGET)

$(TARGET): mod.cpp ../../../include/bevy_abi_mod.h
	$(CXX) $(CXXFLAGS) $(LDFLAGS) -o $@ $<

clean:
	rm -f $(TARGET)

.PHONY: all clean
