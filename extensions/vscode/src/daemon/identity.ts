export interface RunningDaemonIdentity {
  version: string;
  binary_hash?: string;
}

export interface ExpectedDaemonIdentity {
  version?: string;
  binaryHash?: string;
}

export function daemonIdentityMatches(
  running: RunningDaemonIdentity,
  expected: ExpectedDaemonIdentity,
): boolean {
  if (expected.binaryHash && running.binary_hash !== expected.binaryHash) {
    return false;
  }
  if (expected.version && running.version !== expected.version) {
    return false;
  }
  return true;
}
