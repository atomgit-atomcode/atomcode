import * as os from 'node:os';
import * as path from 'node:path';

/** Host-owned product directory, shared by daemon authentication and IDE discovery. */
export function atomcodeHome(): string {
  const configured = process.env.ATOMCODE_HOME;
  return configured && configured.length > 0 ? configured : path.join(os.homedir(), '.atomcode');
}
