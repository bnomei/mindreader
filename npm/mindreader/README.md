# @bnomei/mindreader

The npm launcher for [Mindreader](https://github.com/bnomei/mindreader), selective prospective
memory for AI agents. Instead of archiving conversations, Mindreader keeps deliberately chosen
facts, decisions, preferences, constraints, and relationships in an inspectable Neo4j graph.

Use it when an agent needs precise knowledge that survives across sessions and can be corrected,
retired, scoped, or time-qualified. Do not use it as the only store for complete transcripts,
exact quotations, or arbitrary questions about old conversations.

Optional semantic recall lets an agent approach the graph with approximate intent rather than an
exact query. It builds expiring associative trails to grounded facts, keeps useful trails warm
through reuse, and may include nearby graph context without treating repetition as truth.

```bash
npx -y @bnomei/mindreader@0.6.0 --version
```

The launcher downloads the matching checksummed GitHub Release binary on first use and then
reuses its versioned local cache. It supports GNU/glibc Linux and macOS on x64 and arm64, plus
x64 Windows. See the main repository for installation, configuration, MCP setup, and the complete
agent integration guide.
