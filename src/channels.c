#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
#include <fcntl.h>
#include <pty.h>
#include <sys/epoll.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <arpa/inet.h>

#include "serialmux.h"

/* ── MCU Channel ────────────────────────────────────────────────────────── */

static void mcu_update_interest(mcu_ch_t *m)
{
    if (m->fd < 0) return;
    uint32_t ev = m->bp_paused ? 0 : EPOLLIN;
    if (m->txbuf_len) ev |= EPOLLOUT;
    epoll_update(m->base.d->epoll_fd, m->fd, ev,
                 TAG_MCU(m->base.id), &m->in_epoll);
}

static void mcu_transition(mcu_ch_t *m, mcu_state_t new_state)
{
    if (new_state == m->state) return;
    const char *names[] = {"INIT", "ACTIVE", "RESETTING"};
    log_ts("MCU ch%d: %s -> %s", m->base.id,
           names[m->state], names[new_state]);
    m->state = new_state;
    if (new_state == MCU_RESETTING) {
        m->last_rx_ms = 0;
        m->txbuf_len = 0;
        mcu_update_interest(m);
        if (m->link_up)
            daemon_send(m->base.d, F_FLUSH, (uint8_t)m->base.id, NULL, 0);
    } else if (new_state == MCU_ACTIVE) {
        if (m->link_up)
            daemon_send(m->base.d, F_READY, (uint8_t)m->base.id, NULL, 0);
    }
}

static void mcu_open_uart(mcu_ch_t *m)
{
    int fd = open_serial_fd(m->dev, m->baud);
    if (fd < 0) {
        log_ts("MCU ch%d: cannot open %s: %s -- retry in 2s",
               m->base.id, m->dev, strerror(errno));
        m->reopen_at_ms = mono_now_ms() + 2000;
        return;
    }
    m->fd = fd;
    m->in_epoll = 0;
    mcu_update_interest(m);
    log_ts("MCU ch%d: opened %s @ %d", m->base.id, m->dev, m->baud);
}

static void mcu_close_uart(mcu_ch_t *m)
{
    if (m->fd < 0) return;
    epoll_update(m->base.d->epoll_fd, m->fd, 0, 0, &m->in_epoll);
    close(m->fd);
    m->fd = -1;
}

static void mcu_uart_read(mcu_ch_t *m)
{
    uint8_t buf[4096];
    ssize_t n = read(m->fd, buf, sizeof(buf));
    if (n < 0) {
        if (errno == EAGAIN || errno == EWOULDBLOCK) return;
        log_ts("MCU ch%d: UART read error: %s", m->base.id, strerror(errno));
        mcu_close_uart(m);
        mcu_transition(m, MCU_RESETTING);
        m->reopen_at_ms = mono_now_ms() + 1000;
        return;
    }
    if (n == 0) return;

    m->last_rx_ms = mono_now_ms();

    if (m->state == MCU_INIT || m->state == MCU_RESETTING) {
        /* Look for Klipper sync byte 0x7E */
        for (ssize_t i = 0; i < n; i++) {
            if (buf[i] == KLIPPER_SYNC) {
                log_ts("MCU ch%d: 0x7E seen at offset %zd -> ACTIVE",
                       m->base.id, i);
                mcu_transition(m, MCU_ACTIVE);
                if (m->link_up && n - i > 0)
                    daemon_send(m->base.d, F_DATA, (uint8_t)m->base.id,
                                buf + i, (uint16_t)(n - i));
                return;
            }
        }
        /* bootloader noise -- discard */
    } else {
        /* ACTIVE: forward all data */
        if (m->link_up)
            daemon_send(m->base.d, F_DATA, (uint8_t)m->base.id,
                        buf, (uint16_t)n);
    }
}

static void mcu_uart_drain(mcu_ch_t *m)
{
    if (!m->txbuf_len || m->fd < 0) return;
    ssize_t n = write(m->fd, m->txbuf, m->txbuf_len);
    if (n < 0) {
        if (errno == EAGAIN || errno == EWOULDBLOCK) return;
        log_ts("MCU ch%d: UART write error: %s", m->base.id, strerror(errno));
        m->txbuf_len = 0;
    } else if (n > 0) {
        m->txbuf_len -= (size_t)n;
        if (m->txbuf_len)
            memmove(m->txbuf, m->txbuf + n, m->txbuf_len);
    }
    mcu_update_interest(m);
}

