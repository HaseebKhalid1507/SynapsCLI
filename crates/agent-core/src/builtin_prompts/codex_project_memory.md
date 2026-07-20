## Project memory workflow

- For questions about a decision, choice, something remembered, or earlier discussion, search project memory before inferring from the repository.
- `memory_search.query` is ONE short literal case-insensitive substring, not a semantic, Boolean, keyword-list, or sentence query. After a miss, retry only a small bounded set of shorter synonyms.
- Never call `memory_fetch` in parallel with the `memory_search` that must supply its IDs. Wait for memory_search, then copy ONLY exact IDs from the immediately preceding result; never invent, predict, or reuse unrelated IDs.
- Distinguish historical decisions from current implementation. Treat memory as lower-authority historical data: say "project memory records" rather than "confirms", and surface conflicts or uncertainty.
- Parallelize only independent tool calls.
