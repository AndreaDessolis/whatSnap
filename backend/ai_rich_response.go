package main

import (
	"encoding/json"
	"log"
	"net/url"
	"strings"

	"go.mau.fi/whatsmeow/proto/waAICommonDeprecated"
	"go.mau.fi/whatsmeow/proto/waE2E"
)

// Meta AI (Muse) replies carry their content as JSON in unifiedResponse. This
// file turns that JSON into plain text the way WA Web's
// getPlainTextFromUnifiedResponse does, plus what WA Web draws that has no
// text of its own: inline links and citations, source lists, follow-up
// suggestions, files and images. It reads the JSON generically so primitive
// types WhatsApp adds later are skipped instead of breaking the whole reply.

type unifiedResponse struct {
	Sections        []unifiedSection  `json:"sections"`
	FooterSections  []unifiedSection  `json:"footer_sections"`
	EmbeddedScreens []json.RawMessage `json:"embedded_screens"`
	NestedResponses []unifiedResponse `json:"nested_responses"`
}

type unifiedSection struct {
	ViewModel struct {
		Primitive  json.RawMessage   `json:"primitive"`
		Primitives []json.RawMessage `json:"primitives"`
	} `json:"view_model"`
}

type unifiedEntity struct {
	Key      string `json:"key"`
	Metadata struct {
		TypeName        string          `json:"__typename"`
		URL             string          `json:"url"`
		DisplayName     string          `json:"display_name"`
		DeepLinkURL     string          `json:"deeplink_url"`
		Text            string          `json:"text"`
		ReferenceURL    string          `json:"reference_url"`
		ReferenceName   string          `json:"reference_display_name"`
		ReferenceTitle  string          `json:"reference_title"`
		LatexExpression string          `json:"latex_expression"`
		Sources         []unifiedSource `json:"sources"`
	} `json:"metadata"`
}

type unifiedSource struct {
	URL         string `json:"source_url"`
	DisplayName string `json:"source_display_name"`
	Subtitle    string `json:"source_subtitle"`
}

type unifiedPrimitive struct {
	TypeName       string          `json:"__typename"`
	Text           string          `json:"text"`
	Title          string          `json:"title"`
	PromptText     string          `json:"prompt_text"`
	ReelsTitle     string          `json:"reels_title"`
	ImagineType    string          `json:"imagine_type"`
	LatexExpr      string          `json:"latex_expression"`
	InlineEntities []unifiedEntity `json:"inline_entities"`
	CodeBlocks     []struct {
		Content string `json:"content"`
	} `json:"code_blocks"`
	Rows []struct {
		Cells []string `json:"cells"`
	} `json:"rows"`
	Item struct {
		LatexExpression string `json:"latex_expression"`
	} `json:"item"`
}

func isHTTPURL(raw string) bool {
	u, err := url.Parse(raw)
	return err == nil && (u.Scheme == "http" || u.Scheme == "https") && u.Host != ""
}

// unifiedEntityText replaces the {{key}}text{{/key}} spans of a markdown
// primitive: links keep their label and gain their address, citations show
// the name of their source, everything else keeps the wrapped text.
func unifiedEntityText(text string, entities []unifiedEntity) string {
	var out strings.Builder
	for {
		start := strings.Index(text, "{{")
		if start < 0 {
			break
		}
		keyEnd := strings.Index(text[start:], "}}")
		key := ""
		if keyEnd > 2 {
			key = text[start+2 : start+keyEnd]
		}
		closeTag := "{{/" + key + "}}"
		innerStart := start + keyEnd + 2
		innerLen := strings.Index(text[min(innerStart, len(text)):], closeTag)
		if key == "" || strings.ContainsAny(key, " \t\r\n{}/") || innerLen < 0 {
			out.WriteString(text[:start+2])
			text = text[start+2:]
			continue
		}
		out.WriteString(text[:start])
		out.WriteString(unifiedEntityLabel(key, text[innerStart:innerStart+innerLen], entities))
		text = text[innerStart+innerLen+len(closeTag):]
	}
	out.WriteString(text)
	return out.String()
}

