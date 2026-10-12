# GPU passthrough: NVIDIA + CUDA in a Linux qube

NVIDIA's FreeBSD driver has no CUDA support, so compute workloads live in a
Linux `gpu` qube that receives the physical GPU through bhyve's `passthru`
device. The host keeps a separate (integrated or secondary) GPU for COSMIC.

## Requirements

- CPU and chipset with VT-d / AMD-Vi, enabled in firmware.
- FreeBSD 13.2+ (14.x recommended) with `vmm`, `hw.vmm.iommu.enable=1`.
- The GPU and its companion functions (HDMI audio, USB-C controller on newer
  cards) in an IOMMU group that is not shared with devices the host needs.
- `edk2-bhyve` package: NVIDIA guests must boot UEFI; bhyve's legacy
  loader cannot initialise the GPU's option ROM.
- A second display output or a VNC/remote path, since bhyve does not emulate
  a display for the passed-through card.

## Host setup

1. Locate the card and its functions:

   ```sh
   jos pci list --gpus
   pciconf -l -v | grep -A4 nvidia   # or `none` if no host driver bound
   ```

2. Reserve them for passthrough at boot. Pass *every* function of the card:

   ```sh
   jos pci pptdevs 1/0/0 1/0/1   # -> pptdevs="1/0/0 1/0/1"
   ```

   Add the printed line plus `hw.vmm.iommu.enable=1` to `/boot/loader.conf`
   (see `freebsd/loader.conf.sample`) and reboot. Afterwards `pciconf -l`
   shows `ppt0`, `ppt1` and `jos doctor` reports them as ready.

3. Make sure the host's `nvidia` driver is **not** loaded for that card.
   Either skip `nvidia_load` entirely or rely on `pptdevs` claiming the device
   first (it is attached before other drivers probe).

## Creating the qube

```sh
jos create gpu --kind gpu --os linux --template debian-12 \
    --cpus 8 --memory-mb 16384 --pci 1/0/0 --pci 1/0/1
jos start gpu
```

`jos` adds `-S` (wire guest memory, required for DMA) and one
`-s N,passthru,B/S/F` per device starting at guest slot 4. The guest sees the
GPU as a normal PCI device, so the stock `nvidia-driver` / `cuda-toolkit`
packages from the distribution or NVIDIA's repo work unchanged.

Inside the guest:

```sh
lspci | grep -i nvidia
nvidia-smi
```

## Known limitations and workarounds

- **Code 43 / driver refuses to load**: some consumer cards detect the
  hypervisor. Use a recent driver (470+ allows virtualisation on GeForce);
  if needed add `-o bootrom...` tweaks or hide the hypervisor via
  `extra_bhyve_args`.
- **Large BARs**: cards with 64-bit BARs > 4 GiB (Resizable BAR) need bhyve
  from FreeBSD 14 or newer. Disable Resizable BAR in firmware otherwise.
- **Reset problems**: NVIDIA GPUs usually support FLR; if a guest reboot
  leaves the card wedged, power-cycle the host. `jos kill` destroys the VM
  which triggers a reset attempt.
- **Memory**: with `-S` the full guest RAM is wired at start; make sure the
  host has `vm.max_wired` headroom (see loader.conf sample).
- **No display**: bhyve has no framebuffer for passthrough cards. Attach a
  monitor to the card for a console, or use the serial console plus SSH.

## Roadmap

- Automatic IOMMU group validation in `jos doctor`.
- `jos gpu attach/detach` for hot-moving the card between a Linux CUDA qube
  and a FreeBSD gaming qube.
- Shared-memory / virtio-gpu path for display forwarding into COSMIC.
