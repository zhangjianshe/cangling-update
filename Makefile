# Single-binary release builds for the two host architectures.
# ARM cross-compile needs:
#   rustup target add aarch64-unknown-linux-gnu
#   sudo apt install gcc-aarch64-linux-gnu
#
# Outputs:
#   target/x86_64-unknown-linux-gnu/release/cangling-update
#   target/aarch64-unknown-linux-gnu/release/cangling-update

.PHONY: build x86 arm64 release clean

build:
	cargo build --release

x86:
	rustup target add x86_64-unknown-linux-gnu
	cargo build --release --target x86_64-unknown-linux-gnu

arm64:
	rustup target add aarch64-unknown-linux-gnu
	cargo build --release --target aarch64-unknown-linux-gnu

release: x86 arm64

clean:
	cargo clean
