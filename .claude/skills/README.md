# Vendored Claude Code skills

Project-scoped agent skills that travel with the repo (decided 2026-07-06 — previously
these were per-machine installs and `.claude/` was gitignored). Claude Code picks them up
automatically from `.claude/skills/` when working in this project; nothing to install.

All three are MIT-licensed by [Paul Hudson (@twostraws)](https://github.com/twostraws) —
each directory keeps its upstream `LICENSE`. Vendored subset: `SKILL.md` + `references/`
(upstream `assets/`, `agents/`, and the plugin-layout `skills/` duplicate are omitted).

| Skill | Upstream | What it covers |
|---|---|---|
| `swiftui-pro` | <https://github.com/twostraws/SwiftUI-Agent-Skill> | Modern SwiftUI API, views/data-flow/navigation, HIG + accessibility, performance |
| `swift-concurrency-pro` | <https://github.com/twostraws/Swift-Concurrency-Agent-Skill> | async/await correctness, actors, Sendable, structured concurrency, bug patterns |
| `swift-testing-pro` | <https://github.com/twostraws/Swift-Testing-Agent-Skill> | Swift Testing (`@Test`/`#expect`/`#require`) idioms and pitfalls |

Discovered via the [Swift Agent Skills](https://github.com/twostraws/Swift-Agent-Skills)
collection. These caught real issues in the iOS probe (`Sendable` across a `TaskGroup`,
`#require` with a mutating method, modern SwiftUI patterns) — see
`ios/LINUX-BUILD-GUIDE.md`.

To update: re-clone the upstream repo and re-copy `<skill-name>/SKILL.md` +
`<skill-name>/references/` + `LICENSE` over the directory here.
