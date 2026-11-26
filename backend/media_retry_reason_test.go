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

func TestFormatFileSize(t *testing.T) {
	for n, want := range map[int64]string{500: "500 B", 46080: "45 KB", 1572864: "1.5 MB", 47185920: "45 MB", 3221225472: "3.0 GB"} {
		if got := formatFileSize(n); got != want {
			t.Errorf("%d: got %q want %q", n, got, want)
		}
	}
}