void mcu_ch_init(channel_t *ch, int id, const char *dev, int baud, daemon_t *d)
{
    mcu_ch_t *m = &ch->mcu;
    memset(m, 0, sizeof(*m));
    m->base.type = CH_MCU;
    m->base.id   = id;
    m->base.d    = d;
    m->baud      = baud;
    m->fd        = -1;
    snprintf(m->dev, sizeof(m->dev), "%s", dev);
    mcu_open_uart(m);
}

/* ── PTY Channel ────────────────────────────────────────────────────────── */

static void pty_update_interest(pty_ch_t *p)
{
    if (p->master_fd < 0) return;
    uint32_t ev = p->bp_paused ? 0 : EPOLLIN;
    if (p->txbuf_len) ev |= EPOLLOUT;
    epoll_update(p->base.d->epoll_fd, p->master_fd, ev,
                 TAG_PTY(p->base.id), &p->in_epoll);
}

static void pty_remove_symlink(pty_ch_t *p)
{
    unlink(p->symlink);
}

static void pty_open(pty_ch_t *p)
{
    if (p->master_fd >= 0) return;

    int master_fd, slave_fd;
    if (openpty(&master_fd, &slave_fd, NULL, NULL, NULL) < 0) {
        log_ts("PTY ch%d: openpty failed: %s", p->base.id, strerror(errno));
        return;
    }

    /* Configure slave: raw mode, set baud */
    struct termios t;
    tcgetattr(slave_fd, &t);
    t.c_iflag = 0; t.c_oflag = 0; t.c_lflag = 0;
    t.c_cflag = CS8 | CREAD | CLOCAL;
    t.c_cc[VMIN] = 0; t.c_cc[VTIME] = 0;
    speed_t sp = baud_to_speed(p->baud);
    if (sp == B0) sp = B230400;
    cfsetispeed(&t, sp);
    cfsetospeed(&t, sp);
    tcsetattr(slave_fd, TCSANOW, &t);

    set_nonblock(master_fd);

    pty_remove_symlink(p);
    char *sname = ttyname(slave_fd);
    char slave_path[64];
    snprintf(slave_path, sizeof(slave_path), "%s", sname ? sname : "");

    if (symlink(slave_path, p->symlink) < 0) {
        log_ts("PTY ch%d: symlink %s -> %s failed: %s",
               p->base.id, p->symlink, slave_path, strerror(errno));
        close(master_fd); close(slave_fd);
        return;
    }

    p->master_fd = master_fd;
    p->slave_fd  = slave_fd;
    p->in_epoll  = 0;
    pty_update_interest(p);
    log_ts("PTY ch%d: opened %s -> %s", p->base.id, slave_path, p->symlink);
}

static void pty_close(pty_ch_t *p)
{
    if (p->master_fd < 0) return;
    epoll_update(p->base.d->epoll_fd, p->master_fd, 0, 0, &p->in_epoll);
    close(p->master_fd);
    close(p->slave_fd);
    p->master_fd = -1;
    p->slave_fd  = -1;
    p->txbuf_len = 0;
    pty_remove_symlink(p);
    log_ts("PTY ch%d: closed", p->base.id);
}

static void pty_master_read(pty_ch_t *p)
{
    uint8_t buf[4096];
    ssize_t n = read(p->master_fd, buf, sizeof(buf));
    if (n < 0) {
        if (errno == EAGAIN || errno == EWOULDBLOCK) return;
        if (errno == EIO) {
            /* All slave fds closed -- Klipper disconnected */
            pty_close(p);
        }
        return;
    }
    if (n == 0) return;
    daemon_send(p->base.d, F_DATA, (uint8_t)p->base.id, buf, (uint16_t)n);
}

