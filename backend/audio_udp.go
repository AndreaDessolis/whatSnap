package main

import (
	"encoding/binary"
	"github.com/purpshell/meowcaller"
	"log"
	"math"
	"net"
	"sync"
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

type UDPBridge struct {
	conn       *net.UDPConn
	peerAddr   *net.UDPAddr
	peerAddrMu sync.Mutex
	opusFrame  []byte
	readBuf    []byte
	frameBuf   []float32
	writeBuf   []byte
}

var (
	_ meowcaller.OpusFrameSource = (*UDPBridge)(nil)
	_ meowcaller.OpusPacketSink  = (*UDPBridge)(nil)
)

// What the app is sent, one datagram each (little endian): audio decoded
// here, or one of the peer's standard Opus packets, which the app decodes at
// playout time (src/calls/audio.rs parse_incoming, src/calls/opus_jitter.rs).
const (
	datagramPCM  = 1 // [1][f32 samples...]
	datagramOpus = 2 // [2][ssrc u32][seq u16][timestamp u32][packet]
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
	b.conn.SetReadDeadline(time.Now().Add(readTimeout))
	b.opusFrame = nil
	n, raddr, err := b.conn.ReadFromUDP(b.readBuf)
	if err != nil {
		if netErr, ok := err.(net.Error); !ok || !netErr.Timeout() {
			log.Printf("UDPBridge read error (continuing): %v", err)
		}
		for i := range b.frameBuf {
			b.frameBuf[i] = 0
		}
		return b.frameBuf, nil
	}

	b.peerAddrMu.Lock()
	if b.peerAddr == nil {
		b.peerAddr = raddr
	}
	b.peerAddrMu.Unlock()

	pcm, opusFrame := splitMicDatagram(b.readBuf[:n])
	b.opusFrame = opusFrame
	for i := range b.frameBuf {
		b.frameBuf[i] = 0
	}
	for i := 0; i < len(pcm)/4; i++ {
		b.frameBuf[i] = math.Float32frombits(binary.LittleEndian.Uint32(pcm[i*4:]))
	}
	return b.frameBuf, nil
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
