# Tonans Engineering Contract

Tonans is a local-first deterministic microgrid scenario workbench. It is planning software, not live grid control, a protection study, an interconnection approval tool, or a substitute for licensed engineering judgment.

Before changing code, read `PRODUCT-SPEC.md`. Keep one explicit energy-balance model, expose every material assumption, preserve units and provenance, and reject ambiguous or malformed inputs. No telemetry, remote assets, device commands, SCADA, inverter control, or hidden optimization.

Release evidence must include formatting, lint, tests, dependency audit, a production build, a packaged-binary smoke test, golden hand calculations, deterministic/property invariants, import-abuse tests, browser smoke, SBOM/notices/checksums, and a fresh-context review. Never publish from this repository until the pre-publication gates are closed.