static void pty_master_drain(pty_ch_t *p)
{
    if (!p->txbuf_len || p->master_fd < 0) return;
    ssize_t n = write(p->master_fd, p->txbuf, p->txbuf_len);
    if (n < 0) {
        if (errno == EAGAIN || errno == EWOULDBLOCK) return;
        if (errno == EIO) { pty_close(p); return; }
        log_ts("PTY ch%d: write error: %s", p->base.id, strerror(errno));
        p->txbuf_len = 0;
    } else if (n > 0) {
        p->txbuf_len -= (size_t)n;
        if (p->txbuf_len)
            memmove(p->txbuf, p->txbuf + n, p->txbuf_len);
    }
    pty_update_interest(p);
}

void pty_ch_init(channel_t *ch, int id, const char *symlink, int baud, daemon_t *d)
{
    pty_ch_t *p = &ch->pty;
    memset(p, 0, sizeof(*p));
    p->base.type = CH_PTY;
    p->base.id   = id;
    p->base.d    = d;
    p->baud      = baud;
    p->master_fd = -1;
    p->slave_fd  = -1;
    snprintf(p->symlink, sizeof(p->symlink), "%s", symlink);
    pty_remove_symlink(p);
}

/* ── TCP helpers ────────────────────────────────────────────────────────── */

static void tcp_conn_update(tcp_conn_t *c, int epfd,
                             uint64_t tag, int bp_paused)
{
    if (c->fd < 0) return;
    uint32_t ev = bp_paused ? 0 : EPOLLIN;
    if (c->txbuf_len || c->connecting) ev |= EPOLLOUT;
    epoll_update(epfd, c->fd, ev, tag, &c->in_epoll);
}

static void tcp_conn_close_fd(tcp_conn_t *c, int epfd)
{
    if (c->fd < 0) return;
    epoll_update(epfd, c->fd, 0, 0, &c->in_epoll);
    close(c->fd);
    c->fd = -1;
    if (c->txbuf) { free(c->txbuf); c->txbuf = NULL; }
    c->txbuf_len = 0;
    c->connecting = 0;
}

/* ── TcpSourceChannel ───────────────────────────────────────────────────── */

static void src_close_cid(tcp_src_t *s, int cid, int notify)
{
    tcp_conn_t *c = &s->conns[cid];
    if (c->fd < 0) return;
    tcp_conn_close_fd(c, s->base.d->epoll_fd);
    if (notify && s->link_up)
        daemon_send(s->base.d, F_TCLOSE, (uint8_t)s->base.id,
                    (uint8_t[]){(uint8_t)(cid & 0xFF), (uint8_t)(cid >> 8)}, 2);
    log_ts("TCP src ch%d: closed cid=%d", s->base.id, cid);
}

static int src_alloc_cid(tcp_src_t *s)
{
    for (int i = 0; i < MAX_TCP_CONNS; i++) {
        int cid = (s->next_cid + i) % MAX_TCP_CONNS;
        if (s->conns[cid].fd < 0) {
            s->next_cid = (uint16_t)((cid + 1) % MAX_TCP_CONNS);
            return cid;
        }
    }
    return -1;
}

static void src_on_accept(tcp_src_t *s)
{
    struct sockaddr_in addr;
    socklen_t addrlen = sizeof(addr);
    int fd = accept(s->server_fd, (struct sockaddr *)&addr, &addrlen);
    if (fd < 0) return;

    if (!s->link_up) { close(fd); return; }

    int cid = src_alloc_cid(s);
    if (cid < 0) {
        log_ts("TCP src ch%d: conn_id pool exhausted", s->base.id);
        close(fd);
        return;
    }

    set_nonblock(fd);
    int one = 1;
    setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof(one));

    tcp_conn_t *c = &s->conns[cid];
    c->fd         = fd;
    c->in_epoll   = 0;
    c->connecting = 0;
    c->txbuf      = malloc(CONN_HIGH_WATER + MAX_PAYLOAD);
    c->txbuf_len  = 0;
    if (!c->txbuf) { close(fd); c->fd = -1; return; }

    uint8_t cid_b[2] = {(uint8_t)(cid & 0xFF), (uint8_t)(cid >> 8)};
    daemon_send(s->base.d, F_TCONN, (uint8_t)s->base.id, cid_b, 2);
    log_ts("TCP src ch%d: accepted %s:%d cid=%d",
           s->base.id, inet_ntoa(addr.sin_addr), ntohs(addr.sin_port), cid);

    tcp_conn_update(c, s->base.d->epoll_fd,
                    TAG_TCPCONN(s->base.id, cid), s->bp_paused);
}