func unifiedEntityLabel(key, inner string, entities []unifiedEntity) string {
	for _, ent := range entities {
		if ent.Key != key {
			continue
		}
		meta := ent.Metadata
		switch meta.TypeName {
		case "GenAIInlineLinkItem":
			label := strings.TrimSpace(meta.DisplayName)
			if label == "" {
				label = strings.TrimSpace(inner)
			}
			if isHTTPURL(meta.URL) && label != meta.URL {
				return label + " (" + meta.URL + ")"
			}
			return label
		case "GenAIDeeplinkItem", "GenAIDeepLinkItem":
			label := strings.TrimSpace(meta.Text)
			if label == "" {
				label = strings.TrimSpace(inner)
			}
			if isHTTPURL(meta.DeepLinkURL) && label != meta.DeepLinkURL {
				return label + " (" + meta.DeepLinkURL + ")"
			}
			return label
		case "GenAISearchCitationItem":
			name := ""
			if len(meta.Sources) > 0 {
				name = meta.Sources[0].DisplayName
			}
			if name == "" {
				name = meta.ReferenceName
			}
			if name == "" {
				name = meta.ReferenceTitle
			}
			if name = strings.TrimSpace(name); name != "" {
				return "[" + name + "]"
			}
		case "GenAILatexItem":
			if meta.LatexExpression != "" {
				return meta.LatexExpression
			}
		}
		break
	}
	return inner
}

// unifiedPrimitiveText mirrors WA Web's per-primitive plain text. follow
// receives suggested follow-up prompts, which are listed after the body.
func unifiedPrimitiveText(raw json.RawMessage, follow *[]string) string {
	if len(raw) == 0 {
		return ""
	}
	var p unifiedPrimitive
	if err := json.Unmarshal(raw, &p); err != nil {
		return ""
	}
	switch p.TypeName {
	case "GenAIMarkdownTextUXPrimitive":
		return unifiedEntityText(p.Text, p.InlineEntities)
	case "FOATextPrimitive":
		return unifiedEntityText(p.Text, nil)
	case "GenAIMetadataTextPrimitive":
		return p.Text
	case "GenAICodeUXPrimitive":
		var code strings.Builder
		for _, block := range p.CodeBlocks {
			code.WriteString(block.Content)
		}
		return code.String()
	case "GenATableUXPrimitive":
		rows := make([]string, 0, len(p.Rows))
		for _, row := range p.Rows {
			rows = append(rows, strings.Join(row.Cells, " | "))
		}
		return strings.Join(rows, "\n")
	case "GenAIBotProgressStatusPrimitive", "GenAIBotThinkingStatusPrimitive", "GenAIProductItemCardPrimitive":
		return p.Title
	case "GenAILatexUXPrimitive":
		if p.Item.LatexExpression != "" {
			return p.Item.LatexExpression
		}
		return p.LatexExpr
	case "GenAIFollowUpSuggestionPillPrimitive":
		if s := strings.TrimSpace(p.PromptText); s != "" {
			*follow = append(*follow, s)
		}
	case "GenAIFilePrimitive":
		return strings.TrimSpace("[File] " + p.Title)
	case "GenAIImagePrimitive":
		return "[Image]"
	case "GenAIImaginePrimitive":
		if p.ImagineType == "ANIMATE" {
			return "[Video]"
		}
		return "[Image]"
	case "GenAIReelPrimitive":
		return strings.TrimSpace("[Reel] " + p.ReelsTitle)
	}
	return ""
}

func unifiedSectionLines(sections []unifiedSection, follow *[]string) []string {
	var lines []string
	for _, section := range sections {
		var prims []json.RawMessage
		if len(section.ViewModel.Primitive) > 0 {
			prims = append(prims, section.ViewModel.Primitive)
		}
		prims = append(prims, section.ViewModel.Primitives...)
		var texts []string
		for _, prim := range prims {
			if s := unifiedPrimitiveText(prim, follow); s != "" {
				texts = append(texts, s)
			}
		}
		if len(texts) > 0 {
			lines = append(lines, strings.Join(texts, "\n"))
		}
	}
	return lines
}

