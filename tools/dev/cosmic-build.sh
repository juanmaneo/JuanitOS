#!/bin/sh
# Build the COSMIC desktop (pop-os/cosmic-epoch) from source on FreeBSD and
# turn every component into a pkg(8) package in a local repository that
# `jos-release rootfs` installs from (`[packages] local_repo` in
# release/juanitos.toml). Runs inside the FreeBSD build VM
# (tools/dev/freebsd-vm.sh) as root; nothing here touches the host.
#
#   cosmic-build.sh deps              pkg install the build and run-time dependencies
#   cosmic-build.sh fetch             clone cosmic-epoch at $COSMIC_TAG (default epoch-1.10.0)
#   cosmic-build.sh patch             FreeBSD fixes (the ones x11-wm/cosmic-comp carries)
#   cosmic-build.sh build [NAME...]   cargo build --release (all components by default)
#   cosmic-build.sh package [NAME...] stage into a DESTDIR and pkg create
#   cosmic-build.sh repo              pkg repo: write the catalogue
#   cosmic-build.sh all               deps fetch patch build package repo
#   cosmic-build.sh list              the components and their status
#
# Layout under $COSMIC_WORK (default /root/cosmic):
#   src/      cosmic-epoch checkout (submodules; no wallpapers: git-lfs)
#   target/   shared CARGO_TARGET_DIR (dependencies compile once)
#   stage/    one DESTDIR per component
#   meta/     +MANIFEST per component
#   repo/     the packages and the catalogue. Copy it to the host as
#             target/work/cosmic/repo and point [packages] local_repo at it.
#   gnubin/   GNU install(1) and sed(1) under their Linux names: the
#             upstream justfiles use `install -D` and `sed -i`.
#
# A component that fails to build is reported and skipped (`build` goes on
# with the next one); fix it and re-run `build NAME`. Every component builds
# with the same shared target directory, so a re-run only rebuilds what
# changed.
#
# Sizing: the whole set takes a few hours and ~45 GB of target/ on 8-10
# cores; cosmic-comp alone (fat LTO) is the longest single link.

set -eu

here=$(cd "$(dirname "$0")" && pwd)
work=${COSMIC_WORK:-/root/cosmic}
src="$work/src"
tag=${COSMIC_TAG:-epoch-1.10.0}
version=${COSMIC_VERSION:-${tag#epoch-}}
prefix=/usr/local
jobs=${CARGO_BUILD_JOBS:-8}

# Components in build order: the compositor first (it validates the
# toolchain), then the session and shell, then the applications. A name is
# the submodule directory. `features` is passed to `cargo build`; systemd
# and logind are Linux-only so they are switched off where they are
# default features.
components="cosmic-comp cosmic-session cosmic-settings-daemon cosmic-panel cosmic-applets
pop-launcher cosmic-launcher cosmic-app-library cosmic-workspaces-epoch cosmic-bg cosmic-osd
cosmic-notifications cosmic-randr cosmic-idle xdg-desktop-portal-cosmic cosmic-settings
cosmic-icons cosmic-term cosmic-files cosmic-edit cosmic-screenshot cosmic-viewer cosmic-monitor
cosmic-osk cosmic-player cosmic-initial-setup"

features() {
	case "$1" in
	cosmic-comp) echo "--no-default-features" ;;
	cosmic-session) echo "--no-default-features --features autostart" ;;
	cosmic-notifications | xdg-desktop-portal-cosmic) echo "--no-default-features" ;;
	# cosmic-files' `io-uring` default is harmless here: compio only uses it on Linux.
	*) echo "" ;;
	esac
}

