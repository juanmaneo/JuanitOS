#!/bin/sh
# Ventoy stick image for testing the ISO the way a user boots it from a
# Ventoy USB key: the stick layout Ventoy2Disk.sh writes (MBR, p1 ext4
# "Ventoy" holding the ISO, p2 VTOYEFI), booted under QEMU/KVM either
# through OVMF or the BIOS. Everything lives under $JOS_WORK/ventoy
# (default ./target/work/ventoy), so `cargo clean` reclaims it.
#
#   tools/dev/ventoy-rig.sh make ISO [SIZE_GB]    build disk.img (default 10 GB) and copy ISO into it
#   tools/dev/ventoy-rig.sh start [uefi|bios]     boot the stick with a blank 20 GB virtio target disk
#   tools/dev/ventoy-rig.sh key KEYS...           QEMU monitor sendkey (ret, tab, esc, down, shift-a ...)
#   tools/dev/ventoy-rig.sh type STRING           type a string (letters, digits, - / . space)
#   tools/dev/ventoy-rig.sh shot FILE.png         screenshot (needs ImageMagick)
#   tools/dev/ventoy-rig.sh stop
#
# Needs: curl, xz, sfdisk, mkfs.ext4, fuse2fs (e2fsprogs 1.47+), qemu,
# socat. The ISO is copied through fuse2fs with --sparse=never: Ventoy
# reads the ISO by block list and refuses files with holes ("No bootfile
# found for UEFI!"), which is what `mkfs.ext4 -d` or `debugfs write`
# produce.

set -eu

here=$(cd "$(dirname "$0")/../.." && pwd)
work="${JOS_WORK:-$here/target/work}/ventoy"
ver=${VENTOY_VERSION:-1.1.18}
cmd=${1:-}
disk="$work/disk.img"
mon="$work/mon.sock"
ovmf_code=${OVMF_CODE:-/usr/share/edk2/x64/OVMF_CODE.4m.fd}
ovmf_vars=${OVMF_VARS:-/usr/share/edk2/x64/OVMF_VARS.4m.fd}

monitor() { printf '%s\n' "$*" | socat - "unix-connect:$mon" | grep -v '^QEMU\|^(qemu)' || true; }

case "$cmd" in
make)
	iso=${2:?ISO path}; size=${3:-10}
	[ -f "$iso" ] || { echo "no such ISO: $iso"; exit 1; }
	mkdir -p "$work"
	tgz="$work/ventoy-$ver-linux.tar.gz"
	[ -f "$tgz" ] || curl -fL --retry 3 -# -o "$tgz" \
		"https://github.com/ventoy/Ventoy/releases/download/v$ver/ventoy-$ver-linux.tar.gz"
	[ -d "$work/ventoy-$ver" ] || tar -C "$work" -xzf "$tgz"
	v="$work/ventoy-$ver"

	# Layout from tool/VentoyWorker.sh (MBR mode): p1 from sector 2048 to
	# the last 32 MB, p2 = the 32 MB VTOYEFI image at the end.
	sectors=$((size * 1024 * 1024 * 2))
	p2_start=$((sectors - 65536))
	p1_sectors=$((p2_start - 2048))
	rm -f "$disk"
	truncate -s "${size}G" "$disk"
	printf 'label: dos\nstart=2048, size=%d, type=07, bootable\nstart=%d, size=65536, type=ef\n' \
		"$p1_sectors" "$p2_start" | sfdisk -q "$disk"
	dd status=none conv=notrunc if="$v/boot/boot.img" of="$disk" bs=1 count=440
	xzcat "$v/boot/core.img.xz" | dd status=none conv=notrunc of="$disk" bs=512 count=2047 seek=1
	xzcat "$v/ventoy/ventoy.disk.img.xz" | dd status=none conv=notrunc of="$disk" bs=512 count=65536 seek="$p2_start"
	mkfs.ext4 -q -F -L Ventoy -E offset=1048576 "$disk" "$((p1_sectors / 2))k"

	echo "==> copying $(basename "$iso") into the stick (fully allocated)"
	mnt="$work/mnt"; mkdir -p "$mnt"
	fuse2fs -o offset=1048576,fakeroot "$disk" "$mnt"
	cp --sparse=never "$iso" "$mnt/" && sync
	fusermount3 -u "$mnt" || fusermount -u "$mnt"
	e2fsck -fn "$disk?offset=1048576" >/dev/null && echo "   ext4: clean"
	ls -la "$disk" ;;

start)
	mode=${2:-uefi}
	[ -f "$disk" ] || { echo "no $disk: run make first"; exit 1; }
	[ -f "$work/target.qcow2" ] || qemu-img create -q -f qcow2 "$work/target.qcow2" 20G
	fw=""
	if [ "$mode" = uefi ]; then
		cp "$ovmf_vars" "$work/vars.fd"
		fw="-drive if=pflash,format=raw,readonly=on,file=$ovmf_code -drive if=pflash,format=raw,file=$work/vars.fd"
	fi
	# shellcheck disable=SC2086
	setsid qemu-system-x86_64 -enable-kvm -cpu host -smp 2 -m 4096 -machine q35 $fw \
		-device qemu-xhci -drive if=none,id=stick,format=raw,file="$disk" \
		-device usb-storage,drive=stick,bootindex=0 \
		-drive if=virtio,format=qcow2,file="$work/target.qcow2" \
		-display none -vga std -monitor "unix:$mon,server,nowait" \
		-serial "file:$work/serial.log" > "$work/qemu.log" 2>&1 &
	echo "started ($mode); monitor at $mon, serial in $work/serial.log" ;;

key)
	shift
	for k in "$@"; do monitor "sendkey $k"; sleep 0.2; done ;;

type)
	printf '%s' "${2:?string}" | fold -w1 | while IFS= read -r c; do
		case "$c" in
		' ') k=spc ;; -) k=minus ;; /) k=slash ;; .) k=dot ;; _) k=shift-minus ;;
		[A-Z]) k="shift-$(printf '%s' "$c" | tr 'A-Z' 'a-z')" ;;
		*) k=$c ;;
		esac
		monitor "sendkey $k"; sleep 0.15
	done ;;

shot)
	out=${2:?file.png}
	monitor "screendump $work/shot.ppm"; sleep 3
	magick "$work/shot.ppm" "$out" 2>/dev/null || convert "$work/shot.ppm" "$out"
	echo "$out" ;;

stop)
	monitor quit; rm -f "$work/target.qcow2" "$work/vars.fd" ;;

*)
	sed -n '2,20p' "$0"; exit 1 ;;
esac
