package main

import (
	"encoding/binary"
	"io"
	"net"
	"strconv"
	"sync"
	"testing"
	"time"

	"github.com/purpshell/meowcaller"
)

var (
	testIDR    = []byte{0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 3}
	testPFrame = []byte{0, 0, 0, 1, 0x41, 9}
)

type keyframeAsks struct {
	mu    sync.Mutex
	ssrcs []uint32
}

func (k *keyframeAsks) ask(ssrc uint32) {
	k.mu.Lock()
	k.ssrcs = append(k.ssrcs, ssrc)
	k.mu.Unlock()
}

func (k *keyframeAsks) count() int {
	k.mu.Lock()
	defer k.mu.Unlock()
	return len(k.ssrcs)
}

func readReceivedFrame(t *testing.T, conn net.Conn) (ssrc uint32, orientation, flags byte, payload []byte) {
	t.Helper()
	_ = conn.SetReadDeadline(time.Now().Add(2 * time.Second))
	header := make([]byte, 12)
	if _, err := io.ReadFull(conn, header); err != nil {
		t.Fatalf("reading a frame header: %v", err)
	}
	payload = make([]byte, binary.BigEndian.Uint32(header[0:4]))
	if _, err := io.ReadFull(conn, payload); err != nil {
		t.Fatalf("reading a frame: %v", err)
	}
	return binary.BigEndian.Uint32(header[4:8]), header[8], header[9], payload
}

func connectToReceiver(t *testing.T) (*VideoReceiveBridge, net.Conn, *keyframeAsks) {
	t.Helper()
	asks := &keyframeAsks{}
	vb, port, err := NewVideoReceiveBridge(asks.ask)
	if err != nil {
		t.Fatal(err)
	}
	go vb.Run()
	conn, err := net.Dial("tcp", net.JoinHostPort("127.0.0.1", strconv.Itoa(port)))
	if err != nil {
		t.Fatal(err)
	}
	// The connection is taken in by Run; wait until it asks for its keyframe.
	deadline := time.Now().Add(2 * time.Second)
	for asks.count() == 0 && time.Now().Before(deadline) {
		time.Sleep(5 * time.Millisecond)
	}
	t.Cleanup(func() { conn.Close(); vb.Close() })
	return vb, conn, asks
}

// A decoder can't start on a frame that refers to earlier ones: the stream
// goes through from its first keyframe, and the keyframe is asked for.
func TestVideoReceiveStartsAtAKeyframe(t *testing.T) {
	vb, conn, asks := connectToReceiver(t)
	before := asks.count()
	vb.Offer(meowcaller.ParticipantVideoFrame{SSRC: 5, AccessUnit: testPFrame})
	if asks.count() == before {
		t.Fatal("a stream that starts without a keyframe must ask for one")
	}
	vb.Offer(meowcaller.ParticipantVideoFrame{SSRC: 5, Orientation: 1, AccessUnit: testIDR})
	vb.Offer(meowcaller.ParticipantVideoFrame{SSRC: 5, Orientation: 1, AccessUnit: testPFrame})
	ssrc, orientation, flags, payload := readReceivedFrame(t, conn)
	if ssrc != 5 || orientation != 1 || flags != 0 || string(payload) != string(testIDR) {
		t.Fatalf("first frame = ssrc %d orientation %d flags %d %x, want the keyframe", ssrc, orientation, flags, payload)
	}
	if _, _, _, payload = readReceivedFrame(t, conn); string(payload) != string(testPFrame) {
		t.Fatalf("second frame = %x, want the frame after the keyframe", payload)
	}
}

// One picture: a second camera doesn't interleave with the one showing, but a
// screen share takes over, and a camera that went quiet is replaced.
func TestVideoReceiveShowsOneStream(t *testing.T) {
	vb, conn, _ := connectToReceiver(t)
	vb.Offer(meowcaller.ParticipantVideoFrame{SSRC: 1, AccessUnit: testIDR})
	vb.Offer(meowcaller.ParticipantVideoFrame{SSRC: 2, AccessUnit: testIDR})
	vb.Offer(meowcaller.ParticipantVideoFrame{SSRC: 3, ScreenShare: true, AccessUnit: testIDR})
	if ssrc, _, _, _ := readReceivedFrame(t, conn); ssrc != 1 {
		t.Fatalf("first stream = %d, want 1", ssrc)
	}
	if ssrc, _, flags, _ := readReceivedFrame(t, conn); ssrc != 3 || flags != 1 {
		t.Fatalf("next = ssrc %d flags %d, want the screen share (3, flag 1); stream 2 must not interleave", ssrc, flags)
	}

	vb.mu.Lock()
	vb.lastAt = time.Now().Add(-streamQuietAfter - time.Second)
	vb.mu.Unlock()
	vb.Offer(meowcaller.ParticipantVideoFrame{SSRC: 2, AccessUnit: testIDR})
	if ssrc, _, _, _ := readReceivedFrame(t, conn); ssrc != 2 {
		t.Fatalf("after the share went quiet = %d, want 2", ssrc)
	}
}

// The app's decoder failing ('K') restarts the stream from the next keyframe.
func TestVideoReceiveAppKeyframeRequest(t *testing.T) {
	vb, conn, asks := connectToReceiver(t)
	vb.Offer(meowcaller.ParticipantVideoFrame{SSRC: 4, AccessUnit: testIDR})
	readReceivedFrame(t, conn)
	before := asks.count()
	if _, err := conn.Write([]byte{'K'}); err != nil {
		t.Fatal(err)
	}
	deadline := time.Now().Add(2 * time.Second)
	for asks.count() == before && time.Now().Before(deadline) {
		time.Sleep(5 * time.Millisecond)
	}
	if asks.count() == before {
		t.Fatal("the app's request didn't reach the peer")
	}
	vb.Offer(meowcaller.ParticipantVideoFrame{SSRC: 4, AccessUnit: testPFrame})
	vb.Offer(meowcaller.ParticipantVideoFrame{SSRC: 4, AccessUnit: testIDR})
	if _, _, _, payload := readReceivedFrame(t, conn); string(payload) != string(testIDR) {
		t.Fatalf("after a decoder failure got %x, want the next keyframe", payload)
	}
}

// Close while the media goroutine is still offering frames must not panic.
func TestVideoReceiveCloseWhileOffering(t *testing.T) {
	vb, _, _ := connectToReceiver(t)
	done := make(chan struct{})
	go func() {
		defer close(done)
		for i := 0; i < 2000; i++ {
			vb.Offer(meowcaller.ParticipantVideoFrame{SSRC: 6, AccessUnit: testIDR})
		}
	}()
	vb.Close()
	<-done
}
