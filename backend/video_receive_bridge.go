package main

import (
	"encoding/binary"
	"io"
	"log"
	"net"
	"sync"
	"time"

	"github.com/purpshell/meowcaller"
	"github.com/purpshell/meowcaller/rtp"
)

// VideoReceiveBridge hands the peer's video to the app: meowcaller delivers
// whole H.264 access units, this writes them to the app's one loopback TCP
// connection as
//
//	[u32 payload_len][u32 ssrc][u8 orientation][u8 flags][u16 0][payload]
//
// (big endian; flags bit 0 = a screen share). The app shows one picture, so
// only one stream goes through: on a group call the stream showing stays until
// it has been quiet for streamQuietAfter, and a screen share is shown over a
// camera. A decoder can't start mid-stream or survive a missing frame, so
// after a switch, a reconnect or a dropped frame nothing goes through until
// the next keyframe, which is asked for at once. The app can ask too, by
// sending the byte 'K' (its decoder failed).
type VideoReceiveBridge struct {
	listener        net.Listener
	requestKeyframe func(ssrc uint32)
	frames          chan receivedVideoFrame

	mu                 sync.Mutex
	conn               net.Conn
	ssrc               uint32
	screen             bool
	lastAt             time.Time
	waitingForKeyframe bool
	closed             bool
}

type receivedVideoFrame struct {
	ssrc        uint32
	orientation int
	screen      bool
	accessUnit  []byte
}

const (
	// streamQuietAfter is how long the stream being shown may go without a
	// frame before another one takes its place.
	streamQuietAfter = 2 * time.Second
	// videoReceiveQueue holds about a quarter second of 30 fps video; a
	// connection slower than that loses frames rather than lagging further.
	videoReceiveQueue           = 8
	videoReceiveKeyframeRequest = 'K'
)

func NewVideoReceiveBridge(requestKeyframe func(ssrc uint32)) (*VideoReceiveBridge, int, error) {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return nil, 0, err
	}
	vb := &VideoReceiveBridge{
		listener:           listener,
		requestKeyframe:    requestKeyframe,
		frames:             make(chan receivedVideoFrame, videoReceiveQueue),
		waitingForKeyframe: true,
	}
	return vb, listener.Addr().(*net.TCPAddr).Port, nil
}

// Offer takes one access unit from meowcaller (OnParticipantVideoFrame or
// OnParticipantScreenFrame). It runs on the media goroutine, so it never
// blocks.
func (vb *VideoReceiveBridge) Offer(frame meowcaller.ParticipantVideoFrame) {
	now := time.Now()
	vb.mu.Lock()
	if vb.closed {
		vb.mu.Unlock()
		return
	}
	if frame.SSRC != vb.ssrc {
		quiet := vb.ssrc == 0 || now.Sub(vb.lastAt) > streamQuietAfter
		if !quiet && !(frame.ScreenShare && !vb.screen) {
			vb.mu.Unlock()
			return
		}
		vb.ssrc, vb.screen = frame.SSRC, frame.ScreenShare
		vb.waitingForKeyframe = true
	}
	vb.lastAt = now
	if vb.waitingForKeyframe {
		if !rtp.AUHasIDR(frame.AccessUnit) {
			ssrc := vb.ssrc
			vb.mu.Unlock()
			vb.requestKeyframe(ssrc)
			return
		}
		vb.waitingForKeyframe = false
	}
	// Queued under the lock so Close can't close the channel in between.
	select {
	case vb.frames <- receivedVideoFrame{frame.SSRC, frame.Orientation, frame.ScreenShare, frame.AccessUnit}:
		vb.mu.Unlock()
	default:
		// The app is behind; the frames after this one can't decode without
		// it, so start again from a keyframe.
		vb.waitingForKeyframe = true
		ssrc := vb.ssrc
		vb.mu.Unlock()
		vb.requestKeyframe(ssrc)
	}
}

func (vb *VideoReceiveBridge) restartFromKeyframe() {
	vb.mu.Lock()
	vb.waitingForKeyframe = true
	ssrc := vb.ssrc
	vb.mu.Unlock()
	vb.requestKeyframe(ssrc)
}

// Run accepts the app's connection (a later one replaces it) and writes the
// frames out until Close.
func (vb *VideoReceiveBridge) Run() {
	go vb.writeFrames()
	for {
		conn, err := vb.listener.Accept()
		if err != nil {
			vb.mu.Lock()
			closed := vb.closed
			vb.mu.Unlock()
			if closed {
				return
			}
			log.Printf("VideoReceiveBridge accept error (continuing): %v\n", err)
			time.Sleep(50 * time.Millisecond)
			continue
		}
		if tcp, ok := conn.(*net.TCPConn); ok {
			_ = tcp.SetNoDelay(true)
		}
		vb.mu.Lock()
		if vb.closed {
			vb.mu.Unlock()
			conn.Close()
			return
		}
		previous := vb.conn
		vb.conn = conn
		vb.mu.Unlock()
		if previous != nil {
			previous.Close()
		}
		vb.restartFromKeyframe()
		go vb.readRequests(conn)
	}
}

func (vb *VideoReceiveBridge) readRequests(conn net.Conn) {
	buf := make([]byte, 64)
	for {
		n, err := conn.Read(buf)
		for _, b := range buf[:n] {
			if b == videoReceiveKeyframeRequest {
				vb.restartFromKeyframe()
				break
			}
		}
		if err != nil {
			if err != io.EOF {
				vb.mu.Lock()
				current := vb.conn == conn
				vb.mu.Unlock()
				if current {
					log.Printf("VideoReceiveBridge read error: %v\n", err)
				}
			}
			return
		}
	}
}

func (vb *VideoReceiveBridge) writeFrames() {
	header := make([]byte, 12)
	for frame := range vb.frames {
		vb.mu.Lock()
		conn := vb.conn
		vb.mu.Unlock()
		if conn == nil {
			// Nobody to show it to; the app's connection starts from a keyframe.
			continue
		}
		binary.BigEndian.PutUint32(header[0:4], uint32(len(frame.accessUnit)))
		binary.BigEndian.PutUint32(header[4:8], frame.ssrc)
		header[8] = byte(frame.orientation & 3)
		header[9] = 0
		if frame.screen {
			header[9] = 1
		}
		_ = conn.SetWriteDeadline(time.Now().Add(time.Second))
		_, err := conn.Write(header)
		if err == nil {
			_, err = conn.Write(frame.accessUnit)
		}
		if err != nil {
			log.Printf("VideoReceiveBridge write error, dropping the connection: %v\n", err)
			vb.mu.Lock()
			if vb.conn == conn {
				vb.conn = nil
			}
			vb.mu.Unlock()
			conn.Close()
		}
	}
}

func (vb *VideoReceiveBridge) Close() error {
	vb.mu.Lock()
	if vb.closed {
		vb.mu.Unlock()
		return nil
	}
	vb.closed = true
	conn := vb.conn
	vb.conn = nil
	vb.mu.Unlock()
	if conn != nil {
		conn.Close()
	}
	close(vb.frames)
	return vb.listener.Close()
}
