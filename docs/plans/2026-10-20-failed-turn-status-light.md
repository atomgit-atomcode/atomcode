# Failed turn status light

The title light is a TUI projection, owned by Moment and Host. Reuse native
TurnEnd stop reasons for replay and runtime TurnComplete for failures without a
committed terminal fact. No runtime lifecycle or persistent format changes.

Idle/normal completion stays green; active work, pending starts and automatic
goal recovery stay yellow; unsuccessful terminal reasons and true goal pauses
stay red until the next turn. Explicit cancellation remains neutral. Questions
retain their red attention state. Reset the outcome on view changes and replay
the selected conversation, so another session never inherits the failure light.

Validate replay, view reset, failure-to-retry-to-success, goal recovery and
terminal events without committed facts.
