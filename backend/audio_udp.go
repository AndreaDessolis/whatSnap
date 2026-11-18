package main

import (
	"encoding/binary"
	"github.com/purpshell/meowcaller"
	"log"
	"math"
	"net"
	"sync"
	"sync/atomic"
	"time"
)

// readTimeout bounds how long ReadFrame will wait for a UDP packet from the
// frontend before handing the engine a silence frame instead. meowcaller's
// engine pulls a frame every 60ms (FrameSamples cadence) and its own docs
// warn that "the call must keep sending to hold the relay bridge" - blocking
// that read loop indefinitely (e.g. while the frontend's audio streams are
// still spinning up, or briefly stall) risked the relay dropping the call
// entirely instead of just a moment of silence.
const readTimeout = 100 * time.Millisecond

const maxOpusPacket = 1275

// maxMicBacklog is how many of the app's mic frames may wait here. The engine
// takes one frame per 60 ms tick on the system clock while the app sends one
// per 60 ms of the mic's own clock; a mic clock running even 0.05% fast left
// frames piling up in the socket, about 0.3 s more delay every 10 minutes,
// and a burst at stream start stayed queued for the whole call. Older frames
// beyond this are dropped so our voice is never more than a frame behind.
const maxMicBacklog = 2

type UDPBridge struct {
	conn       *net.UDPConn
	peerAddr   *net.UDPAddr
	peerAddrMu sync.Mutex
	opusFrame  []byte
	readBuf    []byte
	frameBuf   []float32
	writeBuf   []byte
	// waiting holds mic datagrams already read off the socket, oldest first.
	waiting [][]byte
	dropped int
	// pendingRate is an audio rate that couldn't reach the app yet.
	pendingRate atomic.Int32
}

var (
	_ meowcaller.OpusFrameSource   = (*UDPBridge)(nil)
	_ meowcaller.OpusPacketSink    = (*UDPBridge)(nil)
	_ meowcaller.AudioRateListener = (*UDPBridge)(nil)
)

// What the app is sent, one datagram each (little endian): audio decoded
// here, or one of the peer's standard Opus packets, which the app decodes at
// playout time (src/calls/audio.rs parse_incoming, src/calls/opus_jitter.rs).
const (
	datagramPCM  = 1 // [1][f32 samples...]
	datagramOpus = 2 // [2][ssrc u32][seq u16][timestamp u32][packet]
	// [3][rate u32]: the call's audio rate (16000, 24000 or 48000).
	datagramAudioRate = 3
)

func splitMicDatagram(datagram []byte) (pcm, opusFrame []byte) {
	const pcmBytes = meowcaller.FrameSamples * 4
	if len(datagram) <= pcmBytes {
		return datagram, nil
	}
	return datagram[:pcmBytes], datagram[pcmBytes:]
}

func (b *UDPBridge) OpusFrame() []byte {
	return b.opusFrame
}

func NewUDPBridge() (*UDPBridge, int, error) {
	addr, err := net.ResolveUDPAddr("udp", "127.0.0.1:0")
	if err != nil {
		return nil, 0, err
	}
	conn, err := net.ListenUDP("udp", addr)
	if err != nil {
		return nil, 0, err
	}
	return &UDPBridge{
		conn:     conn,
		readBuf:  make([]byte, meowcaller.FrameSamples*4+maxOpusPacket),
		frameBuf: make([]float32, meowcaller.FrameSamples),
		writeBuf: make([]byte, 1+meowcaller.FrameSamples*4),
	}, conn.LocalAddr().(*net.UDPAddr).Port, nil
}

