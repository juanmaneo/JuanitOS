#!/bin/sh
# Fetch FreeBSD release pieces needed for testing on a non-FreeBSD host,
# into $JOS_WORK/freebsd/ (JOS_WORK defaults to ./work, which survives
# `cargo clean`). Everything is verified against the release MANIFEST
# (SHA-256).
#
#   tools/fetch-freebsd.sh loader        # base.txz -> target/work/freebsd/boot/loader.efi
#   tools/fetch-freebsd.sh vm            # qcow2 VM image for `jos-release builder`
#
# Version/arch default to the release profile (release/juanitos.toml).

set -eu

here=$(cd "$(dirname "$0")/.." && pwd)
work="${JOS_WORK:-$here/target/work}/freebsd"
mkdir -p "$work"

# (tr: the profile may have CRLF endings on a Windows checkout)
profile() { tr -d '\r' < "$here/release/juanitos.toml" | sed -n "s/^$1 *= *\"\\(.*\\)\"/\\1/p"; }
ver=$(profile freebsd_version)
arch=$(profile arch)
mirror=$(profile mirror)
: "${ver:=15.1-RELEASE}" "${arch:=amd64}" "${mirror:=https://download.freebsd.org/releases}"

fetch() {
	url=$1; dst=$2
	[ -f "$dst" ] && { echo "   $(basename "$dst"): present"; return 0; }
	echo "   fetching $url"
	curl -fL --retry 3 -# -o "$dst.part" "$url"
	mv "$dst.part" "$dst"
}

verify() {
	file=$1; manifest=$2
	want=$(awk -v f="$(basename "$file")" '$1 == f { print $2 }' "$manifest")
	[ -n "$want" ] || { echo "   $(basename "$file") not in MANIFEST"; return 1; }
	got=$(sha256sum "$file" | cut -c1-64)
	[ "$got" = "$want" ] || { echo "   sha256 MISMATCH for $file"; return 1; }
	echo "   $(basename "$file"): sha256 OK"
}

case "${1:-}" in
loader)
	echo "==> FreeBSD $ver $arch loader.efi"
	fetch "$mirror/$arch/$ver/MANIFEST" "$work/MANIFEST"
	fetch "$mirror/$arch/$ver/base.txz" "$work/base.txz"
	verify "$work/base.txz" "$work/MANIFEST"
	(cd "$work" && tar -xJf base.txz ./boot/loader.efi ./boot/defaults/loader.conf 2>/dev/null)
	ls -la "$work/boot/loader.efi" ;;
vm)
	# Since 15.0 the VM images carry the root file system in their name.
	img="FreeBSD-$ver-$arch-zfs.qcow2"
	echo "==> FreeBSD $ver $arch VM image"
	base="$mirror/VM-IMAGES/$ver/$arch/Latest"
	fetch "$base/CHECKSUM.SHA256" "$work/CHECKSUM.SHA256"
	fetch "$base/$img.xz" "$work/$img.xz"
	want=$(sed -n "s/^SHA256 ($img.xz) = //p" "$work/CHECKSUM.SHA256")
	got=$(sha256sum "$work/$img.xz" | cut -c1-64)
	[ "$got" = "$want" ] || { echo "   sha256 MISMATCH for $img.xz"; exit 1; }
	echo "   $img.xz: sha256 OK"
	[ -f "$work/$img" ] || xz -dk "$work/$img.xz"
	ls -la "$work/$img" ;;
*)
	sed -n 2,9p "$0"; exit 2 ;;
esac
