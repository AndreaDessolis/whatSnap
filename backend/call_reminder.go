package main

import (
	"strings"

	"github.com/purpshell/meowcaller"
	"go.mau.fi/whatsmeow/types"
)

// watchCallReminders turns the server's call link reminders into the same
// notifications WA Web shows (WACallLinkJoinedNotification /
// WAWaitingRoomNotification). Like WA Web, a reminder without a link token is
// ignored.
func (acc *Account) watchCallReminders(client *meowcaller.Client) {
	client.OnCallReminder(func(r meowcaller.CallReminder) {
		if r.LinkToken == "" {
			return
		}
		var kind string
		var people []types.JID
		switch r.Type {
		case meowcaller.CallReminderLinkJoinedCreator, meowcaller.CallReminderLinkCreatorCallConnected:
			kind, people = "joined", r.Participants
		case meowcaller.CallReminderWaitingRoomJoined, meowcaller.CallReminderWaitingRoomMultipleJoined:
			kind = "waiting"
			for _, u := range r.WaitingUsers {
				people = append(people, u.JID)
			}
		default:
			return
		}
		acc.broadcastEvent("call_reminder", map[string]interface{}{
			"call_id": r.CallID, "kind": kind, "names": acc.reminderNames(people), "count": len(people),
		})
	})
}

func (acc *Account) reminderNames(jids []types.JID) string {
	names := make([]string, 0, len(jids))
	for _, j := range jids {
		names = append(names, acc.resolveDisplayName(j))
	}
	switch len(names) {
	case 0:
		return ""
	case 1:
		return names[0]
	default:
		return strings.Join(names[:len(names)-1], ", ") + " and " + names[len(names)-1]
	}
}
