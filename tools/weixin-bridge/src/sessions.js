// 每个微信用户(fromUserId)的状态：atomcode sessionId、最近 contextToken、审批 pending。
export class SessionStore {
  constructor() {
    this.map = new Map(); // fromUserId -> { sessionId?, contextToken?, pending? }
  }
  _entry(user) {
    let e = this.map.get(user);
    if (!e) { e = {}; this.map.set(user, e); }
    return e;
  }
  getSessionId(user) { return this.map.get(user)?.sessionId; }
  setSessionId(user, id) { this._entry(user).sessionId = id; }
  getContextToken(user) { return this.map.get(user)?.contextToken; }
  setContextToken(user, token) { this._entry(user).contextToken = token; }
  getPending(user) { return this.map.get(user)?.pending ?? null; }
  setPending(user, payload) { this._entry(user).pending = payload; }
  clearPending(user) { const e = this.map.get(user); if (e) e.pending = null; }
}