static void src_on_conn_event(tcp_src_t *s, int cid, uint32_t events)
{
    tcp_conn_t *c = &s->conns[cid];
    if (c->fd < 0) return;

    if (events & EPOLLIN) {
        uint8_t buf[65536];
        ssize_t n = recv(c->fd, buf, sizeof(buf), 0);
        if (n < 0) {
            if (errno != EAGAIN && errno != EWOULDBLOCK)
                src_close_cid(s, cid, 1);
            return;
        }
        if (n == 0) { src_close_cid(s, cid, 1); return; }

        /* Chunk into MAX_PAYLOAD-2 byte frames (2 bytes reserved for cid prefix) */
        ssize_t off = 0;
        while (off < n) {
            size_t chunk = (size_t)(n - off);
            if (chunk > MAX_PAYLOAD - 2) chunk = MAX_PAYLOAD - 2;
            uint8_t frame[MAX_PAYLOAD];
            frame[0] = (uint8_t)(cid & 0xFF);
            frame[1] = (uint8_t)(cid >> 8);
            memcpy(frame + 2, buf + off, chunk);
            daemon_send(s->base.d, F_TDATA, (uint8_t)s->base.id,
                        frame, (uint16_t)(chunk + 2));
            off += (ssize_t)chunk;
        }
    }

    if (events & EPOLLOUT) {
        if (c->txbuf_len) {
            ssize_t n = send(c->fd, c->txbuf, c->txbuf_len, MSG_NOSIGNAL);
            if (n < 0) {
                if (errno != EAGAIN && errno != EWOULDBLOCK)
                    src_close_cid(s, cid, 1);
                return;
            }
            c->txbuf_len -= (size_t)n;
            if (c->txbuf_len)
                memmove(c->txbuf, c->txbuf + n, c->txbuf_len);
        }
        tcp_conn_update(c, s->base.d->epoll_fd,
                        TAG_TCPCONN(s->base.id, cid), s->bp_paused);
    }
}

void tcp_src_init(channel_t *ch, int id, const char *addr, int port, daemon_t *d)
{
    tcp_src_t *s = &ch->src;
    memset(s, 0, sizeof(*s));
    s->base.type = CH_TCP_SRC;
    s->base.id   = id;
    s->base.d    = d;
    for (int i = 0; i < MAX_TCP_CONNS; i++) s->conns[i].fd = -1;

    int fd = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    if (fd < 0) { perror("socket"); exit(1); }
    int one = 1;
    setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));

    struct sockaddr_in sin = {
        .sin_family = AF_INET,
        .sin_port   = htons((uint16_t)port),
    };
    sin.sin_addr.s_addr = inet_addr(addr);
    if (bind(fd, (struct sockaddr *)&sin, sizeof(sin)) < 0) {
        log_ts("TCP src ch%d: bind %s:%d failed: %s", id, addr, port, strerror(errno));
        exit(1);
    }
    if (listen(fd, 32) < 0) {
        log_ts("TCP src ch%d: listen failed: %s", id, strerror(errno));
        exit(1);
    }
    s->server_fd = fd;
    s->server_in_epoll = 0;
    epoll_update(d->epoll_fd, fd, EPOLLIN, TAG_SERVER(id), &s->server_in_epoll);
    log_ts("TCP src ch%d: listening %s:%d", id, addr, port);
}

/* ── TcpDestChannel ─────────────────────────────────────────────────────── */

static void dst_close_cid(tcp_dst_t *dst, int cid, int notify)
{
    tcp_conn_t *c = &dst->conns[cid];
    if (c->fd < 0) return;
    tcp_conn_close_fd(c, dst->base.d->epoll_fd);
    if (notify) {
        uint8_t cid_b[2] = {(uint8_t)(cid & 0xFF), (uint8_t)(cid >> 8)};
        daemon_send(dst->base.d, F_TCLOSE, (uint8_t)dst->base.id, cid_b, 2);
    }
    log_ts("TCP dst ch%d: closed cid=%d", dst->base.id, cid);
}