func (b *UDPBridge) ReadFrame() ([]float32, error) {
	b.opusFrame = nil
	// Take everything already queued, then keep only the newest few. (A
	// deadline already in the past fails a read without trying it.)
	b.conn.SetReadDeadline(time.Now().Add(time.Millisecond))
	for len(b.waiting) < 64 {
		datagram, ok := b.readDatagram()
		if !ok {
			break
		}
		b.waiting = append(b.waiting, datagram)
	}
	if extra := len(b.waiting) - maxMicBacklog; extra > 0 {
		b.waiting = b.waiting[extra:]
		if b.dropped += extra; b.dropped == extra || b.dropped%50 < extra {
			log.Printf("UDPBridge: dropped %d stale mic frames so far (mic clock ahead of the call)", b.dropped)
		}
	}
	if len(b.waiting) == 0 {
		b.conn.SetReadDeadline(time.Now().Add(readTimeout))
		if datagram, ok := b.readDatagram(); ok {
			b.waiting = append(b.waiting, datagram)
		}
	}
	for i := range b.frameBuf {
		b.frameBuf[i] = 0
	}
	if len(b.waiting) == 0 {
		return b.frameBuf, nil
	}
	datagram := b.waiting[0]
	b.waiting = b.waiting[1:]
	pcm, opusFrame := splitMicDatagram(datagram)
	b.opusFrame = opusFrame
	for i := 0; i < len(pcm)/4 && i < len(b.frameBuf); i++ {
		b.frameBuf[i] = math.Float32frombits(binary.LittleEndian.Uint32(pcm[i*4:]))
	}
	return b.frameBuf, nil
}

// readDatagram reads one datagram from the app under the deadline already
// set, remembering where the app sends from. A timeout reads as nothing.
func (b *UDPBridge) readDatagram() ([]byte, bool) {
	n, raddr, err := b.conn.ReadFromUDP(b.readBuf)
	if err != nil {
		if netErr, ok := err.(net.Error); !ok || !netErr.Timeout() {
			log.Printf("UDPBridge read error (continuing): %v", err)
		}
		return nil, false
	}
	b.peerAddrMu.Lock()
	first := b.peerAddr == nil
	if first {
		b.peerAddr = raddr
	}
	b.peerAddrMu.Unlock()
	if first {
		if rate := b.pendingRate.Swap(0); rate != 0 {
			b.SetAudioRate(int(rate))
		}
	}
	return append([]byte(nil), b.readBuf[:n]...), true
}

func (b *UDPBridge) Close() error {
	return b.conn.Close()
}

func (b *UDPBridge) WriteFrame(frame []float32) error {
	b.peerAddrMu.Lock()
	paddr := b.peerAddr
	b.peerAddrMu.Unlock()

	if paddr == nil {
		// We don't know where to send yet, drop the frame
		return nil
	}

	size := 1 + len(frame)*4
	buf := b.writeBuf
	if len(buf) < size {
		buf = make([]byte, size)
		b.writeBuf = buf
	}
	buf = buf[:size]
	buf[0] = datagramPCM
	for i, f := range frame {
		binary.LittleEndian.PutUint32(buf[1+i*4:], math.Float32bits(f))
	}
	_, err := b.conn.WriteToUDP(buf, paddr)
	return err
}

// SetAudioRate tells the app the call's audio rate (meowcaller's
// AudioRateListener): above 16000 it encodes super-wideband Opus.
func (b *UDPBridge) SetAudioRate(rate int) {
	b.peerAddrMu.Lock()
	paddr := b.peerAddr
	b.peerAddrMu.Unlock()
	if paddr == nil {
		// The app hasn't sent anything yet; asked again once it has.
		b.pendingRate.Store(int32(rate))
		return
	}
	var buf [5]byte
	buf[0] = datagramAudioRate
	binary.LittleEndian.PutUint32(buf[1:], uint32(rate))
	if _, err := b.conn.WriteToUDP(buf[:], paddr); err != nil {
		log.Printf("UDPBridge: audio rate %d not sent: %v", rate, err)
		b.pendingRate.Store(int32(rate))
		return
	}
	log.Printf("UDPBridge: call audio rate %d Hz", rate)
}

// WriteOpusPacket hands the app one of the peer's Opus packets undecoded.
func (b *UDPBridge) WriteOpusPacket(ssrc uint32, sequence uint16, timestamp uint32, packet []byte) error {
	b.peerAddrMu.Lock()
	paddr := b.peerAddr
	b.peerAddrMu.Unlock()
	if paddr == nil {
		return nil
	}
	buf := make([]byte, 11+len(packet))
	buf[0] = datagramOpus
	binary.LittleEndian.PutUint32(buf[1:], ssrc)
	binary.LittleEndian.PutUint16(buf[5:], sequence)
	binary.LittleEndian.PutUint32(buf[7:], timestamp)
	copy(buf[11:], packet)
	_, err := b.conn.WriteToUDP(buf, paddr)
	return err
}
