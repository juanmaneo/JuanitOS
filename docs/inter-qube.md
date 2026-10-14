# Inter-qube: the share, the guest agent and `jos copy`

QubesOS has *qrexec*: a dom0-mediated RPC channel between qubes, with
`qvm-copy-to-vm` on top of it. JuanitOS does not have an RPC daemon yet,
but it has the two pieces that make the common case work: a dom0
directory exported into every FreeBSD or Linux qube over **virtio-9p**,
and `jos run`, which types commands on a qube's serial console. Together
they give every clone its own hostname and move files between qubes
without the qubes ever seeing each other's storage.

## The share

Before a qube boots, dom0 creates `/var/db/juanitos/share/<qube>/`
(`share_dir` in `jos.toml`) and attaches it to the VM as
`-s 28,virtio-9p,jos=<dir>` (`crates/juanitos-core/src/share.rs`, slot
28 in the layout documented in `bhyve.rs`):

```text
/var/db/juanitos/share/<qube>/     dom0                 guest: /mnt/jos
├── name                           "<qube>\n", written by dom0 every boot
├── in/                            inbox (1777): in/<source>/<item>
│   ├── dom0/                      files from `jos copy /path <qube>`
│   └── <other-qube>/              files from `jos copy <other-qube>:/path <qube>`
└── out/                           outbox (1777): what `jos copy` asks the guest to export
```

The share is strictly per qube: a qube only ever sees its own directory,
and nothing else on dom0 is reachable through it. Files move between two
shares only when dom0 moves them, on the administrator's request. The
share lives on dom0 storage, not in the zvol, so an inbox survives the
qube being recreated from its template; `jos remove` deletes it.

Redox and unikernel qubes have no share (no 9p client); templates do,
so a maintenance boot (`jos template run`) exercises the agent and can
receive files the same way.

## The guest agent

`templates/agent/` holds the agent shipped in every template's
`user-data` (a unit test checks that the embedded copies are up to date):

- FreeBSD: `/usr/local/etc/rc.d/jos_agent`, enabled with
  `jos_agent_enable=YES`. It runs after `hostname` and before `netif`,
  loads `virtio_p9fs`, mounts the share (`mount -t p9fs jos /mnt/jos`)
  and sets the hostname to the content of `/mnt/jos/name`.
- Debian: `/usr/local/sbin/jos-agent` run by `jos-agent.service` early in
  `sysinit.target`; it mounts with `-t 9p -o trans=virtio,version=9p2000.L`
  and also updates `/etc/hostname` and the `127.0.1.1` line of
  `/etc/hosts`.

Without a share (the template booted on another hypervisor) both agents
print a note and do nothing. Because the template's own name stays in
`rc.conf` / `/etc/hostname`, the agent does its work on every boot.

## `jos copy`

```sh
jos copy work:/home/user/report.pdf jail    # qube → qube
jos copy /root/jos_agent work               # dom0 → qube (dom0:/path also works)
jos copy work:/tmp/proj gpu                 # directories too
```

What happens for `work:/tmp/proj` → `gpu`:

1. dom0 prepares `share/gpu/in/work/` and removes a previous
   `share/gpu/in/work/proj` (an item of the same name is replaced);
2. `jos run work` types a command into `work`'s console that copies
   `/tmp/proj` to `/mnt/jos/out/proj` (`cp -RL`: symbolic links are
   followed, the 9p share cannot hold them);
3. dom0 moves `share/work/out/proj` to `share/gpu/in/work/proj`;
4. `gpu` sees it at `/mnt/jos/in/work/proj`, and `jos copy` prints both
   paths.

For a dom0 source step 2 is a `cp -RL` on dom0, and the copy is made
world-writable (`chmod -R a+rwX`) so the guest's unprivileged user can
delete what root put there; copies between qubes keep their owner, which
is the same `user` uid in every template. Every dom0 step goes through
the `Runner`, so `jos copy --dry-run` prints the plan, and the
destination's inbox directories are mode 777 so the guest user can tidy
them.

Only dom0 initiates a transfer, as with `qvm-copy-to-vm` run from dom0.
A qube cannot push to another qube by itself, and the policy layer of
QubesOS (`qrexec` policies asking the user to confirm) is future work:
today the administrator typing `jos copy` *is* the policy.

## Limits and next steps

- `jos copy` reads the source over the serial console, so it needs the
  source qube to be running with its autologin shell free; a long `cp`
  is bounded by `--timeout` (default 600 s).
- The destination need not be running: files wait in its inbox on dom0.
- Large transfers are a `cp` through 9p; fine for sources and documents,
  not meant for disk images.
- A proper qrexec-like agent (a guest daemon on a virtio-console or
  vsock, dom0 policies, `jos run` without the serial console) is the
  natural next step and would reuse the same share layout.
