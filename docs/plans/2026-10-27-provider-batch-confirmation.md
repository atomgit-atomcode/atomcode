# Provider batch confirmation

Baseline: 0d1548f23. Existing unrelated compaction and loop-policy work is retained.

The discovery picker continues to use Space/click to check models. On confirming
multiple checks, show a bounded, scrollable list of model cards, each with its own
context/output limits, vision, supported effort levels and default effort. Missing
metadata is explicitly unknown, never inherited from a different model.

Checks retain selection order. The first checked model is initially the model to
use. On the confirmation page Up/Down (or Tab) only move focus; Space or clicking
a model header makes that model the one to use. Parameter rows do not activate
anything. Enter saves the entire batch and uses the marked model when this is a
connect form. Ctrl+S saves all without switching. Escape cancels. Batch review
consumes text/paste rather than editing invisible single-model fields. Single
selection keeps the existing form and editing controls.

ModelForm owns temporary selection and focus. The selected rows retain their
individual discovery metadata. Existing SaveModels writes atomically through the
provider port, with exactly one use_now draft; failed saves restore both the
selection and the marked model. No persistence or runtime protocol changes.

Render and mouse hit testing share the same layout. Large batches scroll by focus
rather than expanding beyond the panel. Tests cover deterministic defaults,
navigation without switching, per-model metadata, save-only behavior, explicit
mouse selection, save retry, unknown values, scrolling, and narrow widths.
