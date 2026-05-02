#pragma once
#include <stdint.h>
#include <stddef.h>
#include <termios.h>

/* ── Frame protocol ─────────────────────────────────────────────────────── */
#define HDR_SIZE        6       /* magic(2) + type(1) + channel(1) + length(2) */
#define CRC_SIZE        4
#define MAX_PAYLOAD     (16 * 1024)
#define MAX_FRAME       (HDR_SIZE + MAX_PAYLOAD + CRC_SIZE)

#define F_DATA          0x01
#define F_FLUSH         0x02
#define F_READY         0x03
#define F_HELLO         0x05
#define F_ACK           0x06
#define F_TCONN         0x10
#define F_TDATA         0x11
#define F_TCLOSE        0x12
#define F_PING          0x20
#define F_PONG          0x21

/* ── Timing / backpressure ──────────────────────────────────────────────── */
#define MAX_TICK_MS     1000
#define RESET_SILENCE_MS 5000
#define KLIPPER_SYNC    0x7E
#define KA_INTERVAL_MS  3000
#define KA_TIMEOUT_MS   10000

#define LINK_HIGH_WATER (512 * 1024)
#define LINK_LOW_WATER  (256 * 1024)
#define CONN_HIGH_WATER (256 * 1024)

/* ── CRC32 (zlib/reflected poly 0xEDB88320, no external dep) ───────────── */
/* Built at runtime by crc32_init() — call once from main().               */
extern uint32_t g_crc32_tbl[256];
void crc32_init(void);

static inline uint32_t crc32_buf(const uint8_t *data, size_t len)
{
    uint32_t crc = 0xFFFFFFFF;
    for (size_t i = 0; i < len; i++)
        crc = g_crc32_tbl[(crc ^ data[i]) & 0xFF] ^ (crc >> 8);
    return crc ^ 0xFFFFFFFF;
}

/* ── FrameParser ────────────────────────────────────────────────────────── */
struct daemon;

typedef void (*frame_cb_t)(struct daemon *d, uint8_t ftype, uint8_t channel,
                            const uint8_t *payload, uint16_t plen);

typedef struct {
    uint8_t    buf[MAX_FRAME + 64];
    size_t     len;
    frame_cb_t on_frame;
} frame_parser_t;

void fp_feed(frame_parser_t *fp, struct daemon *d, const uint8_t *data, size_t n);

/* ── LinkTxQueue (ring buffer) ──────────────────────────────────────────── */
#define LINK_TXBUF_SIZE (LINK_HIGH_WATER + MAX_FRAME + 64)

typedef struct {
    uint8_t  buf[LINK_TXBUF_SIZE];
    size_t   head;
    size_t   tail;
} link_txq_t;

static inline size_t txq_used(const link_txq_t *q)
{
    return (q->tail + LINK_TXBUF_SIZE - q->head) % LINK_TXBUF_SIZE;
}
static inline int txq_empty(const link_txq_t *q) { return q->head == q->tail; }

int txq_push(link_txq_t *q, const uint8_t *data, size_t n);
int txq_drain(link_txq_t *q, int fd);  /* returns 1 if now empty */

/* ── Channel types ──────────────────────────────────────────────────────── */
typedef enum { CH_MCU = 0, CH_PTY, CH_TCP_SRC, CH_TCP_DST } ch_type_t;

typedef struct { ch_type_t type; int id; struct daemon *d; } ch_base_t;

/* MCU state enum */
typedef enum { MCU_INIT = 0, MCU_ACTIVE, MCU_RESETTING } mcu_state_t;

typedef struct mcu_channel {
    ch_base_t   base;
    char        dev[64];
    int         baud;
    int         fd;
    int         in_epoll;
    mcu_state_t state;
    int         link_up;
    int         bp_paused;
    int64_t     last_rx_ms;
    int64_t     reopen_at_ms;
    uint8_t     txbuf[MAX_PAYLOAD * 2];
    size_t      txbuf_len;
} mcu_ch_t;

typedef struct pty_channel {
    ch_base_t  base;
    char       symlink[128];
    int        baud;
    int        master_fd;
    int        slave_fd;
    int        in_epoll;
    int        bp_paused;
    uint8_t    txbuf[MAX_PAYLOAD * 2];
    size_t     txbuf_len;
} pty_ch_t;

