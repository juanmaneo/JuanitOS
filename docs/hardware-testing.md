# Testing JuanitOS on a real machine

A checklist for the first boot of the live ISO on new hardware, so that
one try brings back everything needed to fix what failed. The output is
one text file written by `jos doctor --report` plus a few photos.

## 1. Firmware settings

In the firmware (BIOS/UEFI) setup:

- **Secure Boot: off.** `jos-boot.efi` and FreeBSD's loader are not
  signed.
- **Intel Virtualization Technology (VT-x) and VT-d: on** (AMD: SVM and
  IOMMU/AMD-Vi). Without VT-x the doctor fails `cpu virtualization` and
  no qube can start; without VT-d it warns on `iommu hardware` (no ACPI
  DMAR table) and only passthrough (`sys-net`'s NIC, `gpu`) fails.
- **Graphics on a hybrid laptop:** keep the hybrid mode in which the
  internal panel is driven by the integrated GPU (Intel Arc on the Lenovo
  16P). The dom0 desktop runs on that GPU; the discrete NVIDIA card is
  the one handed to the `gpu` qube.

Note the firmware version (shown on the setup's main page).

## 2. Write the stick with `dd`

```sh
# on Linux, as root; /dev/sdX is the whole stick, not a partition
dd if=JuanitOS-0.1.0-amd64.iso of=/dev/sdX bs=4M conv=fsync status=progress
```

Use `dd` rather than Ventoy for a first test: Ventoy adds its own boot
chain and disk emulation, so a failure could come from either side
(`docs/installer.md`, *Ventoy* and *Intel Macs*). Check the image first
with `sha256sum -c JuanitOS-0.1.0-amd64.iso.sha256`.

Have a second USB stick formatted FAT32 at hand to bring the report back.

## 3. Boot and watch the icons

Pick the stick in the firmware's boot menu (F12 on Lenovo, Option on a
Mac). The boot card shows six icons that light up in order
(`docs/boot-splash.md`). The last one lit tells how far the boot got:

| Lit up to | What runs next | If it stops there |
|-----------|----------------|-------------------|
| 1 boot    | `jos-boot.efi` hands over to FreeBSD's loader, which reads the kernel (the screen goes black for 1–3 s, this is expected) | The loader failed silently. Reboot: the card then says *The last boot did not finish* and boots verbose. Photograph the last screen. |
| 2 kernel  | The kernel probes the hardware and mounts the root file system | A failed root mount (e.g. *error 19*) switches to text by itself after a few seconds (`jos_bootwatch`) and shows the messages: photograph them. A screen that stays frozen means a driver hangs: reboot and choose *Boot verbose* (below), then photograph the last lines. |
| 3 storage | `rc(8)` starts the services and brings the network up (DHCP can take a while without a cable) | Wait a minute, then press **Esc**: the splash ends on the text console and shows what `rc` is doing. |
| 4 network | The splash ends and the installer starts | Press **Esc**, as above. |
| all       | The graphical installer (or the text installer on a machine without a framebuffer) | — |

**Boot verbose:** press any key other than Enter while the first card is
shown (3 s), then pick *Boot verbose* in the menu. Every kernel and `rc`
message is then printed, and the photo of the last screen usually names
the culprit. *Single user* gives a root shell before `rc` runs.

## 4. Write the report from the live system

The doctor needs a shell, not the installer:

1. In the installer, on the *Welcome* page, choose **Quit** (or, after a
   failed install, **Exit to console**). A login prompt follows: log in
   as `root`; the live medium has no root password.
2. Write the report to `/tmp` (the live root is read-only; `/tmp` is in
   memory):

   ```sh
   jos doctor --report /tmp/report.txt
   ```

   A doctor that ends with `some checks failed` still writes the report;
   that is the case the report is for.
3. Copy it to the FAT stick. Plug it in, find it in the kernel messages
   (`dmesg | tail`, e.g. `da1`) and its partition with `gpart show da1`
   (`da1s1` for an MBR stick, `da1p1` for GPT):

   ```sh
   mount -t msdosfs /dev/da1s1 /mnt
   cp /tmp/report.txt /mnt/
   umount /mnt
   ```

`Alt-F1` … `Alt-F8` switch between text consoles if the installer is in
the way.

The report holds the doctor checks, the problem and platform lines of
the boot messages, the CPU and memory, the `vmm` state, the PCI devices
(`pciconf -lv`), the loaded modules, the loader variables (`kenv`), the
disks and partitions, the mounts, the network interfaces, `loader.conf`,
`release.toml` and the full boot messages. A tool that fails is noted in
place. Serial numbers, UUIDs and asset tags are removed; MAC addresses
and the host name are kept, so look it over before posting it publicly.

## 5. After an install

When the installed system boots, write a second report from it (`jos
doctor --report /root/report.txt` as root) before running `jos setup`: it
shows the `drm-kmod` driver and the `ppt` devices as the dom0 sees them.
Note whether the login screen and a COSMIC session appear.

## 6. What to send back

- `report.txt` (and the one from the installed system, if any);
- photos of the screen where the boot stopped, and which icon was the
  last lit;
- how the stick was written (`dd`, or Ventoy and its version) and the
  firmware boot mode;
- the machine model and firmware version;
- anything typed at a prompt and what it answered.
