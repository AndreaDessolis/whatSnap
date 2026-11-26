package main

import (
	"context"
	"log"

	"go.mau.fi/whatsmeow"
	"go.mau.fi/whatsmeow/proto/waE2E"
	"go.mau.fi/whatsmeow/types"
	"go.mau.fi/whatsmeow/types/events"
)

// Draft sync: the phone shares the unsent text of a chat with its linked
// devices (DraftMessageSync in a protocol message, and Conversation.draft in
// the history sync). We only receive: WA Web has no code yet that sends or
// applies them, so nothing is advertised or sent, and a draft is shown only
// when the server flag WA Web uses for it is on.

func (acc *Account) draftSyncEnabled() bool {
	acc.serverFlagsMu.RLock()
	defer acc.serverFlagsMu.RUnlock()
	return acc.serverFlags.Bool(whatsmeow.ABPropEnableSyncForDraftMessages, false)
}

// draftChat gives the chat as the app keys it (phone number, not LID).
func (acc *Account) draftChat(chat, alt string) (types.JID, bool) {
	for _, raw := range []string{chat, alt} {
		jid, err := types.ParseJID(raw)
		if err != nil || jid.IsEmpty() {
			continue
		}
		jid = jid.ToNonAD()
		if jid.Server != types.HiddenUserServer {
			return jid, true
		}
	}
	jid, err := types.ParseJID(chat)
	if err != nil || jid.IsEmpty() {
		return types.JID{}, false
	}
	pn, err := acc.wa().Store.LIDs.GetPNForLID(context.Background(), jid.ToNonAD())
	if err != nil || pn.IsEmpty() {
		return types.JID{}, false
	}
	return pn.ToNonAD(), true
}

func (acc *Account) broadcastDraft(chat types.JID, text string, editedAtMS int64) {
	acc.broadcastEvent("draft_synced", map[string]interface{}{
		"jid": chat.String(), "text": text, "edited_at_ms": editedAtMS,
	})
}

func (acc *Account) handleDraftSyncMessage(v *events.Message, pm *waE2E.ProtocolMessage) {
	sync := pm.GetDraftMessageSync()
	if !acc.draftSyncEnabled() {
		log.Printf("Draft sync: ignoring a draft from %s, the server flag is off", v.Info.Sender.User)
		return
	}
	chat, ok := acc.draftChat(sync.GetChatJID(), sync.GetAltChatJID())
	if !ok {
		log.Printf("Draft sync: no chat for a synced draft")
		return
	}
	text := ""
	if sync.GetAction() == waE2E.DraftMessageSync_SET {
		text = acc.extractText(sync.GetDraft())
	}
	acc.broadcastDraft(chat, text, sync.GetEditTimestampMS())
}

// applyHistoryDraft shows the draft a history sync carries for a chat.
func (acc *Account) applyHistoryDraft(chat types.JID, draftMsg *waE2E.Message, editedAtMS int64) {
	if draftMsg == nil || !acc.draftSyncEnabled() {
		return
	}
	if text := acc.extractText(draftMsg); text != "" {
		acc.broadcastDraft(chat, text, editedAtMS)
	}
}
