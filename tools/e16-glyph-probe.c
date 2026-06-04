/* e16-glyph-probe — extend the hover-idiom probe with the RENDER
 * glyph step that e16 menus use, and check for DELAYED corruption
 * (the e16 signature: pixmap content shows in a window copy, then
 * later vanishes from the pixmap).
 *
 * Per "item" i (9 items, like an e16 menu):
 *   1. CreatePixmap A_i (139x18 d24); solid fill green
 *   2. CreatePixmap T_i; core PutImage red/blue pattern; tiled
 *      PolyFillRectangle onto A_i
 *   3. CreatePicture on A_i; CompositeText8 two passes (shadow +
 *      fg), fresh glyphset per item (e16 makes one per menu);
 *      FreePicture
 *   4. CWA bg-pixmap=A_i on item window W_i + ClearArea
 *   5. immediate GetImage verify A_i and W_i
 * Then: hover-churn (create/free hilite pixmaps + pictures on the
 * windows, 6 rounds like sweeping the menu), THEN re-verify all
 * A_i and W_i — catches late wipes.
 *
 * Build: gcc -O1 -o e16-glyph-probe e16-glyph-probe.c -lX11 -lXext -lXrender
 */
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <X11/extensions/Xrender.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define W 139
#define H 18
#define NITEMS 9
#define GLYPH_N 5     /* glyphs drawn per pass */
#define GLYPH_SZ 8    /* 8x8 solid-square glyph */
#define GLYPH_X0 7    /* first pen x */
#define GLYPH_Y0 13   /* pen baseline */

static int failures = 0;

static unsigned long pattern_at(int x, int y)
{
    (void)y;
    return (x < W / 2) ? 0xff0000UL : 0x0000ffUL;
}

/* glyph squares: pen at (GLYPH_X0 + k*GLYPH_SZ, GLYPH_Y0), square covers
 * [pen_x, pen_x+8) x [pen_y-8, pen_y) — white (composited fg). */
static int in_glyph(int x, int y)
{
    if (y < GLYPH_Y0 - GLYPH_SZ || y >= GLYPH_Y0)
        return 0;
    int rel = x - GLYPH_X0;
    if (rel < 0 || rel >= GLYPH_N * GLYPH_SZ)
        return 0;
    return 1; /* squares are adjacent: solid run */
}

static void grade(Display *dpy, Drawable d, const char *label, int with_glyphs)
{
    XImage *img = XGetImage(dpy, d, 0, 0, W, H, AllPlanes, ZPixmap);
    if (!img) {
        printf("FAIL %-34s XGetImage NULL\n", label);
        failures++;
        return;
    }
    int bad = 0, first_x = -1, first_y = -1;
    unsigned long got0 = 0, want0 = 0;
    for (int y = 0; y < H; y++)
        for (int x = 0; x < W; x++) {
            unsigned long want = (with_glyphs && in_glyph(x, y)) ? 0xffffffUL
                                                                 : pattern_at(x, y);
            unsigned long got = XGetPixel(img, x, y) & 0xffffff;
            if (got != want) {
                if (!bad) {
                    first_x = x;
                    first_y = y;
                    got0 = got;
                    want0 = want;
                }
                bad++;
            }
        }
    if (bad) {
        printf("FAIL %-34s %d/%d px wrong; first (%d,%d) got=%06lx want=%06lx\n",
               label, bad, W * H, first_x, first_y, got0, want0);
        failures++;
    } else {
        printf("PASS %-34s\n", label);
    }
    XDestroyImage(img);
}

static void fill_solid(Display *dpy, Drawable d, unsigned long color)
{
    GC gc = XCreateGC(dpy, d, 0, NULL);
    XSetForeground(dpy, gc, color);
    XFillRectangle(dpy, d, gc, 0, 0, W, H);
    XFreeGC(dpy, gc);
}

static void core_put_pattern(Display *dpy, Drawable tile)
{
    char *buf = calloc((size_t)W * H, 4);
    XImage *img = XCreateImage(dpy, DefaultVisual(dpy, DefaultScreen(dpy)), 24,
                               ZPixmap, 0, buf, W, H, 32, 0);
    for (int y = 0; y < H; y++)
        for (int x = 0; x < W; x++)
            XPutPixel(img, x, y, pattern_at(x, y));
    GC gc = XCreateGC(dpy, tile, 0, NULL);
    XPutImage(dpy, tile, gc, img, 0, 0, 0, 0, W, H);
    XFreeGC(dpy, gc);
    XDestroyImage(img);
}

static void tiled_fill(Display *dpy, Drawable dst, Pixmap tile)
{
    XGCValues v;
    v.fill_style = FillTiled;
    v.tile = tile;
    v.graphics_exposures = False;
    v.clip_mask = None;
    GC gc = XCreateGC(dpy, dst, GCFillStyle | GCTile | GCGraphicsExposures | GCClipMask, &v);
    XFillRectangle(dpy, dst, gc, 0, 0, W, H);
    XFreeGC(dpy, gc);
}

/* fresh glyphset with one solid 8x8 square glyph (id 1) */
static GlyphSet make_glyphset(Display *dpy)
{
    XRenderPictFormat *a8 = XRenderFindStandardFormat(dpy, PictStandardA8);
    GlyphSet gs = XRenderCreateGlyphSet(dpy, a8);
    Glyph gid = 1;
    XGlyphInfo gi;
    gi.width = GLYPH_SZ;
    gi.height = GLYPH_SZ;
    gi.x = 0;             /* origin at top-left of bitmap */
    gi.y = GLYPH_SZ;      /* pen baseline at bottom */
    gi.xOff = GLYPH_SZ;
    gi.yOff = 0;
    char bits[GLYPH_SZ * GLYPH_SZ];
    memset(bits, 0xff, sizeof bits);
    XRenderAddGlyphs(dpy, gs, &gid, &gi, 1, bits, sizeof bits);
    return gs;
}