comment() {
	case "$1" in
	cosmic-comp) echo "Compositor for the COSMIC desktop" ;;
	cosmic-session) echo "Session manager for the COSMIC desktop" ;;
	cosmic-settings-daemon) echo "Settings daemon for the COSMIC desktop" ;;
	cosmic-panel) echo "Panel for the COSMIC desktop" ;;
	cosmic-applets) echo "Panel applets for the COSMIC desktop" ;;
	pop-launcher) echo "Modular application launcher used by COSMIC" ;;
	cosmic-launcher) echo "Application launcher for the COSMIC desktop" ;;
	cosmic-app-library) echo "Application library for the COSMIC desktop" ;;
	cosmic-workspaces-epoch) echo "Workspaces overview for the COSMIC desktop" ;;
	cosmic-bg) echo "Wallpaper service for the COSMIC desktop" ;;
	cosmic-osd) echo "On-screen display for the COSMIC desktop" ;;
	cosmic-notifications) echo "Notification daemon for the COSMIC desktop" ;;
	cosmic-randr) echo "Display configuration tool for the COSMIC desktop" ;;
	cosmic-idle) echo "Idle management daemon for the COSMIC desktop" ;;
	xdg-desktop-portal-cosmic) echo "XDG desktop portal backend for the COSMIC desktop" ;;
	cosmic-settings) echo "Settings application for the COSMIC desktop" ;;
	cosmic-icons) echo "Icon theme for the COSMIC desktop" ;;
	cosmic-term) echo "Terminal emulator for the COSMIC desktop" ;;
	cosmic-files) echo "File manager for the COSMIC desktop" ;;
	cosmic-edit) echo "Text editor for the COSMIC desktop" ;;
	cosmic-screenshot) echo "Screenshot tool for the COSMIC desktop" ;;
	cosmic-viewer) echo "Image viewer for the COSMIC desktop" ;;
	cosmic-monitor) echo "System monitor for the COSMIC desktop" ;;
	cosmic-osk) echo "On-screen keyboard for the COSMIC desktop" ;;
	cosmic-player) echo "Media player for the COSMIC desktop" ;;
	cosmic-initial-setup) echo "First-run setup wizard for the COSMIC desktop" ;;
	*) echo "$1 (COSMIC desktop)" ;;
	esac
}

# Run-time package dependencies that are not shared libraries (those pkg
# detects itself): DRI drivers and Xwayland are dlopen'ed/exec'ed, the
# keyboard layouts and icon themes are data.
rundeps() {
	case "$1" in
	cosmic-comp) echo "mesa-dri vulkan-loader xwayland xkeyboard-config seatd" ;;
	# The session starts every shell component it finds and logs the
	# missing ones; only the compositor is a hard requirement, the rest is
	# installed by name (dom0::GUI_PACKAGES) so one failed build does not
	# take the desktop down with it.
	cosmic-session) echo "cosmic-comp dbus" ;;
	cosmic-launcher) echo "mesa-dri pop-launcher hicolor-icon-theme" ;;
	cosmic-panel | cosmic-applets | cosmic-app-library | cosmic-workspaces-epoch | cosmic-osd | cosmic-notifications | cosmic-settings | cosmic-term | cosmic-files | cosmic-edit | cosmic-viewer | cosmic-monitor | cosmic-osk | cosmic-player | cosmic-initial-setup | cosmic-screenshot | cosmic-bg)
		echo "mesa-dri hicolor-icon-theme" ;;
	xdg-desktop-portal-cosmic) echo "xdg-desktop-portal" ;;
	*) echo "" ;;
	esac
}

www() {
	case "$1" in
	pop-launcher) echo "https://github.com/pop-os/launcher" ;;
	cosmic-app-library) echo "https://github.com/pop-os/cosmic-applibrary" ;;
	*) echo "https://github.com/pop-os/$1" ;;
	esac
}

log() { printf '==> %s\n' "$*"; }

