# Inter-qube: the share, the guest agent, `jos copy` and the policy

QubesOS has *qrexec*: a dom0-mediated RPC channel between qubes, with
`qvm-copy-to-vm` on top of it and a policy deciding which qube may ask
what of whom. JuanitOS does the same with files rather than a socket
protocol: a dom0 directory exported into every FreeBSD or Linux qube over
**virtio-9p** carries requests one way and answers the other, `jos serve`
on dom0 is the broker applying `jos policy`, and `jos run` (commands
typed on a qube's serial console) lets dom0 drive a qube when it has to.
Together they give every clone its own hostname and move files between
qubes without the qubes ever seeing each other's storage.

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
├── out/                           outbox (1777): what the guest exports (jos copy, jos-copy)
└── req/                           requests (777): <id> from the guest, <id>.reply from dom0
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
edit what root put there; copies between qubes keep their owner, which
is the same `user` uid in every template. Every dom0 step goes through
the `Runner`, so `jos copy --dry-run` prints the plan. dom0 clears the
outbox spot before the guest exports and replaces an inbox item of the
same name, as the guest user cannot remove files on the share (see
below); an inbox is tidied by copying over it, by `rm` as root in the
guest, or on dom0.

`jos copy` is dom0 initiating a transfer, as with `qvm-copy-to-vm` run
from dom0: the administrator typing it *is* the policy. A qube asking
for one is the next section.

## Requests from a qube: `jos-copy` and the policy

Inside a qube (`templates/agent/jos-copy`, installed as
`/usr/local/bin/jos-copy` by every template):

```sh
user@work:~ $ jos-copy ~/report.pdf personal
jos-copy: asked dom0 to copy report.pdf to personal (request 1760450000-812)
jos-copy: ok /mnt/jos/in/work/report.pdf
```

The script copies the file to `/mnt/jos/out/report.pdf`, writes
`copy report.pdf personal` to `/mnt/jos/req/<id>` (atomically: a
temporary file renamed into place) and waits for `/mnt/jos/req/<id>.reply`
(up to `JOS_COPY_TIMEOUT`, 600 s). The exit status says what dom0
answered: 0 delivered, 77 denied, 75 still pending, 1 error.

On dom0, `jos serve` (started by `rc.d/jos` as a `daemon(8)`, pidfile
and log under `/var/run/juanitos/serve.*`; `jos serve --once` for a
single pass) polls every `req/` directory and decides with the
**policy** in `/usr/local/etc/juanitos/policy`
(`crates/juanitos-core/src/requests.rs`):

```sh
jos policy add work personal allow    # copy SRC DST allow|deny|ask
jos policy add '*' vault deny
jos policy show
```

One rule per line, `*` matches any qube, the first matching rule wins and
no match means `ask`. The verdicts:

| verdict | dom0 does                                                     | reply                          |
|---------|---------------------------------------------------------------|--------------------------------|
| `allow` | `mv share/work/out/<item> share/personal/in/work/<item>`      | `ok /mnt/jos/in/work/<item>`   |
| `deny`  | nothing                                                       | `denied: policy`               |
| `ask`   | leaves the request for the operator                           | none until settled             |

Pending requests are listed and settled by hand:

```sh
jos requests                                  # QUBE ID copy ITEM -> DST [verdict]
jos requests approve work 1760450000-812
jos requests deny work 1760450000-812
```

Whatever the outcome, dom0 writes the reply and removes the request; a
refused or failed export is removed from the outbox, and replies are
swept a minute later. dom0 does all the removing because of one
property of bhyve's 9p server found on the way: an unprivileged guest
user can create, write and rename files on the share but **cannot
unlink** them, whatever the directory and file modes say (root in the
guest can; FreeBSD and Debian guests behave the same). `jos-copy`
therefore never deletes, and refuses to export a name still sitting in
the outbox (dom0 has not consumed it yet). A malformed request (bad
name, unknown service, a path instead of a file name) is answered
`error: ...` right away so the guest does not wait; a request whose
destination does not exist or has no share fails the same way when it is
granted. Requests and replies are plain files: `ls
/var/db/juanitos/share/*/req` is the queue, and nothing is lost if the
daemon is restarted.

What a qube cannot do, by construction: name a path outside its own
outbox (the request carries a file name, not a path; dom0 validates it
as a single component), reach another qube's share, or learn anything
about other qubes beyond the reply to its own request. Only `copy` exists
as a service so far; the request line starts with the service name so
others can be added with their own policy rules.

## Limits and next steps

- `jos copy` reads the source over the serial console, so it needs the
  source qube to be running with its autologin shell free; a long `cp`
  is bounded by `--timeout` (default 600 s). `jos-copy` needs no console
  at all.
- The destination need not be running: files wait in its inbox on dom0.
- Large transfers are a `cp` through 9p; fine for sources and documents,
  not meant for disk images.
- `jos serve` polls (every 2 s by default); a guest daemon on a
  virtio-console or vsock would make it event-driven and could replace
  the serial console under `jos run`, keeping the same share layout,
  request format and policy.
