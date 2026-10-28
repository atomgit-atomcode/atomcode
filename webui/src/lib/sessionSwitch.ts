/** Do not fetch a second transcript before its shared-runtime switch succeeds.
 *  Recheck navigation after awaiting: A -> B -> A must not revive B's load. */
export async function loadAfterSessionSwitch<T>(
  accepted: Promise<boolean> | undefined,
  current: () => boolean,
  load: () => Promise<T>,
): Promise<T | undefined> {
  if (accepted && !await accepted) return;
  if (!current()) return;
  return load();
}