setup_env() {
	mkdir -p "$work/gnubin" "$work/target" "$work/stage" "$work/meta" "$work/repo" "$work/logs"
	# GNU install(1), with `install ... /dev/stdin ...` spooled to a file
	# first: coreutils 9 refuses a pipe there ("replaced while being
	# copied") and xdg-desktop-portal-cosmic's recipe installs from sed.
	cat > "$work/gnubin/install" <<'EOF'
#!/bin/sh
spool=""
for a in "$@"; do
	if [ "$a" = /dev/stdin ]; then
		spool=$(mktemp -t install) && cat > "$spool"
		break
	fi
done
if [ -n "$spool" ]; then
	n=$#
	i=0
	while [ "$i" -lt "$n" ]; do
		a=$1
		shift
		[ "$a" = /dev/stdin ] && a=$spool
		set -- "$@" "$a"
		i=$((i + 1))
	done
	/usr/local/bin/ginstall "$@"
	rc=$?
	rm -f "$spool"
	exit "$rc"
fi
exec /usr/local/bin/ginstall "$@"
EOF
	chmod +x "$work/gnubin/install"
	[ -x "$work/gnubin/sed" ] || ln -sf /usr/local/bin/gsed "$work/gnubin/sed"
	[ -x "$work/gnubin/xargs" ] || ln -sf /usr/local/bin/gxargs "$work/gnubin/xargs"
	[ -x "$work/gnubin/ln" ] || ln -sf /usr/local/bin/gln "$work/gnubin/ln"
	PATH="$work/gnubin:$PATH"
	export PATH
	CARGO_TARGET_DIR="$work/target"
	export CARGO_TARGET_DIR
	CARGO_BUILD_JOBS=$jobs
	export CARGO_BUILD_JOBS
	# Crates that link a system library without pkg-config (`#[link]` in
	# xkbcommon) need the ports library directory; the linker driver reads
	# LIBRARY_PATH and cargo does not fingerprint it, unlike RUSTFLAGS.
	LIBRARY_PATH=/usr/local/lib
	export LIBRARY_PATH
	# turbojpeg-sys (cosmic-viewer) would otherwise compile its own libjpeg-turbo.
	TURBOJPEG_SOURCE=pkg-config
	export TURBOJPEG_SOURCE
	# vergen-style build scripts want a git hash; keep them happy outside git.
	export VERGEN_GIT_SHA=${VERGEN_GIT_SHA:-$tag}
}

cmd_deps() {
	log "Installing build and run-time dependencies"
	pkg install -y rust pkgconf just cmake ninja git-lite coreutils gsed findutils \
		seatd libudev-devd libdisplay-info libinput libxkbcommon xkeyboard-config \
		mesa-libs mesa-dri vulkan-loader xwayland pixman wayland wayland-protocols \
		fontconfig freetype2 expat dbus gettext-runtime desktop-file-utils \
		hicolor-icon-theme xdg-desktop-portal xdg-utils libxcb xcb-util xcb-util-wm \
		pulseaudio pipewire gstreamer1 gstreamer1-plugins gstreamer1-plugins-good \
		glib gtk3 llvm bash gmake python3 libglvnd libjpeg-turbo
}

cmd_fetch() {
	if [ -d "$src/.git" ]; then
		log "$src exists; not cloning again"
	else
		log "Cloning cosmic-epoch $tag"
		git clone --depth 1 -b "$tag" https://github.com/pop-os/cosmic-epoch "$src"
	fi
	cd "$src"
	# cosmic-wallpapers needs git-lfs (and 300 MB); the sound theme is not
	# used without a session bus audio service. Everything else is built.
	mods=$(git config --file .gitmodules --get-regexp path | awk '{print $2}' |
		grep -v -e cosmic-wallpapers -e cosmic-sound-theme -e simple-wrapper)
	# shellcheck disable=SC2086
	git submodule update --init --depth 1 --jobs 4 $mods
}

