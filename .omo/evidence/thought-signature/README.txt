turn2: 200 with signed history; turn1: 200 tool_calls w/ embedded signature id
routing fix: agent omo.json categories rewired (mahoquot first, quotio/gemini-3.7-flash-high removed) — the user's 400 came from the quotio(8317)->CLIProxyAPI path which has the same unfixed bug in its Go binary
STREAMING E2E (post-fix, live gateway 18801):
- turn1 stream 200: tool_call id carries ~1KB embedded Gemini 3 signature (base64url after '#')
- turn2 stream 200 with signed history: finish_reason=tool_calls, correct follow-up (read a book added)
- harsh stream 200: unsigned legacy pair + signed call + text turn mixed — model produced the correct todo call, zero thought_signature errors
request-log enabled during capture (user mandate: persistent logs)
