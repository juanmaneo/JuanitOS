/*-
 * SPDX-License-Identifier: BSD-3-Clause
 *
 * Copyright (c) 2026, Juanito and the JuanitOS contributors (see AUTHORS)
 *
 * jos_bootwatch: a muted boot that cannot mount its root shows why.
 *
 * jos-boot boots the kernel with -m (RB_MUTE): the console is muted and
 * vt(4) keeps the boot splash on the screen. When the root file system
 * cannot be mounted the kernel falls back to its `mountroot>` prompt, but
 * with the console muted cngetc() returns -1 at once, which cngets() takes
 * for a DEL: the prompt spins forever, invisible, and keys do nothing.
 *
 * This module looks at the end of the message buffer once a second until
 * root is mounted. When the kernel sits at the prompt, it unmutes the
 * console and replays the end of the message buffer (the failed mount
 * included) with a note. The prompt's next console read takes the splash
 * away (vt(4) drops it in its cngetc) and the prompt works again.
 */

#include <sys/param.h>
#include <sys/systm.h>
#include <sys/kernel.h>
#include <sys/module.h>
#include <sys/callout.h>
#include <sys/cons.h>
#include <sys/msgbuf.h>
#include <sys/reboot.h>

#define	BW_TAIL		3072
#define	BW_PROMPT	"\nmountroot> "

static struct callout bw_callout;
static char bw_buf[BW_TAIL + 1];

/* The last `BW_TAIL` bytes of the message buffer, NUL-terminated. */
static size_t
bw_tail(void)
{
	struct msgbuf *mbp = msgbufp;
	u_int seq, want;
	size_t len = 0;
	int n;

	if (mbp == NULL)
		return (0);
	want = min(BW_TAIL, mbp->msg_size);
	if ((mbp->msg_flags & MSGBUF_WRAP) == 0 && mbp->msg_wseq < want)
		seq = 0;
	else
		seq = MSGBUF_SEQSUB(mbp, mbp->msg_wseq, want);
	while (len < BW_TAIL) {
		n = msgbuf_peekbytes(mbp, bw_buf + len, BW_TAIL - len, &seq);
		if (n <= 0)
			break;
		len += n;
	}
	bw_buf[len] = '\0';
	return (len);
}

static bool
bw_at_prompt(size_t len)
{
	size_t plen = sizeof(BW_PROMPT) - 1;

	return (len >= plen && memcmp(bw_buf + len - plen, BW_PROMPT, plen) == 0);
}

static void
bw_reveal(size_t len)
{
	const char *start = bw_buf;
	const char *nl;

	/* Start the replay on a line boundary. */
	if (len == BW_TAIL && (nl = strchr(bw_buf, '\n')) != NULL)
		start = nl + 1;

	cn_mute = 0;
	cnputs("\n\n");
	cnputs((char *)(uintptr_t)start);
	printf("\n\nJuanitOS: the quiet boot stopped before the root file system"
	    " was mounted;\nthe kernel messages above say why.\n"
	    "Live USB on a Ventoy stick? Update Ventoy (1.1.08 or later)"
	    " or write the ISO\nto the stick with dd.\n"
	    "Type ? for the disks the kernel found, a root as shown"
	    " above, or press Enter\nto give up (the machine restarts).\n");
	cnputs("\nmountroot> ");
}

static void
bw_tick(void *arg __unused)
{
	size_t len;

	if (root_mounted() || !cn_mute)
		return;
	len = bw_tail();
	if (bw_at_prompt(len)) {
		bw_reveal(len);
		return;
	}
	callout_schedule(&bw_callout, hz);
}

static void
bw_start(void *arg __unused)
{
	if ((boothowto & RB_MUTE) == 0 || root_mounted())
		return;
	callout_reset(&bw_callout, hz, bw_tick, NULL);
}
SYSINIT(jos_bootwatch, SI_SUB_LAST, SI_ORDER_ANY, bw_start, NULL);

static int
bw_modevent(module_t mod __unused, int type, void *data __unused)
{
	switch (type) {
	case MOD_LOAD:
		callout_init(&bw_callout, 1);
		return (0);
	case MOD_UNLOAD:
		callout_drain(&bw_callout);
		return (0);
	default:
		return (EOPNOTSUPP);
	}
}

static moduledata_t bw_mod = {
	"jos_bootwatch",
	bw_modevent,
	NULL
};
DECLARE_MODULE(jos_bootwatch, bw_mod, SI_SUB_DRIVERS, SI_ORDER_ANY);
MODULE_VERSION(jos_bootwatch, 1);