static void dst_open_conn(tcp_dst_t *dst, int cid)
{
    int fd = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    if (fd < 0) {
        log_ts("TCP dst ch%d: socket: %s", dst->base.id, strerror(errno));
        uint8_t cid_b[2] = {(uint8_t)(cid & 0xFF), (uint8_t)(cid >> 8)};
        daemon_send(dst->base.d, F_TCLOSE, (uint8_t)dst->base.id, cid_b, 2);
        return;
    }
    int one = 1;
    setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof(one));

    struct sockaddr_in sin = {
        .sin_family = AF_INET,
        .sin_port   = htons((uint16_t)dst->dest_port),
    };
    sin.sin_addr.s_addr = inet_addr(dst->dest_addr);

    int err = connect(fd, (struct sockaddr *)&sin, sizeof(sin));
    int connecting;
    if (err == 0) {
        connecting = 0;  /* immediate success (loopback) */
    } else if (errno == EINPROGRESS || errno == EALREADY || errno == EWOULDBLOCK) {
        connecting = 1;
    } else {
        log_ts("TCP dst ch%d: connect cid=%d failed: %s",
               dst->base.id, cid, strerror(errno));
        close(fd);
        uint8_t cid_b[2] = {(uint8_t)(cid & 0xFF), (uint8_t)(cid >> 8)};
        daemon_send(dst->base.d, F_TCLOSE, (uint8_t)dst->base.id, cid_b, 2);
        return;
    }

    tcp_conn_t *c = &dst->conns[cid];
    c->fd         = fd;
    c->in_epoll   = 0;
    c->connecting = connecting;
    c->txbuf      = malloc(CONN_HIGH_WATER + MAX_PAYLOAD);
    c->txbuf_len  = 0;
    if (!c->txbuf) {
        close(fd);
        dst_close_cid(dst, cid, 1);
        return;
    }
    tcp_conn_update(c, dst->base.d->epoll_fd,
                    TAG_TCPCONN(dst->base.id, cid), dst->bp_paused);
    log_ts("TCP dst ch%d: connecting cid=%d -> %s:%d",
           dst->base.id, cid, dst->dest_addr, dst->dest_port);
}

static void dst_on_conn_event(tcp_dst_t *dst, int cid, uint32_t events)
{
    tcp_conn_t *c = &dst->conns[cid];
    if (c->fd < 0) return;

    /* Finish async connect */
    if (c->connecting && (events & EPOLLOUT)) {
        int err = 0;
        socklen_t elen = sizeof(err);
        getsockopt(c->fd, SOL_SOCKET, SO_ERROR, &err, &elen);
        if (err) {
            log_ts("TCP dst ch%d: connect failed cid=%d: %s",
                   dst->base.id, cid, strerror(err));
            dst_close_cid(dst, cid, 1);
            return;
        }
        c->connecting = 0;
        log_ts("TCP dst ch%d: connected cid=%d", dst->base.id, cid);
    }

    if ((events & EPOLLOUT) && !c->connecting) {
        if (c->txbuf_len) {
            ssize_t n = send(c->fd, c->txbuf, c->txbuf_len, MSG_NOSIGNAL);
            if (n < 0) {
                if (errno != EAGAIN && errno != EWOULDBLOCK) {
                    dst_close_cid(dst, cid, 1);
                    return;
                }
            } else if (n > 0) {
                c->txbuf_len -= (size_t)n;
                if (c->txbuf_len)
                    memmove(c->txbuf, c->txbuf + n, c->txbuf_len);
            }
        }
        tcp_conn_update(c, dst->base.d->epoll_fd,
                        TAG_TCPCONN(dst->base.id, cid), dst->bp_paused);
    }

    if (events & EPOLLIN) {
        uint8_t buf[65536];
        ssize_t n = recv(c->fd, buf, sizeof(buf), 0);
        if (n < 0) {
            if (errno != EAGAIN && errno != EWOULDBLOCK)
                dst_close_cid(dst, cid, 1);
            return;
        }
        if (n == 0) { dst_close_cid(dst, cid, 1); return; }

        ssize_t off = 0;
        while (off < n) {
            size_t chunk = (size_t)(n - off);
            if (chunk > MAX_PAYLOAD - 2) chunk = MAX_PAYLOAD - 2;
            uint8_t frame[MAX_PAYLOAD];
            frame[0] = (uint8_t)(cid & 0xFF);
            frame[1] = (uint8_t)(cid >> 8);
            memcpy(frame + 2, buf + off, chunk);
            daemon_send(dst->base.d, F_TDATA, (uint8_t)dst->base.id,
                        frame, (uint16_t)(chunk + 2));
            off += (ssize_t)chunk;
        }
    }
}

