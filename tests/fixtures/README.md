# Role response schema baseline

`role-response-schema-contracts.json` freezes eight canonical role schemas and their provider projections from before the protocol schema refactor.

Provenance: existing typed role definitions, `close_flattened_schema`, mechanical provider projection, and locked schemars 1.2.3. Capture was observed successful with:

```text
cargo test -q --locked --lib protocol::tests::capture_pre_refactor_schema_contracts -- --exact --nocapture
```

The temporary emitter was removed after capture. Do not regenerate this frozen baseline from current response types. Validation tests compare canonical schemas and constraints against this file. Historical provider snapshots contain `$ref` sibling descriptions rejected by the live API; comparisons omit only those annotations. Provider discriminator ordering is tested on serialized requests because JSON object equality does not preserve wire order. This fixture is offline contract evidence, not proof of live-provider compatibility.
