┌─────┬───────────────────────────────────┬──────────────────────┬──────────┬────────────────────────────────────────────────────────┐
│  #  │             Scenario              │  Who launches whom   │ Protocol │                 Key parts of the plan                  │
├─────┼───────────────────────────────────┼──────────────────────┼──────────┼────────────────────────────────────────────────────────┤
│     │ rust-bot as an MCP server for     │                      │          │                                                        │
│ 1   │ other AI tools (Claude Desktop,   │ They launch rust-bot │ MCP      │ rust_bot_chat, plus acp_list_agents/acp_run_agent;     │
│     │ Cursor, another rust-bot through  │  mcp                 │          │ built on rmcp's server side (decision 21)              │
│     │ mcpServers)                       │                      │          │                                                        │
├─────┼───────────────────────────────────┼──────────────────────┼──────────┼────────────────────────────────────────────────────────┤
│     │                                   │ Parent launches      │          │                                                        │
│ 2   │ rust-bot as a sub-agent of a      │ rust-bot acp         │ ACP      │ Child home + overlay, allowlist, project scope, locks, │
│     │ rust-bot parent                   │ --config … --overlay │          │  graceful stop, memory isolation (decisions 1–22)      │
│     │                                   │  …                   │          │                                                        │
├─────┼───────────────────────────────────┼──────────────────────┼──────────┼────────────────────────────────────────────────────────┤
│     │ rust-bot as parent (ACP client)   │ rust-bot launches    │          │                                                        │
│ 3   │ of other agents, such as Claude   │ their ACP adapter    │ ACP      │ Launch presets, project folder as cwd, AGENTS.md as    │
│     │ Code (through the Agent SDK       │ from a preset        │          │ preamble, fail-closed permissions, derived environment │
│     │ adapter) or Gemini CLI            │                      │          │                                                        │
├─────┼───────────────────────────────────┼──────────────────────┼──────────┼────────────────────────────────────────────────────────┤
│     │                                   │ The editor launches  │          │ Same agent role as scenario 2: absolute --config,      │
│ 4   │ rust-bot as an agent for ACP      │ rust-bot acp         │ ACP      │ stdout hygiene, streaming tool events, permissions to  │
│     │ editors such as Zed or JetBrains  │ (usually without an  │          │ the editor, .acp.lock on its own workspace             │
│     │                                   │ overlay)             │          │                                                        │
└─────┴───────────────────────────────────┴──────────────────────┴──────────┴────────────────────────────────────────────────────────┘


Your Zed check

1. Build the binary with cargo build. The exe is target\debug\rust-bot.exe.
2. In Zed, register a custom agent server with the absolute exe path and args acp --config <absolute path to config.json>. Check Zed's current settings format, since I couldn't verify it.
3. Open a project folder and ask it to read a file. The tool call should show live.
4. Ask it to run a shell command. Zed should ask for permission, and denying should make the agent report the denial.
5. Stop a long command from Zed. Closing the thread should end the process, which you can see in Task Manager.