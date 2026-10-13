# ADR 0026: mdBook Documentation Site

Status: Accepted

Date: 2026-06-18

## Context

Truss has accumulated architecture docs, implementation plans, go-to-market
notes, and ADRs as loose Markdown under `docs/`. That keeps authoring simple, but
it makes the material hard to browse, search, and publish. Matt explicitly asked
for documentation plus a way to view it in the browser with examples.

The site should not fork the content. Existing design docs and ADRs remain the
source of truth, and example pages should render the checked-in files under
`examples/` rather than copying TOML into a second place.

## Decision

Use mdBook as the documentation site generator.

The repository now treats `docs/` as the mdBook root:

- `docs/book.toml` configures the book and emits generated HTML to top-level
  `book/`.
- `docs/SUMMARY.md` is the navigation tree for architecture docs, configuration,
  examples, go-to-market/evaluation notes, and ADRs.
- `docs/introduction.md` is the landing page.
- `docs/examples/*.md` are thin wrappers that explain each example and include the
  live source files from `examples/`.

GitHub Pages publishing is handled by `.github/workflows/docs.yml`. It builds the
book with `mdbook build docs`, uploads the top-level `book/` artifact, and deploys
it with `actions/deploy-pages` on pushes to `main`. The workflow is secret-free;
Matt still needs to enable GitHub Pages for the repository and select GitHub
Actions as the source.

## Consequences

Truss gets a searchable, browsable, Rust-native documentation site with a single
binary local preview path:

```bash
cargo install mdbook
mdbook serve docs --open
```

Generated HTML is ignored and not committed. Existing docs stay Markdown-native,
so future design work can keep using the same files while the book navigation
decides how readers discover them.

mdBook is intentionally a documentation site, not a product app. Richer product
UI, generated diagrams, verified playbooks, and per-agent MCP surfaces remain
separate milestones.
