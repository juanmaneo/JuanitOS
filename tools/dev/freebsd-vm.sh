#!/bin/sh
# Stock FreeBSD VM under QEMU/KVM for development work that needs a
# FreeBSD userland (building COSMIC packages, running bhyve for guest
# experiments, ...). Everything lives under $JOS_WORK (default
# ./target/work) so `cargo clean` reclaims it.
#
#   tools/dev/freebsd-vm.sh create [NAME] [SIZE_GB]   raw disk from the release qcow2 (tools/fetch-freebsd.sh vm)
#   tools/dev/freebsd-vm.sh start  [NAME]             boot in the background; serial on a unix socket, ssh on :2222
#   tools/dev/freebsd-vm.sh stop   [NAME]             ACPI power button, then wait
#   tools/dev/freebsd-vm.sh ssh    [NAME] [CMD...]    ssh root@localhost -p 2222
#   tools/dev/freebsd-vm.sh scp    [NAME] ARGS...     scp with the key and port (remote side as root@127.0.0.1:path)
#   tools/dev/freebsd-vm.sh status [NAME]
#   tools/dev/freebsd-vm.sh trim   [NAME]             zpool trim inside; the sparse disk gives the space back to the host
#
# The serial console is at $JOS_WORK/vm/NAME/serial.sock (talk to it with
# tools/dev/serial.py). A QEMU monitor is at monitor.sock. `create` makes
# an ssh key in $JOS_WORK/vm/id_ed25519; the first boot needs
# tools/dev/serial.py to install it and enable sshd (see `firstboot`);
# after that everything goes over ssh.
#
#   tools/dev/freebsd-vm.sh firstboot [NAME]          via the serial console: log in, install the key, start sshd
#
# Environment: VM_CPUS (default 8), VM_MEM_MB (default 16384),
# VM_SSH_PORT (default 2222), VM_EXTRA (extra qemu arguments, e.g. a
# second disk), VM_NESTED=1 passes the host CPU through so bhyve works
# inside the guest (needs nested virtualisation enabled on the host).

set -eu

here=$(cd "$(dirname "$0")/../.." && pwd)
work="${JOS_WORK:-$here/target/work}"
cmd=${1:-}
name=${2:-freebsd}
dir="$work/vm/$name"
disk="$dir/disk.raw"
ssh_port=${VM_SSH_PORT:-2222}
ovmf_code=${OVMF_CODE:-/usr/share/edk2/x64/OVMF_CODE.4m.fd}
ovmf_vars_src=${OVMF_VARS:-/usr/share/edk2/x64/OVMF_VARS.4m.fd}

profile() { tr -d '\r' < "$here/release/juanitos.toml" | sed -n "s/^$1 *= *\"\\(.*\\)\"/\\1/p"; }

case "$cmd" in
create)
	size=${3:-80}
	ver=$(profile freebsd_version); arch=$(profile arch)
	: "${ver:=15.1-RELEASE}" "${arch:=amd64}"
	src="$work/freebsd/FreeBSD-$ver-$arch-zfs.qcow2"
	[ -f "$src" ] || { echo "missing $src: run tools/fetch-freebsd.sh vm"; exit 1; }
	mkdir -p "$dir"
	[ -f "$disk" ] && { echo "$disk exists"; exit 1; }
	qemu-img convert -O raw "$src" "$disk"
	qemu-img resize -f raw "$disk" "${size}G"
	# Serial console for the loader and kernel: loader.env on the ESP
	# (second GPT partition of the release image, sector 381).
	esp_off=$(sfdisk -J "$disk" | python3 -c '
import json, sys
t = json.load(sys.stdin)["partitiontable"]
p = [p for p in t["partitions"] if p["type"].upper() == "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"][0]
print(p["start"] * t.get("sectorsize", 512))')
	printf 'boot_multicons=YES\nboot_serial=YES\nhw.uart.console=io:0x3f8,br:115200\nautoboot_delay=2\n' > "$dir/loader.env"
	mmd -i "$disk@@$esp_off" ::/efi/freebsd 2>/dev/null || true
	mcopy -o -i "$disk@@$esp_off" "$dir/loader.env" ::/efi/freebsd/loader.env
	cp "$ovmf_vars_src" "$dir/ovmf_vars.fd"
	[ -f "$work/vm/id_ed25519" ] || ssh-keygen -q -t ed25519 -N "" -f "$work/vm/id_ed25519"
	echo "created $disk (${size} GB)"
	;;
firstboot)
	pub=$(cat "$work/vm/id_ed25519.pub")
	python3 "$here/tools/dev/serial.py" "$dir/serial.sock" login root "" 300
	python3 "$here/tools/dev/serial.py" "$dir/serial.sock" run \
		"mkdir -p /root/.ssh && chmod 700 /root/.ssh && echo '$pub' > /root/.ssh/authorized_keys && sysrc sshd_enable=YES && sed -i '' 's/^#PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config && service sshd start" 120
	;;
