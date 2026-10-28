# Role response schema baseline

`role-response-schema-contracts.json` contains the eight canonical role schemas and their provider projections captured before reviewer-blocker adjudication or schema refactoring changed the response types.

Provenance: existing typed role definitions, `close_flattened_schema`, mechanical provider projection, and locked schemars 1.2.3. Capture was observed successful with:

```text
cargo test -q --locked --lib protocol::tests::capture_pre_refactor_schema_contracts -- --exact --nocapture
```

The temporary emitter was removed after capture. Do not regenerate this frozen baseline from changed response types. Existing validation contracts are compared against it; the additive adjudication branch has separate tests. The live API rejects `$ref` sibling descriptions present in the historical provider snapshots. Comparison omits only those provider annotations; canonical schemas and all validation constraints remain checked against this unchanged file. Provider discriminator-first ordering is tested on actual serialized requests, because JSON object equality does not capture wire order. These schemas are offline contract evidence, not live-provider compatibility evidence.
