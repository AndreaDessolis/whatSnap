package main

import (
	"bufio"
	"bytes"
	"io"
	"strings"
	"testing"
)

func TestCommandReaderRecoversAfterOversizedLine(t *testing.T) {
	valid := "{\"action\":\"hello\"}\n"
	reader := bufio.NewReaderSize(io.MultiReader(
		strings.NewReader(strings.Repeat("x", maxCommandBytes*3)+"\n"),
		strings.NewReader(valid)), 4096)
	if _, err := readCommandLine(reader); err != bufio.ErrTooLong {
		t.Fatalf("oversized line: %v", err)
	}
	line, err := readCommandLine(reader)
	if err != nil || string(line) != valid {
		t.Fatalf("next command was lost: %q, %v", line, err)
	}
	if _, err := readCommandLine(reader); err != io.EOF {
		t.Fatalf("end of stream: %v", err)
	}
}

func TestCommandReaderBoundaries(t *testing.T) {
	for _, n := range []int{0, 65535, 65536, maxCommandBytes - 1, maxCommandBytes} {
		input := append(bytes.Repeat([]byte{'x'}, n), '\n')
		line, err := readCommandLine(bufio.NewReader(bytes.NewReader(input)))
		if len(input) > maxCommandBytes {
			if err != bufio.ErrTooLong {
				t.Fatalf("length %d: %v", len(input), err)
			}
		} else if err != nil || !bytes.Equal(line, input) {
			t.Fatalf("length %d: read %d, %v", len(input), len(line), err)
		}
	}
	reader := bufio.NewReader(strings.NewReader("{}"))
	if line, err := readCommandLine(reader); err != nil || string(line) != "{}" {
		t.Fatalf("final unterminated command: %q, %v", line, err)
	}
}

func TestMediaRetryNotifiesEveryWaiter(t *testing.T) {
	for _, success := range []bool{false, true} {
		acc := newAccount("test", "")
		first, removeFirst := acc.addMediaRetryWaiter("media")
		second, removeSecond := acc.addMediaRetryWaiter("media")
		if !acc.notifyMediaRetryWaiters("media", success) {
			t.Fatal("missing waiters")
		}
		for _, ch := range []chan bool{first, second} {
			select {
			case got := <-ch:
				if got != success {
					t.Fatalf("got %v, want %v", got, success)
				}
			default:
				t.Fatal("waiter was abandoned")
			}
		}
		// A late timeout from an old request must not remove a new one.
		third, removeThird := acc.addMediaRetryWaiter("media")
		removeFirst()
		removeSecond()
		acc.notifyMediaRetryWaiters("media", true)
		select {
		case <-third:
		default:
			t.Fatal("old cleanup removed new waiter")
		}
		removeThird()
		acc.cancel()
	}
}

func TestMediaRetryCancellationKeepsOtherWaiters(t *testing.T) {
	acc := newAccount("test", "")
	defer acc.cancel()
	first, removeFirst := acc.addMediaRetryWaiter("media")
	second, removeSecond := acc.addMediaRetryWaiter("media")
	defer removeSecond()
	removeFirst()
	removeFirst()
	acc.notifyMediaRetryWaiters("media", true)
	select {
	case <-first:
		t.Fatal("cancelled waiter notified")
	default:
	}
	select {
	case <-second:
	default:
		t.Fatal("cancelling one waiter removed another")
	}
	if acc.notifyMediaRetryWaiters("media", true) {
		t.Fatal("completed waiters leaked")
	}
}
