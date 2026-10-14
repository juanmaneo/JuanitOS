# Inter-qube: the share, the guest agent, `jos run`, `jos copy` and the policy

QubesOS has *qrexec*: a dom0-mediated RPC channel between qubes, with
`qvm-copy-to-vm` on top of it and a policy deciding which qube may ask
what of whom. JuanitOS does the same with two channels per qube: a dom0
directory exported into every FreeBSD or Linux qube over **virtio-9p**
carries files and requests one way and answers the other, with `jos
serve` on dom0 as the broker applying `jos policy`; and a
**virtio-console** port served by a small daemon in the guest lets dom0
run commands in a qube (`jos run`). Together they give every clone its
own hostname and move files between qubes without the qubes ever seeing
each other's storage.

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

## The agent channel and `jos run`

```sh
jos run work -- docker ps -a
jos run web --user root -- apt-get update
jos run rx -- uname -a                # Redox: the serial console
jos run work --console -- id          # the serial console on purpose
```

Every FreeBSD or Linux qube gets a second device from dom0:
`-s 30,virtio-console,jos=/var/run/juanitos/<qube>.agent,events=/var/run/juanitos/<qube>.events`
(`crates/juanitos-core/src/agent.rs`): two ports, `jos` for dom0 to
talk to the guest and `events` for the guest to call dom0 (next
section). bhyve listens on those UNIX sockets; the guest sees the ports
as `/dev/ttyV0.0` and `.1` with `/dev/vtcon/<name>` aliases (FreeBSD) or
`/dev/virtio-ports/<name>` (Linux). Inside,
`/usr/local/sbin/jos-agentd` (`templates/agent/jos-agentd`, a POSIX
`sh` script shared by both; started by `rc.d/jos_agentd` or
`jos-agentd.service`, early, before the network) reads the port line by
line and runs what dom0 sends:

```text
dom0  -> guest   PING <id>
guest -> dom0    PONG <id>
dom0  -> guest   RUN <id> <user> <command...>
guest -> dom0    <stdout and stderr of the command>
                 END <id> <status>
```

`jos run` connects, pings, sends the command, prints what comes back and
exits with the command's status. The command runs through `su <user> -c`
(root included) with stdin from `/dev/null`, starting in the user's home
directory; the id is dom0's, so the end line cannot be mistaken for
output. If nothing answers the ping within 3 s (a clone made before the
daemon existed, or a template booted elsewhere) `jos run` says so and
types into the serial console instead, which is also what Redox qubes
always get and what `--console` forces. Compared with the console path
there is no login, no echo, no prompt detection, no clash with someone
using `jos console`, and a command may read its input without eating
the marker; one request at a time per qube, since bhyve accepts a single
client on the socket.

bhyve does not remove a stale socket before binding, so dom0 deletes it
before every boot. Two things about the guests: Linux allows a single
open of a virtio port and returns EOF from it while dom0 is not
connected, so the daemon holds one descriptor for both directions and
sleeps a second between reconnections; FreeBSD's port is a tty, which
the daemon puts in raw mode (no echo, no CR/LF translation) while it
holds it open, as the tty forgets its modes on the last close.

Verified on the nested dom0: `work` (FreeBSD) and `web` (Debian) answer
`id`, `pwd`, multi-command lines, exit statuses, output without a final
newline and 2 MB of output in one go, in well under a second; `jos copy`
and `jos-copy` ride on it. The daemon was then added to both templates
the usual way (`jos template run`, install, power off, `jos template
snapshot ... agentd`), and a fresh clone of each answers `jos run` as
soon as it has booted, with nobody logged in on its console.

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
`<id>.tmp` renamed into place; the broker ignores `.tmp` files, having
once failed and removed one while the guest was still writing it) and
rings dom0's doorbell (one line, `REQUEST <id>`, on the qube's events
port: `/dev/vtcon/events` or `/dev/virtio-ports/events`, which the
templates make writable by the qube's users — `perm vtcon/events 0622`
in `devfs.conf`, a udev rule on Debian) and waits for
`/mnt/jos/req/<id>.reply` (up to `JOS_COPY_TIMEOUT`, 600 s). The exit
status says what dom0 answered: 0 delivered, 77 denied, 75 still
pending, 1 error.

On dom0, `jos serve` (started by `rc.d/jos` as a `daemon(8)`, pidfile
and log under `/var/run/juanitos/serve.*`; `jos serve --once` for a
single pass) holds the events port of every running qube open
(`crates/juanitos-core/src/events.rs`: one reader thread per port, dialed
again after the qube restarts) and scans every `req/` directory when a
line arrives on any of them, or every `--interval` seconds (2 by default)
when none does: the poll covers a qube without the port (Redox, a clone
made before it existed) and anything a lost line would leave behind. The
share is the queue, the port only the doorbell: nothing a guest writes
there is interpreted, a line is logged cut to printable ASCII, batches
are bounded and wake-ups at least 200 ms apart, so a hostile qube can at
most make the broker scan a few times a second. Each request is decided
with the **policy** in `/usr/local/etc/juanitos/policy`
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

- `jos copy` runs the export in the source qube with `jos run`, so it
  needs the source qube to be running (with the agent channel, or the
  console free); a long `cp` is bounded by `--timeout` (default 600 s).
  `jos-copy` needs neither.
- The destination need not be running: files wait in its inbox on dom0.
- Large transfers are a `cp` through 9p; fine for sources and documents,
  not meant for disk images.
- A request is answered in well under half a second from the guest's
  point of view (`jos-copy` on `work` and `web` with the poll set to 60 s:
  0.40 s and 0.37 s, most of it `jos-copy`'s own quarter-second wait for
  the reply). bhyve accepts one client per socket, which is why the
  doorbell is a port of its own rather than a line on the `jos` port
  `jos run` uses: `jos serve` can hold it for good without blocking
  `jos run`. Only `jos-copy` rings so far; any guest program may, the
  broker scans whatever the line says.
- The poll interval could be raised now that the doorbell exists; it
  stays at 2 s so that clones made before the port (and Redox qubes, which
  have no share either) keep their behaviour.
