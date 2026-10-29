// Record selected existing schema fixture rows against CLIProxyAPI b2fda98e.
// Input: fixture rows with an added pair field; output: pair, name, output.
// Apply the companion patch with git apply --unidiff-zero first.
// Run inside the reference Go module: go run ./cmd/schema_pairs inputs.json outputs.json.
package main

import (
	"context"
	"encoding/json"
	a "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/antigravity/claude"
	g "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/gemini/claude"
	o "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/gemini/openai/chat-completions"
	"os"
)

func main() {
	b, _ := os.ReadFile(os.Args[1])
	var rows []struct {
		Lines      []string `json:"lines"`
		Path       string   `json:"path"`
		Original   string   `json:"original"`
		Translated string   `json:"translated"`
		Pair       string   `json:"pair"`
		Name       string   `json:"name"`
		Model      string   `json:"model"`
		Input      string   `json:"input"`
		Stream     bool     `json:"stream"`
		Bytes      bool     `json:"bytes"`
	}
	if e := json.Unmarshal(b, &rows); e != nil {
		panic(e)
	}
	var out []map[string]any
	for _, r := range rows {
		raw := []byte(r.Input)
		if r.Bytes {
			raw = nil
			for _, c := range r.Input {
				raw = append(raw, byte(c))
			}
		}
		if r.Path == "stream" {
			var param any
			outputs := [][]string{}
			ctx := context.WithValue(context.Background(), "alt", "")
			for _, line := range r.Lines {
				var chunks [][]byte
				if r.Pair == "claude-gemini" {
					chunks = g.ConvertGeminiResponseToClaude(ctx, r.Model, []byte(r.Original), []byte(r.Translated), []byte(line), &param)
				} else {
					chunks = a.ConvertAntigravityResponseToClaude(ctx, r.Model, []byte(r.Original), []byte(r.Translated), []byte(line), &param)
				}
				texts := []string{}
				for _, chunk := range chunks {
					texts = append(texts, string(chunk))
				}
				outputs = append(outputs, texts)
			}
			out = append(out, map[string]any{"pair": r.Pair, "name": r.Name, "outputs": outputs})
			continue
		}
		var result []byte
		var e error
		switch r.Pair {
		case "claude-gemini":
			if r.Path == "non_stream" {
				result = g.ConvertGeminiResponseToClaudeNonStream(context.Background(), r.Model, []byte(r.Original), []byte(r.Translated), raw, nil)
			} else {
				result, e = g.ConvertClaudeRequestToGemini(r.Model, raw, r.Stream)
			}
		case "openai-gemini":
			result, e = o.ConvertOpenAIRequestToGemini(r.Model, raw, r.Stream)
		case "claude-antigravity":
			result, e = a.ConvertClaudeRequestToAntigravity(r.Model, raw, r.Stream)
		}
		if e != nil {
			panic(e)
		}
		text := string(result)
		if r.Bytes {
			rr := []rune{}
			for _, c := range result {
				rr = append(rr, rune(c))
			}
			text = string(rr)
		}
		out = append(out, map[string]any{"pair": r.Pair, "name": r.Name, "output": text})
	}
	b, _ = json.MarshalIndent(out, "", " ")
	if e := os.WriteFile(os.Args[2], b, 0644); e != nil {
		panic(e)
	}
}
