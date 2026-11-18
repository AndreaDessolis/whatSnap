package main

import (
	"encoding/binary"
	"io"
	"log"
	"net"
	"sync"
	"time"
)

type VideoBridge struct {
	listener net.Listener
	send     func(accessUnit []byte, duration time.Duration) error
	closed   bool
	closedMu sync.Mutex
	// conns are the accepted connections still being read; Close closes them
	// so their readers stop even when the frontend never hangs up.
	conns   map[net.Conn]struct{}
	workers sync.WaitGroup
}

func NewVideoBridge(send func(accessUnit []byte, duration time.Duration) error) (*VideoBridge, int, error) {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return nil, 0, err
	}
	port := listener.Addr().(*net.TCPAddr).Port
	return &VideoBridge{listener: listener, send: send, conns: make(map[net.Conn]struct{})}, port, nil
}

func (vb *VideoBridge) Run() {
	for {
		conn, err := vb.listener.Accept()
		if err != nil {
			vb.closedMu.Lock()
			closed := vb.closed
			vb.closedMu.Unlock()
			if closed {
				return
			}
			log.Printf("VideoBridge accept error (continuing): %v\n", err)
			time.Sleep(50 * time.Millisecond)
			continue
		}
		vb.closedMu.Lock()
		if vb.closed {
			vb.closedMu.Unlock()
			conn.Close()
			return
		}
		vb.conns[conn] = struct{}{}
		vb.workers.Add(1)
		vb.closedMu.Unlock()
		go vb.readFrames(conn)
	}
}

func (vb *VideoBridge) isClosed() bool {
	vb.closedMu.Lock()
	defer vb.closedMu.Unlock()
	return vb.closed
}

func (vb *VideoBridge) readFrames(conn net.Conn) {
	defer vb.workers.Done()
	defer func() {
		conn.Close()
		vb.closedMu.Lock()
		delete(vb.conns, conn)
		vb.closedMu.Unlock()
	}()
	header := make([]byte, 8)
	for {
		if _, err := io.ReadFull(conn, header); err != nil {
			if err != io.EOF && !vb.isClosed() {
				log.Printf("VideoBridge header read error: %v\n", err)
			}
			return
		}
		payloadLen := binary.BigEndian.Uint32(header[0:4])
		durationMs := binary.BigEndian.Uint32(header[4:8])
		const maxPayload = 4 << 20
		if payloadLen > maxPayload {
			log.Printf("VideoBridge frame too large (%d bytes), dropping connection\n", payloadLen)
			return
		}
		payload := make([]byte, payloadLen)
		if _, err := io.ReadFull(conn, payload); err != nil {
			log.Printf("VideoBridge payload read error: %v\n", err)
			return
		}
		if err := vb.send(payload, time.Duration(durationMs)*time.Millisecond); err != nil {
			log.Printf("Failed to send video frame: %v\n", err)
		}
	}
}

// Close stops accepting, closes every accepted connection and waits briefly
// for their readers to finish, so nothing is sent for the call afterwards and
// the readers stop holding the call's send callback.
func (vb *VideoBridge) Close() error {
	vb.closedMu.Lock()
	vb.closed = true
	conns := make([]net.Conn, 0, len(vb.conns))
	for conn := range vb.conns {
		conns = append(conns, conn)
	}
	vb.closedMu.Unlock()
	err := vb.listener.Close()
	for _, conn := range conns {
		conn.Close()
	}
	// Bounded: a reader stuck inside send must not hold up call cleanup.
	done := make(chan struct{})
	go func() {
		vb.workers.Wait()
		close(done)
	}()
	select {
	case <-done:
	case <-time.After(time.Second):
		log.Printf("VideoBridge: a frame reader is still busy after close")
	}
	return err
}
