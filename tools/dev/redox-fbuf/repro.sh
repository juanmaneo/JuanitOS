#!/bin/sh
# Reproducer: bhyve aborts when a Redox OS guest sizes the BARs of the
# `fbuf` display device. See README.md next to this script for the
# analysis. Runs on FreeBSD as root (the build VM of
# tools/dev/freebsd-vm.sh, or any host with vmm.ko loaded, bhyve, the
# edk2-bhyve package and zstd).
#
#   repro.sh fetch      download the latest Redox `server` nightly (SHA-256 checked)
#   repro.sh run        boot it with `-s 4,fbuf`: bhyve aborts (exit 134) as soon as
#                       pcid reaches the display device
#   repro.sh control    the same VM without fbuf: Redox boots to its login prompt
#   repro.sh trace      build bhyve from /usr/src with the BAR bookkeeping prints of
#                       instrument-bhyve.pl and run again, printing the fbuf sequence
#   repro.sh all        fetch, run, control
#
# Environment: REDOX_WORK (scratch, default /root/redox), REDOX_IMAGE (an
# already decompressed image instead of the nightly), BHYVE (binary,
# default bhyve), TIMEOUT (seconds a boot is allowed, default 120),
# SRC_TXZ_URL (FreeBSD source tarball for `trace`).

set -eu

work=${REDOX_WORK:-/root/redox}
image=${REDOX_IMAGE:-$work/redox-server.img}
bhyve=${BHYVE:-bhyve}
timeout_s=${TIMEOUT:-120}
bootrom=/usr/local/share/uefi-firmware/BHYVE_UEFI.fd
vm=redox-fbuf-repro
nmdm=/dev/nmdm-redoxfbuf
img_base=https://static.redox-os.org/img/x86_64

log() { printf '==> %s\n' "$*" >&2; }

cmd_fetch() {
	mkdir -p "$work"
	cd "$work"
	log "Fetching $img_base/SHA256SUM"
	fetch -q -o SHA256SUM "$img_base/SHA256SUM"
	name=$(grep -o 'redox_server_x86_64_[0-9-]*_[0-9]*_harddrive\.img\.zst' SHA256SUM | tail -n 1)
	[ -n "$name" ] || { echo "no server image listed in SHA256SUM"; exit 1; }
	want=$(grep "$name" SHA256SUM | awk '{print $1}')
	if [ ! -f "$name" ]; then
		log "Fetching $name"
		fetch -o "$name" "$img_base/$name"
	fi
	have=$(sha256 -q "$name")
	[ "$have" = "$want" ] || { echo "sha256 mismatch for $name: $have != $want"; exit 1; }
	log "sha256 OK; decompressing"
	zstd -d -f -q "$name" -o "$image"
	printf '%s\n' "$name" > "$work/IMAGE"
	ls -l "$image"
}

# boot NAME [fbuf|nofbuf] -> writes $work/NAME.bhyve.out and $work/NAME.console.log
boot() {
	name=$1
	mode=$2
	[ -f "$image" ] || { echo "no image at $image: run '$0 fetch' or set REDOX_IMAGE"; exit 1; }
	kldload -n vmm
	kldload -n nmdm
	cd "$work"
	bhyvectl --destroy --vm="$vm" 2>/dev/null || true
	# Redox writes to its disk: boot a copy.
	cp "$image" "$work/run.img"
	fbuf_slot=""
	[ "$mode" = fbuf ] && fbuf_slot="-s 4,fbuf,tcp=127.0.0.1:5900,w=1024,h=768"
	: > "$work/$name.console.log"
	cat "${nmdm}B" > "$work/$name.console.log" 2>&1 &
	cat_pid=$!
	sleep 1
	log "Booting Redox ($mode) with $bhyve for at most $timeout_s s"
	set +e
	# shellcheck disable=SC2086
	timeout "$timeout_s" "$bhyve" -c 2 -m 2G -H -w -u \
		-l "bootrom,$bootrom" \
		-s 0,hostbridge -s 3,nvme,run.img $fbuf_slot -s 31,lpc \
		-l "com1,${nmdm}A" "$vm" > "$work/$name.bhyve.out" 2>&1
	status=$?
	set -e
	bhyvectl --destroy --vm="$vm" 2>/dev/null || true
	kill "$cat_pid" 2>/dev/null || true
	echo "$status"
}

cmd_run() {
	status=$(boot run fbuf)
	echo "bhyve exit status: $status (134 = abort)"
	if grep -q 'Assertion failed.*modify_bar_registration' "$work/run.bhyve.out"; then
		echo "REPRODUCED:"
		grep 'Assertion failed' "$work/run.bhyve.out"
		echo "Redox console before the abort (pcid had just reached the fbuf device):"
		tr -d '\r' < "$work/run.console.log" | sed 's/\x1b\[[0-9;]*m//g' | grep -v '^$' | grep '@pcid' | tail -n 4
	else
		echo "NOT REPRODUCED; bhyve output:"
		tail -n 5 "$work/run.bhyve.out"
		echo "console tail:"
		tr -d '\r' < "$work/run.console.log" | grep -v '^$' | tail -n 5
	fi
}

cmd_control() {
	status=$(boot control nofbuf)
	echo "bhyve exit status: $status (124 = still running when the timeout hit, as expected)"
	if tr -d '\r' < "$work/control.console.log" | grep -q 'login:'; then
		echo "CONTROL OK: without fbuf Redox boots to its login prompt"
	else
		echo "CONTROL: no login prompt within $timeout_s s; console tail:"
		tr -d '\r' < "$work/control.console.log" | grep -v '^$' | tail -n 10
	fi
}

cmd_trace() {
	src=/usr/src/usr.sbin/bhyve
	if [ ! -f "$src/pci_emul.c" ]; then
		url=${SRC_TXZ_URL:-https://download.freebsd.org/releases/amd64/$(uname -r | sed 's/-p[0-9]*$//')/src.txz}
		log "Fetching $url"
		mkdir -p "$work"
		fetch -o "$work/src.txz" "$url"
		tar -xf "$work/src.txz" -C /
	fi
	if ! grep -q 'bar-debug' "$src/pci_emul.c"; then
		perl "$(dirname "$0")/instrument-bhyve.pl" "$src"
	fi
	log "Building the instrumented bhyve"
	make -C "$src" -j4 > "$work/bhyve-build.log" 2>&1
	built=$(make -C "$src" -V .OBJDIR)/bhyve
	[ -x "$built" ] || { echo "no $built; see $work/bhyve-build.log"; exit 1; }
	BHYVE=$built BHYVE_BAR_DEBUG=1 export BHYVE BHYVE_BAR_DEBUG
	bhyve=$built
	status=$(boot trace fbuf)
	echo "bhyve exit status: $status"
	echo "BAR traffic of the fbuf device (cfg-debug = guest config writes, bar-debug = bhyve registrations):"
	grep -n 'fbuf\|overlap detected\|Assertion' "$work/trace.bhyve.out"
}

case "${1:-}" in
fetch) cmd_fetch ;;
run) cmd_run ;;
control) cmd_control ;;
trace) cmd_trace ;;
all) cmd_fetch; cmd_run; cmd_control ;;
*) sed -n 2,20p "$0"; exit 2 ;;
esac
