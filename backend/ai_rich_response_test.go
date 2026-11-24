package main

import "testing"

func TestUnifiedResponseText(t *testing.T) {
	data := []byte(`{"response_id":"r","sections":[
	{"view_model":{"__typename":"GenAISingleLayoutViewModel","primitive":{"__typename":"GenAIMarkdownTextUXPrimitive","text":"See {{a}}the docs{{/a}} and {{c}}1{{/c}}.","inline_entities":[
	  {"key":"a","metadata":{"__typename":"GenAIInlineLinkItem","url":"https://example.com/x","display_name":"Docs"}},
	  {"key":"c","metadata":{"__typename":"GenAISearchCitationItem","sources":[{"source_url":"https://news.example/a","source_display_name":"News"}]}}]}}},
	{"view_model":{"__typename":"GenAIGridLayoutViewModel","primitives":[{"__typename":"GenATableUXPrimitive","rows":[{"cells":["a","b"]},{"cells":["1","2"]}]},{"__typename":"GenAIImagePrimitive"},{"__typename":"GenAIUnknownFuture"}]}},
	{"view_model":{"__typename":"GenAISingleLayoutViewModel","primitive":{"__typename":"GenAIFollowUpSuggestionPillPrimitive","prompt_text":"Tell me more"}}}],
	"embedded_screens":[{"content":[{"viewModel":{"sources":[{"source_url":"https://news.example/a","source_display_name":"News"},{"source_url":"https://news.example/a"}]}}]}]}`)
	want := "See Docs (https://example.com/x) and [News].\na | b\n1 | 2\n[Image]\nSources:\n- News (https://news.example/a)\nSuggested follow-ups:\n- Tell me more"
	if got := unifiedResponseText(data); got != want {
		t.Fatalf("got:\n%s\nwant:\n%s", got, want)
	}
}

func TestUnifiedEntityTextEdges(t *testing.T) {
	for in, want := range map[string]string{
		"plain":                        "plain",
		"{{a}}x{{/a}} {{b}}y":          "x {{b}}y",
		"{{ {{a}}x{{/a}}":              "{{ x",
		"{{a}}unclosed":                "{{a}}unclosed",
		"{{a}}one{{/a}}{{a}}two{{/a}}": "onetwo",
	} {
		if got := unifiedEntityText(in, nil); got != want {
			t.Errorf("%q: got %q want %q", in, got, want)
		}
	}
}
