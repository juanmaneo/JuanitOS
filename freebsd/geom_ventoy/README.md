# geom_ventoy

Ventoy's GEOM class: it maps an ISO file on a Ventoy stick to the provider
`ventoy/IMAGE`, so the live system finds `/dev/iso9660/JUANITOS`.

Vendored from [Ventoy](https://github.com/ventoy/Ventoy)
`Unix/ventoy_unix_src/FreeBSD/geom_ventoy_src/15.x/sys/geom/ventoy`
(commit 5fae0ca, Ventoy 1.1.08), BSD-2-Clause (see the file headers;
copyright longpanda and Pawel Jakub Dawidek).

Local changes:

- `#include "g_ventoy.h"` so it builds out of tree;
- `g_ventoy_map_data` sits in its own section `.data.ventoy_map`, aligned
  to 64 KiB, so `ld -r` places it at a 64 KiB file offset. Ventoy's
  `vt_unix_ko_fillmap` only searches there. `jos-release` checks the
  offset and lowers the recorded alignment to a page afterwards: the
  kernel's object linker does not honour more than a page and would
  otherwise fault while loading the module.

Build (done by `jos-release` for the live ISO):

```sh
make SYSDIR=/usr/src/sys
```