cmd_patch() {
	log "Applying FreeBSD fixes"
	cd "$src/cosmic-comp"
	# /sys is a symlink to /usr/src/sys on FreeBSD; what cosmic-comp looks
	# for there is Linux sysfs (same fix as the x11-wm/cosmic-comp port).
	for f in src/backend/kms/surface/timings.rs src/debug.rs src/utils/env.rs; do
		[ -f "$f" ] && sed -i -e 's,"/sys,"/compat/linux/sys,g' "$f"
	done
	# Registry crates are patched in place: cargo only verifies the
	# downloaded .crate archive, not the extracted sources.
	for name in $components; do
		[ -f "$src/$name/Cargo.toml" ] || continue
		(cd "$src/$name" && cargo fetch -q 2>/dev/null) || true
	done
	# fontdb reads fontconfig's configuration from /etc
	# (https://github.com/RazrFalcon/fontdb/issues/61); the font
	# directories themselves already include /usr/local/share/fonts.
	for d in "$HOME"/.cargo/registry/src/*/fontdb-*; do
		[ -f "$d/src/lib.rs" ] || continue
		if grep -q '"/etc/fonts/fonts.conf"' "$d/src/lib.rs"; then
			sed -i -e 's,"/etc/fonts/fonts.conf","/usr/local/etc/fonts/fonts.conf",' "$d/src/lib.rs"
			log "patched $(basename "$d"): fonts.conf under /usr/local/etc"
		fi
		if ! grep -q '/usr/local/share/fonts' "$d/src/lib.rs"; then
			sed -i -e 's,"/usr/share/fonts/".as_ref(), &mut seen);,&\n        self.load_fonts_dir_impl("/usr/local/share/fonts/".as_ref(), \&mut seen);,' "$d/src/lib.rs"
			log "patched $(basename "$d"): /usr/local/share/fonts"
		fi
	done
	# The libdisplay-info bindings cap the library at < 0.4 while FreeBSD
	# ships 0.4.0; the 0.3 API is a subset of it.
	for d in "$HOME"/.cargo/registry/src/*/libdisplay-info-sys-0.3.*; do
		[ -f "$d/Cargo.toml" ] || continue
		if grep -q '< 0.4.0' "$d/Cargo.toml"; then
			sed -i -e 's/< 0\.4\.0/< 0.5.0/' "$d/Cargo.toml"
			log "patched $(basename "$d"): accept libdisplay-info 0.4"
		fi
	done
	# Source patches (tools/dev/cosmic-patches, kept next to this script):
	# Linux-only APIs with a portable alternative.
	patches=$here/cosmic-patches
	apply_patch "$src/cosmic-panel" "$patches/cosmic-panel-freebsd-security-context.patch"
	apply_patch "$src/cosmic-applets" "$patches/cosmic-applets-freebsd-no-bluetooth.patch"
	apply_patch "$src/pop-launcher" "$patches/pop-launcher-freebsd-no-pidfd.patch"
	for d in "$HOME"/.cargo/registry/src/*/pipewire-0.10.*; do
		[ -f "$d/src/lib.rs" ] && apply_patch "$d" "$patches/pipewire-0.10-freebsd-init.patch"
	done
	for d in "$HOME"/.cargo/registry/src/*/pipewire-sys-0.10.*; do
		[ -f "$d/build.rs" ] && apply_patch "$d" "$patches/pipewire-sys-0.10-freebsd-allowlist.patch"
	done
	# xdg-desktop-portal-cosmic takes pipewire-rs from git instead.
	for d in "$HOME"/.cargo/git/checkouts/pipewire-rs-*/*; do
		[ -f "$d/pipewire/src/lib.rs" ] || continue
		apply_patch "$d/pipewire" "$patches/pipewire-0.10-freebsd-init.patch"
		apply_patch "$d/pipewire-sys" "$patches/pipewire-sys-0.10-freebsd-allowlist.patch"
	done
	for d in "$HOME"/.cargo/git/checkouts/libcosmic-*/*; do
		[ -f "$d/iced/winit/src/conversion.rs" ] || continue
		if perl "$patches/libcosmic-freebsd-winit.pl" "$d" | grep -v '^nothing to do'; then
			forget_built_from "$d"
		fi
	done
	# pop-launcher hard-codes the Linux prefix for its plugins and configuration.
	for f in "$src/pop-launcher/src/lib.rs" "$HOME"/.cargo/git/checkouts/launcher-*/*/src/lib.rs; do
		[ -f "$f" ] || continue
		if grep -q '"/usr/lib/pop-launcher"' "$f"; then
			sed -i -e 's,"/usr/lib/pop-launcher","/usr/local/lib/pop-launcher",' \
				-e 's,"/etc/pop-launcher","/usr/local/etc/pop-launcher",' "$f"
			log "patched $f: pop-launcher paths under /usr/local"
		fi
	done
	# libc 0.2.190 dropped FreeBSD's NOTE_PCTRLMASK (still in sys/event.h)
	# and nix 0.31 lists it in its kevent flags; cosmic-osk pulls both.
	for d in "$HOME"/.cargo/registry/src/*/nix-0.3*; do
		[ -f "$d/src/sys/event.rs" ] || continue
		if grep -q '^        NOTE_PCTRLMASK;$' "$d/src/sys/event.rs"; then
			perl -0pi -e 's/        #\[allow\(missing_docs\)\]\n        NOTE_PCTRLMASK;\n//' "$d/src/sys/event.rs"
			log "patched $(basename "$d"): no NOTE_PCTRLMASK in libc 0.2.190"
		fi
	done
	# ddc-hi (display brightness, cosmic-settings-daemon) enables its i2c
	# backend on every unix but only depends on ddc-i2c on Linux.
	for d in "$HOME"/.cargo/registry/src/*/ddc-hi-0.*; do
		[ -f "$d/build.rs" ] || continue
		if grep -q 'var("CARGO_CFG_UNIX").is_ok()' "$d/build.rs"; then
			sed -i -e 's/var("CARGO_CFG_UNIX").is_ok()/var("CARGO_CFG_TARGET_OS") == Ok("linux".into())/' "$d/build.rs"
			log "patched $(basename "$d"): i2c backend is Linux-only"
		fi
	done
	# cargo trusts registry and git sources not to change: forget what it
	# built from the patched crates so they are compiled again.
	for crate in fontdb libdisplay-info-sys pipewire pipewire-sys iced_core iced_winit pop-launcher pop-launcher-plugins cosmic-applets nix ddc-hi; do
		rm -rf "$work/target/release/.fingerprint/$crate-"*
	done
}

# forget_built_from DIR: drop the fingerprints of every crate cargo compiled
# from sources under DIR (found through the dep-info files), so the next
# build compiles them again from the patched sources.
forget_built_from() {
	for dep in $(grep -ls "$1/" "$work/target/release/deps"/*.d 2>/dev/null); do
		hash=$(basename "$dep" .d | sed 's/.*-//')
		rm -rf "$work/target/release/.fingerprint"/*-"$hash"
	done
}

# apply_patch DIR FILE: `patch -p1`, once.
apply_patch() {
	[ -f "$2" ] || { echo "   missing patch $2" >&2; return 1; }
	if patch -d "$1" -p1 -N -s --dry-run < "$2" > /dev/null 2>&1; then
		patch -d "$1" -p1 -N -s < "$2"
		log "applied $(basename "$2") in $1"
	elif patch -d "$1" -p1 -R -s --dry-run < "$2" > /dev/null 2>&1; then
		log "$(basename "$2") already applied in $1"
	else
		echo "   $(basename "$2") does not apply in $1" >&2
		return 1
	fi
}

build_one() {
	name=$1
	dir="$src/$name"
	[ -d "$dir" ] || { echo "no such component: $name"; return 1; }
	log "Building $name"
	cd "$dir"
	[ -f Cargo.toml ] || { echo "   nothing to compile (data only)"; return 0; }
	# shellcheck disable=SC2046
	if { [ -f justfile ] || [ -f Justfile ]; } && grep -qs '^build-release' justfile Justfile; then
		just $(just_vars "$name" "$work/stage/$name") build-release $(features "$name")
	elif [ -f Makefile ]; then
		gmake prefix="$prefix" TARGET=release all
	else
		# shellcheck disable=SC2046
		cargo build --release $(features "$name")
	fi
}

cmd_build() {
	setup_env
	names=${*:-$components}
	failed=""
	for name in $names; do
		if build_one "$name" > "$work/logs/$name.build.log" 2>&1; then
			touch "$work/logs/$name.built"
		else
			failed="$failed $name"
			echo "   FAILED: $name (see $work/logs/$name.build.log)"
			tail -n 5 "$work/logs/$name.build.log" | sed 's/^/   | /'
		fi
	done
	[ -z "$failed" ] || { echo "failed:$failed"; return 1; }
}

# The justfile variables for the install location. pop-launcher knows no
# prefix (it installs under rootdir/usr); it is relocated after staging.
just_vars() {
	case "$1" in
	pop-launcher) echo "rootdir=$2" ;;
	*) echo "rootdir=$2 prefix=$prefix" ;;
	esac
}

depver() { pkg query '%v' "$1" 2>/dev/null || true; }
deporigin() { pkg query '%o' "$1" 2>/dev/null || true; }

manifest() {
	name=$1
	deps=""
	# shellcheck disable=SC2086
	ours=$(echo $components)
	for d in $(rundeps "$name"); do
		case " $ours " in
		*" $d "*) v=$version; o="juanitos/$d" ;;
		*) v=$(depver "$d"); o=$(deporigin "$d") ;;
		esac
		[ -n "$v" ] || { echo "   warning: $name: dependency $d is not installed here; dropped" >&2; continue; }
		deps="$deps $d: { origin: \"$o\", version: \"$v\" },"
	done
	cat <<EOF
name: "$name"
version: "$version"
origin: "juanitos/$name"
comment: "$(comment "$name")"
desc: "$(comment "$name"), built from pop-os/cosmic-epoch $tag by JuanitOS (tools/dev/cosmic-build.sh)."
maintainer: "juanitos@localhost"
www: "$(www "$name")"
prefix: "$prefix"
licenselogic: "single"
licenses: ["GPLv3"]
categories: ["x11", "juanitos"]
deps: {${deps} }
EOF
}

package_one() {
	name=$1
	dir="$src/$name"
	stage="$work/stage/$name"
	[ -f "$work/logs/$name.built" ] || { echo "   $name: not built, skipped"; return 0; }
	log "Packaging $name"
	rm -rf "$stage" "$work/repo/$name-$version.pkg"
	mkdir -p "$stage"
	cd "$dir"
	# pop-launcher's recipes read target/release in the source tree.
	[ -e target ] || ln -s "$work/target" target
	# shellcheck disable=SC2046
	if [ -f justfile ] || [ -f Justfile ]; then
		just $(just_vars "$name" "$stage") install
	else
		gmake prefix="$prefix" TARGET=release DESTDIR="$stage" install
	fi
	if [ "$name" = pop-launcher ] && [ -d "$stage/usr/bin" ]; then
		mkdir -p "$stage$prefix"
		mv "$stage/usr/bin" "$stage$prefix/bin"
		[ -d "$stage/usr/lib" ] && mv "$stage/usr/lib" "$stage$prefix/lib"
	fi
	# Linux-only bits the install recipes drop in anyway.
	rm -rf "$stage$prefix/lib/systemd" "$stage/usr/lib/systemd" "$stage$prefix/share/dconf"
	# Session scripts and desktop entries spell out Linux's /usr/bin.
	find "$stage" -type f \( -name '*.desktop' -o -name '*.service' -o -name 'start-cosmic' -o -name '*.sh' \) \
		-exec sed -i -e 's,/usr/bin/,/usr/local/bin/,g' -e 's,/usr/libexec/,/usr/local/libexec/,g' {} +
	find "$stage" -type d -empty -delete
	# pkg wants the file list relative to the prefix.
	(cd "$stage$prefix" && find . \( -type f -o -type l \) | sed 's,^\./,,' | LC_ALL=C sort) > "$work/meta/$name.plist"
	[ -s "$work/meta/$name.plist" ] || { echo "   $name: nothing staged under $prefix"; return 1; }
	mkdir -p "$work/meta/$name"
	manifest "$name" > "$work/meta/$name/+MANIFEST"
	pkg create -r "$stage" -m "$work/meta/$name" -p "$work/meta/$name.plist" -o "$work/repo"
	ls -l "$work/repo/$name-$version.pkg" | awk '{print "   " $5 " bytes"}'
}

cmd_package() {
	setup_env
	names=${*:-$components}
	for name in $names; do
		package_one "$name" 2>&1 | tee "$work/logs/$name.package.log"
	done
}

cmd_repo() {
	log "Writing the repository catalogue"
	rm -f "$work/repo/meta.txz" "$work/repo/meta.conf" "$work/repo/packagesite.pkg" "$work/repo/packagesite.txz" "$work/repo/data.pkg" "$work/repo/data.txz"
	pkg repo "$work/repo"
	ls "$work/repo"
	printf '%s\n' "$tag" > "$work/repo/COSMIC_TAG"
}

cmd_list() {
	for name in $components; do
		status="-"
		[ -f "$work/logs/$name.built" ] && status="built"
		[ -f "$work/repo/$name-$version.pkg" ] && status="packaged"
		printf '%-28s %s\n' "$name" "$status"
	done
}

case "${1:-}" in
deps) cmd_deps ;;
fetch) cmd_fetch ;;
patch) setup_env; cmd_patch ;;
build) shift; cmd_build "$@" ;;
package) shift; cmd_package "$@" ;;
repo) cmd_repo ;;
list) cmd_list ;;
all) cmd_deps; cmd_fetch; setup_env; cmd_patch; cmd_build; cmd_package; cmd_repo ;;
*) sed -n 2,34p "$0"; exit 2 ;;
esac
