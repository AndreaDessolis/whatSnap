package main

import "testing"

func TestNormalizePairPhone(t *testing.T) {
	cases := []struct {
		raw, cc, want string
		ok            bool
	}{
		{"+44 7911 123456", "44", "447911123456", true},
		{"+44 07911 123456", "44", "447911123456", true},
		{"07911 123456", "44", "447911123456", true},
		{"0044 7911 123456", "44", "447911123456", true},
		{"+1 (202) 555-0123", "1", "12025550123", true},
		{"(202) 555-0123", "1", "12025550123", true},
		{"202-555-0123", "1", "12025550123", true},
		{"+52 55 1234 5678", "52", "525512345678", true},
		{"+52 1 55 1234 5678", "52", "5215512345678", true},
		{"+54 9 11 2345 6789", "54", "5491123456789", true},
		{"+55 (11) 98765-4321", "55", "5511987654321", true},
		{"98765 43210", "91", "919876543210", true},
		{"9198765432", "91", "919198765432", true},
		{"447911123456", "1", "447911123456", true},
		{"+٤٤ ٧٩١١ ١٢٣٤٥٦", "44", "447911123456", true},
		{"+44 7911", "44", "", false},
		{"+44 7911 1234567890123", "44", "", false},
		{"", "44", "", false},
		{"abc", "44", "", false},
	}
	for _, c := range cases {
		got, err := normalizePairPhone(c.raw, c.cc)
		if (err == nil) != c.ok || got != c.want {
			t.Errorf("normalizePairPhone(%q,%q) = %q, %v; want %q ok=%v", c.raw, c.cc, got, err, c.want, c.ok)
		}
	}
}

// Expected values come from WA Web's own isPhoneNumberValid run in node.
func TestValidNationalNumberMatchesWAWeb(t *testing.T) {
	cases := []struct {
		cc, national string
		want         bool
	}{
		{"44", "7911123456", true}, {"44", "07911123456", false},
		{"52", "5512345678", true}, {"52", "15512345678", true},
		{"54", "91123456789", true}, {"54", "1123456789", true},
		{"55", "11987654321", true}, {"55", "1187654321", true},
		{"1", "2025550123", true}, {"1", "5551234567", true},
		{"91", "9876543210", true}, {"91", "98765", false},
		{"49", "15112345678", true}, {"86", "13812345678", true},
		{"33", "612345678", true}, {"33", "0612345678", false},
		{"7", "9123456789", true},
	}
	for _, c := range cases {
		if got := validNationalNumber(c.cc, c.national); got != c.want {
			t.Errorf("validNationalNumber(%s,%s)=%v want %v", c.cc, c.national, got, c.want)
		}
	}
}
