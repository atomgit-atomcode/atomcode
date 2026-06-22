[Solidity] This batch contains Solidity code; pay extra attention to (high-stakes — audit carefully):

#### security (critical)
- Reentrancy: external call before state update (follow checks-effects-interactions or use a reentrancy guard)
- Access control: missing `onlyOwner` / role checks on sensitive functions; unprotected `selfdestruct` / `delegatecall`
- Unchecked low-level call return values (`call` / `send`); arbitrary `delegatecall` target
- Integer overflow/underflow (use Solidity 0.8+ checked math or SafeMath); audit `unchecked { }` blocks

#### economic / logic
- Price/oracle manipulation, flash-loan assumptions; front-running / MEV (no commit-reveal where needed)
- `tx.origin` used for auth (use `msg.sender`); `block.timestamp` / `blockhash` used as randomness

#### robustness
- Gas: unbounded loops over dynamic arrays (DoS), storage writes inside loops; missing event emission for state changes
- Funds locked: no withdrawal path; rounding that favors the caller; uninitialized storage pointers
