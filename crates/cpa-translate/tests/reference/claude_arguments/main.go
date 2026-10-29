// Re-record existing synthetic inputs against CLIProxyAPI b2fda98e (includes e1fb2521).
// Copy under that Go module's cmd directory, then go run ./cmd/claude_arguments fixture.json.
package main

import (
	"encoding/json"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/util"
	"os"
)

func main() {
	b, e := os.ReadFile(os.Args[1])
	if e != nil {
		panic(e)
	}
	var rows []map[string]string
	if e = json.Unmarshal(b, &rows); e != nil {
		panic(e)
	}
	for _, r := range rows {
		r["want"] = util.CleanClaudeToolArgumentsWithTranslated(r["raw"], r["tool"], []byte(r["original"]), []byte(r["translated"]))
	}
	b, e = json.MarshalIndent(rows, "", " ")
	if e != nil {
		panic(e)
	}
	if e = os.WriteFile(os.Args[1], append(b, '\n'), 0644); e != nil {
		panic(e)
	}
}
