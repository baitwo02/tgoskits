CC ?= cc
CFLAGS ?= -std=c11 -Wall -Wextra -Werror -O2

IVC_DIR := ..
APP_PROTO_SOURCE := $(IVC_DIR)/lib/app_proto.c
APP_PROTO_HEADER := $(IVC_DIR)/include/ivc/app_proto.h

.PHONY: all test clean

all: test_app_proto

test_app_proto: test_app_proto.c $(APP_PROTO_SOURCE) $(APP_PROTO_HEADER)
	$(CC) $(CFLAGS) -I$(IVC_DIR)/include -o $@ test_app_proto.c $(APP_PROTO_SOURCE)

test: test_app_proto
	./test_app_proto

clean:
	rm -f test_app_proto