start)
	[ -f "$disk" ] || { echo "no $disk: run '$0 create $name' first"; exit 1; }
	if [ -S "$dir/monitor.sock" ] && [ -f "$dir/qemu.pid" ] && kill -0 "$(cat "$dir/qemu.pid")" 2>/dev/null; then
		echo "already running (pid $(cat "$dir/qemu.pid"))"; exit 0
	fi
	cpu=max
	[ "${VM_NESTED:-0}" = 1 ] && cpu=host
	rm -f "$dir/serial.sock" "$dir/monitor.sock"
	# shellcheck disable=SC2086
	qemu-system-x86_64 \
		-machine q35,accel=kvm -cpu "$cpu" -smp "${VM_CPUS:-8}" -m "${VM_MEM_MB:-16384}" \
		-drive if=pflash,format=raw,readonly=on,file="$ovmf_code" \
		-drive if=pflash,format=raw,file="$dir/ovmf_vars.fd" \
		-drive file="$disk",if=virtio,format=raw,cache=unsafe,discard=unmap,detect-zeroes=unmap \
		-virtfs "local,path=$here,mount_tag=juanitos,security_model=mapped-xattr,id=juanitos" \
		-nic "user,model=virtio-net-pci,hostfwd=tcp:127.0.0.1:$ssh_port-:22" \
		-display none -vga none \
		-serial "unix:$dir/serial.sock,server=on,wait=off" \
		-monitor "unix:$dir/monitor.sock,server=on,wait=off" \
		-pidfile "$dir/qemu.pid" -daemonize \
		${VM_EXTRA:-}
	echo "started: serial $dir/serial.sock, ssh -p $ssh_port root@127.0.0.1"
	;;
stop)
	[ -S "$dir/monitor.sock" ] || { echo "not running"; exit 0; }
	printf 'system_powerdown\n' | python3 - "$dir/monitor.sock" <<'EOF'
import socket, sys, time
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.settimeout(2)
try: s.recv(4096)
except Exception: pass
s.sendall(b"system_powerdown\n"); time.sleep(0.5)
EOF
	i=0
	while [ -f "$dir/qemu.pid" ] && kill -0 "$(cat "$dir/qemu.pid")" 2>/dev/null; do
		i=$((i + 1)); [ $i -gt 120 ] && { echo "still running after 120 s"; exit 1; }
		sleep 1
	done
	echo "stopped"
	;;
ssh)
	shift 2 2>/dev/null || shift $#
	exec ssh -i "$work/vm/id_ed25519" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
		-o LogLevel=ERROR -o ConnectTimeout=10 -o ForwardX11=no -p "$ssh_port" root@127.0.0.1 "$@"
	;;
scp)
	# scp [NAME] SRC... DST   (remote side as :path)
	shift 2 2>/dev/null || shift $#
	exec scp -i "$work/vm/id_ed25519" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
		-o LogLevel=ERROR -P "$ssh_port" "$@"
	;;
status)
	if [ -f "$dir/qemu.pid" ] && kill -0 "$(cat "$dir/qemu.pid")" 2>/dev/null; then
		echo "running (pid $(cat "$dir/qemu.pid"))"
	else
		echo "stopped"
	fi
	;;
trim)
	# The disk is attached with discard=unmap: blocks ZFS trims become
	# holes in disk.raw. Builds leave tens of GB behind otherwise.
	before=$(du -h "$disk" | cut -f1)
	"$0" ssh "$name" 'zpool set autotrim=on zroot; zpool trim -w zroot'
	echo "disk.raw: $before -> $(du -h "$disk" | cut -f1)"
	;;
*)
	sed -n 2,21p "$0"; exit 2 ;;
esac