#define MAX_TCP_CONNS 256

typedef struct {
    int      fd;
    int      in_epoll;
    int      connecting;
    int      close_pending; /* drain txbuf then close, no further reads */
    uint8_t *txbuf;         /* malloc on open, NULL = slot empty */
    size_t   txbuf_len;
} tcp_conn_t;

typedef struct tcp_src_ch {
    ch_base_t   base;
    int         server_fd;
    int         server_in_epoll;
    int         link_up;
    int         bp_paused;
    uint16_t    next_cid;
    tcp_conn_t  conns[MAX_TCP_CONNS];
} tcp_src_t;

typedef struct tcp_dst_ch {
    ch_base_t   base;
    char        dest_addr[64];
    int         dest_port;
    int         bp_paused;
    tcp_conn_t  conns[MAX_TCP_CONNS];
} tcp_dst_t;

typedef union {
    ch_base_t  base;
    mcu_ch_t   mcu;
    pty_ch_t   pty;
    tcp_src_t  src;
    tcp_dst_t  dst;
} channel_t;

void    ch_tick(channel_t *ch, int64_t now_ms);
void    ch_on_frame(channel_t *ch, uint8_t ftype,
                    const uint8_t *payload, uint16_t plen);
void    ch_on_link_up(channel_t *ch);
void    ch_on_link_down(channel_t *ch);
void    ch_pause(channel_t *ch);
void    ch_resume(channel_t *ch);
int64_t ch_deadline(channel_t *ch, int64_t now_ms);
void   ch_close(channel_t *ch);
/* kind: 0x02=mcu 0x03=pty 0x04=server 0x05=tcpconn; info from epoll tag */
void   ch_handle_event(channel_t *ch, uint32_t kind, uint32_t info, uint32_t events);

/* Channel constructors (defined in channels.c) */
void mcu_ch_init(channel_t *ch, int id, const char *dev, int baud, struct daemon *d);
void pty_ch_init(channel_t *ch, int id, const char *symlink, int baud, struct daemon *d);
void tcp_src_init(channel_t *ch, int id, const char *addr, int port, struct daemon *d);
void tcp_dst_init(channel_t *ch, int id, const char *addr, int port, struct daemon *d);

/* ── Daemon ─────────────────────────────────────────────────────────────── */
#define MAX_CHANNELS 256

typedef struct daemon {
    int            epoll_fd;
    int            link_fd;
    int            link_in_epoll;
    int            link_up;
    int            disconnected;
    int            link_bp_paused;
    int            is_exporter;
    char           link_dev[128];
    char           usb_vid[8];
    char           usb_pid[8];
    int64_t        last_rx_ms;
    int64_t        last_tx_ms;
    int64_t        reopen_at_ms;
    link_txq_t     txq;
    frame_parser_t parser;
    channel_t     *channels[MAX_CHANNELS];
} daemon_t;

void daemon_send(daemon_t *d, uint8_t ftype, uint8_t channel,
                 const uint8_t *payload, uint16_t plen);

/* ── epoll tag encoding ─────────────────────────────────────────────────── */
#define TAG_LINK            0x0100000000ULL
#define TAG_MCU(id)         (0x0200000000ULL | (uint32_t)(id))
#define TAG_PTY(id)         (0x0300000000ULL | (uint32_t)(id))
#define TAG_SERVER(id)      (0x0400000000ULL | (uint32_t)(id))
#define TAG_TCPCONN(ch,cid) (0x0500000000ULL | ((uint32_t)(ch) << 16) | (uint32_t)(cid))
#define TAG_KIND(tag)       ((uint32_t)((tag) >> 32))
#define TAG_INFO(tag)       ((uint32_t)((tag) & 0xFFFFFFFF))

void epoll_update(int epfd, int fd, uint32_t events, uint64_t tag, int *in_epoll);

/* ── Utilities ──────────────────────────────────────────────────────────── */
int64_t  mono_now_ms(void);
speed_t  baud_to_speed(int baud);
void   log_ts(const char *fmt, ...) __attribute__((format(printf,1,2)));
int    set_nonblock(int fd);
int    open_serial_fd(const char *dev, int baud);
char  *find_acm_by_usb_id(const char *vid, const char *pid, char *out, size_t outsz);