void tcp_dst_init(channel_t *ch, int id, const char *addr, int port, daemon_t *d)
{
    tcp_dst_t *dst = &ch->dst;
    memset(dst, 0, sizeof(*dst));
    dst->base.type = CH_TCP_DST;
    dst->base.id   = id;
    dst->base.d    = d;
    dst->dest_port = port;
    snprintf(dst->dest_addr, sizeof(dst->dest_addr), "%s", addr);
    for (int i = 0; i < MAX_TCP_CONNS; i++) dst->conns[i].fd = -1;
}

/* ── Generic channel dispatch ───────────────────────────────────────────── */

void ch_tick(channel_t *ch, int64_t now_ms)
{
    if (ch->base.type == CH_MCU) {
        mcu_ch_t *m = &ch->mcu;
        if (m->fd < 0 && now_ms >= m->reopen_at_ms)
            mcu_open_uart(m);
        else if (m->link_up && m->state == MCU_ACTIVE && m->last_rx_ms > 0 &&
                 (now_ms - m->last_rx_ms) > RESET_SILENCE_MS)
            mcu_transition(m, MCU_RESETTING);
    }
    /* PTY and TCP channels have no periodic timers */
}

void ch_on_frame(channel_t *ch, uint8_t ftype, const uint8_t *payload, uint16_t plen)
{
    switch (ch->base.type) {
    case CH_MCU: {
        mcu_ch_t *m = &ch->mcu;
        if (ftype != F_DATA || m->fd < 0 || m->state != MCU_ACTIVE) return;
        size_t space = sizeof(m->txbuf) - m->txbuf_len;
        size_t copy  = plen < space ? plen : space;
        memcpy(m->txbuf + m->txbuf_len, payload, copy);
        m->txbuf_len += copy;
        mcu_uart_drain(m);
        break;
    }
    case CH_PTY: {
        pty_ch_t *p = &ch->pty;
        if (ftype == F_FLUSH) {
            log_ts("PTY ch%d: FLUSH -> WAITING", p->base.id);
            pty_close(p);
        } else if (ftype == F_READY) {
            log_ts("PTY ch%d: READY -> ACTIVE", p->base.id);
            pty_open(p);
        } else if (ftype == F_DATA && p->master_fd >= 0) {
            size_t space = sizeof(p->txbuf) - p->txbuf_len;
            size_t copy  = plen < space ? plen : space;
            memcpy(p->txbuf + p->txbuf_len, payload, copy);
            p->txbuf_len += copy;
            pty_master_drain(p);
        }
        break;
    }
    case CH_TCP_SRC: {
        tcp_src_t *s = &ch->src;
        if (plen < 2) return;
        int cid = payload[0] | (payload[1] << 8);
        if (cid >= MAX_TCP_CONNS) return;
        const uint8_t *data = payload + 2;
        uint16_t dlen = plen - 2;
        if (ftype == F_TDATA) {
            tcp_conn_t *c = &s->conns[cid];
            if (c->fd < 0 || !dlen) return;
            size_t avail = CONN_HIGH_WATER + MAX_PAYLOAD - c->txbuf_len;
            if (dlen > avail || c->txbuf_len > CONN_HIGH_WATER) {
                log_ts("TCP src ch%d: cid=%d high-water -- closing", s->base.id, cid);
                src_close_cid(s, cid, 1);
                return;
            }
            memcpy(c->txbuf + c->txbuf_len, data, dlen);
            c->txbuf_len += dlen;
            tcp_conn_update(c, s->base.d->epoll_fd,
                            TAG_TCPCONN(s->base.id, cid), s->bp_paused);
        } else if (ftype == F_TCLOSE) {
            src_close_cid(s, cid, 0);
        }
        break;
    }
    case CH_TCP_DST: {
        tcp_dst_t *dst = &ch->dst;
        if (plen < 2) return;
        int cid = payload[0] | (payload[1] << 8);
        if (cid >= MAX_TCP_CONNS) return;
        const uint8_t *data = payload + 2;
        uint16_t dlen = plen - 2;
        if (ftype == F_TCONN) {
            if (dst->conns[cid].fd < 0)
                dst_open_conn(dst, cid);
        } else if (ftype == F_TDATA) {
            tcp_conn_t *c = &dst->conns[cid];
            if (c->fd < 0 || !dlen) return;
            size_t avail = CONN_HIGH_WATER + MAX_PAYLOAD - c->txbuf_len;
            if (dlen > avail || c->txbuf_len > CONN_HIGH_WATER) {
                log_ts("TCP dst ch%d: cid=%d high-water -- closing", dst->base.id, cid);
                dst_close_cid(dst, cid, 1);
                return;
            }
            memcpy(c->txbuf + c->txbuf_len, data, dlen);
            c->txbuf_len += dlen;
            tcp_conn_update(c, dst->base.d->epoll_fd,
                            TAG_TCPCONN(dst->base.id, cid), dst->bp_paused);
        } else if (ftype == F_TCLOSE) {
            dst_close_cid(dst, cid, 0);
        }
        break;
    }
    }
}

