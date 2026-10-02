# Chisle Audit

Scan the target (a diff, a file, or the repo tree) and report what to cut, on
both axes. One-shot. Read-only: never edit, never write a flag, never apply fixes.

## Scope

- No argument → audit the current `git diff` (staged + unstaged). Empty diff → audit `HEAD~1..HEAD`.
- A path → audit that file or directory.
- "repo" / "whole repo" → walk the tree (skip vendored/generated/`node_modules`/`dist`/lockfiles).

## What to flag

**Code (the YAGNI axis):**
- Reinvented stdlib (hand-rolled debounce, deep-clone, groupBy, retry loop, date math)
- Abstraction with one implementation (interface/factory/wrapper for a single case)
- New dependency for what a few lines or an installed dep already covers
- Config/option/flag that never varies
- Speculative "for later" scaffolding with no current caller
- Verbose code where a native platform feature (CSS, DB constraint, `<input type>`) does it

**Prose (the compression axis), the half a code-only auditor misses:**
- Comments that restate the code (`i += 1  // increment i`)
- Docstrings/READMEs padded with filler, hedging, ceremony, or duplicated content
- Multi-paragraph explanations where one tight sentence carries the meaning
- Decorative tables/emoji/headings that add tokens, not information
- Dead prose: TODO graveyards, stale "see also" links, obsolete sections

## What NOT to flag

Input validation at trust boundaries, error handling that prevents data loss,
security, accessibility, deliberate `// chisle:` / `// ponytail:` shortcuts already
documented, or domain comments that explain *why* (not *what*).

## Output

One ranked list, biggest cut first. One finding per line. No preamble, no praise.

```
path:line  [code|prose]  <what's bloated> → <the lean replacement>. (~N lines/tokens)
```

End with a two-line summary:

```
N findings: X code, Y prose. Est. removable: ~A lines code, ~B lines prose.
Biggest win: <the single highest-impact cut>.
```

Be honest about uncertainty: mark a finding `(check)` if cutting it might lose
behavior you can't verify from the snippet. Lean toward fewer, high-confidence
findings over a long speculative list.
