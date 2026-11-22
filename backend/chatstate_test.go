package main

import (
	"context"
	"errors"
	"testing"

	"go.mau.fi/whatsmeow/types"
)

func TestChatStateSupersededComposingDoesNotFollowPaused(t *testing.T) {
	var states outgoingChatStates
	jid := types.NewJID("123", types.DefaultUserServer)
	composing := states.queue(jid, types.ChatPresenceComposing, "")
	paused := states.queue(jid, types.ChatPresencePaused, "")
	var got []types.ChatPresence
	send := func(_ context.Context, _ types.JID, state types.ChatPresence, _ types.ChatPresenceMedia) error {
		got = append(got, state)
		return nil
	}
	states.send(context.Background(), jid, paused, send)
	states.send(context.Background(), jid, composing, send)
	states.pauseAll(send)
	if len(got) != 1 || got[0] != types.ChatPresencePaused {
		t.Fatalf("unexpected chat states: %v", got)
	}
}

func TestChatStateShutdownPausesUnsentAndFailedUpdates(t *testing.T) {
	var states outgoingChatStates
	jid := types.NewJID("123", types.DefaultUserServer)
	update := states.queue(jid, types.ChatPresencePaused, "")
	states.send(context.Background(), jid, update, func(context.Context, types.JID, types.ChatPresence, types.ChatPresenceMedia) error {
		return errors.New("temporarily unavailable")
	})
	other := types.NewJID("456", types.DefaultUserServer)
	unsent := states.queue(other, types.ChatPresenceComposing, types.ChatPresenceMediaAudio)
	got := make(map[types.JID]bool)
	send := func(ctx context.Context, jid types.JID, state types.ChatPresence, _ types.ChatPresenceMedia) error {
		if state != types.ChatPresencePaused {
			t.Errorf("shutdown sent %s", state)
		}
		if _, ok := ctx.Deadline(); !ok {
			t.Error("shutdown send has no deadline")
		}
		got[jid] = true
		return nil
	}
	states.pauseAll(send)
	states.send(context.Background(), other, unsent, send)
	if !got[jid] || !got[other] || len(got) != 2 {
		t.Fatalf("missing shutdown updates: %v", got)
	}
	if states.queue(jid, types.ChatPresenceComposing, "") != nil {
		t.Fatal("accepted composing after shutdown")
	}
}
