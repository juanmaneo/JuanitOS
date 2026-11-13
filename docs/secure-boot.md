<!-- SPDX-License-Identifier: BSD-3-Clause -->
# Secure Boot with your own keys

JuanitOS does not ship a key signed by Microsoft, so firmware with
its factory keys refuses its boot files. Instead, the owner makes a
key hierarchy of their own and signs the installed system's boot files
with it. Then they enroll the keys and turn Secure Boot on. Everything is
done by `jos secureboot` on the installed dom0, with the `openssl` of
the base system.

The live ISO is not signed: boot it, and install, with Secure Boot off.

## The keys

| Key | Signs | Firmware variable |
|-----|-------|-------------------|
| PK, Platform Key | changes to PK and KEK | `PK` |
| KEK, Key Exchange Key | changes to db | `KEK` |
| db, signature database key | the boot files | `db` |

They live in `/usr/local/etc/juanitos/secureboot/`, which is mode 700,
next to the qube definitions. For each key there are five files: the
private key `.key`, the certificate as `.crt` (PEM) and `.cer` (DER), the
EFI signature list `.esl`, and the signed variable write `.auth`.
**Back the directory up**, somewhere other than this machine. Without
`PK.key` the keys can only be changed by clearing them in the firmware
setup. Without `db.key` nothing new can be signed.

## Commands

```sh
jos secureboot status            # firmware state, keys, signatures on the ESP
jos secureboot keygen            # make PK, KEK and db (--no-vendor, --force)
jos secureboot sign-esp          # sign the JuanitOS boot files on /boot/efi
jos secureboot sign FILE...      # sign any EFI image in place
jos secureboot enroll            # write db, KEK, PK to a firmware in setup mode
```

- `status` reads the firmware's `SecureBoot` and `SetupMode` flags. It
  says whether the keys exist, and who signed each JuanitOS file on the
  ESP: the JuanitOS key, someone else, or nobody. `--esp PATH` points it
  at another mount.
- `keygen` saves the firmware's current KEK and db as
  `vendor-KEK.esl` and `vendor-db.esl` and keeps their entries enrolled
  next to yours. The option ROMs of many graphics and network cards are
  signed by the vendor's keys and do not run without them, and then the
  screen stays black. `--no-vendor` trusts your keys only. `--force`
  replaces existing keys, and whatever they enrolled stops working.
- `sign-esp` signs `EFI/BOOT/BOOTX64.EFI`, `EFI/JUANITOS/BOOTX64.EFI` (on
  an ESP shared with other systems) and `EFI/FREEBSD/LOADER.EFI`. It
  signs only those that are the installed `/boot/jos-boot.efi` or
  `/boot/loader.efi`, signed or not. Another system's file at one of
  those paths is reported and left alone. It also copies the public halves
  (`.auth`, `.cer`, `.esl`) to `EFI/JUANITOS/keys/` on the ESP.
- `sign` replaces any existing signature. The signed image is written
  beside the file first, so a full disk leaves the old one in place.
- `enroll` refuses unless the firmware is in setup mode. It writes db,
  then KEK, then PK, and the PK write ends setup mode.

## Turning it on

1. `jos secureboot keygen` while Secure Boot is still off, so that the
   firmware's own lists can be read and saved.
2. `jos secureboot sign-esp`.
3. In the firmware setup, clear the Platform Key (often "Reset to Setup
   Mode" or "Delete all Secure Boot keys"). The firmware is then in
   setup mode, and `jos secureboot status` says so.
4. `jos secureboot enroll`. If it fails, use the firmware's own key
   management instead: import `db.auth`, `KEK.auth` and `PK.auth` (or the
   `.cer`/`.esl` files, whichever it takes) from `EFI/JUANITOS/keys` on
   the ESP, in that order.
5. Turn Secure Boot on, and reboot. `jos secureboot status` should show
   `Secure Boot: enabled`.

To go back, turn Secure Boot off, or restore the factory keys in the
firmware setup.

## After an update

`sign-esp` must run again whenever new boot files are copied to the
ESP, after a `freebsd-update` or `pkg` update of the loader, or a new
`jos-boot.efi`. An unsigned loader is refused: `jos-boot` prints
`loading \EFI\FREEBSD\LOADER.EFI: ... ACCESS_DENIED` (or
`SECURITY_VIOLATION`) and the machine does not boot. If that happens,
turn Secure Boot off, boot, run `sign-esp`, and turn it back on.

## Limits

- **The chain stops at the loader.** The firmware checks `jos-boot.efi`
  and `jos-boot` asks the firmware to load `loader.efi`, so it is checked
  too. FreeBSD's loader does not verify the kernel or its modules.
  Secure Boot here keeps a foreign bootloader off the machine. It does
  not prove the kernel is the one you installed.
- **dbx is left alone**, so the firmware's revocation list stays as it
  is.
- **Other systems on a shared ESP** are never signed. With `--no-vendor`
  their vendor-signed loaders (shim, Windows Boot Manager) stop booting.
  With the vendor keys kept, they keep working.
- **Hardware:** `enroll` and the `efivar` calls it makes (`-p -b -N -n`
  to read, `-w -t 0x27 -n ... -f` to write a time-based authenticated
  variable) have not run on real firmware yet. The firmware's own
  import menu is the fallback.
- The 32-bit loader for old Intel Macs (`EFI/BOOT/BOOTIA32.EFI`) is not
  signed. That firmware has no Secure Boot.

## Testing under QEMU

```sh
cargo run -p jos-release -- secureboot-test
```

The test boots a Secure Boot build of OVMF (found under
`/usr/share/edk2` or `/usr/share/OVMF`, or given with `--firmware` and
`--vars`) three times from a small ESP made with mtools. It uses the
`jos-boot.efi` that `bins` built, or `--boot-efi`, and keeps its
output in `release/out/secureboot` (`--out`).

| Case | ESP | Must show |
|------|-----|-----------|
| `signed` | signed pre-loader, signed loader | the chain-load, twice |
| `unsigned-loader` | signed pre-loader, unsigned loader | `jos-boot` refused it, `ACCESS_DENIED` or `SECURITY_VIOLATION` |
| `unsigned-boot` | unsigned pre-loader | the firmware's `Access Denied` or `Security Violation` |

The machine has no screen, so `jos-boot` reports on the serial port. A
signed `jos-boot.efi` stands in for `loader.efi`, which is why the chain
shows up twice. The keys are made and the images signed by the same
code as `jos secureboot`. Only the enrollment differs: the signature
lists are written straight into a copy of OVMF's variable store, which
is what the firmware's own menu would leave there. A run on QEMU/KVM
passes all three cases, which proves the signatures, the key hierarchy
and the refusals. It does not exercise `enroll`.
