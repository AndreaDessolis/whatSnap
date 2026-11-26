package main

import (
	"errors"
	"testing"

	"go.mau.fi/whatsmeow"
	"go.mau.fi/whatsmeow/proto/waMmsRetry"
)

func TestMediaRetryFailureReason(t *testing.T) {
	notOnPhone := func(err error, result waMmsRetry.MediaRetryNotification_ResultType) bool {
		return mediaRetryFailureReason(err, result) == mediaNotOnPhoneMessage
	}
	if !notOnPhone(whatsmeow.ErrMediaNotAvailableOnPhone, 0) {
		t.Error("error code 2 means the phone no longer has the file")
	}
	if !notOnPhone(nil, waMmsRetry.MediaRetryNotification_NOT_FOUND) || !notOnPhone(nil, waMmsRetry.MediaRetryNotification_DECRYPTION_ERROR) {
		t.Error("NOT_FOUND and DECRYPTION_ERROR are permanent, like WA Web's 404")
	}
	if notOnPhone(nil, waMmsRetry.MediaRetryNotification_GENERAL_ERROR) || notOnPhone(errors.New("other"), 0) {
		t.Error("a general error may pass")
	}
}
