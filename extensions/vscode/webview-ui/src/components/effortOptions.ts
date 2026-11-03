/**
 * Effort dropdown options, built from what the selected model actually exposes.
 *
 * The daemon sends `effort_levels` per model (`low|medium|high|xhigh|max`), so a
 * model that declares `low/medium/high` must never offer `max` — the wire clamps
 * such a value anyway, which showed up as "I picked Max and nothing changed".
 * A model that declares no levels keeps every level the wire accepts.
 */

/** Every level the wire accepts, used when an endpoint declares none. */
export const FALLBACK_EFFORT_LEVELS = ['low', 'medium', 'high', 'xhigh', 'max'];

export type EffortOption = { value: string | null; label: string };

/** Display form of a level; the wire keeps it lowercase. */
export function effortLabelOf(level: string): string {
  return level.charAt(0).toUpperCase() + level.slice(1);
}

/**
 * "Default" (leave the API default in effect) first, then exactly this
 * endpoint's levels in the order the daemon reports them.
 */
export function effortOptionsFor(
  levels: string[] | undefined | null,
  defaultLabel: string,
): EffortOption[] {
  // `auto` means "send nothing", so it is never a switchable level.
  const declared = (levels ?? []).filter((level) => level && level !== 'auto');
  const shown = declared.length > 0 ? declared : FALLBACK_EFFORT_LEVELS;
  return [
    { value: null, label: defaultLabel },
    ...shown.map((level) => ({ value: level, label: effortLabelOf(level) })),
  ];
}

/** Label for the effort in force, falling back to "Default" when it is unset. */
export function effortLabelFor(
  options: EffortOption[],
  current: string | null | undefined,
  defaultLabel: string,
): string {
  return options.find((option) => option.value === current)?.label ?? defaultLabel;
}
