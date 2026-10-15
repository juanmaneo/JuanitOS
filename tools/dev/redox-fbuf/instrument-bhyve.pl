#!/usr/bin/env perl
# Add BAR bookkeeping prints to bhyve's sources (usr.sbin/bhyve of a
# FreeBSD 15.1 tree), for tools/dev/redox-fbuf/repro.sh trace. With
# BHYVE_BAR_DEBUG set in the environment the instrumented bhyve prints
# every guest write to a BAR or to the command register and every
# register/unregister of a BAR's MMIO range; a failed registration is
# always printed, right before the assertion that aborts bhyve.
#
#   perl instrument-bhyve.pl [/usr/src/usr.sbin/bhyve]
#
# Idempotent: refuses to patch a tree that already carries the prints.
use strict;
use warnings;

sub patch {
    my ($file, $from, $to) = @_;
    local $/;
    open my $fh, '<', $file or die "$file: $!";
    my $s = <$fh>;
    close $fh;
    my $n = ($s =~ s/\Q$from\E/$to/);
    die "$file: pattern not found (is this the 15.1 source?)\n" unless $n;
    open $fh, '>', $file or die "$file: $!";
    print $fh $s;
    close $fh;
    print "patched $file\n";
}

my $dir = shift // '/usr/src/usr.sbin/bhyve';

{
    local $/;
    open my $fh, '<', "$dir/pci_emul.c" or die "$dir/pci_emul.c: $!";
    my $s = <$fh>;
    close $fh;
    die "$dir: already instrumented\n" if index($s, 'bar-debug') >= 0;
}

# modify_bar_registration(): what was (un)registered and whether it worked.
patch("$dir/pci_emul.c",
    "\tassert(error == 0);\n\n\tif (pe->pe_baraddr != NULL)",
    "\tif (error != 0 || getenv(\"BHYVE_BAR_DEBUG\") != NULL)\n" .
    "\t\tfprintf(stderr, \"bar-debug: %s idx=%d %s base=0x%lx size=0x%lx cmd=0x%x error=%d\\n\",\n" .
    "\t\t    pi->pi_name, idx, registration ? \"register\" : \"unregister\",\n" .
    "\t\t    (unsigned long)pi->pi_bar[idx].addr, (unsigned long)pi->pi_bar[idx].size,\n" .
    "\t\t    pci_get_cfgdata16(pi, PCIR_COMMAND), error);\n" .
    "\tassert(error == 0);\n\n\tif (pe->pe_baraddr != NULL)");

# pci_cfgrw(): guest writes to the BARs and the command register.
patch("$dir/pci_emul.c",
    "\tpe = pi->pi_d;\n\n\t/*\n\t * Config read\n\t */\n",
    "\tpe = pi->pi_d;\n\n" .
    "\tif (!in && getenv(\"BHYVE_BAR_DEBUG\") != NULL &&\n" .
    "\t    (is_pcir_bar(coff) || (coff >= PCIR_COMMAND && coff < PCIR_REVID)))\n" .
    "\t\tfprintf(stderr, \"cfg-debug: %s write coff=0x%x bytes=%d val=0x%x\\n\",\n" .
    "\t\t    pi->pi_name, coff, bytes, *valp);\n\n" .
    "\t/*\n\t * Config read\n\t */\n");

# register_mem_int(): a range silently dropped because its base lies in
# another range, or refused because it overlaps one.
patch("$dir/mem.c",
    "\t\tif (mmio_rb_lookup(rbt, memp->base, &entry) != 0)\n\t\t\terr = mmio_rb_add(rbt, mrp);\n",
    "\t\tif (mmio_rb_lookup(rbt, memp->base, &entry) != 0) {\n" .
    "\t\t\terr = mmio_rb_add(rbt, mrp);\n" .
    "\t\t\tif (err != 0)\n" .
    "\t\t\t\tfprintf(stderr, \"mem-debug: register %s [0x%lx,0x%lx] overlaps an existing range\\n\",\n" .
    "\t\t\t\t    memp->name, (unsigned long)memp->base, (unsigned long)(memp->base + memp->size - 1));\n" .
    "\t\t} else\n" .
    "\t\t\tfprintf(stderr, \"mem-debug: register %s [0x%lx,0x%lx] skipped: base inside %s [0x%lx,0x%lx]\\n\",\n" .
    "\t\t\t    memp->name, (unsigned long)memp->base, (unsigned long)(memp->base + memp->size - 1),\n" .
    "\t\t\t    entry->mr_param.name, (unsigned long)entry->mr_base, (unsigned long)entry->mr_end);\n");

# unregister_mem(): nothing registered at that base.
patch("$dir/mem.c",
    "\terr = mmio_rb_lookup(&mmio_rb_root, memp->base, &entry);\n\tif (err == 0) {\n\t\tmr = &entry->mr_param;\n",
    "\terr = mmio_rb_lookup(&mmio_rb_root, memp->base, &entry);\n" .
    "\tif (err != 0)\n" .
    "\t\tfprintf(stderr, \"mem-debug: unregister %s [0x%lx,0x%lx]: nothing registered there\\n\",\n" .
    "\t\t    memp->name, (unsigned long)memp->base, (unsigned long)(memp->base + memp->size - 1));\n" .
    "\tif (err == 0) {\n\t\tmr = &entry->mr_param;\n");

# mmio_rb_add(): bhyve's own overlap report, normally compiled out.
patch("$dir/mem.c", "#ifdef RB_DEBUG\n\t\tprintf(", "#if 1\n\t\tfprintf(stderr, ");
