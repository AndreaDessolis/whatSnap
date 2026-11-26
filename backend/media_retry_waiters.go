package main

// Register before sending the retry request: the phone may answer immediately.
// Each caller owns its registration, so cancellation never removes other callers.
func (acc *Account) addMediaRetryWaiter(id string) (chan bool, func()) {
	ch := make(chan bool, 1)
	acc.retryWaitersMu.Lock()
	acc.retryWaiters[id] = append(acc.retryWaiters[id], ch)
	acc.retryWaitersMu.Unlock()
	return ch, func() {
		acc.retryWaitersMu.Lock()
		defer acc.retryWaitersMu.Unlock()
		waiters := acc.retryWaiters[id]
		for i, waiter := range waiters {
			if waiter == ch {
				waiters = append(waiters[:i], waiters[i+1:]...)
				if len(waiters) == 0 {
					delete(acc.retryWaiters, id)
				} else {
					acc.retryWaiters[id] = waiters
				}
				break
			}
		}
	}
}

func (acc *Account) notifyMediaRetryWaiters(id string, success bool) bool {
	acc.retryWaitersMu.Lock()
	waiters := acc.retryWaiters[id]
	delete(acc.retryWaiters, id)
	acc.retryWaitersMu.Unlock()
	for _, ch := range waiters {
		ch <- success // Buffered, with exactly one notification per registration.
	}
	return len(waiters) != 0
}

// takeMediaRetryFailure returns why the phone's answer to a media retry gave
// no file, once.
func (acc *Account) takeMediaRetryFailure(id string) string {
	if reason, ok := acc.mediaRetryFailures.LoadAndDelete(id); ok {
		return reason.(string)
	}
	return "Phone could not re-upload the media"
}
