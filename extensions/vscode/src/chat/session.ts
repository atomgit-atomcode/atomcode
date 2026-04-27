// Simple session state tracker
export class SessionState {
  currentSessionId?: string;
  totalTokens = 0;

  reset() {
    this.currentSessionId = undefined;
    this.totalTokens = 0;
  }
}