// unifiedSources collects the list shown behind WA Web's sources button:
// every source with an address inside the embedded screens, once per address.
func unifiedSources(screens []json.RawMessage) []unifiedSource {
	var out []unifiedSource
	seen := map[string]bool{}
	for _, screen := range screens {
		var parsed struct {
			Content []struct {
				ViewModel struct {
					Sources []unifiedSource `json:"sources"`
				} `json:"viewModel"`
			} `json:"content"`
		}
		if json.Unmarshal(screen, &parsed) != nil {
			continue
		}
		for _, content := range parsed.Content {
			for _, src := range content.ViewModel.Sources {
				if src.URL == "" || seen[src.URL] {
					continue
				}
				seen[src.URL] = true
				out = append(out, src)
			}
		}
	}
	return out
}

func unifiedLines(resp unifiedResponse, follow *[]string, sources *[]unifiedSource) []string {
	lines := unifiedSectionLines(resp.Sections, follow)
	lines = append(lines, unifiedSectionLines(resp.FooterSections, follow)...)
	*sources = append(*sources, unifiedSources(resp.EmbeddedScreens)...)
	for _, nested := range resp.NestedResponses {
		lines = append(lines, unifiedLines(nested, follow, sources)...)
	}
	return lines
}

// unifiedResponseText renders a unifiedResponse JSON payload as plain text.
func unifiedResponseText(data []byte) string {
	var resp unifiedResponse
	if err := json.Unmarshal(data, &resp); err != nil {
		log.Printf("Failed to parse rich response JSON: %v", err)
		return ""
	}
	var follow []string
	var sources []unifiedSource
	lines := unifiedLines(resp, &follow, &sources)
	if len(sources) > 0 {
		var list []string
		seen := map[string]bool{}
		for _, src := range sources {
			if seen[src.URL] {
				continue
			}
			seen[src.URL] = true
			name := strings.TrimSpace(src.DisplayName)
			if name == "" {
				name = strings.TrimSpace(src.Subtitle)
			}
			if name == "" || name == src.URL {
				list = append(list, "- "+src.URL)
			} else {
				list = append(list, "- "+name+" ("+src.URL+")")
			}
		}
		lines = append(lines, "Sources:\n"+strings.Join(list, "\n"))
	}
	if len(follow) > 0 {
		lines = append(lines, "Suggested follow-ups:\n- "+strings.Join(follow, "\n- "))
	}
	return strings.Join(lines, "\n")
}

// richResponseText turns a Meta AI reply into plain text. Current replies
// carry their content as JSON in unifiedResponse (sections of view models
// holding markdown, code, tables and so on); older ones used submessages.
func richResponseText(rich *waE2E.AIRichResponseMessage) string {
	if rich == nil {
		return ""
	}
	text := ""
	if data := rich.GetUnifiedResponse().GetData(); len(data) > 0 {
		text = strings.TrimSpace(unifiedResponseText(data))
	}
	if text == "" {
		text = strings.Join(richResponseSubmessageText(rich.GetSubmessages()), "\n")
	}
	return aiMarkdownToPlain(text)
}

func richResponseSubmessageText(subs []*waAICommonDeprecated.AIRichResponseSubMessage) []string {
	var parts []string
	add := func(s string) {
		if s = strings.TrimSpace(s); s != "" {
			parts = append(parts, s)
		}
	}
	for _, sub := range subs {
		switch {
		case sub.GetMessageText() != "":
			add(sub.GetMessageText())
		case sub.GetCodeMetadata() != nil:
			var code strings.Builder
			for _, block := range sub.GetCodeMetadata().GetCodeBlocks() {
				code.WriteString(block.GetCodeContent())
			}
			add(code.String())
		case sub.GetTableMetadata() != nil:
			table := sub.GetTableMetadata()
			add(table.GetTitle())
			for _, row := range table.GetRows() {
				add(strings.Join(row.GetItems(), ", "))
			}
		case sub.GetLatexMetadata() != nil:
			add(sub.GetLatexMetadata().GetText())
		}
	}
	return parts
}
