#!/usr/bin/env perl
# libcosmic's iced fork keeps a stale BSD-only block in iced_winit's window
# attribute conversion (old winit API, fields that only exist for Linux)
# and limits the Linux platform settings to target_os = "linux". The BSDs
# are the same Wayland platform: give them the Linux code paths and drop
# the stale block. Applied to every libcosmic git checkout by
# tools/dev/cosmic-build.sh patch; a perl script rather than a diff because
# the components pin several libcosmic revisions with different line
# numbers and context.
#
#   perl libcosmic-freebsd-winit.pl CHECKOUT_DIR
use strict;
use warnings;

my $dir = shift or die "usage: $0 CHECKOUT_DIR\n";
my $bsd = 'any(target_os = "linux", target_os = "freebsd", target_os = "dragonfly", target_os = "netbsd", target_os = "openbsd")';

sub edit {
    my ($file, @subs) = @_;
    local $/;
    open my $fh, '<', $file or die "$file: $!";
    my $s = <$fh>;
    close $fh;
    my $changed = 0;
    while (my ($from, $to) = splice @subs, 0, 2) {
        $changed += ref $to eq 'CODE' ? ($s =~ s/$from/$to->()/ge) : ($s =~ s/$from/$to/g);
    }
    if ($changed) {
        open $fh, '>', $file or die "$file: $!";
        print $fh $s;
        close $fh;
    }
    return $changed;
}

my $conv = "$dir/iced/winit/src/conversion.rs";
my $n = edit($conv,
    # the stale BSD block
    qr/    #\[cfg\(any\(\n        target_os = "dragonfly",\n        target_os = "freebsd",\n        target_os = "netbsd",\n        target_os = "openbsd"\n    \)\)\]\n    \{\n        use ::winit::platform::wayland::WindowAttributesExtWayland;.*?\n    \}\n\n/s => '',
    # the Linux code paths
    qr/#\[cfg\(target_os = "linux"\)\]\nfn take_activation_token/ => "#[cfg($bsd)]\nfn take_activation_token",
    qr/    #\[cfg\(target_os = "linux"\)\]\n    \{\n        #\[cfg\(feature = "x11"\)\]/ => "    #[cfg($bsd)]\n    {\n        #[cfg(feature = \"x11\")]",
    qr/        #\[cfg\(target_os = "linux"\)\]\n        \{\n            use winit::platform::wayland::WindowAttributesWayland;/ => "        #[cfg($bsd)]\n        {\n            use winit::platform::wayland::WindowAttributesWayland;",
);
my $settings = "$dir/iced/core/src/window/settings.rs";
$n += edit($settings,
    qr/#\[cfg\(target_os = "linux"\)\]\n#\[path = "settings\/linux.rs"\]/ => "#[cfg($bsd)]\n#[path = \"settings/linux.rs\"]",
    qr/    target_os = "linux",\n    target_arch = "wasm32"\n\)\)\)\]/ => "    target_os = \"linux\",\n    target_os = \"freebsd\",\n    target_os = \"dragonfly\",\n    target_os = \"netbsd\",\n    target_os = \"openbsd\",\n    target_arch = \"wasm32\"\n)))]",
);

# Older revisions (before libcosmic introduced the `wayland_platform` cfg
# in its build.rs) gate the whole Wayland side, cctk and the layer-shell
# commands included, on target_os = "linux". Treat the BSDs as Linux in
# every cfg of such a checkout; the theme-detection crate stays Linux-only.
my $old = system("grep -rqs --include=*.rs wayland_platform '$dir'") != 0;
if ($old) {
    my @files = split /\n/, `find '$dir' \\( -name '*.rs' -o -name Cargo.toml \\) -not -path '*/target/*' | xargs grep -l 'target_os = "linux"' 2>/dev/null`;
    for my $f (@files) {
        $n += edit($f,
            qr/^(?!.*linux-theme-detection)(.*)target_os = "linux"(?!, target_os = "freebsd")/m => sub { "$1$bsd" },
        );
    }
}
print $n ? "patched $dir ($n edits)\n" : "nothing to do in $dir\n";
