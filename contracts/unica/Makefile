ROOT := $(abspath $(dir $(lastword $(MAKEFILE_LIST)))/../..)

default: build

all: test

test: build
	cargo test --manifest-path $(ROOT)/Cargo.toml

build:
	stellar contract build --manifest-path $(ROOT)/Cargo.toml
	@ls -l $(ROOT)/target/wasm32v1-none/release/*.wasm

fmt:
	cargo fmt --all --manifest-path $(ROOT)/Cargo.toml

clean:
	cargo clean --manifest-path $(ROOT)/Cargo.toml
