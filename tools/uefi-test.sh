#!/bin/sh
# Boot the JuanitOS UEFI applications under QEMU + OVMF on a Linux host.
# Needs: qemu-system-x86_64, OVMF, mtools (mformat/mcopy). No root.
#
#   tools/uefi-test.sh boot            # jos-boot.efi: card, menu, chain-load error path
#   tools/uefi-test.sh boot-loader [L] # same, with FreeBSD's loader.efi at EFI/FREEBSD
#                                      # (default: work/freebsd/boot/loader.efi from
#                                      #  tools/fetch-freebsd.sh loader)
#   tools/uefi-test.sh loader-only [L] # control: FreeBSD's loader.efi alone (verbose text boot)
#   tools/uefi-test.sh firewall        # jos-firewall.efi with two virtio-nets + config disk;
#                                      # tools/fw-client.py plays the downstream qube
#   tools/uefi-test.sh build           # just (re)build the .efi files and images
#
# Everything lands in $JOS_WORK/uefi-test/ (JOS_WORK defaults to ./work;
# not target/, which `cargo clean` wipes). Screens are captured to
# <name>-NN.ppm via the QEMU monitor; serial output goes to
# <name>.serial.log. Set DISPLAY_QEMU=1 to get a window instead of headless.

set -eu

here=$(cd "$(dirname "$0")/.." && pwd)
work=${JOS_WORK:-$here/work}
out="$work/uefi-test"
uefi="$here/target/uefi/x86_64-unknown-uefi/release"
mkdir -p "$out"

ovmf=""
for f in /usr/share/OVMF/OVMF_CODE_4M.fd /usr/share/OVMF/OVMF_CODE.fd \
         /usr/share/edk2/ovmf/OVMF_CODE.fd /usr/share/edk2/x64/OVMF_CODE.4m.fd \
         /usr/share/qemu/OVMF.fd; do
	[ -f "$f" ] && { ovmf="$f"; break; }
done
[ -n "$ovmf" ] || { echo "OVMF not found (apt install ovmf)"; exit 1; }
vars="$out/OVMF_VARS.fd"
if [ ! -f "$vars" ]; then
	v=$(dirname "$ovmf")/OVMF_VARS_4M.fd
	[ -f "$v" ] || v=$(dirname "$ovmf")/OVMF_VARS.fd
	cp "$v" "$vars"
fi

accel="-accel tcg"
[ -w /dev/kvm ] && accel="-accel kvm -cpu host"

build() {
	for c in jos-boot jos-firewall; do
		(cd "$here/crates/$c" && cargo build --release --target-dir "$here/target/uefi")
	done
}

# fat_image <img> <size-MB> <file>=<dest> ...
fat_image() {
	img=$1; size=$2; shift 2
	rm -f "$img"
	mformat -C -i "$img" -T $((size * 2048)) -v JOSTEST ::
	for spec in "$@"; do
		src=${spec%%=*}; dst=${spec#*=}
		d=$(dirname "$dst")
		p=""
		for comp in $(echo "$d" | tr '/' ' '); do
			p="$p/$comp"
			mmd -i "$img" "::$p" 2>/dev/null || true
		done
		mcopy -i "$img" -o "$src" "::$dst"
	done
}

# run <name> <seconds> <qemu args...>; set RUN_CLIENT to a command that is
# started 8 s after QEMU and whose exit status becomes the run's result.
run() {
	name=$1; secs=$2; shift 2
	client_status=0
	log="$out/$name.serial.log"
	# Unix sockets cannot live on some mounts (WSL's DrvFs), so use /tmp.
	mon="${TMPDIR:-/tmp}/jos-uefi-test-$name-$$.mon"
	rm -f "$log" "$mon"
	display="-display none"
	[ "${DISPLAY_QEMU:-}" = 1 ] && display=""
	qemu-system-x86_64 $accel -m 256 -machine q35 \
		-drive if=pflash,format=raw,readonly=on,file="$ovmf" \
		-drive if=pflash,format=raw,file="$vars" \
		-serial file:"$log" $display -vga std \
		-monitor unix:"$mon",server,nowait \
		-no-reboot "$@" &
	pid=$!
	i=0
	while [ $i -lt "$secs" ] && kill -0 $pid 2>/dev/null; do
		sleep 1; i=$((i + 1))
		if [ $i -eq 8 ] && [ -n "${RUN_CLIENT:-}" ]; then
			echo "--- $name: client ---"
			sh -c "$RUN_CLIENT" || client_status=$?
		fi
		if [ -S "$mon" ] && [ $((i % 2)) -eq 0 ]; then
			printf 'screendump %s/%s-%02d.ppm\n' "$out" "$name" "$i" | \
				socat - UNIX-CONNECT:"$mon" >/dev/null 2>&1 || \
			printf 'screendump %s/%s-%02d.ppm\n' "$out" "$name" "$i" | \
				nc -U -q0 "$mon" >/dev/null 2>&1 || true
		fi
	done
	kill $pid 2>/dev/null || true
	wait $pid 2>/dev/null || true
	rm -f "$mon"
	echo "--- $name: serial log ---"
	cat "$log" 2>/dev/null || true
	ls "$out"/"$name"-*.ppm 2>/dev/null | tail -3
	return $client_status
}

case "${1:-}" in
build)
	build ;;