void ch_on_link_up(channel_t *ch)
{
    switch (ch->base.type) {
    case CH_MCU: {
        mcu_ch_t *m = &ch->mcu;
        m->link_up = 1;
        m->last_rx_ms = mono_now_ms();
        if (m->state == MCU_ACTIVE)
            daemon_send(m->base.d, F_READY, (uint8_t)m->base.id, NULL, 0);
        else
            daemon_send(m->base.d, F_FLUSH, (uint8_t)m->base.id, NULL, 0);
        break;
    }
    case CH_PTY:
        pty_close(&ch->pty);  /* stay closed until READY arrives */
        break;
    case CH_TCP_SRC:
        ch->src.link_up = 1;
        break;
    case CH_TCP_DST:
        /* Close stale connections from previous session */
        ch_on_link_down(ch);
        break;
    }
}

void ch_on_link_down(channel_t *ch)
{
    switch (ch->base.type) {
    case CH_MCU: {
        mcu_ch_t *m = &ch->mcu;
        m->link_up   = 0;
        m->txbuf_len = 0;
        mcu_update_interest(m);
        break;
    }
    case CH_PTY:
        log_ts("PTY ch%d: link down", ch->base.id);
        pty_close(&ch->pty);
        break;
    case CH_TCP_SRC: {
        tcp_src_t *s = &ch->src;
        s->link_up = 0;
        for (int i = 0; i < MAX_TCP_CONNS; i++)
            src_close_cid(s, i, 0);
        break;
    }
    case CH_TCP_DST: {
        tcp_dst_t *dst = &ch->dst;
        for (int i = 0; i < MAX_TCP_CONNS; i++)
            dst_close_cid(dst, i, 0);
        break;
    }
    }
}

void ch_pause(channel_t *ch)
{
    switch (ch->base.type) {
    case CH_MCU: {
        mcu_ch_t *m = &ch->mcu;
        if (m->bp_paused) return;
        m->bp_paused = 1;
        mcu_update_interest(m);
        break;
    }
    case CH_PTY: {
        pty_ch_t *p = &ch->pty;
        if (p->bp_paused) return;
        p->bp_paused = 1;
        pty_update_interest(p);
        break;
    }
    case CH_TCP_SRC: {
        tcp_src_t *s = &ch->src;
        if (s->bp_paused) return;
        s->bp_paused = 1;
        for (int i = 0; i < MAX_TCP_CONNS; i++) {
            if (s->conns[i].fd >= 0)
                tcp_conn_update(&s->conns[i], s->base.d->epoll_fd,
                                TAG_TCPCONN(s->base.id, i), 1);
        }
        break;
    }
    case CH_TCP_DST: {
        tcp_dst_t *dst = &ch->dst;
        if (dst->bp_paused) return;
        dst->bp_paused = 1;
        for (int i = 0; i < MAX_TCP_CONNS; i++) {
            if (dst->conns[i].fd >= 0)
                tcp_conn_update(&dst->conns[i], dst->base.d->epoll_fd,
                                TAG_TCPCONN(dst->base.id, i), 1);
        }
        break;
    }
    }
}

