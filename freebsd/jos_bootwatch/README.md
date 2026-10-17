# jos_bootwatch

Kernel module that shows why a quiet (`-m`) boot stopped before mounting
its root, instead of leaving the splash up over an invisible `mountroot>`
prompt. See `docs/boot-splash.md` ("Root mount failures on screen").

Built and shipped by `jos-release` (`/boot/modules/jos_bootwatch.ko` plus
`/boot/loader.conf.d/jos_bootwatch.conf`). By hand:

```sh
make SYSDIR=/usr/src/sys
```
