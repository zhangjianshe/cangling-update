# Static musl release builds (no host glibc). Runs on Ubuntu 20.04+, Debian 11+, etc.
# x86_64: sudo apt install musl-tools
# ARM64:  a musl cross linker (CI uses taiki-e/setup-cross-toolchain-action)
#
# Outputs:
#   target/x86_64-unknown-linux-musl/release/cangling-update
#   target/aarch64-unknown-linux-musl/release/cangling-update

.PHONY: build x86 arm64 release clean

build:
	cargo build --release

x86:
	rustup target add x86_64-unknown-linux-musl
	CC_x86_64_unknown_linux_musl=$${CC_x86_64_unknown_linux_musl:-musl-gcc} \
		cargo build --release --target x86_64-unknown-linux-musl

arm64:
	rustup target add aarch64-unknown-linux-musl
	cargo build --release --target aarch64-unknown-linux-musl

release: x86 arm64

clean:
	cargo clean
