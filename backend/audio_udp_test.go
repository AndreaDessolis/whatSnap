package main

import (
	"bytes"
	"encoding/binary"
	"math"
	"net"
	"testing"
	"time"

	"github.com/purpshell/meowcaller"
)

func TestUDPBridgeHandsOverTheOpusPacketThatCameWithTheFrame(t *testing.T) {
	bridge, port, err := NewUDPBridge()
	if err != nil {
		t.Fatalf("NewUDPBridge: %v", err)
	}
	defer bridge.Close()
	frontend, err := net.DialUDP("udp", nil, &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1), Port: port})
	if err != nil {
		t.Fatalf("dial bridge: %v", err)
	}
	defer frontend.Close()

	pcm := make([]byte, meowcaller.FrameSamples*4)
	for i := 0; i < meowcaller.FrameSamples; i++ {
		binary.LittleEndian.PutUint32(pcm[i*4:], math.Float32bits(float32(i)/1000))
	}
	opusFrame := []byte{0x58, 1, 2, 3, 4}

	if _, err = frontend.Write(append(append([]byte(nil), pcm...), opusFrame...)); err != nil {
		t.Fatalf("send frame: %v", err)
	}
	frame, err := bridge.ReadFrame()
	if err != nil || len(frame) != meowcaller.FrameSamples || frame[500] != 0.5 {
		t.Fatalf("ReadFrame = %d samples, err %v", len(frame), err)
	}
	if got := bridge.OpusFrame(); !bytes.Equal(got, opusFrame) {
		t.Fatalf("OpusFrame = %x, want %x", got, opusFrame)
	}

	if _, err = frontend.Write(pcm); err != nil {
		t.Fatalf("send PCM frame: %v", err)
	}
	if frame, err = bridge.ReadFrame(); err != nil || frame[500] != 0.5 {
		t.Fatalf("ReadFrame (PCM only) err %v", err)
	}
	if got := bridge.OpusFrame(); got != nil {
		t.Fatalf("OpusFrame after a PCM-only frame = %x, want none", got)
	}

	if _, err = frontend.Write(append(append([]byte(nil), pcm...), opusFrame...)); err != nil {
		t.Fatalf("send frame: %v", err)
	}
	if _, err = bridge.ReadFrame(); err != nil {
		t.Fatalf("ReadFrame: %v", err)
	}
	frame, err = bridge.ReadFrame()
	if err != nil || len(frame) != meowcaller.FrameSamples || frame[500] != 0 {
		t.Fatalf("ReadFrame on timeout = %d samples, err %v", len(frame), err)
	}
	if got := bridge.OpusFrame(); got != nil {
		t.Fatalf("OpusFrame for a silence frame = %x, want none", got)
	}

	if pcmPart, opusPart := splitMicDatagram(make([]byte, 4)); len(pcmPart) != 4 || opusPart != nil {
		t.Fatalf("hello datagram split into %d PCM bytes and %d Opus bytes", len(pcmPart), len(opusPart))
	}
}

// The app tells decoded audio from an Opus packet by the first byte.
func TestUDPBridgeTagsDecodedAudioAndOpusPackets(t *testing.T) {
	bridge, port, err := NewUDPBridge()
	if err != nil {
		t.Fatal(err)
	}
	defer bridge.Close()
	app, err := net.DialUDP("udp", nil, &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1), Port: port})
	if err != nil {
		t.Fatal(err)
	}
	defer app.Close()
	// The app's first mic datagram tells the bridge where it is.
	if _, err := app.Write(make([]byte, meowcaller.FrameSamples*4)); err != nil {
		t.Fatal(err)
	}
	if _, err := bridge.ReadFrame(); err != nil {
		t.Fatal(err)
	}

	if err := bridge.WriteFrame([]float32{0.5, -0.25}); err != nil {
		t.Fatal(err)
	}
	if err := bridge.WriteOpusPacket(0xA1B2C3D4, 7, 96000, []byte{0x58, 1, 2}); err != nil {
		t.Fatal(err)
	}
	buf := make([]byte, 64)
	_ = app.SetReadDeadline(time.Now().Add(2 * time.Second))
	n, err := app.Read(buf)
	if err != nil {
		t.Fatal(err)
	}
	if n != 9 || buf[0] != datagramPCM || math.Float32frombits(binary.LittleEndian.Uint32(buf[1:])) != 0.5 {
		t.Fatalf("PCM datagram = %x", buf[:n])
	}
	n, err = app.Read(buf)
	if err != nil {
		t.Fatal(err)
	}
	if n != 14 || buf[0] != datagramOpus || binary.LittleEndian.Uint32(buf[1:]) != 0xA1B2C3D4 ||
		binary.LittleEndian.Uint16(buf[5:]) != 7 || binary.LittleEndian.Uint32(buf[7:]) != 96000 ||
		!bytes.Equal(buf[11:n], []byte{0x58, 1, 2}) {
		t.Fatalf("Opus datagram = %x", buf[:n])
	}
}

// Mic frames that pile up (the mic's clock ahead of the engine's ticks) are
// dropped down to maxMicBacklog, oldest first, so our voice doesn't lag.
func TestUDPBridgeDropsStaleMicFrames(t *testing.T) {
	bridge, port, err := NewUDPBridge()
	if err != nil {
		t.Fatal(err)
	}
	defer bridge.Close()
	app, err := net.DialUDP("udp", nil, &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1), Port: port})
	if err != nil {
		t.Fatal(err)
	}
	defer app.Close()
	pcm := make([]byte, meowcaller.FrameSamples*4)
	for i := 0; i < 6; i++ {
		binary.LittleEndian.PutUint32(pcm, math.Float32bits(float32(i)))
		if _, err := app.Write(pcm); err != nil {
			t.Fatal(err)
		}
	}
	time.Sleep(50 * time.Millisecond)
	for _, want := range []float32{4, 5} {
		frame, err := bridge.ReadFrame()
		if err != nil || frame[0] != want {
			t.Fatalf("frame %v, want %v (err %v)", frame[0], want, err)
		}
	}
}