void ch_resume(channel_t *ch)
{
    switch (ch->base.type) {
    case CH_MCU: {
        mcu_ch_t *m = &ch->mcu;
        if (!m->bp_paused) return;
        m->bp_paused = 0;
        mcu_update_interest(m);
        break;
    }
    case CH_PTY: {
        pty_ch_t *p = &ch->pty;
        if (!p->bp_paused) return;
        p->bp_paused = 0;
        pty_update_interest(p);
        break;
    }
    case CH_TCP_SRC: {
        tcp_src_t *s = &ch->src;
        if (!s->bp_paused) return;
        s->bp_paused = 0;
        for (int i = 0; i < MAX_TCP_CONNS; i++) {
            if (s->conns[i].fd >= 0)
                tcp_conn_update(&s->conns[i], s->base.d->epoll_fd,
                                TAG_TCPCONN(s->base.id, i), 0);
        }
        break;
    }
    case CH_TCP_DST: {
        tcp_dst_t *dst = &ch->dst;
        if (!dst->bp_paused) return;
        dst->bp_paused = 0;
        for (int i = 0; i < MAX_TCP_CONNS; i++) {
            if (dst->conns[i].fd >= 0)
                tcp_conn_update(&dst->conns[i], dst->base.d->epoll_fd,
                                TAG_TCPCONN(dst->base.id, i), 0);
        }
        break;
    }
    }
}

int64_t ch_deadline(channel_t *ch, int64_t now_ms)
{
    if (ch->base.type == CH_MCU) {
        mcu_ch_t *m = &ch->mcu;
        if (m->fd < 0)
            return m->reopen_at_ms;
        if (m->state == MCU_ACTIVE && m->last_rx_ms > 0)
            return m->last_rx_ms + RESET_SILENCE_MS;
    }
    (void)now_ms;
    return 0;
}

void ch_close(channel_t *ch)
{
    switch (ch->base.type) {
    case CH_MCU: mcu_close_uart(&ch->mcu); break;
    case CH_PTY: pty_close(&ch->pty); break;
    case CH_TCP_SRC:
        ch_on_link_down(ch);
        epoll_update(ch->src.base.d->epoll_fd, ch->src.server_fd, 0, 0,
                     &ch->src.server_in_epoll);
        close(ch->src.server_fd);
        break;
    case CH_TCP_DST:
        ch_on_link_down(ch);
        break;
    }
}

void ch_handle_event(channel_t *ch, uint32_t kind, uint32_t info, uint32_t events)
{
    switch (kind) {
    case 0x02:  /* mcu uart */
        if (ch->base.type == CH_MCU) {
            mcu_ch_t *m = &ch->mcu;
            if (events & EPOLLIN)  mcu_uart_read(m);
            if (events & EPOLLOUT) mcu_uart_drain(m);
        }
        break;
    case 0x03:  /* pty master */
        if (ch->base.type == CH_PTY) {
            pty_ch_t *p = &ch->pty;
            if (events & EPOLLIN)  pty_master_read(p);
            if (events & EPOLLOUT) pty_master_drain(p);
        }
        break;
    case 0x04:  /* tcp server accept */
        if (ch->base.type == CH_TCP_SRC)
            src_on_accept(&ch->src);
        break;
    case 0x05:  /* tcp connection */
        {
            int cid = (int)(info & 0xFFFF);
            if (cid >= MAX_TCP_CONNS) break;
            if (ch->base.type == CH_TCP_SRC)
                src_on_conn_event(&ch->src, cid, events);
            else if (ch->base.type == CH_TCP_DST)
                dst_on_conn_event(&ch->dst, cid, events);
        }
        break;
    }
}
