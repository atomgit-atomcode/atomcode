import type { AuthStatusResponse } from '../daemon/types';

export type AuthDisplayState = 'signed_in' | 'expired' | 'signed_out';

export function classifyAuthDisplayState(
  auth: Pick<AuthStatusResponse, 'logged_in' | 'expired'> | undefined,
): AuthDisplayState {
  if (auth?.expired) return 'expired';
  return auth?.logged_in ? 'signed_in' : 'signed_out';
}
