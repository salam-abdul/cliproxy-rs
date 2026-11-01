# Writing

For docs, comments, commit messages and reports. The basics (lead with the point, no filler, no em dashes) are in `AGENTS.md`.

## Prose

- Name the mechanism or the number, not a feeling. "Usage snapshots expire after 30 seconds" beats "usage stays fresh". If a sentence could appear unchanged in another project's docs, cut it.
- Write "is" or "has", not serves as or stands as. Use, not utilize.
- Cut puffery (robust, seamless, pivotal, comprehensive), "not just X, but Y", abstract metaphor nouns (harness, scaffolding, north star), decorative emoji and a bold lead-in that restates its line.
- Use one term for one concept, matching the server and the dashboard: an account is a signed-in credential, a client key is what a tool sends, the management key opens the dashboard.
- Before you send, ask what makes the text read as machine-written, and fix that.

## Comments

- Delete: a paraphrase of the next line, divider banners, migration history ("used to", "previously"), a doc comment that repeats the name, a plan or ticket reference.
- Keep: why a non-obvious choice beat the obvious one, an invariant a reader cannot infer, a fail-closed contract, a `// SAFETY:` justification, a `ponytail:` ceiling with its upgrade path.
- State each fact once, at the site that owns it. A caller links to the owner instead of restating it.

## Docs

- Operation docs live only in the files `AGENTS.md` lists. Each behavior has one owning page; other pages link to it.
- Docs state what a user can do, the defaults and limits that apply, and what fails closed. Types, fields and signatures belong in rustdoc.
- Use tables for settings, limits, ports and paths. Use prose for behavior.
- Examples use synthetic names, keys and paths.

## Commit messages

- The subject is one imperative sentence that names the behavior, as in the existing history: `Bound request log capture and zstd decoding`.
- The body, when needed, says why and what was verified, including any suite not run.
