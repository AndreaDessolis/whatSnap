package main

import (
	"encoding/json"
	"log"
	"time"

	"github.com/purpshell/meowcaller"
)

// Call waiting, modelled on WhatsApp's call engine "pending call"
// (call_waiting.cc in the VoIP wasm): a call that rings while we're on
// another one is not declined as busy. It is held and shown as waiting, with
// its own tone (kToneTypeIncomingPendingCall). Only one call waits; a further
// one is declined as busy ("pending call auto rejected"). When the current
// call ends, for any reason, the waiting call's offer is handled as a normal
// incoming call and rings ("Handle buffered message for pending call").
// "End and answer" is ending the current call with an answer queued; Decline
// rejects the waiting call (reject_pending_call). A waiting call whose caller
// gives up is a missed call.

type waitingCall struct {
	call    *meowcaller.Call
	payload map[string]interface{}
	// answerNow: the user chose End and answer, so the app answers the call
	// as soon as it becomes the incoming call.
	answerNow bool
}

// holdWaitingCall keeps call as the waiting call and tells the app. It
// returns false when another call is already waiting.
func (acc *Account) holdWaitingCall(call *meowcaller.Call, payload map[string]interface{}) bool {
	acc.activeCallsMutex.Lock()
	// A group call re-rings with the same call id; that is still the one
	// waiting call, not a second one.
	rering := acc.waiting != nil && acc.waiting.call.ID() == call.ID()
	if !rering && acc.waiting != nil && acc.waiting.call.State() != meowcaller.CallPhaseEnded {
		acc.activeCallsMutex.Unlock()
		return false
	}
	if rering {
		acc.waiting.call = call
		acc.waiting.payload = payload
	} else {
		acc.waiting = &waitingCall{call: call, payload: payload}
	}
	acc.activeCallsMutex.Unlock()

	call.OnEnd(func(reason string) {
		acc.activeCallsMutex.Lock()
		still := acc.waiting != nil && acc.waiting.call == call
		if still {
			acc.waiting = nil
		}
		acc.activeCallsMutex.Unlock()
		if !still {
			return
		}
		summary := acc.finishLiveCall(call.ID(), reason)
		acc.broadcastEvent("call_waiting_ended", map[string]string{
			"call_id": call.ID(),
			"reason":  reason,
			"summary": summary,
		})
	})
	if rering {
		return true
	}
	log.Printf("Already on a call; holding incoming call %s from %v as waiting\n", call.ID(), call.Peer())
	acc.broadcastEvent("call_waiting", payload)
	return true
}

// promoteWaitingCall turns the waiting call into the incoming call once no
// other call is left. Safe to call any time; it does nothing otherwise.
func (acc *Account) promoteWaitingCall() {
	acc.activeCallsMutex.Lock()
	for id, c := range acc.activeCalls {
		if c.State() == meowcaller.CallPhaseEnded {
			delete(acc.activeCalls, id)
		}
	}
	w := acc.waiting
	if w == nil || len(acc.activeCalls) > 0 {
		acc.activeCallsMutex.Unlock()
		return
	}
	acc.waiting = nil
	if w.call.State() == meowcaller.CallPhaseEnded {
		acc.activeCallsMutex.Unlock()
		return
	}
	acc.activeCalls[w.call.ID()] = w.call
	acc.activeCallsMutex.Unlock()

	log.Printf("Current call ended; waiting call %s now rings (answer now: %v)\n", w.call.ID(), w.answerNow)
	acc.attachCallHandlers(w.call)
	payload := make(map[string]interface{}, len(w.payload)+1)
	for k, v := range w.payload {
		payload[k] = v
	}
	if w.answerNow {
		payload["answer_now"] = true
	}
	acc.broadcastEvent("incoming_call", payload)
}

// handleAnswerWaitingCall ends the current call and answers the waiting one
// (WhatsApp's "End and accept").
func (acc *Account) handleAnswerWaitingCall(data []byte) {
	var req struct {
		CallID string `json:"call_id"`
	}
	if err := json.Unmarshal(data, &req); err != nil {
		log.Println("Invalid answer_waiting_call format:", err)
		return
	}
	acc.activeCallsMutex.Lock()
	if acc.waiting == nil || acc.waiting.call.ID() != req.CallID {
		acc.activeCallsMutex.Unlock()
		log.Println("Cannot answer waiting call, not found:", req.CallID)
		acc.broadcastEvent("call_waiting_ended", map[string]string{"call_id": req.CallID, "reason": "gone"})
		return
	}
	acc.waiting.answerNow = true
	current := make([]*meowcaller.Call, 0, len(acc.activeCalls))
	for _, c := range acc.activeCalls {
		current = append(current, c)
	}
	acc.activeCallsMutex.Unlock()

	for _, c := range current {
		acc.updateLiveCall(c.ID(), func(lc *liveCall) { lc.localHangup = true })
		if err := c.Hangup(); err != nil {
			log.Println("Failed to end the current call for a waiting one:", err)
			acc.activeCallsMutex.Lock()
			delete(acc.activeCalls, c.ID())
			acc.activeCallsMutex.Unlock()
		}
	}
	acc.promoteWaitingCall()
	// A call whose end never reaches OnEnd would hold the waiting one forever.
	time.AfterFunc(2*time.Second, func() {
		acc.activeCallsMutex.Lock()
		for _, c := range current {
			delete(acc.activeCalls, c.ID())
		}
		acc.activeCallsMutex.Unlock()
		acc.promoteWaitingCall()
	})
}

// handleDeclineWaitingCall declines the waiting call and keeps the current one.
func (acc *Account) handleDeclineWaitingCall(data []byte) {
	var req struct {
		CallID string `json:"call_id"`
	}
	if err := json.Unmarshal(data, &req); err != nil {
		log.Println("Invalid decline_waiting_call format:", err)
		return
	}
	acc.activeCallsMutex.Lock()
	w := acc.waiting
	if w == nil || w.call.ID() != req.CallID {
		acc.activeCallsMutex.Unlock()
		return
	}
	acc.waiting = nil
	acc.activeCallsMutex.Unlock()

	acc.updateLiveCall(req.CallID, func(c *liveCall) { c.localReject = true })
	if err := w.call.Reject(); err != nil {
		log.Println("Failed to decline waiting call:", err)
	}
	acc.finishLiveCall(req.CallID, "rejected")
}
