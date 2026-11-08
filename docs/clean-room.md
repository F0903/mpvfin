# Clean-room implementation policy

`mpvfin` is an independent implementation.

## Allowed implementation inputs

- mpv's public manual, public C headers, and official examples;
- Jellyfin's published OpenAPI document, server API definitions, and official
  SDK documentation;
- black-box observations made against a Jellyfin server owned by a developer;
- behavior described by users or written as original interoperability tests.

## Prohibited implementation inputs

- copying, translating, or mechanically adapting code from
  `jellyfin-mpv-shim`;
- importing that project, vendoring it, or making it a build/test dependency;
- copying its internal data structures, test fixtures, comments, or module
  layout.

The existing shim may be used only as a high-level behavioral reference: for
example, confirming that a feature exists or comparing externally observable
results. When behavior is unclear, this project derives the contract from the
official APIs or records a fresh black-box test.

Every contribution should be explainable from an official public contract or
from original design work in this repository.

