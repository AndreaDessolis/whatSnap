package main

import (
	_ "embed"
	"encoding/json"
	"errors"
	"regexp"
	"strings"
	"sync"
)

// phone_countries.json is WhatsApp Web's per-calling-code number table (the
// formats, lengths and mobile patterns its "Link with phone number" screen
// validates against), converted to JSON.
//
//go:embed phone_countries.json
var phoneCountriesJSON []byte

type phoneCountry struct {
	Formats [][]any `json:"f"`
	Lengths []int   `json:"l"`
	Mobile  string  `json:"m"`
	formats []phoneFormat
	mobile  *regexp.Regexp
}

type phoneFormat struct {
	re      *regexp.Regexp
	leading []*regexp.Regexp
}

var (
	phoneTableOnce sync.Once
	phoneTable     map[string]*phoneCountry
)

func loadPhoneTable() map[string]*phoneCountry {
	phoneTableOnce.Do(func() {
		table := map[string]*phoneCountry{}
		if err := json.Unmarshal(phoneCountriesJSON, &table); err != nil {
			// Compiled-in data, so a failure is a build defect: validation then
			// accepts nothing rather than everything.
			table = map[string]*phoneCountry{}
		}
		phoneTable = table
	})
	return phoneTable
}

func (c *phoneCountry) compile() {
	if c.formats != nil || c.mobile != nil {
		return
	}
	c.formats = []phoneFormat{}
	for _, f := range c.Formats {
		pat, _ := f[0].(string)
		re, err := regexp.Compile(pat)
		if err != nil {
			continue
		}
		pf := phoneFormat{re: re}
		if lead, ok := f[1].([]any); ok {
			for _, l := range lead {
				if s, ok := l.(string); ok {
					if lre, err := regexp.Compile(s); err == nil {
						pf.leading = append(pf.leading, lre)
					}
				}
			}
		}
		c.formats = append(c.formats, pf)
	}
	if c.Mobile != "" {
		c.mobile, _ = regexp.Compile("^(?:" + c.Mobile + ")$")
	}
}

var phoneCCPattern = regexp.MustCompile(`^(1|2[07]|3[0-469]|4[013-9]|5[1-8]|6[0-6]|7|8[1246]|9[0-58])`)

// findCallingCode splits the calling code off a full international number the
// way WA Web does: the known one-to-two digit codes, else the first three digits.
func findCallingCode(digits string) string {
	if m := phoneCCPattern.FindString(digits); m != "" {
		return m
	}
	if len(digits) >= 3 {
		return digits[:3]
	}
	return digits
}

// validNationalNumber is WA Web's isPhoneNumberValid: the number matches one of
// the country's display formats, or has an allowed length and the mobile pattern.
func validNationalNumber(cc, national string) bool {
	if national == "" {
		return false
	}
	c := loadPhoneTable()[cc]
	if c == nil {
		return false
	}
	c.compile()
	for _, f := range c.formats {
		if !f.re.MatchString(national) {
			continue
		}
		if len(f.leading) == 0 {
			return true
		}
		for _, l := range f.leading {
			if l.MatchString(national) {
				return true
			}
		}
	}
	okLen := false
	for _, n := range c.Lengths {
		if n == len(national) {
			okLen = true
			break
		}
	}
	return okLen && c.mobile != nil && c.mobile.MatchString(national)
}

func validInternationalNumber(digits string) bool {
	if len(digits) < 7 || len(digits) > 15 {
		return false
	}
	cc := findCallingCode(digits)
	return validNationalNumber(cc, digits[len(cc):])
}

var (
	errPairPhoneEmpty   = errors.New("Enter your phone number.")
	errPairPhoneInvalid = errors.New("That doesn't look like a valid phone number for the selected country. Check the country and the number.")
)

// normalizePairPhone turns whatever was typed or pasted into the full
// international digits WhatsApp wants. selectedCC is the calling code of the
// country picked in the login window. Beyond WA Web's rules (digits only, then
// validate against the country table) it also takes "00" international
// prefixes and a national trunk "0" typed after the country code, because a
// number that validates once those are removed is unambiguous.
func normalizePairPhone(raw, selectedCC string) (string, error) {
	var digits strings.Builder
	trimmed := strings.TrimSpace(raw)
	plus := strings.HasPrefix(trimmed, "+")
	for _, r := range raw {
		switch {
		case r >= '0' && r <= '9':
			digits.WriteRune(r)
		case r >= 0x0660 && r <= 0x0669:
			digits.WriteRune('0' + (r - 0x0660))
		case r >= 0x06F0 && r <= 0x06F9:
			digits.WriteRune('0' + (r - 0x06F0))
		case r >= 0xFF10 && r <= 0xFF19:
			digits.WriteRune('0' + (r - 0xFF10))
		}
	}
	d := digits.String()
	if d == "" {
		return "", errPairPhoneEmpty
	}
	international := plus
	if !plus && strings.HasPrefix(d, "00") {
		international = true
		d = strings.TrimLeft(d, "0")
	}
	var candidates []string
	add := func(s string) {
		if s != "" {
			candidates = append(candidates, s)
		}
	}
	// After the country code the number may start with the national trunk digit.
	trunkless := func(full string) string {
		if selectedCC != "" && strings.HasPrefix(full, selectedCC+"0") {
			return selectedCC + strings.TrimLeft(full[len(selectedCC):], "0")
		}
		return ""
	}
	if international {
		add(d)
		add(trunkless(d))
	} else {
		if selectedCC != "" && strings.HasPrefix(d, selectedCC) {
			add(d)
			add(trunkless(d))
		}
		if selectedCC != "" {
			add(selectedCC + d)
			if strings.HasPrefix(d, "0") {
				add(selectedCC + strings.TrimLeft(d, "0"))
			}
		}
		add(d)
	}
	for _, c := range candidates {
		if validInternationalNumber(c) {
			return c, nil
		}
	}
	return "", errPairPhoneInvalid
}
