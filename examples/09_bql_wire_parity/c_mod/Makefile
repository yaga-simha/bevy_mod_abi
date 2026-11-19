CC ?= cc
UNAME_S := $(shell uname -s)

CFLAGS := -std=c99 -Wall -Wextra -Wpedantic -Werror -O2 -fPIC -I../../../include

ifeq ($(UNAME_S),Darwin)
    EXT := dylib
    LDFLAGS := -dynamiclib
else
    EXT := so
    LDFLAGS := -shared
endif

TARGET_V1 := libbql_parity_c_v1.$(EXT)
TARGET_V2 := libbql_parity_c_v2.$(EXT)

.PHONY: all clean

all: $(TARGET_V1) $(TARGET_V2)

$(TARGET_V1): bql_parity_mod.c ../../../include/bevy_abi_mod.h
	$(CC) $(CFLAGS) $(LDFLAGS) bql_parity_mod.c -o $(TARGET_V1)

$(TARGET_V2): bql_parity_mod.c ../../../include/bevy_abi_mod.h
	$(CC) $(CFLAGS) -DBQL_SCHEMA_V2=1 $(LDFLAGS) bql_parity_mod.c -o $(TARGET_V2)

clean:
	rm -f $(TARGET_V1) $(TARGET_V2)
