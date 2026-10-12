#!/bin/sh
# Build every Rust artifact of JuanitOS. Runs inside the Dockerfile image
# (entrypoint) but works on any Linux with the same toolchain:
#   rustup target add x86_64-unknown-uefi; apt install mtools
#
# Output (relative to CARGO_TARGET_DIR, default target/docker):
#   release/jos, jos-install, jos-release, jos-splash      host binaries (Linux here;
#                                                          FreeBSD builds its own)
#   x86_64-unknown-uefi/release/jos-boot.efi               graphical pre-loader
#   x86_64-unknown-uefi/release/jos-firewall.efi           sys-firewall unikernel
#   jos-firewall.img                                       its 2 MB FAT boot image
#
# Arguments: `check` runs fmt/clippy/tests instead of building release.

set -eu

# Repository root: the script's parent directory when run from tools/, the
# current directory when installed elsewhere (the Docker entrypoint).
here=$(cd "$(dirname "$0")/.." && pwd)
[ -f "$here/Cargo.toml" ] || here=$(pwd)
[ -f "$here/Cargo.toml" ] || { echo "run from the JuanitOS repository root"; exit 1; }
cd "$here"
tgt=${CARGO_TARGET_DIR:-$here/target/docker}
export CARGO_TARGET_DIR="$tgt"
uefi="$tgt/x86_64-unknown-uefi/release"

if [ "${1:-}" = check ]; then
	cargo fmt --all -- --check
	cargo clippy --all-targets -- -D warnings
	cargo test
	cargo clippy --target x86_64-unknown-freebsd --workspace -- -D warnings
	for c in jos-boot jos-firewall; do
		(cd "crates/$c" && cargo clippy --all-targets -- -D warnings)
	done
	(cd crates/jos-boot && cargo test --lib --target x86_64-unknown-linux-gnu)
	echo "check: OK"
	exit 0
fi

echo "==> Host workspace"
cargo build --release --workspace

echo "==> UEFI applications"
for c in jos-boot jos-firewall; do
	(cd "crates/$c" && cargo build --release --target x86_64-unknown-uefi)
done

echo "==> Firewall boot image"
img="$tgt/jos-firewall.img"
rm -f "$img"
mformat -C -i "$img" -T 4096 -v JOSFW ::
mmd -i "$img" ::/EFI ::/EFI/BOOT
mcopy -i "$img" "$uefi/jos-firewall.efi" ::/EFI/BOOT/BOOTX64.EFI

echo "==> Kernel splash preview"
"$tgt/release/jos-splash" render "$tgt/juanitos-splash.png" --card --stage kernel

echo
echo "Artifacts in $tgt:"
ls -la "$tgt/release/jos" "$tgt/release/jos-install" "$tgt/release/jos-release" \
	"$tgt/release/jos-splash" "$uefi/jos-boot.efi" "$uefi/jos-firewall.efi" "$img" \
	"$tgt/juanitos-splash.png"
