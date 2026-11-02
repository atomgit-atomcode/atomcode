// Which scheme a stored git credential is written under.
//
// git's `store` helper matches the stored prefix against the remote URL, so this
// value decides whether a token may travel in cleartext — it is the one place in
// the credential form where a wrong default costs more than a broken request.
// Both decisions live here rather than inside the panel so they can be pinned
// without a DOM (the test runner here has none).

export type CredentialScheme = 'https' | 'http';

/**
 * What the credential form saves.
 *
 * `https` unless the person ticked the box for this host. Not inferred from what
 * they typed into the host field: someone pasting an `http://` remote should
 * still have to say out loud that they accept a cleartext token.
 */
export function schemeFor(insecure: boolean): CredentialScheme {
  return insecure ? 'http' : 'https';
}

/**
 * Whether a *stored* scheme deserves the "token goes out in cleartext" mark.
 *
 * Tolerant on purpose: the value comes from the daemon's credential file, which a
 * person may have edited by hand, and a stray `HTTP` must not read as safe.
 */
export function needsCleartextWarning(scheme: string): boolean {
  return scheme.trim().toLowerCase() === 'http';
}