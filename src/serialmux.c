#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdarg.h>
#include <stdint.h>
#include <errno.h>
#include <time.h>
#include <signal.h>
#include <unistd.h>
#include <fcntl.h>
#include <dirent.h>
#include <termios.h>
#include <sys/epoll.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/types.h>

#include "serialmux.h"

/* ── Globals ────────────────────────────────────────────────────────────── */
uint32_t g_crc32_tbl[256];
static volatile sig_atomic_t g_stop = 0;

/* ── CRC32 ──────────────────────────────────────────────────────────────── */
void crc32_init(void)
{
    for (uint32_t i = 0; i < 256; i++) {
        uint32_t c = i;
        for (int k = 0; k < 8; k++)
            c = (c & 1) ? (0xEDB88320U ^ (c >> 1)) : (c >> 1);
        g_crc32_tbl[i] = c;
    }
}

/* ── Utilities ──────────────────────────────────────────────────────────── */
int64_t mono_now_ms(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

void log_ts(const char *fmt, ...)
{
    time_t t = time(NULL);
    struct tm tm;
    localtime_r(&t, &tm);
    fprintf(stderr, "%02d:%02d:%02d ", tm.tm_hour, tm.tm_min, tm.tm_sec);
    va_list ap;
    va_start(ap, fmt);
    vfprintf(stderr, fmt, ap);
    va_end(ap);
    fputc('\n', stderr);
    fflush(stderr);
}

int set_nonblock(int fd)
{
    int fl = fcntl(fd, F_GETFL);
    if (fl < 0) return -1;
    return fcntl(fd, F_SETFL, fl | O_NONBLOCK);
}

void epoll_update(int epfd, int fd, uint32_t events, uint64_t tag, int *in_epoll)
{
    if (events == 0) {
        if (*in_epoll) {
            epoll_ctl(epfd, EPOLL_CTL_DEL, fd, NULL);
            *in_epoll = 0;
        }
        return;
    }
    struct epoll_event ev = { .events = events, .data.u64 = tag };
    if (*in_epoll)
        epoll_ctl(epfd, EPOLL_CTL_MOD, fd, &ev);
    else {
        epoll_ctl(epfd, EPOLL_CTL_ADD, fd, &ev);
        *in_epoll = 1;
    }
}

/* ── Serial port open ───────────────────────────────────────────────────── */
speed_t baud_to_speed(int baud)
{
    switch (baud) {
    case 1200:   return B1200;
    case 2400:   return B2400;
    case 4800:   return B4800;
    case 9600:   return B9600;
    case 19200:  return B19200;
    case 38400:  return B38400;
    case 57600:  return B57600;
    case 115200: return B115200;
    case 230400: return B230400;
    case 460800: return B460800;
    case 921600: return B921600;
    default:     return B0;
    }
}

int open_serial_fd(const char *dev, int baud)
{
    int fd = open(dev, O_RDWR | O_NOCTTY | O_NONBLOCK);
    if (fd < 0) return -1;

    speed_t sp = baud_to_speed(baud);
    if (sp == B0) { close(fd); errno = EINVAL; return -1; }

    struct termios t;
    if (tcgetattr(fd, &t) < 0) { close(fd); return -1; }
    t.c_iflag = 0;
    t.c_oflag = 0;
    t.c_cflag = CS8 | CREAD | CLOCAL;
    t.c_lflag = 0;
    t.c_cc[VMIN]  = 0;
    t.c_cc[VTIME] = 0;
    cfsetispeed(&t, sp);
    cfsetospeed(&t, sp);
    if (tcsetattr(fd, TCSAFLUSH, &t) < 0) { close(fd); return -1; }
    return fd;
}

/* ── USB sysfs discovery ────────────────────────────────────────────────── */
static int read_sysfs_str(const char *path, char *out, size_t outsz)
{
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1;
    ssize_t n = read(fd, out, outsz - 1);
    close(fd);
    if (n <= 0) return -1;
    out[n] = '\0';
    /* strip trailing newline */
    while (n > 0 && (out[n-1] == '\n' || out[n-1] == '\r'))
        out[--n] = '\0';
    return 0;
}

char *find_acm_by_usb_id(const char *vid, const char *pid, char *out, size_t outsz)
{
    DIR *dir = opendir("/sys/class/tty");
    if (!dir) return NULL;

    struct dirent *ent;
    while ((ent = readdir(dir))) {
        if (strncmp(ent->d_name, "ttyACM", 6) != 0)
            continue;

        char spath[512];
        snprintf(spath, sizeof(spath), "/sys/class/tty/%s", ent->d_name);

        /* Resolve symlink to real path */
        char real[512];
        if (!realpath(spath, real))
            snprintf(real, sizeof(real), "%s", spath);

        /* Walk up looking for idVendor/idProduct */
        char cur[512];
        snprintf(cur, sizeof(cur), "%s", real);
        for (int depth = 0; depth < 8; depth++) {
            char vpath[600], ppath[600];
            snprintf(vpath, sizeof(vpath), "%s/idVendor", cur);
            snprintf(ppath, sizeof(ppath), "%s/idProduct", cur);

            char v[16], p[16];
            if (read_sysfs_str(vpath, v, sizeof(v)) == 0 &&
                read_sysfs_str(ppath, p, sizeof(p)) == 0) {
                if (strcmp(v, vid) == 0 && strcmp(p, pid) == 0) {
                    snprintf(out, outsz, "/dev/%s", ent->d_name);
                    closedir(dir);
                    return out;
                }
                break;  /* found id files but no match — don't climb further */
            }
            /* Go up one level */
            char *slash = strrchr(cur, '/');
            if (!slash || slash == cur) break;
            *slash = '\0';
        }
    }
    closedir(dir);
    return NULL;
}

/* ── FrameParser ────────────────────────────────────────────────────────── */
void fp_feed(frame_parser_t *fp, struct daemon *d,
             const uint8_t *data, size_t n)
{
    size_t pos = 0;
    while (pos < n) {
        /* Fill: copy as much input as fits, then parse all complete frames */
        size_t space = sizeof(fp->buf) - fp->len;
        size_t copy  = n - pos;
        if (copy > space) copy = space;
        memcpy(fp->buf + fp->len, data + pos, copy);
        fp->len += copy;
        pos     += copy;

    while (fp->len >= 2) {
        /* Find magic */
        size_t i = 0;
        while (i + 1 < fp->len &&
               !(fp->buf[i] == 0xAA && fp->buf[i+1] == 0x55))
            i++;

        if (i + 1 >= fp->len) {
            /* No complete magic found; keep last byte in case it's 0xAA */
            fp->buf[0] = fp->buf[fp->len - 1];
            fp->len = 1;
            break;
        }

        if (i > 0) {
            memmove(fp->buf, fp->buf + i, fp->len - i);
            fp->len -= i;
        }

        if (fp->len < (size_t)HDR_SIZE)
            break;

        /* Parse header: magic(2) type(1) channel(1) length(2 LE) */
        uint8_t  ftype   = fp->buf[2];
        uint8_t  channel = fp->buf[3];
        uint16_t length;
        memcpy(&length, fp->buf + 4, 2);
        /* le16 — we're little-endian (MIPS EL / ARM LE), but be correct */
#if __BYTE_ORDER__ == __ORDER_BIG_ENDIAN__
        length = __builtin_bswap16(length);
#endif

        if (length > MAX_PAYLOAD) {
            /* Bad length — skip magic and rescan */
            memmove(fp->buf, fp->buf + 2, fp->len - 2);
            fp->len -= 2;
            continue;
        }

        size_t total = (size_t)HDR_SIZE + length + CRC_SIZE;
        if (fp->len < total)
            break;

        const uint8_t *payload = fp->buf + HDR_SIZE;
        uint32_t crc_rx;
        memcpy(&crc_rx, fp->buf + HDR_SIZE + length, 4);
#if __BYTE_ORDER__ == __ORDER_BIG_ENDIAN__
        crc_rx = __builtin_bswap32(crc_rx);
#endif

        if (crc32_buf(payload, length) != crc_rx) {
            memmove(fp->buf, fp->buf + 2, fp->len - 2);
            fp->len -= 2;
            continue;
        }

        fp->on_frame(d, ftype, channel, payload, length);

        memmove(fp->buf, fp->buf + total, fp->len - total);
        fp->len -= total;
    }
    }
}

/* ── LinkTxQueue ────────────────────────────────────────────────────────── */
int txq_push(link_txq_t *q, const uint8_t *data, size_t n)
{
    if (n > LINK_TXBUF_SIZE - 1 - txq_used(q))
        return -1;  /* no space */

    size_t space_to_end = LINK_TXBUF_SIZE - q->tail;
    if (n <= space_to_end) {
        memcpy(q->buf + q->tail, data, n);
    } else {
        memcpy(q->buf + q->tail, data, space_to_end);
        memcpy(q->buf, data + space_to_end, n - space_to_end);
    }
    q->tail = (q->tail + n) % LINK_TXBUF_SIZE;
    return 0;
}

int txq_drain(link_txq_t *q, int fd)
{
    if (txq_empty(q)) return 1;

    /* Write the contiguous segment from head */
    size_t head = q->head;
    size_t tail = q->tail;
    size_t avail = (tail >= head)
        ? tail - head
        : LINK_TXBUF_SIZE - head;

    ssize_t written = write(fd, q->buf + head, avail);
    if (written < 0) {
        if (errno == EAGAIN || errno == EWOULDBLOCK) return 0;
        return -1;  /* real error */
    }
    q->head = (q->head + (size_t)written) % LINK_TXBUF_SIZE;
    return txq_empty(q) ? 1 : 0;
}

/* ── Build a frame and push onto txq ────────────────────────────────────── */
static void enqueue_frame(daemon_t *d, uint8_t ftype, uint8_t channel,
                           const uint8_t *payload, uint16_t plen)
{
    uint8_t hdr[HDR_SIZE];
    hdr[0] = 0xAA; hdr[1] = 0x55;
    hdr[2] = ftype;
    hdr[3] = channel;
    hdr[4] = (uint8_t)(plen & 0xFF);
    hdr[5] = (uint8_t)(plen >> 8);

    uint32_t crc = plen ? crc32_buf(payload, plen) : 0x00000000U;
    uint8_t crc_b[4];
    crc_b[0] = (uint8_t)(crc);
    crc_b[1] = (uint8_t)(crc >> 8);
    crc_b[2] = (uint8_t)(crc >> 16);
    crc_b[3] = (uint8_t)(crc >> 24);

    txq_push(&d->txq, hdr, HDR_SIZE);
    if (plen) txq_push(&d->txq, payload, plen);
    txq_push(&d->txq, crc_b, 4);

    d->last_tx_ms = mono_now_ms();

    if (d->link_in_epoll) {
        epoll_update(d->epoll_fd, d->link_fd,
                     EPOLLIN | EPOLLOUT, TAG_LINK, &d->link_in_epoll);
    }
}

/* ── daemon_send (called by channels) ──────────────────────────────────── */
void daemon_send(daemon_t *d, uint8_t ftype, uint8_t channel,
                 const uint8_t *payload, uint16_t plen)
{
    if (d->disconnected) return;

    /* Backpressure: bulk data frames only */
    if (ftype == F_DATA || ftype == F_TDATA) {
        if (!d->link_bp_paused && txq_used(&d->txq) >= LINK_HIGH_WATER) {
            d->link_bp_paused = 1;
            for (int i = 0; i < MAX_CHANNELS; i++)
                if (d->channels[i]) ch_pause(d->channels[i]);
        }
    }
    enqueue_frame(d, ftype, channel, payload, plen);
}

/* ── Frame dispatch ─────────────────────────────────────────────────────── */
static void on_frame(daemon_t *d, uint8_t ftype, uint8_t channel,
                     const uint8_t *payload, uint16_t plen)
{
    if (ftype == F_PING) {
        enqueue_frame(d, F_PONG, 0, NULL, 0);
        return;
    }
    if (ftype == F_PONG) return;

    if (ftype == F_HELLO || ftype == F_ACK) {
        if (ftype == F_HELLO) {
            enqueue_frame(d, F_ACK, 0, NULL, 0);
            if (d->link_up) {
                log_ts("Link: peer reconnected, rebroadcasting");
                for (int i = 0; i < MAX_CHANNELS; i++)
                    if (d->channels[i]) ch_on_link_up(d->channels[i]);
                return;
            }
        }
        if (!d->link_up) {
            d->link_up = 1;
            log_ts("Link: handshake complete (%s)",
                   d->is_exporter ? "exporter" : "host");
            for (int i = 0; i < MAX_CHANNELS; i++)
                if (d->channels[i]) ch_on_link_up(d->channels[i]);
        }
        return;
    }

    channel_t *ch = d->channels[channel];
    if (!ch) return;
    ch_on_frame(ch, ftype, payload, plen);
}

/* ── Link open/close ────────────────────────────────────────────────────── */
static int64_t g_reopen_delay_ms = 500;

static void schedule_reopen(daemon_t *d)
{
    d->reopen_at_ms = mono_now_ms() + g_reopen_delay_ms;
    g_reopen_delay_ms = (g_reopen_delay_ms * 2 > 8000) ? 8000 : g_reopen_delay_ms * 2;
    d->disconnected = 1;
}

static void open_link(daemon_t *d)
{
    char dev_buf[128];
    const char *dev = NULL;

    if (d->usb_vid[0]) {
        dev = find_acm_by_usb_id(d->usb_vid, d->usb_pid, dev_buf, sizeof(dev_buf));
        if (!dev) {
            log_ts("Link: USB %s:%s not found -- retrying", d->usb_vid, d->usb_pid);
            schedule_reopen(d);
            return;
        }
    } else {
        dev = d->link_dev;
    }

    int fd = open_serial_fd(dev, 115200);
    if (fd < 0) {
        log_ts("Link: cannot open %s: %s", dev, strerror(errno));
        schedule_reopen(d);
        return;
    }

    g_reopen_delay_ms = 500;
    d->link_fd      = fd;
    d->disconnected = 0;
    d->link_up      = 0;
    d->link_bp_paused = 0;
    d->last_rx_ms   = mono_now_ms();
    d->last_tx_ms   = mono_now_ms();
    memset(&d->txq, 0, sizeof(d->txq));
    memset(&d->parser, 0, sizeof(d->parser));
    d->parser.on_frame = on_frame;
    epoll_update(d->epoll_fd, fd, EPOLLIN, TAG_LINK, &d->link_in_epoll);
    log_ts("Link: opened %s", dev);
    enqueue_frame(d, F_HELLO, 0, NULL, 0);
}

static void close_link(daemon_t *d, const char *reason)
{
    if (d->disconnected) return;
    log_ts("Link: down -- %s", reason);
    d->disconnected = 1;
    d->link_up      = 0;

    if (d->link_fd >= 0) {
        epoll_update(d->epoll_fd, d->link_fd, 0, 0, &d->link_in_epoll);
        close(d->link_fd);
        d->link_fd = -1;
    }
    memset(&d->txq, 0, sizeof(d->txq));

    if (d->link_bp_paused) {
        d->link_bp_paused = 0;
        for (int i = 0; i < MAX_CHANNELS; i++)
            if (d->channels[i]) ch_resume(d->channels[i]);
    }
    for (int i = 0; i < MAX_CHANNELS; i++)
        if (d->channels[i]) ch_on_link_down(d->channels[i]);

    schedule_reopen(d);
}

/* ── Link event handler ─────────────────────────────────────────────────── */
static void handle_link_event(daemon_t *d, uint32_t events)
{
    if (d->link_fd < 0) return;

    if (events & EPOLLIN) {
        static uint8_t buf[65536];
        ssize_t n = read(d->link_fd, buf, sizeof(buf));
        if (n < 0) {
            if (errno != EAGAIN && errno != EWOULDBLOCK)
                close_link(d, strerror(errno));
            /* fall through to drain EPOLLOUT even on EAGAIN */
        } else if (n > 0) {
            /* n == 0 on non-blocking TTY is not EOF */
            d->last_rx_ms = mono_now_ms();
            fp_feed(&d->parser, d, buf, (size_t)n);
        }
    }

    if (d->link_fd < 0) return;  /* fp_feed may have triggered close_link */

    if (events & EPOLLOUT) {
        int rc = txq_drain(&d->txq, d->link_fd);
        if (rc < 0) { close_link(d, strerror(errno)); return; }

        if (d->link_bp_paused && txq_used(&d->txq) <= LINK_LOW_WATER) {
            d->link_bp_paused = 0;
            for (int i = 0; i < MAX_CHANNELS; i++)
                if (d->channels[i]) ch_resume(d->channels[i]);
        }

        if (txq_empty(&d->txq))
            epoll_update(d->epoll_fd, d->link_fd, EPOLLIN, TAG_LINK, &d->link_in_epoll);
    }
}

/* ── Keepalive ──────────────────────────────────────────────────────────── */
static void tick_keepalive(daemon_t *d, int64_t now_ms)
{
    if (d->disconnected) return;
    if (d->last_rx_ms > 0 && (now_ms - d->last_rx_ms) > KA_TIMEOUT_MS) {
        close_link(d, "keepalive timeout");
        return;
    }
    if (d->link_up && txq_empty(&d->txq) &&
        d->last_tx_ms > 0 && (now_ms - d->last_tx_ms) >= KA_INTERVAL_MS) {
        enqueue_frame(d, F_PING, 0, NULL, 0);
    }
}

/* ── Timeout calculation ────────────────────────────────────────────────── */
static int next_timeout_ms(daemon_t *d, int64_t now_ms)
{
    int64_t deadline = now_ms + MAX_TICK_MS;

    if (d->disconnected) {
        if (d->reopen_at_ms < deadline) deadline = d->reopen_at_ms;
    } else if (d->link_up) {
        int64_t ka_tx = d->last_tx_ms + KA_INTERVAL_MS;
        int64_t ka_rx = d->last_rx_ms + KA_TIMEOUT_MS;
        if (ka_tx < deadline) deadline = ka_tx;
        if (d->last_rx_ms > 0 && ka_rx < deadline) deadline = ka_rx;
    }

    for (int i = 0; i < MAX_CHANNELS; i++) {
        if (!d->channels[i]) continue;
        int64_t dl = ch_deadline(d->channels[i], now_ms);
        if (dl > 0 && dl < deadline) deadline = dl;
    }

    int64_t rem = deadline - now_ms;
    if (rem <= 0) return 0;
    if (rem > MAX_TICK_MS) rem = MAX_TICK_MS;
    return (int)rem;
}

/* ── Main epoll dispatch ────────────────────────────────────────────────── */
static void dispatch_event(daemon_t *d, struct epoll_event *ev)
{
    uint64_t tag  = ev->data.u64;
    uint32_t kind = TAG_KIND(tag);
    uint32_t info = TAG_INFO(tag);

    switch (kind) {
    case 0x01:  /* link */
        handle_link_event(d, ev->events);
        break;
    case 0x02:  /* mcu uart */
    case 0x03:  /* pty master */
    case 0x04:  /* tcp server */
    case 0x05:  /* tcp conn */ {
        /* Find the channel and let it handle the event */
        int ch_id = (kind == 0x05) ? (int)((info >> 16) & 0xFF) : (int)info;
        channel_t *ch = d->channels[ch_id];
        if (ch) ch_handle_event(ch, kind, info, ev->events);
        break;
    }
    default:
        break;
    }
}

/* ── Run loop ───────────────────────────────────────────────────────────── */
static void daemon_run(daemon_t *d)
{
    log_ts("serialmux %s started", d->is_exporter ? "exporter" : "host");
    open_link(d);

    struct epoll_event evs[64];
    while (!g_stop) {
        int64_t now_ms = mono_now_ms();

        if (d->disconnected && now_ms >= d->reopen_at_ms)
            open_link(d);

        tick_keepalive(d, now_ms);

        for (int i = 0; i < MAX_CHANNELS; i++)
            if (d->channels[i]) ch_tick(d->channels[i], now_ms);

        int timeout_ms = next_timeout_ms(d, mono_now_ms());
        int n = epoll_wait(d->epoll_fd, evs, 64, timeout_ms);
        for (int i = 0; i < n; i++)
            dispatch_event(d, &evs[i]);
    }
    log_ts("Shutting down");
}

/* ── Signal handler ─────────────────────────────────────────────────────── */
static void sig_handler(int s) { (void)s; g_stop = 1; }

/* ── CLI helpers ────────────────────────────────────────────────────────── */
static const char *USAGE =
"usage: serialmux exporter|host [--usb VID:PID] [link_dev] channel_spec...\n"
"\n"
"MODES\n"
"  exporter   Runs on the MCU machine (e.g. K1). Reads UARTs, writes to link.\n"
"  host       Runs on the Klipper machine (e.g. Pi). Exposes PTYs to Klipper.\n"
"\n"
"LINK DEVICE (exactly one required)\n"
"  link_dev        Explicit device path, e.g. /dev/ttyGS0\n"
"  --usb VID:PID   Discover CDC ACM by USB ID, e.g. --usb 1d6b:0104\n"
"\n"
"CHANNEL SPECS\n"
"  mcu:<id>:<uart_device>:<baud>     exporter MCU channel\n"
"  mcu:<id>:<pty_symlink>[:<baud>]   host MCU channel\n"
"  tcp:<id>:<addr>:<port>            TCP tunnel channel\n"
"\n"
"EXAMPLES\n"
"  serialmux exporter --usb 1d6b:0104 mcu:0:/dev/ttyS7:230400 mcu:1:/dev/ttyS1:230400\n"
"  serialmux host /dev/ttyGS0 mcu:0:/tmp/klipper_mcu mcu:1:/tmp/klipper_toolhead\n";

static void die_usage(const char *msg)
{
    if (msg) fprintf(stderr, "error: %s\n\n", msg);
    fputs(USAGE, stderr);
    exit(1);
}

static int valid_hex_str(const char *s)
{
    if (!s || !*s) return 0;
    for (; *s; s++)
        if (!((*s >= '0' && *s <= '9') || (*s >= 'a' && *s <= 'f') ||
              (*s >= 'A' && *s <= 'F')))
            return 0;
    return 1;
}

static int parse_int(const char *s, int min, int max, const char *name)
{
    char *end;
    long v = strtol(s, &end, 10);
    if (*end || v < min || v > max) {
        fprintf(stderr, "error: %s must be %d-%d, got: %s\n", name, min, max, s);
        exit(1);
    }
    return (int)v;
}

static int valid_baud(int b) { return baud_to_speed(b) != B0; }

/* Build channel objects from spec strings */
static void build_channels(daemon_t *d, int is_exporter,
                            char **specs, int n_specs)
{
    for (int s = 0; s < n_specs; s++) {
        char spec[256];
        snprintf(spec, sizeof(spec), "%s", specs[s]);

        char *parts[6] = {NULL};
        int np = 0;
        char *tok = spec;
        char *p;
        while ((p = strchr(tok, ':')) && np < 5) {
            *p = '\0';
            parts[np++] = tok;
            tok = p + 1;
        }
        parts[np++] = tok;

        if (np < 3) die_usage("channel spec too short");

        const char *kind = parts[0];
        int cid = parse_int(parts[1], 0, 255, "channel id");
        if (d->channels[cid]) {
            fprintf(stderr, "error: duplicate channel id %d\n", cid);
            exit(1);
        }

        channel_t *ch = calloc(1, sizeof(channel_t));
        if (!ch) { perror("calloc"); exit(1); }

        if (strcmp(kind, "mcu") == 0) {
            if (is_exporter) {
                if (np != 4) die_usage("exporter mcu spec: mcu:<id>:<dev>:<baud>");
                int baud = parse_int(parts[3], 1, 999999, "baud");
                if (!valid_baud(baud)) {
                    fprintf(stderr, "error: unsupported baud %d\n", baud);
                    exit(1);
                }
                mcu_ch_init(ch, cid, parts[2], baud, d);
            } else {
                if (np < 3 || np > 4) die_usage("host mcu spec: mcu:<id>:<symlink>[:<baud>]");
                int baud = (np == 4) ? parse_int(parts[3], 1, 999999, "baud") : 230400;
                if (!valid_baud(baud)) {
                    fprintf(stderr, "error: unsupported baud %d\n", baud);
                    exit(1);
                }
                pty_ch_init(ch, cid, parts[2], baud, d);
            }
        } else if (strcmp(kind, "tcp") == 0) {
            if (np != 4) die_usage("tcp spec: tcp:<id>:<addr>:<port>");
            int port = parse_int(parts[3], 1, 65535, "port");
            if (is_exporter)
                tcp_src_init(ch, cid, parts[2], port, d);
            else
                tcp_dst_init(ch, cid, parts[2], port, d);
        } else {
            fprintf(stderr, "error: unknown channel type '%s'\n", kind);
            exit(1);
        }

        d->channels[cid] = ch;
    }
}

/* ── main ───────────────────────────────────────────────────────────────── */
int main(int argc, char **argv)
{
    crc32_init();

    if (argc < 3) die_usage(NULL);

    int argi = 1;
    const char *mode = argv[argi++];
    int is_exporter;
    if (strcmp(mode, "exporter") == 0)     is_exporter = 1;
    else if (strcmp(mode, "host") == 0)    is_exporter = 0;
    else die_usage("mode must be 'exporter' or 'host'");

    char usb_vid[8] = {0}, usb_pid[8] = {0};
    char link_dev[128] = {0};

    /* Scan for --usb and link_dev before channel specs */
    while (argi < argc && argv[argi][0] != '\0') {
        if (strcmp(argv[argi], "--usb") == 0) {
            argi++;
            if (argi >= argc) die_usage("--usb requires VID:PID argument");
            char *colon = strchr(argv[argi], ':');
            if (!colon) die_usage("--usb VID:PID must contain a colon");
            size_t vlen = (size_t)(colon - argv[argi]);
            char vid[8], pid[8];
            if (vlen == 0 || vlen >= sizeof(vid)) die_usage("invalid VID");
            memcpy(vid, argv[argi], vlen); vid[vlen] = '\0';
            snprintf(pid, sizeof(pid), "%s", colon + 1);
            if (!valid_hex_str(vid) || !valid_hex_str(pid))
                die_usage("VID and PID must be hex strings");
            snprintf(usb_vid, sizeof(usb_vid), "%s", vid);
            snprintf(usb_pid, sizeof(usb_pid), "%s", pid);
            argi++;
            continue;
        }
        if (argv[argi][0] == '/' && link_dev[0] == '\0') {
            snprintf(link_dev, sizeof(link_dev), "%s", argv[argi]);
            argi++;
            continue;
        }
        /* First non-flag, non-path arg: start of channel specs */
        break;
    }

    if (!usb_vid[0] && !link_dev[0])
        die_usage("one of link_dev or --usb is required");
    if (usb_vid[0] && link_dev[0])
        die_usage("link_dev and --usb are mutually exclusive");

    if (argi >= argc) die_usage("at least one channel spec is required");

    char **specs   = argv + argi;
    int    n_specs = argc - argi;

    int epfd = epoll_create1(EPOLL_CLOEXEC);
    if (epfd < 0) { perror("epoll_create1"); return 1; }

    daemon_t *d = calloc(1, sizeof(daemon_t));
    if (!d) { perror("calloc"); return 1; }
    d->epoll_fd    = epfd;
    d->link_fd     = -1;
    d->disconnected = 1;
    d->is_exporter = is_exporter;
    memcpy(d->usb_vid, usb_vid, sizeof(usb_vid));
    memcpy(d->usb_pid, usb_pid, sizeof(usb_pid));
    if (link_dev[0])
        memcpy(d->link_dev, link_dev, sizeof(link_dev));

    build_channels(d, is_exporter, specs, n_specs);

    struct sigaction sa = { .sa_handler = sig_handler };
    sigaction(SIGINT, &sa, NULL);
    sigaction(SIGTERM, &sa, NULL);
    signal(SIGPIPE, SIG_IGN);

    daemon_run(d);
    return 0;
}