boot)
	[ -f "$uefi/jos-boot.efi" ] || build
	fat_image "$out/boot.img" 8 "$uefi/jos-boot.efi=EFI/BOOT/BOOTX64.EFI"
	run boot 14 -drive format=raw,file="$out/boot.img" ;;
boot-loader)
	loader=${2:-$work/freebsd/boot/loader.efi}
	[ -f "$loader" ] || "$here/tools/fetch-freebsd.sh" loader
	[ -f "$uefi/jos-boot.efi" ] || build
	fat_image "$out/boot-loader.img" 16 \
		"$uefi/jos-boot.efi=EFI/BOOT/BOOTX64.EFI" \
		"$loader=EFI/FREEBSD/LOADER.EFI"
	run boot-loader 20 -drive format=raw,file="$out/boot-loader.img" ;;
loader-only)
	loader=${2:-$work/freebsd/boot/loader.efi}
	[ -f "$loader" ] || "$here/tools/fetch-freebsd.sh" loader
	fat_image "$out/loader-only.img" 16 "$loader=EFI/BOOT/BOOTX64.EFI"
	run loader-only 16 -drive format=raw,file="$out/loader-only.img" ;;
firewall)
	[ -f "$uefi/jos-firewall.efi" ] || build
	fat_image "$out/firewall.img" 4 "$uefi/jos-firewall.efi=EFI/BOOT/BOOTX64.EFI"
	# Config disk: what `jos firewall render` produces, NUL-terminated,
	# padded to a sector. Upstream gets DHCP from QEMU's user network.
	cfg="$out/fwcfg.txt"
	printf 'firewall sys-firewall\ngateway 10.137.0.2\nupstream dhcp\nclient work 10.137.0.3 02:4a:5b:98:d2:60\npolicy work allow\n' > "$cfg"
	raw="$out/fwcfg.raw"
	{ cat "$cfg"; printf '\0'; } > "$raw"
	truncate -s 4096 "$raw"
	# bhyve layout: slot 3 upstream, slot 4 downstream, slot 30 (0x1e)
	# config disk. QEMU takes PCI slots in hex. q35 puts these on the root
	# bus as bhyve does. Disable the firmware's iPXE on the NICs so the
	# SimpleNetwork protocol is what the unikernel gets.
	RUN_CLIENT="python3 '$here/tools/fw-client.py'" \
	run firewall 40 \
		-drive format=raw,file="$out/firewall.img",if=none,id=boot \
		-device virtio-blk-pci,drive=boot,addr=0x2 \
		-netdev user,id=up -device virtio-net-pci,netdev=up,addr=0x3,mac=02:4a:00:00:00:01,romfile= \
		-netdev socket,id=down,listen=:7777 -device virtio-net-pci,netdev=down,addr=0x4,mac=02:4a:00:00:00:02,romfile= \
		-drive format=raw,file="$raw",if=none,id=cfg \
		-device virtio-blk-pci,drive=cfg,addr=0x1e ;;
*)
	sed -n 2,14p "$0"; exit 2 ;;
esac