/* mimic e16: CreatePicture on pixmap, CompositeText8 ×2 (shadow then
 * fg — both white here so verification is single-color), FreePicture */
static void draw_glyphs(Display *dpy, Pixmap dst, GlyphSet gs)
{
    XRenderPictFormat *rgb = XRenderFindStandardFormat(dpy, PictStandardRGB24);
    Picture pic = XRenderCreatePicture(dpy, dst, rgb, 0, NULL);
    XRenderColor white = { 0xffff, 0xffff, 0xffff, 0xffff };
    Picture src = XRenderCreateSolidFill(dpy, &white);
    char glyphs[GLYPH_N] = { 1, 1, 1, 1, 1 };
    XGlyphElt8 elt;
    elt.glyphset = gs;
    elt.chars = glyphs;
    elt.nchars = GLYPH_N;
    elt.xOff = GLYPH_X0;
    elt.yOff = GLYPH_Y0;
    XRenderPictFormat *a8 = XRenderFindStandardFormat(dpy, PictStandardA8);
    /* two passes like e16's shadow+fg (same coords/color here) */
    XRenderCompositeText8(dpy, PictOpOver, src, pic, a8, 0, 0, 0, 0, &elt, 1);
    XRenderCompositeText8(dpy, PictOpOver, src, pic, a8, 0, 0, 0, 0, &elt, 1);
    XRenderFreePicture(dpy, src);
    XRenderFreePicture(dpy, pic);
}

int main(void)
{
    Display *dpy = XOpenDisplay(NULL);
    if (!dpy) {
        fprintf(stderr, "cannot open display\n");
        return 2;
    }
    Window wins[NITEMS];
    Pixmap items[NITEMS];
    char label[96];

    for (int i = 0; i < NITEMS; i++) {
        wins[i] = XCreateSimpleWindow(dpy, DefaultRootWindow(dpy), 10,
                                      10 + i * (H + 2), W, H, 0, 0, 0xcccccc);
        XMapWindow(dpy, wins[i]);
    }
    XSync(dpy, False);
    usleep(200 * 1000);

    /* ── creation pass: item pixmaps with glyphs ───────────────── */
    for (int i = 0; i < NITEMS; i++) {
        items[i] = XCreatePixmap(dpy, DefaultRootWindow(dpy), W, H, 24);
        fill_solid(dpy, items[i], 0x00ff00);
        Pixmap tile = XCreatePixmap(dpy, DefaultRootWindow(dpy), W, H, 24);
        core_put_pattern(dpy, tile);
        tiled_fill(dpy, items[i], tile);
        XFreePixmap(dpy, tile);
        GlyphSet gs = make_glyphset(dpy); /* fresh per item, like e16 per menu */
        draw_glyphs(dpy, items[i], gs);
        XRenderFreeGlyphSet(dpy, gs);
        XSetWindowBackgroundPixmap(dpy, wins[i], items[i]);
        XClearArea(dpy, wins[i], 0, 0, 0, 0, False);
        XSync(dpy, False);
        snprintf(label, sizeof label, "create: pixmap %d", i);
        grade(dpy, items[i], label, 1);
        snprintf(label, sizeof label, "create: window %d", i);
        grade(dpy, wins[i], label, 1);
    }

    /* ── hover churn: 6 rounds of hilite create/draw/swap/free ──── */
    for (int round = 0; round < 6; round++) {
        int i = round % NITEMS;
        Pixmap hi = XCreatePixmap(dpy, DefaultRootWindow(dpy), W, H, 24);
        fill_solid(dpy, hi, 0x808080);
        Pixmap tile = XCreatePixmap(dpy, DefaultRootWindow(dpy), W, H, 24);
        core_put_pattern(dpy, tile);
        tiled_fill(dpy, hi, tile);
        GlyphSet gs = make_glyphset(dpy);
        draw_glyphs(dpy, hi, gs);
        XRenderFreeGlyphSet(dpy, gs);
        /* hover on */
        XSetWindowBackgroundPixmap(dpy, wins[i], hi);
        XClearArea(dpy, wins[i], 0, 0, 0, 0, False);
        /* hover off — restore original */
        XSetWindowBackgroundPixmap(dpy, wins[i], items[i]);
        XClearArea(dpy, wins[i], 0, 0, 0, 0, False);
        XSync(dpy, False);
        XFreePixmap(dpy, tile);
        XFreePixmap(dpy, hi); /* e16 frees old hilites as it goes */
    }
    XSync(dpy, False);
    usleep(300 * 1000); /* let any deferred work land */

    /* ── late verification: did anything rot? ──────────────────── */
    for (int i = 0; i < NITEMS; i++) {
        snprintf(label, sizeof label, "late: pixmap %d", i);
        grade(dpy, items[i], label, 1);
        snprintf(label, sizeof label, "late: window %d", i);
        grade(dpy, wins[i], label, 1);
    }

    printf(failures ? "RESULT: %d failure(s)\n" : "RESULT: all pass\n", failures);
    XCloseDisplay(dpy);
    return failures ? 1 : 0;
}
