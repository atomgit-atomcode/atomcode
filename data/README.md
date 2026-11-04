# Model card data

`model-cards.json` is an offline snapshot for twelve explicit model identities from the usage tables. It supplies evaluation data only; AtomCode's existing model configuration remains authoritative for context, image input, tool support and other capabilities. The TUI and WebUI viewers read this same embedded catalogue; viewing never switches the active model.

## Regenerate

Use Node >= 22.18 with native TypeScript stripping, or Node >= 24, and a local whatmodel checkout. No network or installed npm dependencies are required by this exporter.

```sh
node --experimental-strip-types scripts/export-model-cards.mjs \
  --whatmodel /path/to/whatmodel \
  --output data/model-cards.json
```

Edit `COMMON_MODELS` in the exporter to add a verified whatmodel identity and `WIRE_NAMES` for confirmed wire identifiers, then regenerate. Both frontends consume the resulting file without per-model code changes. Unknown IDs, duplicate selection IDs and unreviewed evaluation sources fail before replacing the existing output. The output records the upstream commit, input hashes and upstream data update time. Repeated export of the same inputs is deterministic.

## Read

- `models[whatmodelId]`: model identity, original valid evaluation records, task performance and original strong/weak task results. Empty/null fields are not guessed or filled.
- `benchmarks[evaluation.bench]`: metric definition, units and direction. Accuracy fractions, Elo, error ratios and linear units must not be treated as the same scale.
- `sources[evaluation.source_id]`: original reviewed source information, credits and licensing.
- `task_definitions`: original whatmodel task descriptions and their reviewed benchmark membership.
- `upstream` and `attribution`: provenance and data notices for the data-information view. Source links do not require badges or links on the primary model card.

Task fields `g`, `pm`, `n`, `rank` and `total` preserve whatmodel's definitions. They are computed by whatmodel on its **complete** population before selecting these twelve cards. `comparison_set` identifies the population/scale; only compare scores in the same task and comparison set. Missing rank or best/worst task is retained as null. The exporter does not invent task recommendations, normalize all tests into a new score, or require five results where upstream has fewer.

## Product entry points

- TUI: open `/model`, focus a model and press F2. Up/Down or PgUp/PgDn scroll; Left/Right browse catalogue models, Tab cycles a second card, `d` toggles data notes, Esc returns to the selector. The wheel scrolls the card. All these actions are read-only.
- WebUI: open the model selector and click the information button next to a model. The dialog supports two-model comparison, keyboard dismissal and mobile layouts. Sources and attribution appear only under Data notes.
- The authenticated `/model-cards` endpoint serves the embedded snapshot only on demand. `/models` adds `model_card_id`, configured `context_window` and `supports_vision`. It never substitutes whatmodel's context or capability declarations.

## Identity boundary

The `models` keys are **whatmodel IDs**. `wire_model_bindings` is a separate, explicit list of reviewed wire identities; both frontends use the same shared matcher. For example, `glm-5-3-flash` is a catalogue identity; `glm5.3-flash`, a gateway alias or a custom profile name is matched only through a reviewed `wire_model_bindings` entry; a custom profile name is never used for matching. DeepSeek official floating aliases must not be mapped to an older checkpoint merely because the label contains `v4`.

The initial snapshot does not bind private/local endpoints, `AtomGit-*`, `deepseek-flash`, undated channel aliases, `glm5.3-flash-pro`, `glm-5.1-fallback`, `ark-code-latest`, `stealth/space-bunny-alpha`, `sensenova-6.8-flash-lite`, `agnes-2.5-flash`, `fledge-alpha-free`, `lumen-alpha` or `doubao-seed-evolving*`. This is an intentional absence of a verified mapping, not a claim about those models' abilities.

## Data notices

whatmodel's numerical compilation and derived scores carry **CC BY-SA 4.0**, separately from its MIT code licence and AtomCode's application code. The snapshot retains its data notice, source-specific ledger entries, and a commit-pinned link to full upstream notices. Underlying sources retain their own licences. This file is a selected export with no endorsement by whatmodel or its sources. Do not drop attribution from the data when relocating provenance out of the main card.

## Verify

```sh
node --test scripts/tests/model-cards.test.mjs
WHATMODEL_CHECKOUT=/path/to/whatmodel \
  node --experimental-strip-types --test scripts/tests/model-cards.test.mjs
```

The first command checks the checked-in snapshot without the external checkout. The second additionally verifies exact upstream score/record preservation, full-population ranks, deterministic output and failure without overwriting an existing file.


## Models with no public evaluation record

The snapshot distinguishes three states: `available` (verified wire binding and card), `no_public_data` (explicitly reviewed as absent from the current evaluation catalogue), and `unverified` (unknown or ambiguous mapping). `no_public_evaluation_models` initially records `lumen-alpha`. This says nothing about whether the model itself exists or can be used.

Both frontends keep model use available. No-public-data entries show a dedicated explanation and no synthetic card, zero scores, guessed alias, rankings or strengths. WebUI opens that explanation without a catalogue fetch; manual catalogue browsing is a separate action, and keeps the configured model's no-data state visible. TUI keeps the same explanation when browsing reference cards. Reopening does not repeatedly search upstream. If a later whatmodel checkout contains a previously unlisted name, the exporter fails and asks for explicit identity review rather than retaining a stale no-data marker.
