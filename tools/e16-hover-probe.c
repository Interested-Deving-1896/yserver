/* e16-hover-probe — replicate e16's menu-item hover drawing idiom
 * against a running X server and verify each intermediate with
 * XGetImage. Mirrors the exact request sequence from e16.xtrace
 * (2026-06-04, silence HW run):
 *
 *   1. CreatePixmap SLICE (139x18 d24); fill GREEN  (stand-in for the
 *      CopyArea menu-image slice)
 *   2. CreatePixmap TILE (139x18 d24); ShmPutImage RED/BLUE pattern
 *   3. CreateGC fill-style=Tiled tile=TILE; PolyFillRectangle SLICE
 *      full-rect          → SLICE must now equal TILE's pattern
 *   4. ChangeWindowAttributes bg-pixmap=SLICE on a mapped window W;
 *      XClearArea(W)      → W must show the pattern
 *
 * Each step is read back with XGetImage and graded. A second pass
 * uses core PutImage for the tile to split SHM-vs-core suspicion.
 *
 * Build:  gcc -O1 -o e16-hover-probe e16-hover-probe.c -lX11 -lXext
 * Run:    DISPLAY=:7 ./e16-hover-probe
 */
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <X11/extensions/XShm.h>
#include <sys/ipc.h>
#include <sys/shm.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define W 139
#define H 18

static int failures = 0;

/* tile pattern: left half RED, right half BLUE */
static unsigned long pattern_at(int x, int y)
{
    (void)y;
    return (x < W / 2) ? 0xff0000UL : 0x0000ffUL;
}

static void grade(Display *dpy, Drawable d, const char *label, int expect_pattern,
                  unsigned long solid)
{
    XImage *img = XGetImage(dpy, d, 0, 0, W, H, AllPlanes, ZPixmap);
    if (!img) {
        printf("FAIL %-28s XGetImage returned NULL\n", label);
        failures++;
        return;
    }
    int bad = 0, first_x = -1, first_y = -1;
    unsigned long got0 = 0, want0 = 0;
    for (int y = 0; y < H; y++)
        for (int x = 0; x < W; x++) {
            unsigned long want = expect_pattern ? pattern_at(x, y) : solid;
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
        printf("FAIL %-28s %d/%d px wrong; first (%d,%d) got=%06lx want=%06lx\n",
               label, bad, W * H, first_x, first_y, got0, want0);
        failures++;
    } else {
        printf("PASS %-28s\n", label);
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

/* write pattern into TILE via MIT-SHM PutImage (e16's path) */
static int shm_put_pattern(Display *dpy, Drawable tile)
{
    XShmSegmentInfo shminfo;
    XImage *img = XShmCreateImage(dpy, DefaultVisual(dpy, DefaultScreen(dpy)),
                                  24, ZPixmap, NULL, &shminfo, W, H);
    if (!img)
        return 0;
    shminfo.shmid = shmget(IPC_PRIVATE, (size_t)img->bytes_per_line * H, IPC_CREAT | 0600);
    if (shminfo.shmid < 0)
        return 0;
    shminfo.shmaddr = img->data = shmat(shminfo.shmid, NULL, 0);
    shminfo.readOnly = False;
    if (!XShmAttach(dpy, &shminfo))
        return 0;
    XSync(dpy, False);
    for (int y = 0; y < H; y++)
        for (int x = 0; x < W; x++)
            XPutPixel(img, x, y, pattern_at(x, y));
    GC gc = XCreateGC(dpy, tile, 0, NULL);
    XShmPutImage(dpy, tile, gc, img, 0, 0, 0, 0, W, H, False);
    XSync(dpy, False);
    XFreeGC(dpy, gc);
    XShmDetach(dpy, &shminfo);
    XSync(dpy, False);
    XDestroyImage(img);
    shmdt(shminfo.shmaddr);
    shmctl(shminfo.shmid, IPC_RMID, NULL);
    return 1;
}

/* write pattern into TILE via core PutImage */
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
    XDestroyImage(img); /* frees buf */
}

static void run_pass(Display *dpy, Window win, int use_shm, const char *tag)
{
    char label[64];
    Window root = DefaultRootWindow(dpy);

    /* 1. SLICE = green */
    Pixmap slice = XCreatePixmap(dpy, root, W, H, 24);
    fill_solid(dpy, slice, 0x00ff00);
    snprintf(label, sizeof label, "%s slice solid green", tag);
    grade(dpy, slice, label, 0, 0x00ff00);

    /* 2. TILE pattern */
    Pixmap tile = XCreatePixmap(dpy, root, W, H, 24);
    if (use_shm) {
        if (!shm_put_pattern(dpy, tile)) {
            printf("SKIP %s tile via shm (no MIT-SHM)\n", tag);
            XFreePixmap(dpy, slice);
            XFreePixmap(dpy, tile);
            return;
        }
    } else {
        core_put_pattern(dpy, tile);
    }
    snprintf(label, sizeof label, "%s tile after PutImage", tag);
    grade(dpy, tile, label, 1, 0);

    /* 3. tiled PolyFillRectangle onto SLICE, exact e16 GC values */
    XGCValues v;
    v.fill_style = FillTiled;
    v.tile = tile;
    v.graphics_exposures = False;
    v.clip_mask = None;
    GC tgc = XCreateGC(dpy, slice, GCFillStyle | GCTile | GCGraphicsExposures | GCClipMask, &v);
    XFillRectangle(dpy, slice, tgc, 0, 0, W, H);
    XFreeGC(dpy, tgc);
    snprintf(label, sizeof label, "%s slice after tiled fill", tag);
    grade(dpy, slice, label, 1, 0);

    /* 4. bg-pixmap swap + ClearArea on the mapped window */
    XSetWindowBackgroundPixmap(dpy, win, slice);
    XClearArea(dpy, win, 0, 0, 0, 0, False);
    XSync(dpy, False);
    snprintf(label, sizeof label, "%s window after ClearArea", tag);
    grade(dpy, win, label, 1, 0);

    XFreePixmap(dpy, slice);
    XFreePixmap(dpy, tile);
}

int main(void)
{
    Display *dpy = XOpenDisplay(NULL);
    if (!dpy) {
        fprintf(stderr, "cannot open display\n");
        return 2;
    }
    Window win = XCreateSimpleWindow(dpy, DefaultRootWindow(dpy), 10, 10, W, H, 0, 0,
                                     0xcccccc);
    XMapWindow(dpy, win);
    XSync(dpy, False);
    /* give the server a moment to map/compose */
    usleep(300 * 1000);

    run_pass(dpy, win, 1, "[shm] ");
    run_pass(dpy, win, 0, "[core]");

    printf(failures ? "RESULT: %d failure(s)\n" : "RESULT: all pass\n", failures);
    XCloseDisplay(dpy);
    return failures ? 1 : 0;
}
